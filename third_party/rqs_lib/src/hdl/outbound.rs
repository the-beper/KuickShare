use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Duration;

use anyhow::anyhow;
use bytes::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use libaes::{AES_256_KEY_LEN, Cipher};
use p256::ecdh::diffie_hellman;
use p256::elliptic_curve::sec1::{FromSec1Point, ToSec1Point};
use p256::{PublicKey, Sec1Point};
use prost::Message;
use rand::RngExt;
use sha2::{Digest, Sha256, Sha512};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast::error::TryRecvError;
use tokio::sync::broadcast::{Receiver, Sender};

use super::info::{InternalFileInfo, TransferMetadata, TransferPayload, TransferPayloadKind};
use super::{InnerState, MigratableStream, TextPayloadInfo, TextPayloadType, TransferState};
use crate::channel::{self, ChannelMessage, MessageClient, TransferAction, TransferKind};
use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::upgrade_path_info::Medium;
use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::{
    EventType, UpgradePathInfo,
};
use crate::location_nearby_connections::connection_response_frame::ResponseStatus;
use crate::location_nearby_connections::payload_transfer_frame::{
    PacketType, PayloadChunk, PayloadHeader, payload_header,
};
use crate::location_nearby_connections::{KeepAliveFrame, OfflineFrame, PayloadTransferFrame};
use crate::securegcm::ukey2_alert::AlertType;
use crate::securegcm::ukey2_client_init::CipherCommitment;
use crate::securegcm::{
    DeviceToDeviceMessage, GcmMetadata, Type, Ukey2Alert, Ukey2ClientFinished, Ukey2ClientInit,
    Ukey2HandshakeCipher, Ukey2Message, Ukey2ServerInit, ukey2_message,
};
use crate::securemessage::{
    EcP256PublicKey, EncScheme, GenericPublicKey, Header, HeaderAndBody, PublicKeyType,
    SecureMessage, SigScheme,
};
use crate::sharing_nearby::{
    FileMetadata, IntroductionFrame, TextMetadata, file_metadata, paired_key_result_frame,
};
use crate::upgrade::{self, Reply, UpgradeEvent, UpgradeState};
use crate::utils::{
    DeviceType, RemoteDeviceInfo, create_text_preview, decode_point, encode_point,
    gen_ecdsa_keypair, gen_random, hkdf_extract_expand, read_frame, to_four_digit_string,
};
use crate::{DEVICE_NAME, location_nearby_connections, sharing_nearby};

type HmacSha256 = Hmac<Sha256>;

const SANE_FRAME_LENGTH: i32 = 5 * 1024 * 1024;
const SANITY_DURATION: Duration = Duration::from_micros(10);
/// Pause before the first encrypted frame of a Bluetooth session (see `process_connection_response`).
const BLE_ENCRYPTION_SETTLE: Duration = Duration::from_millis(200);
/// Peer frames read while a chunk is written and kept for the next `handle()`; beyond this
/// only the frames the transfer needs (upgrade negotiation) are still kept, the rest is dropped.
const MAX_PENDING_FRAMES: usize = 32;
/// Hard bound on the queue, whatever the frame, so a flooding peer cannot grow memory.
const MAX_PENDING_FRAMES_HARD: usize = 256;

#[derive(Clone)]
pub enum OutboundPayload {
    Files(Vec<String>),
    Text {
        text: String,
        /// Doesn't support WiFi outbound payload yet due to default SSID and SecurityType
        text_type: TextPayloadType,
    },
}

/// Hand-written so that no `Debug` output (a log line, a `{:?}` of a `SendInfo`) shows the
/// text being sent or the paths of the files.
impl std::fmt::Debug for OutboundPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Files(files) => f
                .debug_struct("Files")
                .field("count", &files.len())
                .finish(),
            Self::Text { text, text_type } => f
                .debug_struct("Text")
                .field("len", &text.len())
                .field("text_type", text_type)
                .finish(),
        }
    }
}

#[derive(Debug)]
pub struct OutboundRequest {
    endpoint_id: [u8; 4],
    socket: MigratableStream,
    /// Partially received frame (length prefix, then body), kept across cancelled reads.
    length_buf: [u8; 4],
    length_filled: usize,
    frame_buf: Vec<u8>,
    frame_filled: usize,
    /// Keep-alives ignored while the handshake waits for the ConnectionResponse.
    keep_alives_ignored: u8,
    /// Frames are plain `[length][OfflineFrame]` (no SecureMessage): set once the upgrade
    /// to a channel with disabled encryption completes.
    plain_frames: bool,
    /// Decoded frames read while a chunk was written (keep-alives, upgrade frames, ...), in
    /// arrival order. They are already decrypted, so `handle()` only has to process them.
    pending_frames: VecDeque<OfflineFrame>,
    /// The peer cancelled while we were sending: `send_payloads` ends the session with `NotAnError`.
    peer_cancelled: bool,
    /// The user cancelled and `cancel_peer` has closed the session: `send_payloads` ends it
    /// with `NotAnError` instead of leaving the cancel message to a second `handle()`.
    user_cancelled: bool,
    /// FILE payloads whose bytes are fully on the wire but that the peer has not confirmed
    /// with a PAYLOAD_ACK yet. Only filled for a peer at safe-to-disconnect level 6 or more;
    /// while it is non empty a disconnect is a failed transfer (see `await_file_acks`).
    unacked_files: HashSet<i64>,
    pub state: InnerState,
    sender: Sender<ChannelMessage>,
    receiver: Receiver<ChannelMessage>,
    payload: OutboundPayload,
    upgrade: UpgradeState,
    /// The offer being followed, echoed back if joining fails.
    offer: Option<UpgradePathInfo>,
    /// Mediums of the connection request instead of the ones the upgrade link implies.
    mediums: Option<Vec<i32>>,
}

impl OutboundRequest {
    pub fn new(
        endpoint_id: [u8; 4],
        socket: TcpStream,
        id: String,
        sender: Sender<ChannelMessage>,
        payload: OutboundPayload,
        rdi: RemoteDeviceInfo,
    ) -> Self {
        let mut request = Self::new_over(
            endpoint_id,
            MigratableStream::Tcp(socket),
            id,
            sender,
            payload,
            rdi,
        );
        // The peer is already on a network (the LAN): no Wi-Fi upgrade is announced or taken.
        request.upgrade = UpgradeState::new(None);
        request
    }

    pub fn new_over(
        endpoint_id: [u8; 4],
        socket: MigratableStream,
        id: String,
        sender: Sender<ChannelMessage>,
        payload: OutboundPayload,
        rdi: RemoteDeviceInfo,
    ) -> Self {
        let receiver = sender.subscribe();

        let transfer_metadata = match &payload {
            OutboundPayload::Files(files) => TransferMetadata {
                source: Some(rdi),
                payload_kind: TransferPayloadKind::Files,
                payload: Some(TransferPayload::Files(files.to_vec())),
                id: Default::default(),
                pin_code: Default::default(),
                payload_preview: Default::default(),
                total_bytes: Default::default(),
                ack_bytes: Default::default(),
                saved_paths: Vec::new(),
                refusal: None,
            },
            OutboundPayload::Text { text, text_type } => {
                let (payload_kind, transfer_payload) = match text_type {
                    TextPayloadType::Url => {
                        (TransferPayloadKind::Url, TransferPayload::Url(text.clone()))
                    }
                    TextPayloadType::Text => (
                        TransferPayloadKind::Text,
                        TransferPayload::Text(text.clone()),
                    ),
                    TextPayloadType::Wifi => (
                        TransferPayloadKind::Text,
                        TransferPayload::Text(text.clone()),
                    ),
                };
                TransferMetadata {
                    source: Some(rdi),
                    payload_kind,
                    payload: Some(transfer_payload),
                    id: Default::default(),
                    pin_code: Default::default(),
                    payload_preview: Some(create_text_preview(text, 32).clone()),
                    total_bytes: text.len() as u64,
                    ack_bytes: Default::default(),
                    saved_paths: Vec::new(),
                    refusal: None,
                }
            }
        };

        Self {
            endpoint_id,
            socket,
            length_buf: [0; 4],
            length_filled: 0,
            frame_buf: Vec::new(),
            frame_filled: 0,
            keep_alives_ignored: 0,
            plain_frames: false,
            pending_frames: VecDeque::new(),
            peer_cancelled: false,
            user_cancelled: false,
            unacked_files: HashSet::new(),
            state: InnerState {
                id,
                server_seq: 0,
                client_seq: 0,
                state: TransferState::Initial,
                encryption_done: true,
                transfer_metadata: Some(transfer_metadata),
                ..Default::default()
            },
            sender,
            receiver,
            payload,
            upgrade: UpgradeState::new(crate::wifi_link()),
            offer: None,
            mediums: None,
        }
    }

    /// Call before [`Self::send_connection_request`].
    pub fn set_mediums(&mut self, mediums: Vec<i32>) {
        self.mediums = Some(mediums);
    }

    pub async fn handle(&mut self) -> Result<(), anyhow::Error> {
        // Frames that arrived while a chunk was being written come first, in order.
        if self.replay_pending_frames().await? {
            if self.upgrade.take_hold_released() {
                self.send_payloads().await?;
            }
            return Ok(());
        }

        tokio::select! {
            i = self.receiver.recv() => {
                match i {
                    Ok(channel_msg) => {
                        if channel_msg.id != self.state.id {
                            return Ok(());
                        }

                        if let channel::Message::Lib { action }  = &channel_msg.msg {
                            debug!("outbound: got: {:?}", channel_msg);
                            match action {
                                TransferAction::TransferCancel => {
                                    self.update_state(
                                        |e| {
                                            e.state = TransferState::Cancelled;
                                        },
                                        true,
                                    ).await;
                                    self.cancel_peer(&[]).await;
                                    return Err(anyhow!(crate::errors::AppError::NotAnError));
                                },
                                _ => {}
                            }
                        }
                    }
                    Err(e) => {
                        error!("inbound: channel error: {}", e);
                    }
                }
            },
            h = read_frame(
                &mut self.socket,
                &mut self.length_buf,
                &mut self.length_filled,
                &mut self.frame_buf,
                &mut self.frame_filled,
                SANE_FRAME_LENGTH as usize,
            ) => {
                self._handle(h?).await?
            },
            ev = self.upgrade.next_event() => {
                self.on_upgrade_event(ev).await?;
            }
        }

        if self.upgrade.take_hold_released() {
            self.send_payloads().await?;
        }

        Ok(())
    }

    /// Processes the frames kept by `send_unless_cancelled`. Returns whether there were any.
    async fn replay_pending_frames(&mut self) -> Result<bool, anyhow::Error> {
        let replayed = !self.pending_frames.is_empty();
        while let Some(offline) = self.pending_frames.pop_front() {
            self.process_offline_frame(&offline).await?;
        }
        Ok(replayed)
    }

    pub async fn _handle(&mut self, frame_data: Vec<u8>) -> Result<(), anyhow::Error> {
        let current_state = &self.state;
        // Now determine what will be the request type based on current state
        match current_state.state {
            TransferState::SentUkeyClientInit => {
                debug!("Handling State::SentUkeyClientInit frame");
                let msg = Ukey2Message::decode(&*frame_data)?;
                self.update_state(
                    |e| {
                        e.server_init_data = Some(frame_data);
                    },
                    false,
                )
                .await;
                self.process_ukey2_server_init(&msg).await?;

                // Advance current state
                self.update_state(
                    |e: &mut InnerState| {
                        e.state = TransferState::SentUkeyClientFinish;
                        e.encryption_done = true;
                    },
                    false,
                )
                .await;
            }
            TransferState::SentUkeyClientFinish => {
                debug!("Handling State::SentUkeyClientFinish frame");
                let frame = location_nearby_connections::OfflineFrame::decode(&*frame_data)?;
                if crate::utils::is_keep_alive(&frame) {
                    self.keep_alives_ignored += 1;
                    if self.keep_alives_ignored > crate::utils::MAX_HANDSHAKE_KEEP_ALIVES {
                        return Err(anyhow!(
                            "The peer only sent keep-alives instead of a ConnectionResponse"
                        ));
                    }
                    debug!("Ignoring a keep-alive while waiting for the ConnectionResponse");
                    return Ok(());
                }
                self.process_connection_response(&frame).await?;

                // Advance current state
                self.update_state(
                    |e: &mut InnerState| {
                        e.state = TransferState::SentPairedKeyEncryption;
                        e.server_init_data = Some(frame_data);
                        e.encryption_done = true;
                    },
                    false,
                )
                .await;
            }
            _ if self.plain_frames => {
                trace!("Handling plain OfflineFrame");
                let offline = location_nearby_connections::OfflineFrame::decode(&*frame_data)?;
                self.process_offline_frame(&offline).await?;
            }
            _ => {
                trace!("Handling SecureMessage frame");
                let smsg = SecureMessage::decode(&*frame_data)?;
                self.decrypt_and_process_secure_message(&smsg).await?;
            }
        }

        Ok(())
    }

    pub async fn send_connection_request(&mut self) -> Result<(), anyhow::Error> {
        let device_name = DEVICE_NAME.read().unwrap().clone();
        // The query is capped: a stuck NM D-Bus call must not delay the handshake.
        let has_upgrade = self.mediums.is_none() && self.upgrade.enabled();
        let supports_5ghz = if has_upgrade {
            match self.upgrade.link() {
                Some(link) => {
                    tokio::time::timeout(Duration::from_millis(500), link.supports_5ghz())
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or(true)
                }
                None => true,
            }
        } else {
            true
        };
        let request = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::ConnectionRequest.into(),
                ),
                connection_request: Some(location_nearby_connections::ConnectionRequestFrame {
                    endpoint_id: Some(String::from_utf8_lossy(&self.endpoint_id).to_string()),
                    endpoint_name: Some(device_name.clone().into()),
                    endpoint_info: Some(
                        RemoteDeviceInfo {
                            name: device_name.clone().into(),
                            device_type: DeviceType::Laptop,
                        }
                        .serialize(),
                    ),
                    mediums: if let Some(m) = &self.mediums {
                        m.clone()
                    } else if self.upgrade.enabled() {
                        // The receiver hosts the network we join; the LAN is where we are already.
                        vec![Medium::WifiHotspot.into(), Medium::WifiDirect.into()]
                    } else {
                        vec![Medium::WifiLan.into()]
                    },
                    medium_metadata: has_upgrade.then(|| {
                        location_nearby_connections::MediumMetadata {
                            supports_5_ghz: Some(supports_5ghz),
                            ap_frequency: Some(-1),
                            ..Default::default()
                        }
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };

        self.send_frame(request.encode_to_vec()).await?;

        Ok(())
    }

    pub async fn send_ukey2_client_init(&mut self) -> Result<(), anyhow::Error> {
        let (secret_key, public_key) = gen_ecdsa_keypair();

        let encoded_point = public_key.to_sec1_point(false);
        let x = encoded_point.x().unwrap();
        let y = encoded_point.y().unwrap();

        let pkey = GenericPublicKey {
            r#type: PublicKeyType::EcP256.into(),
            ec_p256_public_key: Some(EcP256PublicKey {
                x: encode_point(Bytes::from(x.to_vec()))?,
                y: encode_point(Bytes::from(y.to_vec()))?,
            }),
            ..Default::default()
        };

        let finish_frame = Ukey2Message {
            message_type: Some(ukey2_message::Type::ClientFinish.into()),
            message_data: Some(
                Ukey2ClientFinished {
                    public_key: Some(pkey.encode_to_vec()),
                }
                .encode_to_vec(),
            ),
        };

        let sha512 = Sha512::digest(finish_frame.encode_to_vec());
        let frame = Ukey2Message {
            message_type: Some(ukey2_message::Type::ClientInit.into()),
            message_data: Some(
                Ukey2ClientInit {
                    version: Some(1),
                    random: Some(gen_random(32)),
                    next_protocol: Some(String::from("AES_256_CBC-HMAC_SHA256")),
                    cipher_commitments: vec![CipherCommitment {
                        handshake_cipher: Some(Ukey2HandshakeCipher::P256Sha512.into()),
                        commitment: Some(sha512.to_vec()),
                    }],
                }
                .encode_to_vec(),
            ),
        };

        self.send_frame(frame.encode_to_vec()).await?;

        self.update_state(
            |e| {
                e.state = TransferState::SentUkeyClientInit;
                e.private_key = Some(secret_key);
                e.public_key = Some(public_key);
                e.client_init_msg_data = Some(frame.encode_to_vec());
                e.ukey_client_finish_msg_data = Some(finish_frame.encode_to_vec());
            },
            false,
        )
        .await;

        Ok(())
    }

    async fn process_ukey2_server_init(&mut self, msg: &Ukey2Message) -> Result<(), anyhow::Error> {
        if msg.message_type() != ukey2_message::Type::ServerInit {
            self.send_ukey2_alert(AlertType::BadMessageType).await?;
            return Err(anyhow!(
                "UKey2: message_type({:?}) != ServerInit",
                msg.message_type
            ));
        }

        let server_init = match Ukey2ServerInit::decode(msg.message_data()) {
            Ok(uk2si) => uk2si,
            Err(e) => {
                return Err(anyhow!("UKey2: Ukey2ClientFinished::decode: {}", e));
            }
        };

        if server_init.version() != 1 {
            self.send_ukey2_alert(AlertType::BadVersion).await?;
            return Err(anyhow!("UKey2: server_init.version != 1"));
        }

        if server_init.random().len() != 32 {
            self.send_ukey2_alert(AlertType::BadRandom).await?;
            return Err(anyhow!("UKey2: server_init.random.len != 32"));
        }

        if server_init.handshake_cipher() != Ukey2HandshakeCipher::P256Sha512 {
            self.send_ukey2_alert(AlertType::BadHandshakeCipher).await?;
            return Err(anyhow!("UKey2: handshake_cipher != P256Sha512"));
        }

        let server_public_key = match GenericPublicKey::decode(server_init.public_key()) {
            Ok(spk) => spk,
            Err(e) => {
                return Err(anyhow!("UKey2: GenericPublicKey::decode: {}", e));
            }
        };

        self.finalize_key_exchange(server_public_key).await?;
        self.send_frame(self.state.ukey_client_finish_msg_data.clone().unwrap())
            .await?;

        let frame = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::ConnectionResponse.into(),
                ),
                connection_response: Some(location_nearby_connections::ConnectionResponseFrame {
					response: Some(location_nearby_connections::connection_response_frame::ResponseStatus::Accept.into()),
					// The level this sender answers at: it reads PAYLOAD_ACK for the files
					// it sends, and a disconnect before them fails the transfer.
					safe_to_disconnect_version: Some(6),
					os_info: Some(location_nearby_connections::OsInfo {
						r#type: Some(location_nearby_connections::os_info::OsType::Linux.into())
					}),
					..Default::default()
				}),
                ..Default::default()
            }),
        };

        self.send_frame(frame.encode_to_vec()).await?;

        Ok(())
    }

    async fn process_connection_response(
        &mut self,
        frame: &location_nearby_connections::OfflineFrame,
    ) -> Result<(), anyhow::Error> {
        let v1_frame = frame
            .v1
            .as_ref()
            .ok_or_else(|| anyhow!("Missing required fields"))?;

        if v1_frame.r#type() != location_nearby_connections::v1_frame::FrameType::ConnectionResponse
        {
            return Err(anyhow!(format!(
                "Unexpected frame type: {:?}",
                v1_frame.r#type()
            )));
        }

        if v1_frame.connection_response.is_none() {
            return Err(anyhow!(format!("Unexpected None connection_response",)));
        }

        if v1_frame.connection_response.as_ref().unwrap().response() != ResponseStatus::Accept {
            return Err(anyhow!(format!("Connection rejected by third party",)));
        }

        // The peer's safe-to-disconnect level: six or more means it answers our sent FILE
        // payloads with PAYLOAD_ACK, and a disconnect before them is a failed transfer.
        self.state.peer_disconnect_version = v1_frame
            .connection_response
            .as_ref()
            .and_then(|r| r.safe_to_disconnect_version)
            .unwrap_or(0)
            .max(0);
        debug!(
            "peer safe-to-disconnect version: {}",
            self.state.peer_disconnect_version
        );

        let paired_encryption = sharing_nearby::Frame {
            version: Some(sharing_nearby::frame::Version::V1.into()),
            v1: Some(sharing_nearby::V1Frame {
                r#type: Some(sharing_nearby::v1_frame::FrameType::PairedKeyEncryption.into()),
                paired_key_encryption: Some(sharing_nearby::PairedKeyEncryptionFrame {
                    secret_id_hash: Some(gen_random(6)),
                    signed_data: Some(gen_random(72)),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };

        // Over Bluetooth the phone switches its reading side to the encrypted channel a few
        // milliseconds after it sends its own ConnectionResponse; a first encrypted frame that
        // arrives before that is parsed as a plain frame and the phone drops the session with
        // "Incorrect sequence number" (seen on a Galaxy A55 in 2 of 3 sends, inferred from its log).
        if matches!(self.socket, MigratableStream::Duplex(_)) {
            tokio::time::sleep(BLE_ENCRYPTION_SETTLE).await;
        }
        self.send_encrypted_frame(&paired_encryption).await?;

        Ok(())
    }

    async fn decrypt_and_process_secure_message(
        &mut self,
        smsg: &SecureMessage,
    ) -> Result<(), anyhow::Error> {
        let offline = Self::decrypt_secure_message(&mut self.state, smsg)?;
        self.process_offline_frame(&offline).await
    }

    /// Verifies and decrypts a SecureMessage from the peer, consuming one client sequence number.
    /// Takes the state instead of `self` so it can run while the socket is borrowed.
    fn decrypt_secure_message(
        state: &mut InnerState,
        smsg: &SecureMessage,
    ) -> Result<OfflineFrame, anyhow::Error> {
        let recv_hmac_key = state
            .recv_hmac_key
            .as_ref()
            .ok_or_else(|| anyhow!("Missing the receive HMAC key"))?;
        let mut hmac = HmacSha256::new_from_slice(recv_hmac_key)?;
        hmac.update(&smsg.header_and_body);
        if !hmac
            .finalize()
            .into_bytes()
            .as_slice()
            .eq(smsg.signature.as_slice())
        {
            return Err(anyhow!("hmac!=signature"));
        }

        let header_and_body = HeaderAndBody::decode(&*smsg.header_and_body)?;

        let msg_data = header_and_body.body;
        let key = state
            .decrypt_key
            .as_ref()
            .ok_or_else(|| anyhow!("Missing the decryption key"))?;

        let mut cipher = Cipher::new_256(key[..AES_256_KEY_LEN].try_into()?);
        cipher.set_auto_padding(true);
        let decrypted = cipher.cbc_decrypt(header_and_body.header.iv(), &msg_data);

        let d2d_msg = DeviceToDeviceMessage::decode(&*decrypted)?;

        state.client_seq += 1;
        let seq = state.client_seq;
        if d2d_msg.sequence_number() != seq {
            return Err(anyhow!(
                "Error d2d_msg.sequence_number invalid ({} vs {})",
                d2d_msg.sequence_number(),
                seq
            ));
        }

        Ok(location_nearby_connections::OfflineFrame::decode(
            d2d_msg.message(),
        )?)
    }

    async fn process_offline_frame(&mut self, offline: &OfflineFrame) -> Result<(), anyhow::Error> {
        let v1_frame = offline
            .v1
            .as_ref()
            .ok_or_else(|| anyhow!("Missing required fields"))?;
        match v1_frame.r#type() {
            location_nearby_connections::v1_frame::FrameType::PayloadTransfer => {
                trace!("Received FrameType::PayloadTransfer");
                let payload_transfer = v1_frame
                    .payload_transfer
                    .as_ref()
                    .ok_or_else(|| anyhow!("Missing required fields"))?;

                if payload_transfer.packet_type() == PacketType::PayloadAck {
                    match Self::payload_ack_id(offline) {
                        Some(id) => self.note_payload_ack(id),
                        None => debug!("payload ack without a payload header"),
                    }
                    return Ok(());
                }

                let header = payload_transfer
                    .payload_header
                    .as_ref()
                    .ok_or_else(|| anyhow!("Missing required fields"))?;
                let chunk = payload_transfer
                    .payload_chunk
                    .as_ref()
                    .ok_or_else(|| anyhow!("Missing required fields"))?;

                match header.r#type() {
                    payload_header::PayloadType::Bytes => {
                        trace!("Processing PayloadType::Bytes");
                        // `None` until the last chunk; the payload is then forgotten here.
                        let Some(buffer) = crate::safety::assemble(
                            &mut self.state.payload_buffers,
                            header,
                            chunk,
                            &crate::safety::Limits::default(),
                        )?
                        else {
                            return Ok(());
                        };

                        debug!("Bytes payload complete");
                        let inner_frame = sharing_nearby::Frame::decode(buffer.as_slice())?;
                        self.process_transfer_setup(&inner_frame).await?;
                    }
                    payload_header::PayloadType::File => {
                        error!("Unhandled PayloadType::File: {:?}", header.r#type())
                    }
                    payload_header::PayloadType::Stream => {
                        error!("Unhandled PayloadType::Stream: {:?}", header.r#type())
                    }
                    payload_header::PayloadType::UnknownPayloadType => {
                        error!(
                            "Invalid PayloadType::UnknownPayloadType: {:?}",
                            header.r#type()
                        )
                    }
                }
            }
            location_nearby_connections::v1_frame::FrameType::KeepAlive => {
                trace!("Sending keepalive");
                self.send_keepalive(true).await?;
            }
            location_nearby_connections::v1_frame::FrameType::BandwidthUpgradeNegotiation => {
                self.process_bwu_frame(offline).await?;
            }
            location_nearby_connections::v1_frame::FrameType::Disconnection => {
                let request = is_safe_disconnect_request(offline);
                if request {
                    self.encrypt_and_send(&safe_disconnect_ack_frame()).await?;
                }
                debug!("The peer is disconnecting");
                // After the transfer ended this is the handshake's last frame: the session is
                // over, and waiting for the peer's FIN only logs a lost connection. A request
                // crossed the one we sent at the end: the ack to ours is read first, or it
                // stays unread at a peer that waits to hand it over (a BLE bridge, 5 s).
                if matches!(
                    self.state.state,
                    TransferState::Finished | TransferState::Cancelled
                ) {
                    if request && self.state.peer_disconnect_version >= 1 {
                        self.finish_safe_disconnect(tokio::time::Instant::now() + CANCEL_TIMEOUT)
                            .await;
                    }
                    return Err(anyhow!(crate::errors::AppError::NotAnError));
                }
            }
            other => {
                error!("Unhandled offline frame encrypted: {:?}", other);
            }
        }

        Ok(())
    }

    /// The payload id of a `PAYLOAD_ACK`; `None` for any other frame, or an ack without a
    /// header.
    fn payload_ack_id(offline: &OfflineFrame) -> Option<i64> {
        let v1 = offline.v1.as_ref()?;
        let transfer = v1.payload_transfer.as_ref()?;
        if v1.r#type() != location_nearby_connections::v1_frame::FrameType::PayloadTransfer
            || transfer.packet_type() != PacketType::PayloadAck
        {
            return None;
        }
        transfer.payload_header.as_ref().map(|h| h.id())
    }

    /// One ack per sent FILE payload: the peer confirmed the delivery of those bytes
    /// (crQuickShare `AckFile`). An id that is not owed (a repeat, a Bytes payload) changes
    /// nothing.
    fn note_payload_ack(&mut self, id: i64) {
        if self.unacked_files.remove(&id) {
            debug!("a file payload is acknowledged by the peer");
        }
    }

    async fn process_transfer_setup(
        &mut self,
        frame: &sharing_nearby::Frame,
    ) -> Result<(), anyhow::Error> {
        let v1_frame = frame
            .v1
            .as_ref()
            .ok_or_else(|| anyhow!("Missing required fields"))?;

        if v1_frame.r#type() == sharing_nearby::v1_frame::FrameType::Cancel {
            info!("Transfer canceled");
            self.update_state(
                |e| {
                    e.state = TransferState::Cancelled;
                },
                true,
            )
            .await;
            self.disconnection().await?;
            self.close_after_request().await;
            return Err(anyhow!(crate::errors::AppError::NotAnError));
        }

        match self.state.state {
            TransferState::SentPairedKeyEncryption => {
                debug!("Processing State::SentPairedKeyEncryption");
                self.process_paired_key_encryption_frame(v1_frame).await?;
                self.update_state(
                    |e| {
                        e.state = TransferState::SentPairedKeyResult;
                    },
                    false,
                )
                .await;
            }
            TransferState::SentPairedKeyResult => {
                debug!("Processing State::SentPairedKeyResult");
                self.process_paired_key_result(v1_frame).await?;
                self.update_state(
                    |e| {
                        e.state = TransferState::SentIntroduction;
                    },
                    true,
                )
                .await;
            }
            TransferState::SentIntroduction => {
                debug!("Processing State::SentIntroduction");
                self.process_consent(v1_frame).await?;
            }
            TransferState::SendingFiles => {}
            _ => {
                info!(
                    "Unhandled connection state in process_transfer_setup: {:?}",
                    self.state.state
                );
            }
        }

        Ok(())
    }

    async fn process_paired_key_encryption_frame(
        &mut self,
        v1_frame: &sharing_nearby::V1Frame,
    ) -> Result<(), anyhow::Error> {
        if v1_frame.paired_key_encryption.is_none() {
            return Err(anyhow!("Missing required fields"));
        }

        let paired_result = sharing_nearby::Frame {
            version: Some(sharing_nearby::frame::Version::V1.into()),
            v1: Some(sharing_nearby::V1Frame {
                r#type: Some(sharing_nearby::v1_frame::FrameType::PairedKeyResult.into()),
                paired_key_result: Some(sharing_nearby::PairedKeyResultFrame {
                    status: Some(paired_key_result_frame::Status::Unable.into()),
                }),
                ..Default::default()
            }),
        };

        self.send_encrypted_frame(&paired_result).await?;

        Ok(())
    }

    async fn process_paired_key_result(
        &mut self,
        v1_frame: &sharing_nearby::V1Frame,
    ) -> Result<(), anyhow::Error> {
        if v1_frame.paired_key_result.is_none() {
            return Err(anyhow!("Missing required fields"));
        }

        let mut file_metadata: Vec<FileMetadata> = vec![];
        let mut text_metadata: Vec<TextMetadata> = vec![];
        let mut transferred_files: HashMap<i64, InternalFileInfo> = HashMap::new();
        let mut total_to_send: u64 = 0;
        let mut text_payload_info: Option<TextPayloadInfo> = None;

        match &self.payload {
            OutboundPayload::Files(files) => {
                for f in files {
                    let path = Path::new(f);
                    if !path.is_file() {
                        warn!("A path to send is not a file");
                        continue;
                    }

                    let file = match File::open(f) {
                        Ok(_f) => _f,
                        Err(e) => {
                            error!("Failed to open a file to send: {e}");
                            continue;
                        }
                    };
                    let fmetadata = match file.metadata() {
                        Ok(_fm) => _fm,
                        Err(e) => {
                            error!("Failed to get the metadata of a file to send: {e}");
                            continue;
                        }
                    };

                    let ftype = mime_guess::from_path(path)
                        .first_or_octet_stream()
                        .to_string();

                    let meta_type = if ftype.starts_with("image/") {
                        file_metadata::Type::Image
                    } else if ftype.starts_with("video/") {
                        file_metadata::Type::Video
                    } else if ftype.starts_with("audio/") {
                        file_metadata::Type::Audio
                    } else if path.extension().unwrap_or_default() == "apk" {
                        file_metadata::Type::App
                    } else {
                        file_metadata::Type::Unknown
                    };

                    info!("File type to send: {}", ftype);
                    let fname = utf8_file_name(path)?;
                    let fmeta = FileMetadata {
                        payload_id: Some(rand::rng().random::<i64>()),
                        name: Some(fname),
                        size: Some(fmetadata.size() as i64),
                        mime_type: Some(ftype),
                        r#type: Some(meta_type.into()),
                        ..Default::default()
                    };
                    transferred_files.insert(
                        fmeta.payload_id(),
                        InternalFileInfo {
                            payload_id: fmeta.payload_id(),
                            file_url: path.to_path_buf(),
                            bytes_transferred: 0,
                            total_size: fmeta.size(),
                            file: Some(file),
                        },
                    );
                    file_metadata.push(fmeta);
                    total_to_send += fmetadata.size();
                }
            }
            OutboundPayload::Text { text, text_type } => {
                let payload_id = rand::rng().random::<i64>();
                let text_bytes = text.as_bytes();
                total_to_send = text_bytes.len() as u64;

                text_payload_info = Some(TextPayloadInfo::from_type(text_type, payload_id));

                let tmeta = TextMetadata {
                    text_title: None,
                    r#type: Some(text_type.to_proto_type().into()),
                    payload_id: Some(payload_id),
                    size: Some(text_bytes.len() as i64),
                    id: Some(payload_id),
                };
                text_metadata.push(tmeta);

                info!(
                    "Text type to send: {:?}, size: {}",
                    text_type,
                    text_bytes.len()
                );
            }
        }

        self.update_state(
            |e| {
                if let Some(tmd) = e.transfer_metadata.as_mut() {
                    tmd.total_bytes = total_to_send;
                }
                e.transferred_files = transferred_files;
                if text_payload_info.is_some() {
                    e.text_payload = text_payload_info;
                }
            },
            false,
        )
        .await;

        let intro_frame = if text_metadata.is_empty() {
            IntroductionFrame {
                file_metadata,
                ..Default::default()
            }
        } else {
            IntroductionFrame {
                file_metadata,
                text_metadata,
                ..Default::default()
            }
        };

        let introduction = sharing_nearby::Frame {
            version: Some(sharing_nearby::frame::Version::V1.into()),
            v1: Some(sharing_nearby::V1Frame {
                r#type: Some(sharing_nearby::v1_frame::FrameType::Introduction.into()),
                introduction: Some(intro_frame),
                ..Default::default()
            }),
        };

        self.send_encrypted_frame(&introduction).await?;

        Ok(())
    }

    /// Starts joining the hotspot the receiver offered, in the background. Any failure
    /// leaves the session on the current channel.
    async fn follow_offer(&mut self, info: &UpgradePathInfo) {
        if !self.upgrade.enabled() {
            debug!("upgrade: no link set, ignoring offer");
            return;
        }
        let Some(link) = self.upgrade.link().filter(|_| self.upgrade.can_start()) else {
            debug!("upgrade: declining offer");
            let _ = self
                .encrypt_and_send(&upgrade::upgrade_failure_frame(Some(info.clone())))
                .await;
            return;
        };
        match upgrade::info_to_credentials(info) {
            Ok(creds) => {
                info!(
                    "upgrade: receiver offers {:?}: {} MHz, {}:{}",
                    info.medium(),
                    creds.frequency_mhz,
                    creds.gateway,
                    creds.port
                );
                let endpoint_id = String::from_utf8_lossy(&self.endpoint_id).into_owned();
                let task = tokio::spawn(upgrade::prepare_join(
                    link,
                    creds,
                    endpoint_id,
                    info.supports_client_introduction_ack(),
                    info.supports_disabling_encryption(),
                ));
                self.upgrade
                    .set_disable_encryption(info.supports_disabling_encryption());
                self.offer = Some(info.clone());
                self.upgrade.begin_preparing(task);
            }
            Err(e) => {
                warn!(
                    "upgrade: {:?} offer not possible ({e:#}); staying on the current channel",
                    info.medium()
                );
                self.upgrade.fail().await;
                self.send_upgrade_failure(info.clone()).await;
            }
        }
    }

    /// Tells the receiver to stop waiting for us; best effort.
    async fn send_upgrade_failure(&mut self, info: UpgradePathInfo) {
        let _ = self
            .encrypt_and_send(&upgrade::upgrade_failure_frame(Some(info)))
            .await;
    }

    async fn on_upgrade_event(&mut self, ev: UpgradeEvent) -> Result<(), anyhow::Error> {
        match ev {
            UpgradeEvent::Prepared(Ok(upgrade::Prepared::Joined(stream))) => {
                // A failure here is not recoverable on the prior channel: let the session end.
                self.encrypt_and_send(&upgrade::event_frame(EventType::LastWriteToPriorChannel))
                    .await?;
                self.offer = None;
                self.upgrade.begin_draining(stream);
            }
            UpgradeEvent::Prepared(Ok(
                upgrade::Prepared::Hosted(..) | upgrade::Prepared::HostedLan(..),
            )) => {}
            UpgradeEvent::Prepared(Err(e)) => {
                warn!("upgrade: not possible ({e:#}); staying on the current channel");
                self.upgrade.fail().await;
                if let Some(info) = self.offer.take() {
                    self.send_upgrade_failure(info).await;
                }
            }
            UpgradeEvent::DrainTimedOut => {
                if !self.upgrade.peer_last_write_seen() {
                    return Err(anyhow!("upgrade: no LAST_WRITE from the peer in time"));
                }
                warn!("upgrade: no SAFE_TO_CLOSE in time, switching anyway");
                self.finish_upgrade().await;
            }
            UpgradeEvent::OfferTimedOut => {
                debug!("upgrade: no offer from the receiver, sending on the current channel");
            }
            // Only the hosting side waits for a connection.
            UpgradeEvent::Connected(_) | UpgradeEvent::WaitTimedOut => {}
        }
        Ok(())
    }

    async fn process_bwu_frame(&mut self, frame: &OfflineFrame) -> Result<(), anyhow::Error> {
        let Some(bwu) = upgrade::parse_bwu(frame) else {
            return Ok(());
        };
        let event = bwu.event_type();
        match event {
            EventType::UpgradePathAvailable => match bwu.upgrade_path_info.as_ref() {
                Some(info) => self.follow_offer(info).await,
                None => debug!("upgrade: offer without path info"),
            },
            EventType::LastWriteToPriorChannel | EventType::SafeToClosePriorChannel => {
                if self.upgrade.on_peer_event(event) == Some(Reply::SendSafeToClose) {
                    self.encrypt_and_send(&upgrade::event_frame(
                        EventType::SafeToClosePriorChannel,
                    ))
                    .await?;
                }
                if self.upgrade.is_complete() {
                    self.finish_upgrade().await;
                }
            }
            other => debug!("upgrade: ignoring {other:?}"),
        }
        Ok(())
    }

    /// Closes the prior channel and continues on the new stream with the same keys and
    /// sequence numbers.
    async fn finish_upgrade(&mut self) {
        let Some(new) = self.upgrade.take_stream() else {
            return;
        };
        // Plaintext on purpose: it must not advance the sequence counters.
        let _ = self
            .send_frame(upgrade::plain_disconnection_frame().encode_to_vec())
            .await;
        // Let the peer read everything before the socket goes away.
        upgrade::close_gracefully(&mut self.socket).await;
        self.socket = MigratableStream::Tcp(new);
        // A frame half read on the prior channel does not continue on the new one.
        self.length_filled = 0;
        self.frame_filled = 0;
        self.plain_frames = self.upgrade.disable_encryption();
        if self.plain_frames {
            info!("upgrade: now on the hotspot, plain frames");
        } else {
            info!("upgrade: now on the hotspot");
        }
    }

    /// Sends the text or the files after the receiver accepted.
    async fn send_payloads(&mut self) -> Result<(), anyhow::Error> {
        // Handle sending Text
        if let Some(ref text_payload_info) = self.state.text_payload {
            let payload_id = text_payload_info.get_i64_value();

            if let OutboundPayload::Text { ref text, .. } = self.payload {
                let text_bytes = text.as_bytes().to_vec();
                let text_len = text_bytes.len() as i64;

                info!("Sending text payload, size: {}", text_len);

                let payload_header = PayloadHeader {
                    id: Some(payload_id),
                    r#type: Some(payload_header::PayloadType::Bytes.into()),
                    total_size: Some(text_len),
                    is_sensitive: Some(false),
                    ..Default::default()
                };

                // Send the text content
                let wrapper = location_nearby_connections::OfflineFrame {
                    version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
                    v1: Some(location_nearby_connections::V1Frame {
                        r#type: Some(
                            location_nearby_connections::v1_frame::FrameType::PayloadTransfer
                                .into(),
                        ),
                        payload_transfer: Some(PayloadTransferFrame {
                            packet_type: Some(PacketType::Data.into()),
                            payload_chunk: Some(PayloadChunk {
                                offset: Some(0),
                                flags: Some(0),
                                body: Some(text_bytes.clone()),
                                index: Some(0),
                            }),
                            payload_header: Some(payload_header.clone()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                };

                self.encrypt_and_send(&wrapper).await?;

                // Send lastChunk marker
                let wrapper = location_nearby_connections::OfflineFrame {
                    version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
                    v1: Some(location_nearby_connections::V1Frame {
                        r#type: Some(
                            location_nearby_connections::v1_frame::FrameType::PayloadTransfer
                                .into(),
                        ),
                        payload_transfer: Some(PayloadTransferFrame {
                            packet_type: Some(PacketType::Data.into()),
                            payload_chunk: Some(PayloadChunk {
                                offset: Some(text_len),
                                flags: Some(1), // lastChunk
                                body: Some(vec![]),
                                index: Some(1),
                            }),
                            payload_header: Some(payload_header),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                };

                self.encrypt_and_send(&wrapper).await?;

                // Update progress
                self.update_state(
                    |e| {
                        if let Some(tmd) = e.transfer_metadata.as_mut() {
                            tmd.ack_bytes = text_len as u64;
                        }
                    },
                    true,
                )
                .await;

                // Mark as finished
                info!("Text transfer finished");
                self.update_state(
                    |e| {
                        e.state = TransferState::Finished;
                    },
                    true,
                )
                .await;
                self.disconnection().await?;
                return Ok(());
            }
        }

        // Handle sending Files
        let ids: Vec<i64> = self.state.transferred_files.keys().cloned().collect();
        info!("We are sending: {:?}", ids);
        let mut ids_iter = ids.into_iter();

        // A write that blocks (the peer stopped reading) must not keep a cancel from the user.
        let mut cancel_rx = self.sender.subscribe();
        let transfer_id = self.state.id.clone();

        // Loop through all files
        'send_all_files: loop {
            let current = match ids_iter.next() {
                Some(i) => i,
                None => {
                    // The delivery counts once every sent file is acknowledged: wait for the
                    // peer's acks before reporting success. A disconnect before them is a
                    // failed transfer, like crQuickShare treats it.
                    self.await_file_acks(&mut cancel_rx).await?;
                    info!("All files have been transferred");
                    self.update_state(
                        |e| {
                            e.state = TransferState::Finished;
                        },
                        true,
                    )
                    .await;
                    self.disconnection().await?;
                    // Breaking instead of NotAnError to allow peacefull termination
                    break;
                }
            };

            // The position of the next chunk of this file, the last-chunk marker included: a
            // peer at safe-to-disconnect level 4 or more rejects a repeated index as a duplicate.
            let mut chunk_index: i32 = 0;
            // Loop until we reached end of file
            loop {
                // Since this task's runtime is blocked with the outer loop,
                // OutboundRequest::handle() will not be called again.
                // Thus, we need to check for cancellation here.
                match self.receiver.try_recv() {
                    Ok(channel_msg) => {
                        if channel_msg.id == self.state.id {
                            // TODO: if-let chains will be available in 1.88
                            if let channel::Message::Lib { action } = &channel_msg.msg {
                                debug!("outbound: got: {:?}", channel_msg);
                                match action {
                                    TransferAction::TransferCancel => {
                                        self.update_state(
                                            |e| {
                                                e.state = TransferState::Cancelled;
                                            },
                                            true,
                                        )
                                        .await;
                                        self.cancel_peer(&[]).await;
                                        break 'send_all_files;
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    Err(e) => {
                        match e {
                            TryRecvError::Empty => {}
                            e => {
                                error!("inbound: channel error: {}", e)
                            }
                        };
                    }
                };

                // Workaround to limit scope of the immutable borrow on self
                let (curr_state, buffer, bytes_read) = {
                    let curr_state = match self.state.transferred_files.get(&current) {
                        Some(s) => s,
                        None => break,
                    };

                    trace!("> Currently sending file {}", current);
                    if curr_state.bytes_transferred == curr_state.total_size {
                        debug!("File {current} finished");
                        self.update_state(
                            |e| {
                                e.transferred_files.remove(&current);
                            },
                            false,
                        )
                        .await;
                        break;
                    }

                    if curr_state.file.is_none() {
                        warn!("File {current} is none");
                        break;
                    }

                    let mut buffer = vec![0u8; 512 * 1024];
                    let bytes_read = curr_state.file.as_ref().unwrap().read(&mut buffer)?;

                    (
                        InternalFileInfo {
                            payload_id: curr_state.payload_id,
                            file_url: curr_state.file_url.clone(),
                            bytes_transferred: curr_state.bytes_transferred,
                            total_size: curr_state.total_size,
                            file: None,
                        },
                        buffer,
                        bytes_read,
                    )
                };

                let sending_buffer = buffer[..bytes_read].to_vec();
                trace!(
                    "> File ready: {bytes_read} bytes && {} && left to send: {} with current offset: {}",
                    sending_buffer.len(),
                    curr_state.total_size - curr_state.bytes_transferred,
                    curr_state.bytes_transferred
                );

                let payload_header = PayloadHeader {
                    id: Some(current),
                    r#type: Some(payload_header::PayloadType::File.into()),
                    total_size: Some(curr_state.total_size),
                    is_sensitive: Some(false),
                    file_name: curr_state
                        .file_url
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned()),
                    ..Default::default()
                };

                let wrapper = location_nearby_connections::OfflineFrame {
                    version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
                    v1: Some(location_nearby_connections::V1Frame {
                        r#type: Some(
                            location_nearby_connections::v1_frame::FrameType::PayloadTransfer
                                .into(),
                        ),
                        payload_transfer: Some(PayloadTransferFrame {
                            packet_type: Some(PacketType::Data.into()),
                            payload_chunk: Some(PayloadChunk {
                                offset: Some(curr_state.bytes_transferred),
                                flags: Some(0),
                                body: Some(buffer[..bytes_read].to_vec()),
                                index: Some(chunk_index),
                            }),
                            payload_header: Some(payload_header.clone()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                };

                if !self
                    .send_unless_cancelled(
                        &wrapper,
                        &mut cancel_rx,
                        &transfer_id,
                        SEND_STALL_TIMEOUT,
                    )
                    .await?
                {
                    break 'send_all_files;
                }
                chunk_index += 1;
                self.update_state(
                    |e| {
                        if let Some(mu) = e.transferred_files.get_mut(&current) {
                            mu.bytes_transferred += bytes_read as i64;
                        }

                        if let Some(tmd) = e.transfer_metadata.as_mut() {
                            tmd.ack_bytes += bytes_read as u64;
                        }
                    },
                    true,
                )
                .await;

                // If we just sent the last bytes of the file, mark it as finished
                if curr_state.bytes_transferred + bytes_read as i64 == curr_state.total_size {
                    debug!(
                        "File {current} finished, curr offset: {} over total: {}",
                        curr_state.bytes_transferred + bytes_read as i64,
                        curr_state.total_size
                    );

                    let wrapper = location_nearby_connections::OfflineFrame {
                        version: Some(
                            location_nearby_connections::offline_frame::Version::V1.into(),
                        ),
                        v1: Some(location_nearby_connections::V1Frame {
                            r#type: Some(
                                location_nearby_connections::v1_frame::FrameType::PayloadTransfer
                                    .into(),
                            ),
                            payload_transfer: Some(PayloadTransferFrame {
                                packet_type: Some(PacketType::Data.into()),
                                payload_chunk: Some(PayloadChunk {
                                    offset: Some(curr_state.total_size),
                                    flags: Some(1), // lastChunk
                                    body: Some(vec![]),
                                    index: Some(chunk_index),
                                }),
                                payload_header: Some(payload_header),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                    };

                    // The delivery counts once the peer acks. A disconnect before that ack
                    // fails the transfer (crQuickShare: "Peer disconnected before
                    // acknowledging files"). Only a peer that answers with acks at all is
                    // tracked, older peers end at the last chunk. Owed before the marker is
                    // written: its ack can be read while the write is still running.
                    if self.state.peer_disconnect_version >= 6 {
                        self.unacked_files.insert(current);
                    }
                    if !self
                        .send_unless_cancelled(
                            &wrapper,
                            &mut cancel_rx,
                            &transfer_id,
                            SEND_STALL_TIMEOUT,
                        )
                        .await?
                    {
                        break 'send_all_files;
                    }
                    break;
                }
            }
        }
        if self.peer_cancelled || self.user_cancelled {
            // Like the Cancel frame handled in `process_transfer_setup`: end the session now
            // instead of waiting for a peer that may never close the socket.
            return Err(anyhow!(crate::errors::AppError::NotAnError));
        }
        Ok(())
    }

    /// Waits for the peer's PAYLOAD_ACK of every file whose bytes are already sent, before
    /// the transfer is reported as finished. Keep-alives are answered meanwhile, other frames
    /// kept for `handle()`. A peer that disconnects with files still
    /// unacknowledged, or does not ack them within `ACK_WAIT`, fails the transfer: it may
    /// otherwise show "sending" for ever (crQuickShare fails the same way).
    async fn await_file_acks(
        &mut self,
        cancel_rx: &mut Receiver<ChannelMessage>,
    ) -> Result<(), anyhow::Error> {
        if self.unacked_files.is_empty() {
            return Ok(());
        }
        debug!(
            "waiting for {} file acks before finishing the transfer",
            self.unacked_files.len()
        );
        let deadline = tokio::time::Instant::now() + ACK_WAIT;
        // Taken out first: `ack_frame` queues again what it does not handle.
        for offline in std::mem::take(&mut self.pending_frames) {
            self.ack_frame(offline, deadline).await?;
        }
        let id = self.state.id.clone();
        let mut cancel = std::pin::pin!(wait_for_cancel(cancel_rx, &id));
        while !self.unacked_files.is_empty() {
            tokio::select! {
                () = &mut cancel => {
                    self.update_state(
                        |e| {
                            e.state = TransferState::Cancelled;
                        },
                        true,
                    )
                    .await;
                    self.cancel_peer(&[]).await;
                    return Err(anyhow!(crate::errors::AppError::NotAnError));
                }
                h = tokio::time::timeout_at(
                    deadline,
                    read_frame(
                        &mut self.socket,
                        &mut self.length_buf,
                        &mut self.length_filled,
                        &mut self.frame_buf,
                        &mut self.frame_filled,
                        SANE_FRAME_LENGTH as usize,
                    ),
                ) => {
                    match h {
                        Ok(Ok(frame_data)) => {
                            let offline =
                                Self::decode_peer_frame(&mut self.state, self.plain_frames, &frame_data)?;
                            self.ack_frame(offline, deadline).await?
                        }
                        Ok(Err(e)) if e.kind() == std::io::ErrorKind::InvalidData => {
                            return Err(anyhow!(e).context(
                                "the peer sent an invalid frame before acknowledging the files",
                            ));
                        }
                        Ok(Err(e)) => {
                            return Err(anyhow!(e).context(
                                "the peer disconnected before acknowledging the files",
                            ));
                        }
                        Err(_) => {
                            return Err(anyhow!(
                                "the peer did not acknowledge the files within {} s",
                                ACK_WAIT.as_secs()
                            ));
                        }
                    }
                }
                ev = self.upgrade.next_event() => {
                    self.on_upgrade_event(ev).await?;
                }
            }
        }
        Ok(())
    }

    /// Handles one decoded peer frame while the file acks are awaited, without the full
    /// state machine: only what can arrive here is acted on, anything else is kept for the
    /// next `handle()` (the state is still `SendingFiles`, which takes no sending action
    /// twice). This keeps `await_file_acks` out of the `process_offline_frame` call cycle,
    /// which an `async fn` must not recurse through.
    async fn ack_frame(
        &mut self,
        offline: OfflineFrame,
        deadline: tokio::time::Instant,
    ) -> Result<(), anyhow::Error> {
        if let Some(id) = Self::payload_ack_id(&offline) {
            self.note_payload_ack(id);
            return Ok(());
        }
        let frame_type = offline.v1.as_ref().map(|v1| v1.r#type());
        if frame_type == Some(location_nearby_connections::v1_frame::FrameType::Disconnection) {
            // The peer ended the session without acknowledging the files still open.
            return Err(anyhow!(
                "the peer disconnected before acknowledging the files"
            ));
        }
        if Self::is_cancel_frame(&offline) {
            info!("Transfer canceled by the peer while its files were unacknowledged");
            self.peer_cancelled = true;
            self.update_state(
                |e| {
                    e.state = TransferState::Cancelled;
                },
                true,
            )
            .await;
            return Err(anyhow!(crate::errors::AppError::NotAnError));
        }
        // A CONTROL frame for an owed file is the receiver cancelling or failing it: that
        // ack will not come.
        let refused = offline
            .v1
            .as_ref()
            .and_then(|v1| v1.payload_transfer.as_ref())
            .filter(|p| p.packet_type() == PacketType::Control)
            .and_then(|p| p.payload_header.as_ref())
            .is_some_and(|h| self.unacked_files.contains(&h.id()));
        if refused {
            return Err(anyhow!("the peer refused a file before acknowledging it"));
        }
        match frame_type {
            Some(location_nearby_connections::v1_frame::FrameType::KeepAlive) => {
                // Bounded by the wait: a peer that stopped reading must not hold the task.
                tokio::time::timeout_at(deadline, self.send_keepalive(true))
                    .await
                    .map_err(|_| {
                        anyhow!("the peer stopped reading before acknowledging the files")
                    })??;
            }
            // Upgrade frames included: the payload is held until an upgrade has ended
            // (`hold_payload`), so none is in flight here.
            _ => {
                // Kept for the next `handle()`, with the same bounds as during a send.
                if self.pending_frames.len() < MAX_PENDING_FRAMES
                    || (self.pending_frames.len() < MAX_PENDING_FRAMES_HARD
                        && !Self::is_droppable_frame(&offline))
                {
                    self.pending_frames.push_back(offline);
                } else {
                    warn!("Too many peer frames while waiting for file acks, dropping one");
                }
            }
        }
        Ok(())
    }

    async fn process_consent(
        &mut self,
        v1_frame: &sharing_nearby::V1Frame,
    ) -> Result<(), anyhow::Error> {
        if v1_frame.r#type() != sharing_nearby::v1_frame::FrameType::Response
            || v1_frame.connection_response.is_none()
        {
            return Err(anyhow!("Missing required fields"));
        }

        match v1_frame.connection_response.as_ref().unwrap().status() {
            sharing_nearby::connection_response_frame::Status::Accept => {
                info!("State is now State::SendingFiles");
                self.update_state(
                    |e| {
                        e.state = TransferState::SendingFiles;
                    },
                    true,
                )
                .await;

                if matches!(self.payload, OutboundPayload::Files(_)) && self.upgrade.needs_hold() {
                    // The receiver may offer a faster link right after accepting.
                    self.upgrade.hold_payload();
                    return Ok(());
                }
                self.send_payloads().await?;
            }
            sharing_nearby::connection_response_frame::Status::Reject
            | sharing_nearby::connection_response_frame::Status::NotEnoughSpace
            | sharing_nearby::connection_response_frame::Status::UnsupportedAttachmentType
            | sharing_nearby::connection_response_frame::Status::TimedOut => {
                warn!(
                    "Cannot process: consent denied: {:?}",
                    v1_frame.connection_response.as_ref().unwrap().status()
                );
                self.update_state(
                    |e| {
                        e.state = TransferState::Disconnected;
                    },
                    true,
                )
                .await;
                self.disconnection().await?;
                self.close_after_request().await;
                return Err(anyhow!(crate::errors::AppError::NotAnError));
            }
            sharing_nearby::connection_response_frame::Status::Unknown => {
                error!("Unknown consent type: aborting");
                self.update_state(
                    |e| {
                        e.state = TransferState::Disconnected;
                    },
                    true,
                )
                .await;
                self.disconnection().await?;
                self.close_after_request().await;
                return Err(anyhow!(crate::errors::AppError::NotAnError));
            }
        }

        Ok(())
    }

    /// Sends a payload frame. A write that cannot finish (the peer cancelled or stopped reading,
    /// so the socket buffers are full) would block this task for ever and with it the user's
    /// cancel: the write is raced against a cancel of this transfer and given `stall` to make
    /// progress. It is also raced against the peer's own frames, which are not read otherwise
    /// while chunks are written: a cancel or disconnection from the peer ends the send, and so
    /// does the loss of the connection. Any other frame (keep-alive, upgrade frame, payload ACK)
    /// is kept, decoded, and handled by the next `handle()`, up to `MAX_PENDING_FRAMES`.
    /// `Ok(false)` means the transfer was cancelled (by the user or the peer) and is over.
    async fn send_unless_cancelled(
        &mut self,
        frame: &OfflineFrame,
        cancel_rx: &mut Receiver<ChannelMessage>,
        transfer_id: &str,
        stall: Duration,
    ) -> Result<bool, anyhow::Error> {
        enum Outcome {
            Written(Result<std::io::Result<()>, tokio::time::error::Elapsed>),
            UserCancel,
            PeerCancel,
            /// The connection broke: closed, reset or timed out.
            Lost(std::io::Error),
            /// The peer sent an oversized or invalid frame.
            Invalid(std::io::Error),
            Failed(anyhow::Error),
        }

        // Needs `&mut self`, which the borrowed socket below rules out.
        let data = length_prefixed(&self.frame_payload(frame).await?);
        // How much of `data` is on the wire: a write dropped by the select below leaves it
        // known, so a cancel can tell a clean frame boundary from a frame cut in the middle.
        let mut written = 0;
        let outcome = {
            let (mut reader, mut writer) = tokio::io::split(&mut self.socket);
            let write = tokio::time::timeout(stall, async {
                while written < data.len() {
                    let n = writer.write(&data[written..]).await?;
                    if n == 0 {
                        return Err(std::io::ErrorKind::WriteZero.into());
                    }
                    written += n;
                }
                writer.flush().await
            });
            let mut write = std::pin::pin!(write);
            let mut cancel = std::pin::pin!(wait_for_cancel(cancel_rx, transfer_id));
            loop {
                tokio::select! {
                    // A finished write wins over a frame that is already waiting, so a peer that
                    // closes right after the last chunk does not turn a sent file into a cancel.
                    biased;
                    r = &mut write => break Outcome::Written(r),
                    () = &mut cancel => break Outcome::UserCancel,
                    // Cancel-safe: a frame cut short by the end of the write resumes in `handle`.
                    r = read_frame(
                        &mut reader,
                        &mut self.length_buf,
                        &mut self.length_filled,
                        &mut self.frame_buf,
                        &mut self.frame_filled,
                        SANE_FRAME_LENGTH as usize,
                    ) => match r {
                        Ok(peer_frame) => match Self::decode_peer_frame(
                            &mut self.state,
                            self.plain_frames,
                            &peer_frame,
                        ) {
                            Ok(offline) if Self::is_cancel_frame(&offline) => {
                                break Outcome::PeerCancel;
                            }
                            // Taken at once, never queued: a dropped ack would fail a
                            // delivered file (`note_payload_ack`; `self` is split here).
                            Ok(offline) if Self::payload_ack_id(&offline).is_some() => {
                                if let Some(id) = Self::payload_ack_id(&offline)
                                    && self.unacked_files.remove(&id)
                                {
                                    debug!("a file payload is acknowledged by the peer");
                                }
                            }
                            Ok(offline)
                                if self.pending_frames.len() < MAX_PENDING_FRAMES
                                    || (self.pending_frames.len() < MAX_PENDING_FRAMES_HARD
                                        && !Self::is_droppable_frame(&offline)) =>
                            {
                                self.pending_frames.push_back(offline);
                            }
                            Ok(_) => warn!("Too many peer frames during a send, dropping one"),
                            Err(e) => break Outcome::Failed(e),
                        },
                        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                            break Outcome::Invalid(e);
                        }
                        Err(e) => break Outcome::Lost(e),
                    },
                }
            }
        };
        match outcome {
            Outcome::Written(Ok(Ok(()))) => Ok(true),
            // Reset, broken pipe or end of stream: the manager moves the transfer to
            // `Disconnected` when this error comes out of `handle`.
            Outcome::Written(Ok(Err(e))) | Outcome::Lost(e) => {
                info!("Transfer lost the peer while sending: {e}");
                Err(anyhow!("lost the peer while sending: {e}"))
            }
            Outcome::Invalid(e) => {
                info!("Transfer got an invalid frame while sending: {e}");
                Err(anyhow!("the peer sent an invalid frame while sending: {e}"))
            }
            Outcome::Written(Err(_)) => {
                info!(
                    "Transfer lost the peer while sending: no progress for {}s",
                    stall.as_secs()
                );
                Err(anyhow!(
                    "the peer stopped reading: a write made no progress for {}s",
                    stall.as_secs()
                ))
            }
            Outcome::Failed(e) => Err(e),
            Outcome::PeerCancel => {
                info!("Transfer canceled by the peer while sending");
                self.peer_cancelled = true;
                // No disconnection frame: the peer is gone and the write may have been cut
                // in the middle of a frame.
                self.update_state(
                    |e| {
                        e.state = TransferState::Cancelled;
                    },
                    true,
                )
                .await;
                Ok(false)
            }
            Outcome::UserCancel => {
                self.update_state(
                    |e| {
                        e.state = TransferState::Cancelled;
                    },
                    true,
                )
                .await;
                // A frame cut in the middle must be completed before anything else goes out;
                // nothing sent yet (or all of it) is a clean boundary.
                let unsent = if written == 0 {
                    &[][..]
                } else {
                    &data[written..]
                };
                self.cancel_peer(unsent).await;
                Ok(false)
            }
        }
    }

    /// Decodes a frame read while sending, decrypting it (one client sequence number) if needed.
    fn decode_peer_frame(
        state: &mut InnerState,
        plain_frames: bool,
        data: &[u8],
    ) -> Result<OfflineFrame, anyhow::Error> {
        Ok(if plain_frames {
            OfflineFrame::decode(data)?
        } else {
            Self::decrypt_secure_message(state, &SecureMessage::decode(data)?)?
        })
    }

    /// Whether a frame read while sending can be lost when the queue is full: keep-alives and
    /// payload ACKs/progress. Upgrade negotiation and any other frame must be kept.
    fn is_droppable_frame(offline: &OfflineFrame) -> bool {
        offline.v1.as_ref().is_some_and(|v1| {
            matches!(
                v1.r#type(),
                location_nearby_connections::v1_frame::FrameType::KeepAlive
                    | location_nearby_connections::v1_frame::FrameType::PayloadTransfer
            )
        })
    }

    /// Whether a frame read while sending is the peer cancelling: a Disconnection, or a
    /// Cancel of the sharing protocol.
    fn is_cancel_frame(offline: &OfflineFrame) -> bool {
        let Some(v1) = offline.v1.as_ref() else {
            return false;
        };
        match v1.r#type() {
            location_nearby_connections::v1_frame::FrameType::Disconnection => true,
            location_nearby_connections::v1_frame::FrameType::PayloadTransfer => v1
                .payload_transfer
                .as_ref()
                .filter(|p| {
                    p.payload_header.as_ref().map(|h| h.r#type())
                        == Some(payload_header::PayloadType::Bytes)
                })
                .and_then(|p| p.payload_chunk.as_ref())
                .and_then(|c| c.body.as_deref())
                .and_then(|b| sharing_nearby::Frame::decode(b).ok())
                .and_then(|f| f.v1)
                .is_some_and(|f| f.r#type() == sharing_nearby::v1_frame::FrameType::Cancel),
            _ => false,
        }
    }

    /// Tells the peer that the user cancelled and closes our side, all within `CANCEL_TIMEOUT`;
    /// failures are only logged, the session is over anyway. Sends the sharing Cancel, then
    /// the Disconnection, then a FIN, and waits for the peer to close so that the caller
    /// releases the Wi-Fi link only after the frames left the socket buffers (a send queue
    /// full of file data is not empty when the writes return).
    ///
    /// `unsent` is the rest of a frame whose write was interrupted. It goes out first: a new
    /// frame in the middle of it would corrupt the stream. It can be large (a chunk) and the
    /// peer may not be reading; then the timeout ends the attempt and the caller drops the
    /// socket, which closes it.
    pub(crate) async fn cancel_peer(&mut self, unsent: &[u8]) {
        info!("Transfer canceled by the user");
        self.user_cancelled = true;
        let deadline = tokio::time::Instant::now() + CANCEL_TIMEOUT;
        let closing = async {
            let sent: Result<(), anyhow::Error> = async {
                if !unsent.is_empty() {
                    self.socket.write_all(unsent).await?;
                }
                self.send_encrypted_frame(&sharing_cancel_frame()).await?;
                self.disconnection().await
            }
            .await;
            match sent {
                Ok(()) if self.state.peer_disconnect_version >= 1 => {
                    self.finish_safe_disconnect(deadline).await;
                }
                Ok(()) => {}
                Err(e) => debug!("Cancel frames not sent: {e}"),
            }
            if let Err(e) = self.socket.shutdown().await {
                debug!("Closing the write half failed: {e}");
            }
            // Ends when the peer closes (or resets) after reading everything we sent.
            let mut sink = [0u8; 4096];
            while matches!(self.socket.read(&mut sink).await, Ok(n) if n > 0) {}
        };
        if tokio::time::timeout_at(deadline, closing).await.is_err() {
            debug!("The peer did not close after the cancel in time");
        }
    }

    /// The rest of the handshake after our request (upstream `EndpointManager`): reads the
    /// peer's frames until `deadline` (see `handshake_step`).
    async fn finish_safe_disconnect(&mut self, deadline: tokio::time::Instant) {
        loop {
            let read = tokio::time::timeout_at(
                deadline,
                read_frame(
                    &mut self.socket,
                    &mut self.length_buf,
                    &mut self.length_filled,
                    &mut self.frame_buf,
                    &mut self.frame_filled,
                    SANE_FRAME_LENGTH as usize,
                ),
            )
            .await;
            let Ok(Ok(frame_data)) = read else {
                return;
            };
            let Ok(offline) =
                Self::decode_peer_frame(&mut self.state, self.plain_frames, &frame_data)
            else {
                return;
            };
            match handshake_step(&offline) {
                // The peer's own request crossed ours: answered, and the ack to ours is still to
                // be read, or it stays unread at a peer that waits to hand it over.
                HandshakeStep::Request => {
                    if tokio::time::timeout_at(
                        deadline,
                        self.encrypt_and_send(&safe_disconnect_ack_frame()),
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                }
                HandshakeStep::Done => {
                    debug!("Safe-to-disconnect handshake done");
                    return;
                }
                // A reply cut by the deadline may be half on the wire: nothing more is written.
                HandshakeStep::KeepAlive => {
                    if tokio::time::timeout_at(deadline, self.send_keepalive(true))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                HandshakeStep::Other => {}
            }
        }
    }

    /// The end of a session that sent our Disconnection and has nothing more to do: the
    /// handshake (a peer at level 1 or more), then the FIN, within `CANCEL_TIMEOUT`.
    async fn close_after_request(&mut self) {
        let deadline = tokio::time::Instant::now() + CANCEL_TIMEOUT;
        if self.state.peer_disconnect_version >= 1 {
            self.finish_safe_disconnect(deadline).await;
        }
        if let Ok(Err(e)) = tokio::time::timeout_at(deadline, self.socket.shutdown()).await {
            debug!("Closing the write half failed: {e}");
        }
    }

    /// Our Disconnection: a safe-to-disconnect request to a peer that reads one (level 1 or
    /// more), a plain one otherwise. The peer's ack is taken in `process_offline_frame` or
    /// `finish_safe_disconnect`.
    async fn disconnection(&mut self) -> Result<(), anyhow::Error> {
        let frame = disconnection_frame(self.state.peer_disconnect_version >= 1);
        if self.state.encryption_done {
            self.encrypt_and_send(&frame).await
        } else {
            self.send_frame(frame.encode_to_vec()).await
        }
    }

    async fn finalize_key_exchange(
        &mut self,
        raw_peer_key: GenericPublicKey,
    ) -> Result<(), anyhow::Error> {
        let peer_p256_key = raw_peer_key
            .ec_p256_public_key
            .ok_or_else(|| anyhow!("Missing required fields"))?;

        let mut bytes = vec![0x04];
        bytes.extend(decode_point(&peer_p256_key.x));
        bytes.extend(decode_point(&peer_p256_key.y));

        let encoded_point = Sec1Point::from_bytes(bytes)?;
        let peer_key = Option::<PublicKey>::from(PublicKey::from_sec1_point(&encoded_point))
            .ok_or_else(|| anyhow!("The peer's public key is not on the curve"))?;
        let priv_key = self.state.private_key.as_ref().unwrap();

        let dhs = diffie_hellman(priv_key.to_nonzero_scalar(), peer_key.as_affine());
        let derived_secret = Sha256::digest(dhs.raw_secret_bytes());

        let mut ukey_info: Vec<u8> = vec![];
        ukey_info.extend_from_slice(self.state.client_init_msg_data.as_ref().unwrap());
        ukey_info.extend_from_slice(self.state.server_init_data.as_ref().unwrap());

        let auth_label = "UKEY2 v1 auth".as_bytes();
        let next_label = "UKEY2 v1 next".as_bytes();

        let auth_string = hkdf_extract_expand(auth_label, &derived_secret, &ukey_info, 32)?;
        let next_secret = hkdf_extract_expand(next_label, &derived_secret, &ukey_info, 32)?;

        let salt_hex = "82AA55A0D397F88346CA1CEE8D3909B95F13FA7DEB1D4AB38376B8256DA85510";
        let salt =
            hex::decode(salt_hex).map_err(|e| anyhow!("Failed to decode salt_hex: {}", e))?;

        let d2d_client = hkdf_extract_expand(&salt, &next_secret, "client".as_bytes(), 32)?;
        let d2d_server = hkdf_extract_expand(&salt, &next_secret, "server".as_bytes(), 32)?;

        let key_salt_hex = "BF9D2A53C63616D75DB0A7165B91C1EF73E537F2427405FA23610A4BE657642E";
        let key_salt = hex::decode(key_salt_hex)
            .map_err(|e| anyhow!("Failed to decode key_salt_hex: {}", e))?;

        let client_key = hkdf_extract_expand(&key_salt, &d2d_client, "ENC:2".as_bytes(), 32)?;
        let client_hmac_key = hkdf_extract_expand(&key_salt, &d2d_client, "SIG:1".as_bytes(), 32)?;
        let server_key = hkdf_extract_expand(&key_salt, &d2d_server, "ENC:2".as_bytes(), 32)?;
        let server_hmac_key = hkdf_extract_expand(&key_salt, &d2d_server, "SIG:1".as_bytes(), 32)?;

        self.update_state(
            |e| {
                e.decrypt_key = Some(server_key);
                e.recv_hmac_key = Some(server_hmac_key);
                e.encrypt_key = Some(client_key);
                e.send_hmac_key = Some(client_hmac_key);
                e.pin_code = Some(to_four_digit_string(&auth_string));
                e.encryption_done = true;

                if let Some(ref mut tm) = e.transfer_metadata {
                    tm.pin_code = Some(to_four_digit_string(&auth_string));
                }
            },
            true,
        )
        .await;

        Ok(())
    }

    async fn send_ukey2_alert(&mut self, atype: AlertType) -> Result<(), anyhow::Error> {
        let alert = Ukey2Alert {
            r#type: Some(atype.into()),
            error_message: None,
        };

        let data = Ukey2Message {
            message_type: Some(atype.into()),
            message_data: Some(alert.encode_to_vec()),
        };

        self.send_frame(data.encode_to_vec()).await
    }

    async fn send_encrypted_frame(
        &mut self,
        frame: &sharing_nearby::Frame,
    ) -> Result<(), anyhow::Error> {
        let frame_data = frame.encode_to_vec();
        let body_size = frame_data.len();

        let payload_header = PayloadHeader {
            id: Some(rand::rng().random_range(i64::MIN..i64::MAX)),
            r#type: Some(payload_header::PayloadType::Bytes.into()),
            total_size: Some(body_size as i64),
            is_sensitive: Some(false),
            ..Default::default()
        };

        let transfer = PayloadTransferFrame {
            packet_type: Some(PacketType::Data.into()),
            payload_chunk: Some(PayloadChunk {
                offset: Some(0),
                flags: Some(0),
                body: Some(frame_data),
                index: Some(0),
            }),
            payload_header: Some(payload_header.clone()),
            ..Default::default()
        };

        let wrapper = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::PayloadTransfer.into(),
                ),
                payload_transfer: Some(transfer),
                ..Default::default()
            }),
        };

        // Encrypt and send offline
        self.encrypt_and_send(&wrapper).await?;

        // Send lastChunk
        let transfer = PayloadTransferFrame {
            packet_type: Some(PacketType::Data.into()),
            payload_chunk: Some(PayloadChunk {
                offset: Some(body_size as i64),
                flags: Some(1), // lastChunk
                body: Some(vec![]),
                index: Some(1),
            }),
            payload_header: Some(payload_header),
            ..Default::default()
        };

        let wrapper = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::PayloadTransfer.into(),
                ),
                payload_transfer: Some(transfer),
                ..Default::default()
            }),
        };

        // Encrypt and send offline
        self.encrypt_and_send(&wrapper).await?;

        Ok(())
    }

    /// The one place that chooses the framing: SecureMessage, or plain after an upgrade
    /// with disabled encryption.
    async fn encrypt_and_send(&mut self, frame: &OfflineFrame) -> Result<(), anyhow::Error> {
        let data = self.frame_payload(frame).await?;
        self.send_frame(data).await
    }

    /// The bytes of `frame` as they go on the wire after the length prefix.
    async fn frame_payload(&mut self, frame: &OfflineFrame) -> Result<Vec<u8>, anyhow::Error> {
        if self.plain_frames {
            return Ok(frame.encode_to_vec());
        }
        let d2d_msg = DeviceToDeviceMessage {
            sequence_number: Some(self.get_server_seq_inc().await),
            message: Some(frame.encode_to_vec()),
        };

        let key = self.state.encrypt_key.as_ref().unwrap();
        let msg_data = d2d_msg.encode_to_vec();
        let iv = gen_random(16);

        let mut cipher = Cipher::new_256(&key[..AES_256_KEY_LEN].try_into().unwrap());
        cipher.set_auto_padding(true);
        let encrypted = cipher.cbc_encrypt(&iv, &msg_data);

        let hb = HeaderAndBody {
            body: encrypted,
            header: Header {
                encryption_scheme: EncScheme::Aes256Cbc.into(),
                signature_scheme: SigScheme::HmacSha256.into(),
                iv: Some(iv),
                public_metadata: Some(
                    GcmMetadata {
                        r#type: Type::DeviceToDeviceMessage.into(),
                        version: Some(1),
                    }
                    .encode_to_vec(),
                ),
                ..Default::default()
            },
        };

        let mut hmac = HmacSha256::new_from_slice(self.state.send_hmac_key.as_ref().unwrap())?;
        hmac.update(&hb.encode_to_vec());
        let result = hmac.finalize();

        let smsg = SecureMessage {
            header_and_body: hb.encode_to_vec(),
            signature: result.into_bytes().to_vec(),
        };

        Ok(smsg.encode_to_vec())
    }

    async fn send_keepalive(&mut self, ack: bool) -> Result<(), anyhow::Error> {
        let ack_frame = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(location_nearby_connections::v1_frame::FrameType::KeepAlive.into()),
                keep_alive: Some(KeepAliveFrame { ack: Some(ack) }),
                ..Default::default()
            }),
        };

        if self.state.encryption_done {
            self.encrypt_and_send(&ack_frame).await
        } else {
            self.send_frame(ack_frame.encode_to_vec()).await
        }
    }

    async fn send_frame(&mut self, data: Vec<u8>) -> Result<(), anyhow::Error> {
        let prefixed_length = length_prefixed(&data);
        self.socket.write_all(&prefixed_length).await?;
        self.socket.flush().await?;

        Ok(())
    }

    async fn get_server_seq_inc(&mut self) -> i32 {
        self.update_state(
            |e| {
                e.server_seq += 1;
            },
            false,
        )
        .await;

        self.state.server_seq
    }

    async fn update_state<F>(&mut self, f: F, inform: bool)
    where
        F: FnOnce(&mut InnerState),
    {
        f(&mut self.state);

        if !inform {
            return;
        }

        let _ = self.sender.send(ChannelMessage {
            id: self.state.id.clone(),
            msg: channel::Message::Client(MessageClient {
                kind: TransferKind::Outbound,
                state: Some(self.state.state.clone()),
                metadata: self.state.transfer_metadata.clone(),
            }),
        });
        // Add a small sleep timer to allow the Tokio runtime to have
        // some spare time to process channel's message. Otherwise it
        // get spammed by new requests. Currently set to 10 micro secs.
        tokio::time::sleep(SANITY_DURATION).await;
    }
}

/// `[length][data]` as it goes on the wire, the length being big-endian.
fn length_prefixed(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 4);
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
    out
}

/// The name the peer is shown for a local file. A name that is not UTF-8 cannot be sent as it
/// is (and a lossy one would name another file): the send fails with this message instead.
fn utf8_file_name(path: &Path) -> Result<String, anyhow::Error> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("Failed to get the file name of {}", path.display()))?;
    name.to_str().map(str::to_owned).ok_or_else(|| {
        anyhow!(
            "The file name of {} is not valid UTF-8 and cannot be sent",
            path.display()
        )
    })
}

/// How long the cancel frames, the close and the peer's reply to it may take together.
pub(super) const CANCEL_TIMEOUT: Duration = Duration::from_secs(2);

/// The sharing-level Cancel, sent as a Bytes payload like the phones do.
pub(super) fn sharing_cancel_frame() -> sharing_nearby::Frame {
    sharing_nearby::Frame {
        version: Some(sharing_nearby::frame::Version::V1.into()),
        v1: Some(sharing_nearby::V1Frame {
            r#type: Some(sharing_nearby::v1_frame::FrameType::Cancel.into()),
            ..Default::default()
        }),
    }
}

/// Whether the peer asks for the safe-to-disconnect handshake: a Disconnection with
/// `request_safe_to_disconnect` and no ack yet.
pub(super) fn is_safe_disconnect_request(offline: &OfflineFrame) -> bool {
    offline
        .v1
        .as_ref()
        .filter(|v1| v1.r#type() == location_nearby_connections::v1_frame::FrameType::Disconnection)
        .and_then(|v1| v1.disconnection.as_ref())
        .is_some_and(|d| d.request_safe_to_disconnect() && !d.ack_safe_to_disconnect())
}

/// A Disconnection of ours: with `request`, the safe-to-disconnect request that a peer at level 1
/// or more answers with its ack; without, the plain one older peers get.
pub(super) fn disconnection_frame(request: bool) -> OfflineFrame {
    OfflineFrame {
        version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
        v1: Some(location_nearby_connections::V1Frame {
            r#type: Some(location_nearby_connections::v1_frame::FrameType::Disconnection.into()),
            disconnection: Some(location_nearby_connections::DisconnectionFrame {
                request_safe_to_disconnect: request.then_some(true),
                ack_safe_to_disconnect: request.then_some(false),
            }),
            ..Default::default()
        }),
    }
}

/// What the safe-to-disconnect handshake does with one peer frame read after our request.
pub(super) enum HandshakeStep {
    /// The peer's ack, or a plain Disconnection: the handshake is over.
    Done,
    /// The peer's own request, crossing ours: answered with the ack, then the ack to ours is
    /// still read.
    Request,
    /// A keep-alive, answered while the handshake goes on.
    KeepAlive,
    /// Anything else (a chunk still in flight): read past.
    Other,
}

pub(super) fn handshake_step(offline: &OfflineFrame) -> HandshakeStep {
    match offline.v1.as_ref().map(|v1| v1.r#type()) {
        Some(location_nearby_connections::v1_frame::FrameType::Disconnection)
            if is_safe_disconnect_request(offline) =>
        {
            HandshakeStep::Request
        }
        Some(location_nearby_connections::v1_frame::FrameType::Disconnection) => {
            HandshakeStep::Done
        }
        Some(location_nearby_connections::v1_frame::FrameType::KeepAlive) => {
            HandshakeStep::KeepAlive
        }
        _ => HandshakeStep::Other,
    }
}

/// The answer to a safe-to-disconnect request. Level 6, declared in the connection
/// response for the payload acks, includes the handshake of level 1, so a peer may wait for
/// this before it closes (upstream `EndpointManager::ProcessDisconnectionFrame`, which
/// answers with both flags set; crQuickShare does the same).
pub(super) fn safe_disconnect_ack_frame() -> OfflineFrame {
    OfflineFrame {
        version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
        v1: Some(location_nearby_connections::V1Frame {
            r#type: Some(location_nearby_connections::v1_frame::FrameType::Disconnection.into()),
            disconnection: Some(location_nearby_connections::DisconnectionFrame {
                request_safe_to_disconnect: Some(true),
                ack_safe_to_disconnect: Some(true),
            }),
            ..Default::default()
        }),
    }
}

/// How long one write may make no progress before the peer is taken to have stopped reading.
const SEND_STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long, from the end of the last chunk, the peer has to acknowledge every sent file;
/// keep-alives do not extend it. A receiver acks each file once it is synced, so the last
/// ack waits for the sync of the last file (crQuickShare fails a send whose acks do not
/// come; upstream's sender waits 1 s, `wait_payload_received_ack_millis`).
const ACK_WAIT: Duration = Duration::from_secs(60);

/// Resolves when a cancel of the transfer `id` arrives; never when the bus closes.
pub(crate) async fn wait_for_cancel(rx: &mut Receiver<ChannelMessage>, id: &str) {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        match rx.recv().await {
            Ok(ChannelMessage {
                id: message_id,
                msg:
                    channel::Message::Lib {
                        action: TransferAction::TransferCancel,
                    },
            }) if message_id == id => return,
            Ok(_) | Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => std::future::pending::<()>().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    /// Sender capability reaches the wire; an unknown 5 GHz ability stays true for
    /// backwards compatibility, but a 2.4-only adapter must be reported accurately.
    #[tokio::test]
    async fn connection_request_reports_real_wifi_band_capability() {
        use crate::upgrade::tests::FakeLink;
        for (capability, expected) in [(Some(false), false), (Some(true), true), (None, true)] {
            let (mut peer, ours) = tokio::io::duplex(4096);
            let (bus, _rx) = tokio::sync::broadcast::channel(8);
            let mut req = OutboundRequest::new_over(
                *b"SEND",
                MigratableStream::Duplex(ours),
                "id".into(),
                bus,
                OutboundPayload::Text {
                    text: "x".into(),
                    text_type: TextPayloadType::Text,
                },
                RemoteDeviceInfo {
                    device_type: crate::DeviceType::Unknown,
                    name: "test".into(),
                },
            );
            req.upgrade = UpgradeState::new(Some(std::sync::Arc::new(FakeLink {
                capability,
                ..Default::default()
            })));
            req.send_connection_request().await.unwrap();
            let frame = upgrade::read_plain_frame(&mut peer).await.unwrap();
            let metadata = frame
                .v1
                .unwrap()
                .connection_request
                .unwrap()
                .medium_metadata
                .unwrap();
            assert_eq!(metadata.supports_5_ghz, Some(expected));
            assert_eq!(metadata.ap_frequency, Some(-1));
        }
    }

    /// After an upgrade with disabled encryption frames are bare OfflineFrames both ways.
    #[tokio::test]
    async fn plain_mode_reads_and_writes_bare_offline_frames() {
        let (mut peer, ours) = tokio::io::duplex(4096);
        let (bus, _rx) = tokio::sync::broadcast::channel(8);
        let mut req = OutboundRequest::new_over(
            *b"SEND",
            MigratableStream::Duplex(ours),
            "id".into(),
            bus,
            OutboundPayload::Text {
                text: "x".into(),
                text_type: TextPayloadType::Text,
            },
            RemoteDeviceInfo {
                device_type: crate::DeviceType::Unknown,
                name: "test".into(),
            },
        );
        req.plain_frames = true;
        req.state.state = TransferState::SentPairedKeyEncryption;

        crate::upgrade::tests::write_plain_keep_alive(&mut peer).await;
        tokio::time::timeout(Duration::from_secs(1), req.handle())
            .await
            .expect("handle() did not read the plain frame")
            .unwrap();
        let reply = upgrade::read_plain_frame(&mut peer).await.unwrap();
        crate::upgrade::tests::assert_keep_alive_ack(&reply);
    }

    /// The phone may send keep-alives while it thinks; only `MAX_HANDSHAKE_KEEP_ALIVES` are accepted.
    #[tokio::test]
    async fn handshake_keep_alives_are_capped() {
        let (mut peer, ours) = tokio::io::duplex(4096);
        let (bus, _rx) = tokio::sync::broadcast::channel(8);
        let mut req = OutboundRequest::new_over(
            *b"SEND",
            MigratableStream::Duplex(ours),
            "id".into(),
            bus,
            OutboundPayload::Text {
                text: "x".into(),
                text_type: TextPayloadType::Text,
            },
            RemoteDeviceInfo {
                device_type: crate::DeviceType::Unknown,
                name: "test".into(),
            },
        );
        req.state.state = TransferState::SentUkeyClientFinish;

        for n in 1..=crate::utils::MAX_HANDSHAKE_KEEP_ALIVES {
            crate::upgrade::tests::write_plain_keep_alive(&mut peer).await;
            req.handle()
                .await
                .unwrap_or_else(|e| panic!("keep-alive {n} was refused: {e}"));
        }
        crate::upgrade::tests::write_plain_keep_alive(&mut peer).await;
        let err = req.handle().await.unwrap_err();
        assert!(err.to_string().contains("only sent keep-alives"), "{err}");
    }

    fn is_peer_cancel(
        state: &mut InnerState,
        plain_frames: bool,
        data: &[u8],
    ) -> Result<bool, anyhow::Error> {
        let offline = OutboundRequest::decode_peer_frame(state, plain_frames, data)?;
        Ok(OutboundRequest::is_cancel_frame(&offline))
    }

    fn plain_request(
        peer_buffer: usize,
    ) -> (
        OutboundRequest,
        tokio::io::DuplexStream,
        Sender<ChannelMessage>,
    ) {
        let (peer, ours) = tokio::io::duplex(peer_buffer);
        let (bus, _rx) = tokio::sync::broadcast::channel(8);
        let mut req = OutboundRequest::new_over(
            *b"SEND",
            MigratableStream::Duplex(ours),
            "id".into(),
            bus.clone(),
            OutboundPayload::Text {
                text: "x".into(),
                text_type: TextPayloadType::Text,
            },
            RemoteDeviceInfo {
                device_type: crate::DeviceType::Unknown,
                name: "test".into(),
            },
        );
        req.plain_frames = true;
        (req, peer, bus)
    }

    fn big_frame() -> OfflineFrame {
        OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::PayloadTransfer.into(),
                ),
                payload_transfer: Some(PayloadTransferFrame {
                    payload_chunk: Some(
                        location_nearby_connections::payload_transfer_frame::PayloadChunk {
                            body: Some(vec![7u8; 64 * 1024]),
                            ..Default::default()
                        },
                    ),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        }
    }

    /// The next plain frame the peer reads, `None` at the end of the stream.
    async fn read_plain_frame(peer: &mut tokio::io::DuplexStream) -> Option<OfflineFrame> {
        let mut len = [0u8; 4];
        peer.read_exact(&mut len).await.ok()?;
        let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
        peer.read_exact(&mut body).await.ok()?;
        Some(OfflineFrame::decode(&*body).unwrap())
    }

    /// A cancel while a frame is half written: the frame is completed, then the sharing Cancel
    /// and the Disconnection follow, then the stream ends (FIN) and the call waits for the
    /// peer to close.
    #[tokio::test]
    async fn a_user_cancel_mid_write_completes_the_frame_then_cancels_and_closes() {
        let (mut req, mut peer, bus) = plain_request(64);
        let mut cancel_rx = bus.subscribe();
        let send = tokio::spawn(async move {
            let sent = req
                .send_unless_cancelled(&big_frame(), &mut cancel_rx, "id", Duration::from_secs(30))
                .await;
            (sent.unwrap(), req.state.state, req.user_cancelled)
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        bus.send(ChannelMessage {
            id: "id".into(),
            msg: channel::Message::Lib {
                action: TransferAction::TransferCancel,
            },
        })
        .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let first = read_plain_frame(&mut peer).await.unwrap();
        assert_eq!(first, big_frame(), "the cut frame was not completed");
        let cancel = read_plain_frame(&mut peer).await.unwrap();
        assert!(OutboundRequest::is_cancel_frame(&cancel));
        assert!(cancel.v1.unwrap().disconnection.is_none());
        let _last_chunk = read_plain_frame(&mut peer).await.unwrap();
        let disconnection = read_plain_frame(&mut peer).await.unwrap();
        assert_eq!(
            disconnection.v1.unwrap().r#type(),
            location_nearby_connections::v1_frame::FrameType::Disconnection
        );
        assert!(
            read_plain_frame(&mut peer).await.is_none(),
            "no FIN after the cancel"
        );
        drop(peer);
        let (sent, state, user_cancelled) = tokio::time::timeout(Duration::from_secs(5), send)
            .await
            .expect("the cancel did not return")
            .unwrap();
        assert!(!sent);
        assert_eq!(state, TransferState::Cancelled);
        assert!(user_cancelled);
    }

    /// A cancel between two writes: the Cancel and the Disconnection go out, then the FIN, and
    /// `handle()` ends the session.
    #[tokio::test]
    async fn a_user_cancel_between_writes_sends_cancel_and_disconnection_then_closes() {
        let (mut req, mut peer, bus) = plain_request(1 << 16);
        let handle = tokio::spawn(async move { req.handle().await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        bus.send(ChannelMessage {
            id: "id".into(),
            msg: channel::Message::Lib {
                action: TransferAction::TransferCancel,
            },
        })
        .unwrap();

        let cancel = read_plain_frame(&mut peer).await.unwrap();
        assert!(OutboundRequest::is_cancel_frame(&cancel));
        let _last_chunk = read_plain_frame(&mut peer).await.unwrap();
        let disconnection = read_plain_frame(&mut peer).await.unwrap();
        assert_eq!(
            disconnection.v1.unwrap().r#type(),
            location_nearby_connections::v1_frame::FrameType::Disconnection
        );
        assert!(
            read_plain_frame(&mut peer).await.is_none(),
            "no FIN after the cancel"
        );
        drop(peer);
        let err = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("handle() did not end")
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            err.downcast_ref(),
            Some(crate::errors::AppError::NotAnError)
        ));
    }

    /// The peer never reads and the buffer is tiny: the write blocks, yet a cancel gets through.
    #[tokio::test]
    async fn a_blocked_write_does_not_keep_a_cancel_from_the_user() {
        let (mut req, _peer, bus) = plain_request(64);
        let mut cancel_rx = bus.subscribe();
        let send = tokio::spawn(async move {
            let sent = req
                .send_unless_cancelled(&big_frame(), &mut cancel_rx, "id", Duration::from_secs(30))
                .await;
            (sent.unwrap(), req.state.state)
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        bus.send(ChannelMessage {
            id: "id".into(),
            msg: channel::Message::Lib {
                action: TransferAction::TransferCancel,
            },
        })
        .unwrap();
        let (sent, state) = tokio::time::timeout(Duration::from_secs(5), send)
            .await
            .expect("the cancel did not get through a blocked write")
            .unwrap();
        assert!(!sent);
        assert_eq!(state, TransferState::Cancelled);
    }

    /// The phone cancels while our write is blocked: it is noticed at once, after the harmless
    /// keep-alive that came before it, instead of after the stall timeout.
    #[tokio::test]
    async fn a_peer_disconnection_ends_a_blocked_write() {
        let (mut req, mut peer, bus) = plain_request(64);
        let mut cancel_rx = bus.subscribe();
        let send = tokio::spawn(async move {
            let sent = req
                .send_unless_cancelled(&big_frame(), &mut cancel_rx, "id", Duration::from_secs(30))
                .await;
            (sent.unwrap(), req.state.state)
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        crate::upgrade::tests::write_plain_keep_alive(&mut peer).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!send.is_finished(), "a keep-alive ended the send");
        let disconnection = OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::Disconnection.into(),
                ),
                ..Default::default()
            }),
        };
        peer.write_all(&length_prefixed(&disconnection.encode_to_vec()))
            .await
            .unwrap();
        let (sent, state) = tokio::time::timeout(Duration::from_secs(5), send)
            .await
            .expect("the peer's disconnection was not noticed during a blocked write")
            .unwrap();
        assert!(!sent);
        assert_eq!(state, TransferState::Cancelled);
    }

    fn keep_alive_frame() -> OfflineFrame {
        OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(location_nearby_connections::v1_frame::FrameType::KeepAlive.into()),
                keep_alive: Some(KeepAliveFrame { ack: Some(false) }),
                ..Default::default()
            }),
        }
    }

    /// A sharing-protocol frame of the given type, carried as a Bytes payload like the phone does.
    fn sharing_frame_payload(
        frame_type: sharing_nearby::v1_frame::FrameType,
        payload_type: payload_header::PayloadType,
    ) -> OfflineFrame {
        let sharing = sharing_nearby::Frame {
            version: Some(sharing_nearby::frame::Version::V1.into()),
            v1: Some(sharing_nearby::V1Frame {
                r#type: Some(frame_type.into()),
                ..Default::default()
            }),
        };
        OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::PayloadTransfer.into(),
                ),
                payload_transfer: Some(PayloadTransferFrame {
                    packet_type: Some(PacketType::Data.into()),
                    payload_header: Some(PayloadHeader {
                        id: Some(1),
                        r#type: Some(payload_type.into()),
                        ..Default::default()
                    }),
                    payload_chunk: Some(PayloadChunk {
                        offset: Some(0),
                        flags: Some(0),
                        body: Some(sharing.encode_to_vec()),
                        index: None,
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        }
    }

    fn sharing_cancel() -> OfflineFrame {
        sharing_frame_payload(
            sharing_nearby::v1_frame::FrameType::Cancel,
            payload_header::PayloadType::Bytes,
        )
    }

    /// What the peer puts on the wire (after the length prefix) in an encrypted session: the
    /// mirror of `frame_payload`, using the keys our side receives with.
    fn peer_secure_message(state: &InnerState, seq: i32, frame: &OfflineFrame) -> Vec<u8> {
        let d2d = DeviceToDeviceMessage {
            sequence_number: Some(seq),
            message: Some(frame.encode_to_vec()),
        };
        let key = state.decrypt_key.as_ref().unwrap();
        let iv = gen_random(16);
        let mut cipher = Cipher::new_256(&key[..AES_256_KEY_LEN].try_into().unwrap());
        cipher.set_auto_padding(true);
        let hb = HeaderAndBody {
            body: cipher.cbc_encrypt(&iv, &d2d.encode_to_vec()),
            header: Header {
                encryption_scheme: EncScheme::Aes256Cbc.into(),
                signature_scheme: SigScheme::HmacSha256.into(),
                iv: Some(iv),
                public_metadata: Some(
                    GcmMetadata {
                        r#type: Type::DeviceToDeviceMessage.into(),
                        version: Some(1),
                    }
                    .encode_to_vec(),
                ),
                ..Default::default()
            },
        };
        let mut hmac = HmacSha256::new_from_slice(state.recv_hmac_key.as_ref().unwrap()).unwrap();
        hmac.update(&hb.encode_to_vec());
        SecureMessage {
            header_and_body: hb.encode_to_vec(),
            signature: hmac.finalize().into_bytes().to_vec(),
        }
        .encode_to_vec()
    }

    /// The sharing Cancel is recognised only as a Bytes payload; other sharing frames and other
    /// payload types are of no use to a send in progress.
    #[test]
    fn only_a_bytes_payload_carrying_cancel_is_a_peer_cancel() {
        let mut state = InnerState::default();
        let mut check = |frame: &OfflineFrame| {
            is_peer_cancel(&mut state, true, &frame.encode_to_vec()).unwrap()
        };
        assert!(check(&sharing_cancel()));
        assert!(!check(&keep_alive_frame()));
        assert!(!check(&sharing_frame_payload(
            sharing_nearby::v1_frame::FrameType::Response,
            payload_header::PayloadType::Bytes,
        )));
        assert!(!check(&sharing_frame_payload(
            sharing_nearby::v1_frame::FrameType::Cancel,
            payload_header::PayloadType::File,
        )));
    }

    /// The phone's sharing Cancel (not a Disconnection) ends a blocked write too.
    #[tokio::test]
    async fn a_peer_sharing_cancel_ends_a_blocked_write() {
        let (mut req, mut peer, bus) = plain_request(64);
        let mut cancel_rx = bus.subscribe();
        let send = tokio::spawn(async move {
            let sent = req
                .send_unless_cancelled(&big_frame(), &mut cancel_rx, "id", Duration::from_secs(30))
                .await;
            (sent.unwrap(), req.state.state)
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        peer.write_all(&length_prefixed(&sharing_cancel().encode_to_vec()))
            .await
            .unwrap();
        let (sent, state) = tokio::time::timeout(Duration::from_secs(5), send)
            .await
            .expect("the peer's cancel was not noticed during a blocked write")
            .unwrap();
        assert!(!sent);
        assert_eq!(state, TransferState::Cancelled);
    }

    /// A keep-alive read while a chunk is written neither ends the send nor is lost: the next
    /// `handle()` answers it.
    #[tokio::test]
    async fn a_frame_read_during_a_write_is_handled_afterwards() {
        let (mut req, mut peer, bus) = plain_request(64);
        req.state.encryption_done = false;
        let mut cancel_rx = bus.subscribe();
        let send = tokio::spawn(async move {
            let sent = req
                .send_unless_cancelled(&big_frame(), &mut cancel_rx, "id", Duration::from_secs(30))
                .await;
            (sent.unwrap(), req)
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        crate::upgrade::tests::write_plain_keep_alive(&mut peer).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!send.is_finished(), "a keep-alive ended the send");

        // Drain the chunk so the write completes.
        let mut chunk = vec![0u8; big_frame().encode_to_vec().len() + 4];
        peer.read_exact(&mut chunk).await.unwrap();
        let (sent, mut req) = tokio::time::timeout(Duration::from_secs(5), send)
            .await
            .expect("the send did not finish")
            .unwrap();
        assert!(sent);
        assert_eq!(req.pending_frames.len(), 1);

        tokio::time::timeout(Duration::from_secs(5), req.handle())
            .await
            .expect("handle() did not replay the frame")
            .unwrap();
        assert!(req.pending_frames.is_empty());
        let mut len = [0u8; 4];
        peer.read_exact(&mut len).await.unwrap();
        let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
        peer.read_exact(&mut body).await.unwrap();
        let reply = OfflineFrame::decode(&*body).unwrap();
        let v1 = reply.v1.unwrap();
        assert_eq!(
            v1.r#type(),
            location_nearby_connections::v1_frame::FrameType::KeepAlive
        );
        assert_eq!(v1.keep_alive.unwrap().ack, Some(true));
    }

    /// Beyond the soft cap only keep-alives and payload frames may be lost.
    #[test]
    fn only_keep_alives_and_payload_frames_are_droppable() {
        use location_nearby_connections::v1_frame::FrameType;
        let frame = |t: FrameType| OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(t.into()),
                ..Default::default()
            }),
        };
        assert!(OutboundRequest::is_droppable_frame(&frame(
            FrameType::KeepAlive
        )));
        assert!(OutboundRequest::is_droppable_frame(&frame(
            FrameType::PayloadTransfer
        )));
        assert!(!OutboundRequest::is_droppable_frame(&frame(
            FrameType::BandwidthUpgradeNegotiation
        )));
        assert!(!OutboundRequest::is_droppable_frame(
            &OfflineFrame::default()
        ));
    }

    /// An oversized frame from the peer is reported as invalid, not as a lost peer.
    #[tokio::test]
    async fn an_oversized_frame_during_a_write_is_reported_as_invalid() {
        let (mut req, mut peer, bus) = plain_request(64);
        let mut cancel_rx = bus.subscribe();
        let send = tokio::spawn(async move {
            req.send_unless_cancelled(&big_frame(), &mut cancel_rx, "id", Duration::from_secs(30))
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        peer.write_all(&[0x7f, 0xff, 0xff, 0xff]).await.unwrap();
        let err = tokio::time::timeout(Duration::from_secs(5), send)
            .await
            .expect("the send did not finish")
            .unwrap()
            .unwrap_err();
        assert!(err.to_string().contains("invalid frame"), "{err}");
    }

    /// A reset or closed connection ends a blocked write at once, as an error the manager turns
    /// into `Disconnected`.
    #[tokio::test]
    async fn a_closed_connection_ends_a_blocked_write_with_an_error() {
        let (mut req, peer, bus) = plain_request(64);
        let mut cancel_rx = bus.subscribe();
        let send = tokio::spawn(async move {
            req.send_unless_cancelled(&big_frame(), &mut cancel_rx, "id", Duration::from_secs(30))
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(peer);
        let err = tokio::time::timeout(Duration::from_secs(5), send)
            .await
            .expect("the closed connection was not noticed during a blocked write")
            .unwrap()
            .unwrap_err();
        assert!(
            err.to_string().contains("lost the peer while sending"),
            "{err}"
        );
    }

    /// In an encrypted session every frame read while sending consumes one client sequence
    /// number, the Cancel included, so a `handle()` after it still decrypts.
    #[tokio::test]
    async fn an_encrypted_peer_cancel_keeps_the_sequence_in_step() {
        let (mut peer, ours) = tokio::io::duplex(1 << 16);
        let (bus, _rx) = tokio::sync::broadcast::channel(8);
        let mut req = OutboundRequest::new_over(
            *b"SEND",
            MigratableStream::Duplex(ours),
            "id".into(),
            bus,
            OutboundPayload::Text {
                text: "x".into(),
                text_type: TextPayloadType::Text,
            },
            RemoteDeviceInfo {
                device_type: crate::DeviceType::Unknown,
                name: "test".into(),
            },
        );
        req.state.state = TransferState::SendingFiles;
        req.state.decrypt_key = Some(vec![1; 32]);
        req.state.recv_hmac_key = Some(vec![2; 32]);
        req.state.encrypt_key = Some(vec![3; 32]);
        req.state.send_hmac_key = Some(vec![4; 32]);

        let keep_alive = peer_secure_message(&req.state, 1, &keep_alive_frame());
        assert!(!is_peer_cancel(&mut req.state, false, &keep_alive).unwrap());
        assert_eq!(req.state.client_seq, 1);
        let cancel = peer_secure_message(&req.state, 2, &sharing_cancel());
        assert!(is_peer_cancel(&mut req.state, false, &cancel).unwrap());
        assert_eq!(req.state.client_seq, 2);

        let next = peer_secure_message(&req.state, 3, &keep_alive_frame());
        peer.write_all(&length_prefixed(&next)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), req.handle())
            .await
            .expect("handle() did not read the frame")
            .expect("the frame after the Cancel was not decrypted");
        assert_eq!(req.state.client_seq, 3);
    }

    /// A peer that stopped reading ends the session after the stall timeout instead of freezing it.
    #[tokio::test]
    async fn a_write_that_makes_no_progress_ends_the_session() {
        let (mut req, _peer, bus) = plain_request(64);
        let mut cancel_rx = bus.subscribe();
        let err = tokio::time::timeout(
            Duration::from_secs(5),
            req.send_unless_cancelled(
                &big_frame(),
                &mut cancel_rx,
                "id",
                Duration::from_millis(200),
            ),
        )
        .await
        .expect("the stalled write was never given up")
        .unwrap_err();
        assert!(err.to_string().contains("stopped reading"), "{err}");
    }

    #[test]
    fn a_file_name_that_is_not_utf8_is_an_error() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let bad = Path::new(OsStr::from_bytes(b"/tmp/caf\xe9.txt"));
        let err = utf8_file_name(bad).unwrap_err().to_string();
        assert!(err.contains("not valid UTF-8"), "{err}");
        assert_eq!(utf8_file_name(Path::new("/tmp/a.txt")).unwrap(), "a.txt");
        assert!(utf8_file_name(Path::new("/")).is_err());
    }

    // ---- PAYLOAD_ACK: the peer confirms the files it received ----

    /// A directory under the system temp dir, removed on drop.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("rqs-outbound-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A files request in plain-frame mode, its files open and not yet sent, so that
    /// `send_payloads` runs against it directly. The payload ids are 1.. in the order of
    /// `sizes`.
    fn plain_files_request(
        dir: &std::path::Path,
        sizes: &[i64],
    ) -> (
        OutboundRequest,
        tokio::io::DuplexStream,
        Sender<ChannelMessage>,
    ) {
        let paths: Vec<String> = sizes
            .iter()
            .enumerate()
            .map(|(n, size)| {
                let path = dir.join(format!("f{n}.bin"));
                std::fs::write(&path, vec![7u8; usize::try_from(*size).unwrap()]).unwrap();
                path.to_string_lossy().into_owned()
            })
            .collect();
        let (peer, ours) = tokio::io::duplex(1 << 20);
        let (bus, _rx) = tokio::sync::broadcast::channel(8);
        let mut req = OutboundRequest::new_over(
            *b"SEND",
            MigratableStream::Duplex(ours),
            "id".into(),
            bus.clone(),
            OutboundPayload::Files(paths.clone()),
            RemoteDeviceInfo {
                device_type: crate::DeviceType::Unknown,
                name: "test".into(),
            },
        );
        req.plain_frames = true;
        req.state.state = TransferState::SendingFiles;
        for (n, (path, size)) in paths.iter().zip(sizes).enumerate() {
            let id = (n + 1) as i64;
            req.state.transferred_files.insert(
                id,
                InternalFileInfo {
                    payload_id: id,
                    file_url: std::path::PathBuf::from(path),
                    bytes_transferred: 0,
                    total_size: *size,
                    file: Some(File::open(path).unwrap()),
                },
            );
        }
        (req, peer, bus)
    }

    /// The ack a receiver sends for one completed FILE payload (crQuickShare `AckFile`).
    fn payload_ack_frame(id: i64) -> OfflineFrame {
        OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::PayloadTransfer.into(),
                ),
                payload_transfer: Some(PayloadTransferFrame {
                    packet_type: Some(PacketType::PayloadAck.into()),
                    payload_header: Some(PayloadHeader {
                        id: Some(id),
                        total_size: Some(-1),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        }
    }

    fn disconnection_frame() -> OfflineFrame {
        OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::Disconnection.into(),
                ),
                disconnection: Some(location_nearby_connections::DisconnectionFrame {
                    ..Default::default()
                }),
                ..Default::default()
            }),
        }
    }

    /// One length-prefixed plain frame to the peer, like the session under test reads them.
    async fn write_plain_frame(peer: &mut tokio::io::DuplexStream, frame: &OfflineFrame) {
        let data = frame.encode_to_vec();
        peer.write_all(&(data.len() as u32).to_be_bytes())
            .await
            .unwrap();
        peer.write_all(&data).await.unwrap();
        peer.flush().await.unwrap();
    }

    /// The new wire fields survive a roundtrip: the safe-to-disconnect level on the
    /// connection response and the ack packet type. The ack tests exercise them end to end
    /// as well.
    #[test]
    fn the_new_wire_fields_roundtrip() {
        let response = location_nearby_connections::ConnectionResponseFrame {
            response: Some(
                location_nearby_connections::connection_response_frame::ResponseStatus::Accept
                    .into(),
            ),
            safe_to_disconnect_version: Some(6),
            ..Default::default()
        };
        let decoded = location_nearby_connections::ConnectionResponseFrame::decode(
            response.encode_to_vec().as_slice(),
        )
        .unwrap();
        assert_eq!(decoded.safe_to_disconnect_version, Some(6));

        let decoded = OfflineFrame::decode(payload_ack_frame(17).encode_to_vec().as_slice())
            .expect("an ack frame decodes");
        let transfer = decoded.v1.unwrap().payload_transfer.unwrap();
        assert_eq!(transfer.packet_type(), PacketType::PayloadAck);
        let header = transfer.payload_header.unwrap();
        assert_eq!(header.id(), 17);
        assert_eq!(header.total_size(), -1);
        assert!(transfer.payload_chunk.is_none(), "an ack carries no chunk");
    }

    /// A peer at safe-to-disconnect level 6 acks every file: the transfer is finished only
    /// once they are all confirmed, and the close follows the last ack. A keep-alive while
    /// acks are owed is answered, not dropped.
    #[tokio::test]
    async fn a_peer_that_acks_the_files_gets_a_finished_transfer() {
        let tmp = TempDir::new("acks");
        let (mut req, mut peer, _bus) = plain_files_request(&tmp.0, &[4, 6]);
        req.state.peer_disconnect_version = 6;
        let task = tokio::spawn(async move {
            let result = req.send_payloads().await;
            (result, req.state.state)
        });
        // File 1: the chunk, then the last-chunk marker, then the ack.
        read_plain_frame(&mut peer).await.unwrap();
        read_plain_frame(&mut peer).await.unwrap();
        write_plain_frame(&mut peer, &payload_ack_frame(1)).await;
        // File 2, and a keep-alive while its ack is still owed.
        read_plain_frame(&mut peer).await.unwrap();
        read_plain_frame(&mut peer).await.unwrap();
        write_plain_frame(&mut peer, &keep_alive_frame()).await;
        let ka = read_plain_frame(&mut peer).await.unwrap();
        assert_eq!(
            ka.v1.unwrap().keep_alive.unwrap().ack,
            Some(true),
            "the keep-alive was not answered"
        );
        write_plain_frame(&mut peer, &payload_ack_frame(2)).await;
        let close = read_plain_frame(&mut peer).await.unwrap();
        assert_eq!(
            close.v1.unwrap().r#type(),
            location_nearby_connections::v1_frame::FrameType::Disconnection
        );
        let (result, state) = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the ack wait never ended")
            .unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(state, TransferState::Finished);
    }

    /// A peer that vanishes with files unacknowledged: not a finished transfer.
    #[tokio::test]
    async fn a_peer_that_closes_before_acking_fails_the_transfer() {
        let tmp = TempDir::new("acks-eof");
        let (mut req, mut peer, _bus) = plain_files_request(&tmp.0, &[4]);
        req.state.peer_disconnect_version = 6;
        let task = tokio::spawn(async move {
            let result = req.send_payloads().await;
            (result, req.state.state)
        });
        read_plain_frame(&mut peer).await.unwrap();
        read_plain_frame(&mut peer).await.unwrap();
        drop(peer);
        let (result, state) = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the lost peer was never given up")
            .unwrap();
        let err = result.expect_err("a peer gone before the acks ended as finished");
        assert!(
            err.to_string()
                .contains("disconnected before acknowledging"),
            "{err}"
        );
        assert_eq!(state, TransferState::SendingFiles);
    }

    /// A protocol Disconnection with files still unacknowledged is the same failure: the
    /// peer ended the session without confirming the delivery.
    #[tokio::test]
    async fn a_disconnection_before_the_acks_fails_the_transfer() {
        let tmp = TempDir::new("acks-disc");
        let (mut req, mut peer, _bus) = plain_files_request(&tmp.0, &[4]);
        req.state.peer_disconnect_version = 6;
        let task = tokio::spawn(async move {
            let result = req.send_payloads().await;
            (result, req.state.state)
        });
        read_plain_frame(&mut peer).await.unwrap();
        read_plain_frame(&mut peer).await.unwrap();
        write_plain_frame(&mut peer, &disconnection_frame()).await;
        let (result, state) = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the disconnection was never taken")
            .unwrap();
        let err = result.expect_err("a disconnection before the acks ended as finished");
        assert!(
            err.to_string()
                .contains("disconnected before acknowledging"),
            "{err}"
        );
        assert_eq!(state, TransferState::SendingFiles);
    }

    /// The receiver cancels while acks are owed: a cancelled transfer, not a failure.
    #[tokio::test]
    async fn a_peer_cancel_while_acks_are_owed_cancels_the_transfer() {
        let tmp = TempDir::new("acks-cancel");
        let (mut req, mut peer, _bus) = plain_files_request(&tmp.0, &[4]);
        req.state.peer_disconnect_version = 6;
        let task = tokio::spawn(async move {
            let result = req.send_payloads().await;
            (result, req.state.state)
        });
        read_plain_frame(&mut peer).await.unwrap();
        read_plain_frame(&mut peer).await.unwrap();
        write_plain_frame(&mut peer, &sharing_cancel()).await;
        let (result, state) = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the cancel was never taken")
            .unwrap();
        assert!(matches!(
            result.err().unwrap().downcast_ref(),
            Some(crate::errors::AppError::NotAnError)
        ));
        assert_eq!(state, TransferState::Cancelled);
    }

    /// A CONTROL frame for a sent file, as a receiver that cancels or fails it sends.
    fn payload_control_frame(id: i64) -> OfflineFrame {
        let mut frame = payload_ack_frame(id);
        if let Some(transfer) = frame
            .v1
            .as_mut()
            .and_then(|v1| v1.payload_transfer.as_mut())
        {
            transfer.packet_type = Some(PacketType::Control.into());
        }
        frame
    }

    /// A frame the ack wait keeps for `handle()` (an upgrade offer read during a write) does
    /// not keep the wait from reading the acks. Multi-threaded so that a spinning wait fails
    /// the timeout instead of hanging the test.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_queued_frame_does_not_stall_the_ack_wait() {
        let tmp = TempDir::new("acks-queued");
        let (mut req, mut peer, _bus) = plain_files_request(&tmp.0, &[4]);
        req.state.peer_disconnect_version = 6;
        req.pending_frames
            .push_back(upgrade::event_frame(EventType::UpgradePathAvailable));
        let task = tokio::spawn(async move {
            let result = req.send_payloads().await;
            (result, req.state.state, req.pending_frames.len())
        });
        read_plain_frame(&mut peer).await.unwrap();
        read_plain_frame(&mut peer).await.unwrap();
        write_plain_frame(&mut peer, &payload_ack_frame(1)).await;
        let close = read_plain_frame(&mut peer).await.unwrap();
        assert_eq!(
            close.v1.unwrap().r#type(),
            location_nearby_connections::v1_frame::FrameType::Disconnection
        );
        let (result, state, queued) = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the ack wait spun on the queued frame")
            .unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(state, TransferState::Finished);
        assert_eq!(queued, 1, "the offer is kept for handle()");
    }

    /// A CONTROL frame with the id of a sent file is the receiver giving up on it, not an
    /// ack: the send fails at once instead of finishing or waiting out `ACK_WAIT`.
    #[tokio::test]
    async fn a_control_frame_is_not_an_ack() {
        let tmp = TempDir::new("acks-control");
        let (mut req, mut peer, _bus) = plain_files_request(&tmp.0, &[4]);
        req.state.peer_disconnect_version = 6;
        let task = tokio::spawn(async move { req.send_payloads().await });
        read_plain_frame(&mut peer).await.unwrap();
        read_plain_frame(&mut peer).await.unwrap();
        write_plain_frame(&mut peer, &payload_control_frame(1)).await;
        let err = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the refused file was waited for")
            .unwrap()
            .expect_err("a CONTROL frame finished the transfer");
        assert!(err.to_string().contains("refused a file"), "{err}");
        drop(peer);
    }

    /// The user cancels while acks are owed: a cancelled transfer, and the peer is told.
    #[tokio::test]
    async fn a_user_cancel_while_acks_are_owed_cancels_the_transfer() {
        let tmp = TempDir::new("acks-user-cancel");
        let (mut req, mut peer, bus) = plain_files_request(&tmp.0, &[4]);
        req.state.peer_disconnect_version = 6;
        let task = tokio::spawn(async move {
            let result = req.send_payloads().await;
            (result, req.state.state)
        });
        read_plain_frame(&mut peer).await.unwrap();
        read_plain_frame(&mut peer).await.unwrap();
        // Past the marker write: the session is in the ack wait.
        tokio::time::sleep(Duration::from_millis(50)).await;
        bus.send(ChannelMessage {
            id: "id".into(),
            msg: channel::Message::Lib {
                action: TransferAction::TransferCancel,
            },
        })
        .unwrap();
        // The sharing Cancel goes to the peer before the session ends.
        let cancel = read_plain_frame(&mut peer).await.expect("the cancel frame");
        assert_eq!(
            cancel.v1.unwrap().r#type(),
            location_nearby_connections::v1_frame::FrameType::PayloadTransfer
        );
        drop(peer);
        let (result, state) = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the cancel was never taken")
            .unwrap();
        assert!(matches!(
            result.err().unwrap().downcast_ref(),
            Some(crate::errors::AppError::NotAnError)
        ));
        assert_eq!(state, TransferState::Cancelled);
    }

    /// Keep-alives prove the peer is there but do not stand in for the acks: a peer that
    /// answers keep-alives and never acks still fails the send at `ACK_WAIT`.
    #[tokio::test(start_paused = true)]
    async fn keep_alives_do_not_extend_the_ack_wait() {
        let tmp = TempDir::new("acks-keepalive");
        let (mut req, mut peer, _bus) = plain_files_request(&tmp.0, &[4]);
        req.state.peer_disconnect_version = 6;
        let start = tokio::time::Instant::now();
        let task = tokio::spawn(async move { req.send_payloads().await });
        read_plain_frame(&mut peer).await.unwrap();
        read_plain_frame(&mut peer).await.unwrap();
        let peer_task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                write_plain_frame(&mut peer, &keep_alive_frame()).await;
                if read_plain_frame(&mut peer).await.is_none() {
                    break;
                }
            }
        });
        let err = tokio::time::timeout(ACK_WAIT * 2, task)
            .await
            .expect("keep-alives kept the ack wait open")
            .unwrap()
            .expect_err("a send without acks finished");
        assert!(err.to_string().contains("did not acknowledge"), "{err}");
        assert!(start.elapsed() < ACK_WAIT + Duration::from_secs(1));
        peer_task.abort();
    }

    /// A safe-to-disconnect request (the peer read our level 6) gets the ack with both
    /// flags, as upstream's EndpointManager answers it.
    /// A frame half read on the prior channel must not be completed with the new channel's
    /// bytes: the switch starts the read state over.
    #[tokio::test]
    async fn the_switch_to_the_new_channel_starts_the_read_state_over() {
        let tmp = TempDir::new("switch");
        let (mut req, _peer, _bus) = plain_files_request(&tmp.0, &[]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let new = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        req.length_filled = 2;
        req.frame_filled = 3;
        req.upgrade.begin_draining(new);
        req.finish_upgrade().await;
        assert!(matches!(req.socket, MigratableStream::Tcp(_)));
        assert_eq!((req.length_filled, req.frame_filled), (0, 0));
    }

    #[tokio::test]
    async fn a_safe_to_disconnect_request_is_acknowledged() {
        let tmp = TempDir::new("safe-disc");
        let (mut req, mut peer, _bus) = plain_files_request(&tmp.0, &[]);
        let mut request = disconnection_frame();
        if let Some(d) = request.v1.as_mut().and_then(|v1| v1.disconnection.as_mut()) {
            d.request_safe_to_disconnect = Some(true);
        }
        req.process_offline_frame(&request).await.unwrap();
        let reply = read_plain_frame(&mut peer).await.unwrap();
        let disconnection = reply.v1.unwrap().disconnection.unwrap();
        assert_eq!(disconnection.request_safe_to_disconnect, Some(true));
        assert_eq!(disconnection.ack_safe_to_disconnect, Some(true));
        // A plain Disconnection (no request) gets no reply.
        req.process_offline_frame(&disconnection_frame())
            .await
            .unwrap();
        drop(req);
        assert!(read_plain_frame(&mut peer).await.is_none());
    }

    /// Every chunk of a file carries its position, the last-chunk marker included: a peer at
    /// safe-to-disconnect level 4 (a Pixel 7) rejects a repeated index as a duplicate and the
    /// file never completes there.
    #[tokio::test]
    async fn file_chunks_carry_their_index() {
        let tmp = TempDir::new("index");
        // Two data chunks of at most 512 KiB, then the marker.
        let (mut req, mut peer, _bus) = plain_files_request(&tmp.0, &[600_000]);
        let task = tokio::spawn(async move { req.send_payloads().await });
        let mut indices = Vec::new();
        for _ in 0..3 {
            let frame = read_plain_frame(&mut peer).await.unwrap();
            let chunk = frame
                .v1
                .unwrap()
                .payload_transfer
                .unwrap()
                .payload_chunk
                .unwrap();
            indices.push((chunk.index, chunk.flags()));
        }
        assert_eq!(indices, [(Some(0), 0), (Some(1), 0), (Some(2), 1)]);
        drop(peer);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    /// A finished send to a peer at level 1 or more ends with the safe-to-disconnect handshake:
    /// our close asks for it, and the peer's ack ends the session at once (the Pixel 7 at
    /// level 4 otherwise takes the end for a lost link and tries to reconnect).
    #[tokio::test]
    async fn a_finished_send_ends_with_the_safe_disconnect_handshake() {
        let tmp = TempDir::new("safe-end");
        let (mut req, mut peer, _bus) = plain_files_request(&tmp.0, &[4]);
        req.state.peer_disconnect_version = 4;
        let task = tokio::spawn(async move {
            req.send_payloads().await.unwrap();
            loop {
                if let Err(e) = req.handle().await {
                    break (e, req.state.state);
                }
            }
        });
        read_plain_frame(&mut peer).await.unwrap();
        read_plain_frame(&mut peer).await.unwrap();
        let close = read_plain_frame(&mut peer).await.unwrap();
        let disconnection = close.v1.unwrap().disconnection.unwrap();
        assert_eq!(disconnection.request_safe_to_disconnect, Some(true));
        assert_eq!(disconnection.ack_safe_to_disconnect, Some(false));
        write_plain_frame(&mut peer, &safe_disconnect_ack_frame()).await;
        let (ended, state) = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the session waited past the peer's ack")
            .unwrap();
        assert!(
            matches!(
                ended.downcast_ref(),
                Some(crate::errors::AppError::NotAnError)
            ),
            "{ended:?}"
        );
        assert_eq!(state, TransferState::Finished);
    }

    /// A user cancel to a peer at level 1 or more: the Cancel, our request, and the peer's own
    /// request that crosses it is answered with the ack before the FIN.
    #[tokio::test]
    async fn a_user_cancel_answers_a_crossing_safe_disconnect_request() {
        let tmp = TempDir::new("safe-cancel");
        let (mut req, mut peer, _bus) = plain_files_request(&tmp.0, &[]);
        req.state.peer_disconnect_version = 4;
        let task = tokio::spawn(async move { req.cancel_peer(&[]).await });
        // The sharing Cancel is a Bytes payload: its chunk, then the last-chunk marker.
        let _cancel = read_plain_frame(&mut peer).await.unwrap();
        let _marker = read_plain_frame(&mut peer).await.unwrap();
        let ours = read_plain_frame(&mut peer).await.unwrap();
        assert_eq!(
            ours.v1
                .unwrap()
                .disconnection
                .unwrap()
                .request_safe_to_disconnect,
            Some(true)
        );
        let crossing = super::disconnection_frame(true);
        write_plain_frame(&mut peer, &crossing).await;
        let reply = read_plain_frame(&mut peer).await.unwrap();
        let reply = reply.v1.unwrap().disconnection.unwrap();
        assert_eq!(
            (
                reply.request_safe_to_disconnect,
                reply.ack_safe_to_disconnect
            ),
            (Some(true), Some(true))
        );
        // Then the FIN: the peer reads the end of the stream, and closes its side.
        assert!(read_plain_frame(&mut peer).await.is_none());
        drop(peer);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the cancel waited past the handshake")
            .unwrap();
    }

    /// A peer at level 1 or more that never answers (and never closes) holds a user cancel no
    /// longer than `CANCEL_TIMEOUT`.
    #[tokio::test(start_paused = true)]
    async fn a_silent_peer_does_not_hold_the_cancel_past_its_bound() {
        let tmp = TempDir::new("safe-silent");
        let (mut req, mut peer, _bus) = plain_files_request(&tmp.0, &[]);
        req.state.peer_disconnect_version = 4;
        let started = tokio::time::Instant::now();
        let task = tokio::spawn(async move { req.cancel_peer(&[]).await });
        let reader = tokio::spawn(async move {
            while read_plain_frame(&mut peer).await.is_some() {}
            // Kept open: only the deadline can end the wait.
            std::future::pending::<()>().await;
        });
        tokio::time::timeout(CANCEL_TIMEOUT * 2, task)
            .await
            .expect("the cancel was held past its bound")
            .unwrap();
        assert!(started.elapsed() <= CANCEL_TIMEOUT + Duration::from_millis(50));
        reader.abort();
    }

    /// A peer below level 6 never acks: the close follows the last chunk like before, and
    /// nothing waits for acks that will not come.
    #[tokio::test]
    async fn an_old_peer_is_not_waited_for() {
        let tmp = TempDir::new("acks-old");
        let (mut req, mut peer, _bus) = plain_files_request(&tmp.0, &[4]);
        // peer_disconnect_version stays 0.
        let task = tokio::spawn(async move {
            let result = req.send_payloads().await;
            (result, req.state.state, req.unacked_files.is_empty())
        });
        read_plain_frame(&mut peer).await.unwrap();
        read_plain_frame(&mut peer).await.unwrap();
        let close = read_plain_frame(&mut peer).await.unwrap();
        let v1 = close.v1.unwrap();
        assert_eq!(
            v1.r#type(),
            location_nearby_connections::v1_frame::FrameType::Disconnection
        );
        assert_eq!(
            v1.disconnection.unwrap().request_safe_to_disconnect,
            None,
            "a level-0 peer gets the plain close"
        );
        let (result, state, no_unacked) = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the transfer waited for acks no old peer sends")
            .unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(state, TransferState::Finished);
        assert!(no_unacked);
    }
}
