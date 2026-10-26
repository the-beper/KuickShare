use std::fs::OpenOptions;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, anyhow};
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
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast::{Receiver, Sender};

use super::outbound::{CANCEL_TIMEOUT, sharing_cancel_frame};
use super::{InnerState, TransferState};
use crate::channel::{self, ChannelMessage, MessageClient, TransferAction, TransferKind};
use crate::hdl::info::{InternalFileInfo, TransferMetadata, TransferPayload, TransferPayloadKind};
use crate::hdl::{MigratableStream, TextPayloadInfo};
use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::EventType;
use crate::location_nearby_connections::payload_transfer_frame::{
    PacketType, PayloadChunk, PayloadHeader, payload_header,
};
use crate::location_nearby_connections::{KeepAliveFrame, OfflineFrame, PayloadTransferFrame};
use crate::securegcm::ukey2_alert::AlertType;
use crate::securegcm::{
    DeviceToDeviceMessage, GcmMetadata, Type, Ukey2Alert, Ukey2ClientFinished, Ukey2ClientInit,
    Ukey2HandshakeCipher, Ukey2Message, Ukey2ServerInit, ukey2_message,
};
use crate::securemessage::{
    EcP256PublicKey, EncScheme, GenericPublicKey, Header, HeaderAndBody, PublicKeyType,
    SecureMessage, SigScheme,
};
use crate::sharing_nearby::wifi_credentials_metadata::SecurityType;
use crate::sharing_nearby::{paired_key_result_frame, text_metadata};
use crate::upgrade::{self, Reply, UpgradeEvent, UpgradeState};
use crate::utils::{
    DeviceType, RemoteDeviceInfo, decode_point, encode_point, gen_ecdsa_keypair, gen_random,
    get_download_dir, hkdf_extract_expand, read_length_prefix, received_file_name,
    stream_read_exact, to_four_digit_string,
};
use crate::{location_nearby_connections, sharing_nearby};

type HmacSha256 = Hmac<Sha256>;

const SANE_FRAME_LENGTH: i32 = 5 * 1024 * 1024;
const SANITY_DURATION: Duration = Duration::from_micros(10);

#[derive(Debug)]
pub struct InboundRequest {
    socket: MigratableStream,
    /// Partially received length prefix, kept across cancelled `handle` calls.
    length_buf: [u8; 4],
    length_filled: usize,
    /// Keep-alives ignored while the handshake waits for the ConnectionResponse.
    keep_alives_ignored: u8,
    /// Frames are plain `[length][OfflineFrame]` (no SecureMessage): set once the upgrade
    /// to a channel with disabled encryption completes.
    plain_frames: bool,
    pub state: InnerState,
    sender: Sender<ChannelMessage>,
    receiver: Receiver<ChannelMessage>,
    upgrade: UpgradeState,
}

impl InboundRequest {
    /// A session over the LAN: the peer is already on a network, so no Wi-Fi upgrade is offered.
    pub fn new(socket: TcpStream, id: String, sender: Sender<ChannelMessage>) -> Self {
        let mut request = Self::new_over(MigratableStream::Tcp(socket), id, sender);
        request.upgrade = UpgradeState::new(None);
        request
    }

    /// Runs the session over any stream. Nothing here depends on the peer address: `id` is
    /// chosen by the caller (the remote address for TCP, the device address for BLE).
    pub fn new_over(socket: MigratableStream, id: String, sender: Sender<ChannelMessage>) -> Self {
        let receiver = sender.subscribe();

        Self {
            socket,
            length_buf: [0; 4],
            length_filled: 0,
            keep_alives_ignored: 0,
            plain_frames: false,
            state: InnerState {
                id,
                server_seq: 0,
                client_seq: 0,
                state: TransferState::Initial,
                encryption_done: true,
                ..Default::default()
            },
            sender,
            receiver,
            upgrade: UpgradeState::new(crate::wifi_link()),
        }
    }

    /// Offer to disable encryption on the upgraded channel (never done outside tests).
    #[cfg(test)]
    pub(crate) fn offer_disabling_encryption(&mut self) {
        self.upgrade.set_disable_encryption(true);
    }

    pub async fn handle(&mut self) -> Result<(), anyhow::Error> {
        tokio::select! {
            i = self.receiver.recv() => {
                match i {
                    Ok(channel_msg) => {
                        if channel_msg.id != self.state.id {
                            return Ok(());
                        }

                        if let channel::Message::Lib { action } = &channel_msg.msg {
                            debug!("inbound: got: {:?}", channel_msg);
                            match action {
                                TransferAction::ConsentAccept => {
                                    self.accept_transfer().await?;
                                },
                                TransferAction::ConsentDecline => {
                                    self.update_state(
                                        |e| {
                                            e.state = TransferState::Rejected;
                                        },
                                        true,
                                    ).await;

                                    self.reject_transfer(Some(
                                        sharing_nearby::connection_response_frame::Status::Reject
                                    )).await?;
                                    return Err(anyhow!(crate::errors::AppError::NotAnError));
                                },
                                TransferAction::TransferCancel => {
                                    self.update_state(
                                        |e| {
                                            e.state = TransferState::Cancelled;
                                        },
                                        true,
                                    ).await;
                                    self.cancel_peer().await;
                                    return Err(anyhow!(crate::errors::AppError::NotAnError));
                                },
                            }
                        }

                    }
                    Err(e) => {
                        error!("inbound: channel error: {}", e);
                    }
                }
            },
            h = read_length_prefix(&mut self.socket, &mut self.length_buf, &mut self.length_filled) => {
                self._handle(h?).await?
            },
            ev = self.upgrade.next_event() => {
                self.on_upgrade_event(ev).await?;
            }
        }

        Ok(())
    }

    pub async fn _handle(&mut self, length_buf: [u8; 4]) -> Result<(), anyhow::Error> {
        let msg_length = u32::from_be_bytes(length_buf) as usize;
        // Ensure the message length is not unreasonably big to avoid allocation attacks
        if msg_length > SANE_FRAME_LENGTH as usize {
            error!("Message length too big");
            return Err(anyhow!("value"));
        }

        // Allocate buffer for the actual message and read it
        let mut frame_data = vec![0u8; msg_length];
        stream_read_exact(&mut self.socket, &mut frame_data).await?;

        let current_state = &self.state;
        // Now determine what will be the request type based on current state
        match current_state.state {
            TransferState::Initial => {
                debug!("Handling State::Initial frame");
                let frame = location_nearby_connections::OfflineFrame::decode(&*frame_data)?;
                let rdi = self.process_connection_request(&frame)?;
                if let Some(cr) = frame
                    .v1
                    .as_ref()
                    .and_then(|v| v.connection_request.as_ref())
                {
                    self.upgrade.set_peer_mediums(&cr.mediums);
                    self.upgrade.set_peer_endpoint_id(cr.endpoint_id());
                }
                info!("RemoteDeviceInfo: {:?}", &rdi);

                // Advance current state
                self.update_state(
                    |e: &mut InnerState| {
                        e.state = TransferState::ReceivedConnectionRequest;
                        e.remote_device_info = Some(rdi);
                    },
                    false,
                )
                .await;
            }
            TransferState::ReceivedConnectionRequest => {
                debug!("Handling State::ReceivedConnectionRequest frame");
                let msg = Ukey2Message::decode(&*frame_data)?;
                self.process_ukey2_client_init(&msg).await?;

                self.update_state(
                    |e: &mut InnerState| {
                        e.state = TransferState::SentUkeyServerInit;
                        e.client_init_msg_data = Some(frame_data);
                    },
                    false,
                )
                .await;
            }
            TransferState::SentUkeyServerInit => {
                debug!("Handling State::SentUkeyServerInit frame");
                let msg = Ukey2Message::decode(&*frame_data)?;
                self.process_ukey2_client_finish(&msg, &frame_data).await?;

                self.update_state(
                    |e: &mut InnerState| {
                        e.state = TransferState::ReceivedUkeyClientFinish;
                    },
                    false,
                )
                .await;
            }
            TransferState::ReceivedUkeyClientFinish => {
                debug!("Handling State::ReceivedUkeyClientFinish frame");
                let frame = location_nearby_connections::OfflineFrame::decode(&*frame_data)?;
                // The phone can take seconds to answer (seen on the first connection after the
                // service started) and sends keep-alives while it does; only so many are accepted.
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

                self.update_state(
                    |e: &mut InnerState| {
                        e.state = TransferState::SentConnectionResponse;
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

    fn process_connection_request(
        &self,
        frame: &location_nearby_connections::OfflineFrame,
    ) -> Result<RemoteDeviceInfo, anyhow::Error> {
        let v1_frame = frame
            .v1
            .as_ref()
            .ok_or_else(|| anyhow!("Missing required fields"))?;

        if v1_frame.r#type() != location_nearby_connections::v1_frame::FrameType::ConnectionRequest
        {
            return Err(anyhow!(format!(
                "Unexpected frame type: {:?}",
                v1_frame.r#type()
            )));
        }

        let connection_request = v1_frame
            .connection_request
            .as_ref()
            .ok_or_else(|| anyhow!("Missing required fields"))?;

        let endpoint_info = connection_request
            .endpoint_info
            .as_ref()
            .ok_or_else(|| anyhow!("Missing endpoint info"))?;

        // Check if endpoint info length is greater than 17
        if endpoint_info.len() <= 17 {
            return Err(anyhow!("Endpoint info too short"));
        }

        let device_name_length = endpoint_info[17] as usize;
        // Validate length including device name
        if endpoint_info.len() < device_name_length + 18 {
            return Err(anyhow!(
                "Endpoint info too short to contain the device name"
            ));
        }

        // Extract and validate device name based on length
        let device_name = std::str::from_utf8(&endpoint_info[18..(18 + device_name_length)])
            .map_err(|_| anyhow!("Device name is not valid UTF-8"))?;

        // Parsing the device type
        let raw_device_type = (endpoint_info[0] & 7) >> 1_usize;

        Ok(RemoteDeviceInfo {
            name: device_name.to_string(),
            device_type: DeviceType::from_raw_value(raw_device_type),
        })
    }

    async fn process_ukey2_client_init(&mut self, msg: &Ukey2Message) -> Result<(), anyhow::Error> {
        if msg.message_type() != ukey2_message::Type::ClientInit {
            self.send_ukey2_alert(AlertType::BadMessageType).await?;
            return Err(anyhow!(
                "UKey2: message_type({:?}) != ClientInit",
                msg.message_type
            ));
        }

        let client_init = match Ukey2ClientInit::decode(msg.message_data()) {
            Ok(uk2ci) => uk2ci,
            Err(e) => {
                self.send_ukey2_alert(AlertType::BadMessageData).await?;
                return Err(anyhow!("UKey2: Ukey2ClientInit::decode: {}", e));
            }
        };

        if client_init.version() != 1 {
            self.send_ukey2_alert(AlertType::BadVersion).await?;
            return Err(anyhow!("UKey2: client_init.version != 1"));
        }

        if client_init.random().len() != 32 {
            self.send_ukey2_alert(AlertType::BadRandom).await?;
            return Err(anyhow!("UKey2: client_init.random.len != 32"));
        }

        // Searching for preferred cipher commitment
        let mut found = false;
        for commitment in &client_init.cipher_commitments {
            trace!("CipherCommitment: {:?}", commitment.handshake_cipher());
            if Ukey2HandshakeCipher::P256Sha512 == commitment.handshake_cipher() {
                found = true;
                self.update_state(
                    |e| {
                        e.cipher_commitment = Some(commitment.clone());
                    },
                    false,
                )
                .await;
                break;
            }
        }

        if !found {
            self.send_ukey2_alert(AlertType::BadHandshakeCipher).await?;
            return Err(anyhow!("UKey2: badHandshakeCipher"));
        }

        if client_init.next_protocol() != "AES_256_CBC-HMAC_SHA256" {
            self.send_ukey2_alert(AlertType::BadNextProtocol).await?;
            return Err(anyhow!(
                "UKey2: badNextProtocol: {}",
                client_init.next_protocol()
            ));
        }

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

        let server_init = Ukey2ServerInit {
            version: Some(1),
            random: Some(rand::rng().random::<[u8; 32]>().to_vec()),
            handshake_cipher: Some(Ukey2HandshakeCipher::P256Sha512.into()),
            public_key: Some(pkey.encode_to_vec()),
        };

        let server_init_msg = Ukey2Message {
            message_type: Some(ukey2_message::Type::ServerInit.into()),
            message_data: Some(server_init.encode_to_vec()),
        };

        let server_init_data = server_init_msg.encode_to_vec();
        self.update_state(
            |e| {
                e.private_key = Some(secret_key);
                e.public_key = Some(public_key);
                e.server_init_data = Some(server_init_data.clone());
            },
            false,
        )
        .await;

        self.send_frame(server_init_data).await?;

        Ok(())
    }

    async fn process_ukey2_client_finish(
        &mut self,
        msg: &Ukey2Message,
        frame_data: &Vec<u8>,
    ) -> Result<(), anyhow::Error> {
        if msg.message_type() != ukey2_message::Type::ClientFinish {
            self.send_ukey2_alert(AlertType::BadMessageType).await?;
            return Err(anyhow!(
                "UKey2: message_type({:?}) != ClientFinish",
                msg.message_type
            ));
        }

        let sha512 = Sha512::digest(frame_data);
        if self.state.cipher_commitment.as_ref().unwrap().commitment() != sha512.as_slice() {
            error!("cipher_commitment isn't equals to sha512(frame_data)");
            return Err(anyhow!("UKey2: cipher_commitment != sha512"));
        }

        let client_finish = match Ukey2ClientFinished::decode(msg.message_data()) {
            Ok(uk2cf) => uk2cf,
            Err(e) => {
                return Err(anyhow!("UKey2: Ukey2ClientFinished::decode: {}", e));
            }
        };

        if client_finish.public_key.is_none() {
            return Err(anyhow!("UKey2: client_finish.public_key None"));
        }

        let client_public_key = match GenericPublicKey::decode(client_finish.public_key()) {
            Ok(cpk) => cpk,
            Err(e) => {
                return Err(anyhow!("UKey2: GenericPublicKey::decode: {}", e));
            }
        };

        self.finalize_key_exchange(client_public_key).await?;

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

        let response = location_nearby_connections::OfflineFrame {
			version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
			v1: Some(location_nearby_connections::V1Frame {
				r#type: Some(location_nearby_connections::v1_frame::FrameType::ConnectionResponse.into()),
				connection_response: Some(location_nearby_connections::ConnectionResponseFrame {
					response: Some(location_nearby_connections::connection_response_frame::ResponseStatus::Accept.into()),
					os_info: Some(location_nearby_connections::OsInfo {
						r#type: Some(location_nearby_connections::os_info::OsType::Linux.into())
					}),
					..Default::default()
				}),
				..Default::default()
			})
		};

        self.send_frame(response.encode_to_vec()).await?;

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

        self.send_encrypted_frame(&paired_encryption).await?;

        Ok(())
    }

    async fn decrypt_and_process_secure_message(
        &mut self,
        smsg: &SecureMessage,
    ) -> Result<(), anyhow::Error> {
        let mut hmac = HmacSha256::new_from_slice(self.state.recv_hmac_key.as_ref().unwrap())?;
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
        let key = self.state.decrypt_key.as_ref().unwrap();

        let mut cipher = Cipher::new_256(key[..AES_256_KEY_LEN].try_into()?);
        cipher.set_auto_padding(true);
        let decrypted = cipher.cbc_decrypt(header_and_body.header.iv(), &msg_data);

        let d2d_msg = DeviceToDeviceMessage::decode(&*decrypted)?;

        let seq = self.get_client_seq_inc().await;
        if d2d_msg.sequence_number() != seq {
            return Err(anyhow!(
                "Error d2d_msg.sequence_number invalid ({} vs {})",
                d2d_msg.sequence_number(),
                seq
            ));
        }

        let offline = location_nearby_connections::OfflineFrame::decode(d2d_msg.message())?;
        self.process_offline_frame(&offline).await
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
                        let payload_id = header.id();

                        if header.total_size() > SANE_FRAME_LENGTH.into() {
                            self.state.payload_buffers.remove(&payload_id);
                            return Err(anyhow!(
                                "Payload too large: {} bytes",
                                header.total_size()
                            ));
                        }

                        self.state
                            .payload_buffers
                            .entry(payload_id)
                            .or_insert_with(|| Vec::with_capacity(header.total_size() as usize));

                        // Get the current length of the buffer, if it exists, without holding a mutable borrow.
                        let buffer_len = self.state.payload_buffers.get(&payload_id).unwrap().len();
                        if chunk.offset() != buffer_len as i64 {
                            self.state.payload_buffers.remove(&payload_id);
                            return Err(anyhow!(
                                "Unexpected chunk offset: {}, expected: {}",
                                chunk.offset(),
                                buffer_len
                            ));
                        }

                        let buffer = self.state.payload_buffers.get_mut(&payload_id).unwrap();
                        if let Some(body) = &chunk.body {
                            buffer.extend(body);
                        }

                        if (chunk.flags() & 1) == 1 {
                            debug!("Chunk flags & 1 == 1 ?? End of data ??");

                            if self.state.text_payload.is_some()
                                && self.state.text_payload.as_ref().unwrap().get_i64_value()
                                    == payload_id
                            {
                                info!("Transfer finished");

                                match self.state.text_payload.clone().unwrap() {
                                    TextPayloadInfo::Url(_) => {
                                        let payload = std::str::from_utf8(&buffer)?.to_owned();
                                        self.update_state(
                                            |e| {
                                                if let Some(tmd) = e.transfer_metadata.as_mut() {
                                                    tmd.payload =
                                                        Some(TransferPayload::Url(payload));
                                                }
                                            },
                                            false,
                                        )
                                        .await;
                                    }
                                    TextPayloadInfo::Text(_) => {
                                        let payload = std::str::from_utf8(&buffer)?.to_owned();
                                        self.update_state(
                                            |e| {
                                                if let Some(tmd) = e.transfer_metadata.as_mut() {
                                                    tmd.payload =
                                                        Some(TransferPayload::Text(payload));
                                                }
                                            },
                                            false,
                                        )
                                        .await;
                                    }
                                    // FIXME: ChannelMessage's structure needs to be redone as well
                                    // It needs to have TextPayloadInfo instead of TextPayloadType
                                    TextPayloadInfo::Wifi((_, ssid, security_type)) => {
                                        // ~~Payload seems to be within two DLE (0x10) characters
                                        // At least for WpaPsk, not sure about Wep~~
                                        //
                                        // Nope, that's wrong, my Wi-Fi password just happened to have 16 characters
                                        // which tripped up the previous logic.
                                        //
                                        // So, the password seems to start with 0x0A followed by a byte indicating
                                        // the password or payload length. And, the payload ends with 0x10 0x??
                                        // The last byte is sometimes 00 and other times 01
                                        fn parse_password_payload(
                                            buffer: &mut Vec<u8>,
                                        ) -> anyhow::Result<String>
                                        {
                                            if buffer.len() < 4 {
                                                anyhow::bail!("Buffer too short ({buffer:?})");
                                            }

                                            if buffer[(buffer.len() - 1) - 1] != 0x10 {
                                                anyhow::bail!(
                                                    "Buffer ({buffer:?}) doesn't ends with 0x10 0x?? as expected"
                                                );
                                            }

                                            let len = *buffer
                                                .get(1)
                                                .expect("Validated for minimum length of 4")
                                                as usize;

                                            let payload_buffer = buffer.get(2..2 + len).with_context(||anyhow!( "Buffer too short ({buffer:?}) can't retrieve payload of length {len}"))?;

                                            Ok(String::from_utf8(payload_buffer.to_owned())?)
                                        }

                                        let payload = match security_type {
                                            kind @ SecurityType::UnknownSecurityType => {
                                                kind.as_str_name().into()
                                            }
                                            SecurityType::Open => "".into(),
                                            SecurityType::WpaPsk | SecurityType::Wep => {
                                                parse_password_payload(buffer)
                                                    .inspect_err(|err| error!("{err:#}"))
                                                    .unwrap_or_default()
                                            }
                                        };

                                        self.update_state(
                                            |e| {
                                                if let Some(tmd) = e.transfer_metadata.as_mut() {
                                                    tmd.payload = Some(TransferPayload::Wifi {
                                                        ssid,
                                                        password: payload,
                                                        security_type: security_type,
                                                    });
                                                }
                                            },
                                            false,
                                        )
                                        .await;
                                    }
                                }

                                self.update_state(
                                    |e| {
                                        e.state = TransferState::Finished;
                                    },
                                    true,
                                )
                                .await;
                                self.disconnection().await?;
                                return Err(anyhow!(crate::errors::AppError::NotAnError));
                            } else {
                                let inner_frame = sharing_nearby::Frame::decode(buffer.as_slice())?;
                                self.process_transfer_setup(&inner_frame).await?;
                            }
                        }
                    }
                    payload_header::PayloadType::File => {
                        trace!("Processing PayloadType::File");
                        let payload_id = header.id();

                        let file_internal = self
                            .state
                            .transferred_files
                            .get_mut(&payload_id)
                            .ok_or_else(|| {
                                anyhow!("File payload ID ({}) is not known", payload_id)
                            })?;

                        let current_offset = file_internal.bytes_transferred;
                        if chunk.offset() != current_offset {
                            return Err(anyhow!(
                                "Invalid offset into file {}, expected {}",
                                chunk.offset(),
                                current_offset
                            ));
                        }

                        let chunk_size = chunk.body().len();
                        if current_offset + chunk_size as i64 > file_internal.total_size {
                            return Err(anyhow!(
                                "Transferred file size exceeds previously specified value: {} vs {}",
                                current_offset + chunk_size as i64,
                                file_internal.total_size
                            ));
                        }

                        if !chunk.body().is_empty() {
                            file_internal
                                .file
                                .as_ref()
                                .unwrap()
                                .write_all_at(chunk.body(), current_offset as u64)?;
                            file_internal.bytes_transferred += chunk_size as i64;

                            self.update_state(
                                |e| {
                                    if let Some(tmd) = e.transfer_metadata.as_mut() {
                                        tmd.ack_bytes += chunk_size as u64;
                                    }
                                },
                                true,
                            )
                            .await;
                        } else if (chunk.flags() & 1) == 1 {
                            self.state.transferred_files.remove(&payload_id);
                            if self.state.transferred_files.is_empty() {
                                info!("Transfer finished");
                                self.update_state(
                                    |e| {
                                        e.state = TransferState::Finished;
                                    },
                                    true,
                                )
                                .await;
                                self.disconnection().await?;
                                return Err(anyhow!(crate::errors::AppError::NotAnError));
                            }
                        }
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
            _ => {
                error!("Unhandled offline frame encrypted: {:?}", offline);
            }
        }

        Ok(())
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
            return Err(anyhow!(crate::errors::AppError::NotAnError));
        }

        match self.state.state {
            TransferState::SentConnectionResponse => {
                debug!("Processing State::SentConnectionResponse");
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
                        e.state = TransferState::ReceivedPairedKeyResult;
                    },
                    false,
                )
                .await;
            }
            TransferState::ReceivedPairedKeyResult => {
                debug!("Processing State::ReceivedPairedKeyResult");
                self.process_introduction(v1_frame).await?;
            }
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
        &self,
        v1_frame: &sharing_nearby::V1Frame,
    ) -> Result<(), anyhow::Error> {
        if v1_frame.paired_key_result.is_none() {
            return Err(anyhow!(
                "Missing required fields: expected a PairedKeyResult, got a {:?} frame",
                v1_frame.r#type()
            ));
        }

        Ok(())
    }

    async fn process_introduction(
        &mut self,
        v1_frame: &sharing_nearby::V1Frame,
    ) -> Result<(), anyhow::Error> {
        let introduction = v1_frame.introduction.as_ref().ok_or_else(|| {
            anyhow!(
                "Missing required fields: expected an Introduction, got a {:?} frame (response status: {:?})",
                v1_frame.r#type(),
                v1_frame.connection_response.as_ref().map(|r| r.status())
            )
        })?;

        // No need to inform the channel here, we'll do it anyway with files info
        self.update_state(
            |e| {
                e.state = TransferState::WaitingForUserConsent;
            },
            false,
        )
        .await;

        if !introduction.file_metadata.is_empty() && introduction.text_metadata.is_empty() {
            trace!("process_introduction: handling file_metadata");
            let mut files_name = Vec::with_capacity(introduction.file_metadata.len());
            let mut saved_paths = Vec::with_capacity(introduction.file_metadata.len());
            let mut total_bytes: u64 = 0;

            for file in &introduction.file_metadata {
                info!("File name: {}", file.name());
                let name = received_file_name(file.name(), file.mime_type()).unwrap_or_else(|| {
                    warn!("Unusable file name from the peer: {:?}", file.name());
                    "file".to_owned()
                });

                let dest = free_path(&get_download_dir(), &name, |p| {
                    p.exists()
                        || self
                            .state
                            .transferred_files
                            .values()
                            .any(|f| f.file_url == p)
                });
                info!("Destination: {:?}", dest);

                saved_paths.push(dest.to_string_lossy().into_owned());
                let info = InternalFileInfo {
                    payload_id: file.payload_id(),
                    file_url: dest,
                    bytes_transferred: 0,
                    total_size: file.size(),
                    file: None,
                };
                total_bytes += info.total_size as u64;
                self.state.transferred_files.insert(file.payload_id(), info);
                files_name.push(name.clone());
            }

            let metadata = TransferMetadata {
                id: self.state.id.clone(),
                source: self.state.remote_device_info.clone(),
                payload_kind: TransferPayloadKind::Files,
                payload_preview: Default::default(),
                payload: Some(TransferPayload::Files(files_name)),
                pin_code: self.state.pin_code.clone(),
                total_bytes,
                ack_bytes: Default::default(),
                saved_paths,
            };

            info!("Asking for user consent: {:?}", metadata);
            self.update_state(
                |e| {
                    e.transfer_metadata = Some(metadata);
                },
                true,
            )
            .await;
        } else if introduction.text_metadata.len() == 1 {
            trace!("process_introduction: handling text_metadata");
            let meta = introduction.text_metadata.first().unwrap();

            match meta.r#type() {
                text_metadata::Type::Url => {
                    let metadata = TransferMetadata {
                        id: self.state.id.clone(),
                        source: self.state.remote_device_info.clone(),
                        payload_kind: TransferPayloadKind::Url,
                        payload_preview: Some(meta.text_title.clone().unwrap_or_default()),
                        pin_code: self.state.pin_code.clone(),
                        payload: Default::default(),
                        total_bytes: Default::default(),
                        ack_bytes: Default::default(),
                        saved_paths: Vec::new(),
                    };

                    info!("Asking for user consent: {:?}", metadata);
                    self.update_state(
                        |e| {
                            e.text_payload = Some(TextPayloadInfo::Url(meta.payload_id()));
                            e.transfer_metadata = Some(metadata);
                        },
                        true,
                    )
                    .await;
                }
                text_metadata::Type::PhoneNumber
                | text_metadata::Type::Address
                | text_metadata::Type::Text => {
                    let metadata = TransferMetadata {
                        id: self.state.id.clone(),
                        source: self.state.remote_device_info.clone(),
                        payload_kind: TransferPayloadKind::Text,
                        payload_preview: Some(meta.text_title.clone().unwrap_or_default()),
                        pin_code: self.state.pin_code.clone(),
                        payload: Default::default(),
                        total_bytes: Default::default(),
                        ack_bytes: Default::default(),
                        saved_paths: Vec::new(),
                    };

                    info!("Asking for user consent: {:?}", metadata);
                    self.update_state(
                        |e| {
                            e.text_payload = Some(TextPayloadInfo::Text(meta.payload_id()));
                            e.transfer_metadata = Some(metadata);
                        },
                        true,
                    )
                    .await;
                }
                text_metadata::Type::Unknown => {
                    // Reject transfer
                    self.reject_transfer(Some(
						sharing_nearby::connection_response_frame::Status::UnsupportedAttachmentType,
					))
					.await?;
                }
            }
        } else if introduction.wifi_credentials_metadata.len() == 1 {
            trace!("process_introduction: handling wifi_credentials_metadata");
            let meta = introduction.wifi_credentials_metadata.first().unwrap();

            let metadata = TransferMetadata {
                id: self.state.id.clone(),
                source: self.state.remote_device_info.clone(),
                payload_kind: TransferPayloadKind::WiFi,
                payload_preview: Some(meta.ssid.clone().unwrap_or_default()),
                pin_code: self.state.pin_code.clone(),
                payload: Default::default(),
                total_bytes: Default::default(),
                ack_bytes: Default::default(),
                saved_paths: Vec::new(),
            };

            self.update_state(
                |e| {
                    e.text_payload = Some(TextPayloadInfo::Wifi((
                        meta.payload_id(),
                        meta.ssid().to_owned(),
                        meta.security_type(),
                    )));
                    e.transfer_metadata = Some(metadata);
                },
                true,
            )
            .await;
        } else {
            // Reject transfer
            self.reject_transfer(Some(
                sharing_nearby::connection_response_frame::Status::UnsupportedAttachmentType,
            ))
            .await?;
        }

        Ok(())
    }

    /// Tells the peer that the user cancelled and closes our side, as the outbound side does:
    /// the sharing Cancel, the Disconnection, a FIN, then a wait for the peer to close, all
    /// within `CANCEL_TIMEOUT`. Failures are only logged. Called between two frames: the
    /// writes of `handle()` are not interrupted by the select that leads here.
    async fn cancel_peer(&mut self) {
        info!("Transfer canceled by the user");
        let closing = async {
            let sent: Result<(), anyhow::Error> = async {
                self.send_encrypted_frame(&sharing_cancel_frame()).await?;
                self.disconnection().await
            }
            .await;
            if let Err(e) = sent {
                debug!("Cancel frames not sent: {e}");
            }
            if let Err(e) = self.socket.shutdown().await {
                debug!("Closing the write half failed: {e}");
            }
            let mut sink = [0u8; 4096];
            while matches!(self.socket.read(&mut sink).await, Ok(n) if n > 0) {}
        };
        if tokio::time::timeout(CANCEL_TIMEOUT, closing).await.is_err() {
            debug!("The peer did not close after the cancel in time");
        }
    }

    async fn disconnection(&mut self) -> Result<(), anyhow::Error> {
        let frame = location_nearby_connections::OfflineFrame {
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
        };

        if self.state.encryption_done {
            self.encrypt_and_send(&frame).await
        } else {
            self.send_frame(frame.encode_to_vec()).await
        }
    }

    async fn accept_transfer(&mut self) -> Result<(), anyhow::Error> {
        let ids: Vec<i64> = self.state.transferred_files.keys().cloned().collect();

        for id in ids {
            let taken: Vec<PathBuf> = self
                .state
                .transferred_files
                .values()
                .map(|f| f.file_url.clone())
                .collect();
            let mfi = self.state.transferred_files.get_mut(&id).unwrap();

            // `create_new`: never truncate a file another transfer created since the offer.
            let file = loop {
                match OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&mfi.file_url)
                {
                    Ok(f) => break f,
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        let dir = mfi.file_url.parent().unwrap_or(Path::new(".")).to_owned();
                        let name = mfi
                            .file_url
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("file")
                            .to_owned();
                        mfi.file_url =
                            free_path(&dir, &name, |p| p.exists() || taken.iter().any(|t| t == p));
                    }
                    Err(e) => return Err(e.into()),
                }
            };
            info!("Created file: {:?}", &file);
            mfi.file = Some(file);
        }

        let frame = sharing_nearby::Frame {
            version: Some(sharing_nearby::frame::Version::V1.into()),
            v1: Some(sharing_nearby::V1Frame {
                r#type: Some(sharing_nearby::v1_frame::FrameType::Response.into()),
                connection_response: Some(sharing_nearby::ConnectionResponseFrame {
                    status: Some(sharing_nearby::connection_response_frame::Status::Accept.into()),
                }),
                ..Default::default()
            }),
        };

        self.send_encrypted_frame(&frame).await?;

        self.update_state(
            |e| {
                e.state = TransferState::ReceivingFiles;
            },
            true,
        )
        .await;

        // Text and URLs are not worth a hotspot.
        if !self.state.transferred_files.is_empty() {
            self.start_upgrade().await;
        }

        Ok(())
    }

    /// Starts hosting a hotspot in the background; the offer goes out once it is up.
    /// Any failure leaves the session on the current channel.
    async fn start_upgrade(&mut self) {
        let Some(link) = self.upgrade.link().filter(|_| self.upgrade.can_start()) else {
            return;
        };
        if self.upgrade.peer_endpoint_id().is_none() {
            return;
        }
        match TcpListener::bind(("0.0.0.0", 0)).await {
            Ok(listener) => {
                let task = tokio::spawn(upgrade::prepare_host(link, listener));
                self.upgrade.begin_preparing(task);
            }
            Err(e) => warn!("upgrade: not possible ({e}); staying on the current channel"),
        }
    }

    async fn offer_upgrade(
        &mut self,
        creds: upgrade::WifiCredentials,
        listener: TcpListener,
    ) -> Result<(), anyhow::Error> {
        let endpoint_id = self
            .upgrade
            .peer_endpoint_id()
            .ok_or_else(|| anyhow!("unknown peer endpoint id"))?
            .to_owned();
        info!(
            "upgrade: offering hotspot {:?} ({}:{})",
            creds.ssid, creds.gateway, creds.port
        );
        self.encrypt_and_send(&upgrade::path_available_frame(
            &creds,
            self.upgrade.disable_encryption(),
        ))
        .await?;
        self.upgrade
            .begin_waiting(tokio::spawn(upgrade::accept_introduced(
                listener,
                endpoint_id,
                creds.gateway,
                self.upgrade.disable_encryption(),
            )));
        Ok(())
    }

    async fn on_upgrade_event(&mut self, ev: UpgradeEvent) -> Result<(), anyhow::Error> {
        match ev {
            UpgradeEvent::Prepared(Ok(upgrade::Prepared::Hosted(creds, listener))) => {
                if let Err(e) = self.offer_upgrade(creds, listener).await {
                    warn!("upgrade: not possible ({e:#}); staying on the current channel");
                    self.upgrade.fail().await;
                }
            }
            UpgradeEvent::Prepared(Ok(upgrade::Prepared::Joined(_))) => {}
            UpgradeEvent::Prepared(Err(e)) => {
                warn!("upgrade: not possible ({e:#}); staying on the current channel");
                self.upgrade.fail().await;
            }
            UpgradeEvent::Connected(Ok(mut stream)) => {
                let acked = upgrade::write_plain_frame(
                    &mut stream,
                    &upgrade::client_introduction_ack_frame(),
                )
                .await;
                match acked {
                    Ok(()) => {
                        self.encrypt_and_send(&upgrade::event_frame(
                            EventType::LastWriteToPriorChannel,
                        ))
                        .await?;
                        self.upgrade.begin_draining(stream);
                    }
                    Err(e) => {
                        warn!("upgrade: ACK failed ({e:#}); staying on the current channel");
                        self.upgrade.fail().await;
                    }
                }
            }
            UpgradeEvent::Connected(Err(e)) => {
                warn!("upgrade: accept failed ({e}); staying on the current channel");
                self.upgrade.fail().await;
            }
            UpgradeEvent::WaitTimedOut => {
                warn!("upgrade: peer did not connect in time; staying on the current channel");
                self.upgrade.fail().await;
            }
            UpgradeEvent::DrainTimedOut => {
                if !self.upgrade.peer_last_write_seen() {
                    return Err(anyhow!("upgrade: no LAST_WRITE from the peer in time"));
                }
                warn!("upgrade: no SAFE_TO_CLOSE in time, switching anyway");
                self.finish_upgrade().await;
            }
            UpgradeEvent::OfferTimedOut => {}
        }
        Ok(())
    }

    async fn process_bwu_frame(&mut self, frame: &OfflineFrame) -> Result<(), anyhow::Error> {
        let Some(bwu) = upgrade::parse_bwu(frame) else {
            return Ok(());
        };
        let event = bwu.event_type();
        match event {
            EventType::UpgradeFailure if self.upgrade.is_waiting() => {
                warn!("upgrade: peer reported a failure; staying on the current channel");
                self.upgrade.fail().await;
            }
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
        self.plain_frames = self.upgrade.disable_encryption();
        if self.plain_frames {
            info!("upgrade: now on the hotspot, plain frames");
        } else {
            info!("upgrade: now on the hotspot");
        }
    }

    async fn reject_transfer(
        &mut self,
        reason: Option<sharing_nearby::connection_response_frame::Status>,
    ) -> Result<(), anyhow::Error> {
        let sreason = if let Some(r) = reason {
            r
        } else {
            sharing_nearby::connection_response_frame::Status::Reject
        };

        let frame = sharing_nearby::Frame {
            version: Some(sharing_nearby::frame::Version::V1.into()),
            v1: Some(sharing_nearby::V1Frame {
                r#type: Some(sharing_nearby::v1_frame::FrameType::Response.into()),
                connection_response: Some(sharing_nearby::ConnectionResponseFrame {
                    status: Some(sreason.into()),
                }),
                ..Default::default()
            }),
        };

        self.send_encrypted_frame(&frame).await?;

        Ok(())
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
                e.decrypt_key = Some(client_key);
                e.recv_hmac_key = Some(client_hmac_key);
                e.encrypt_key = Some(server_key);
                e.send_hmac_key = Some(server_hmac_key);
                e.pin_code = Some(to_four_digit_string(&auth_string));
                e.encryption_done = true;
            },
            false,
        )
        .await;

        info!("Pin code: {:?}", self.state.pin_code);

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
        if self.plain_frames {
            return self.send_frame(frame.encode_to_vec()).await;
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

        self.send_frame(smsg.encode_to_vec()).await?;

        Ok(())
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
        let length = data.len();

        // Prepare length prefix in big-endian format
        let length_bytes = [
            (length >> 24) as u8,
            (length >> 16) as u8,
            (length >> 8) as u8,
            length as u8,
        ];

        let mut prefixed_length = Vec::with_capacity(length + 4);
        prefixed_length.extend_from_slice(&length_bytes);
        prefixed_length.extend_from_slice(&data);

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

    async fn get_client_seq_inc(&mut self) -> i32 {
        self.update_state(
            |e| {
                e.client_seq += 1;
            },
            false,
        )
        .await;

        self.state.client_seq
    }

    async fn update_state<F>(&mut self, f: F, inform: bool)
    where
        F: FnOnce(&mut InnerState),
    {
        f(&mut self.state);

        if !inform {
            return;
        }

        trace!("Sending msg into the channel");
        let _ = self.sender.send(ChannelMessage {
            id: self.state.id.clone(),
            msg: channel::Message::Client(MessageClient {
                kind: TransferKind::Inbound,
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

/// `dir/name`, or `dir/stem (N).ext` with the first N >= 1 for which `taken` is false.
fn free_path(dir: &Path, name: &str, taken: impl Fn(&Path) -> bool) -> PathBuf {
    let first = dir.join(name);
    if !taken(&first) {
        return first;
    }
    let p = Path::new(name);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = p.extension().and_then(|e| e.to_str());
    (1..)
        .map(|n| match ext {
            Some(e) => dir.join(format!("{stem} ({n}).{e}")),
            None => dir.join(format!("{stem} ({n})")),
        })
        .find(|c| !taken(c))
        .expect("unbounded range")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;
    use tokio::sync::broadcast;

    use super::*;
    use crate::upgrade::tests::FakeLink;

    #[test]
    fn free_path_numbers_around_taken_names() {
        let dir = Path::new("/d");
        let taken = |p: &Path| {
            ["/d/a.txt", "/d/a (1).txt", "/d/b"]
                .iter()
                .any(|t| p == Path::new(t))
        };
        assert_eq!(free_path(dir, "c.txt", taken), Path::new("/d/c.txt"));
        assert_eq!(free_path(dir, "a.txt", taken), Path::new("/d/a (2).txt"));
        assert_eq!(free_path(dir, "b", taken), Path::new("/d/b (1)"));
    }

    #[test]
    fn create_new_does_not_truncate_existing_file() {
        let dir = std::env::temp_dir().join(format!("rqs-a1-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.txt");
        std::fs::write(&f, b"keep").unwrap();
        let err = OpenOptions::new().write(true).create_new(true).open(&f);
        assert_eq!(err.unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&f).unwrap(), b"keep");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// While the hotspot is being brought up in the background, the old channel is still read.
    #[tokio::test]
    async fn peer_frames_are_processed_while_the_hotspot_comes_up() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut peer = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        let (bus, _rx) = broadcast::channel(8);
        let mut req = InboundRequest::new(socket, "id".into(), bus);

        let link = Arc::new(FakeLink {
            host_delay: Duration::from_secs(5),
            ..Default::default()
        });
        req.upgrade = UpgradeState::new(Some(link.clone()));
        let hosting = TcpListener::bind("127.0.0.1:0").await.unwrap();
        req.upgrade
            .begin_preparing(tokio::spawn(upgrade::prepare_host(link.clone(), hosting)));

        // An absurd frame length makes `handle` fail right away, which proves it ran.
        peer.write_all(&[0xff; 4]).await.unwrap();
        let handled = tokio::time::timeout(Duration::from_secs(1), req.handle())
            .await
            .expect("handle() was blocked by the pending hotspot");
        assert!(handled.is_err());
    }

    /// After an upgrade with disabled encryption frames are bare OfflineFrames both ways.
    #[tokio::test]
    async fn plain_mode_reads_and_writes_bare_offline_frames() {
        let (mut peer, ours) = tokio::io::duplex(4096);
        let (bus, _rx) = tokio::sync::broadcast::channel(8);
        let mut req = InboundRequest::new_over(MigratableStream::Duplex(ours), "id".into(), bus);
        req.plain_frames = true;
        req.state.state = TransferState::SentConnectionResponse;

        crate::upgrade::tests::write_plain_keep_alive(&mut peer).await;
        tokio::time::timeout(Duration::from_secs(1), req.handle())
            .await
            .expect("handle() did not read the plain frame")
            .unwrap();
        let reply = upgrade::read_plain_frame(&mut peer).await.unwrap();
        crate::upgrade::tests::assert_keep_alive_ack(&reply);
    }

    /// A length prefix cut in two by a cancelled `handle` must not lose the first bytes.
    #[tokio::test]
    async fn cancelled_handle_keeps_a_partial_length_prefix() {
        let (mut peer, ours) = tokio::io::duplex(64);
        let (bus, _rx) = broadcast::channel(8);
        let mut req = InboundRequest::new_over(MigratableStream::Duplex(ours), "id".into(), bus);

        peer.write_all(&[0xff, 0xff]).await.unwrap();
        // Nothing complete yet: the read is dropped by the timeout, as `select!` would.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), req.handle())
                .await
                .is_err()
        );
        peer.write_all(&[0xff, 0xff]).await.unwrap();
        // The absurd length is only seen if the first two bytes were kept.
        let handled = tokio::time::timeout(Duration::from_secs(1), req.handle())
            .await
            .expect("the first half of the prefix was lost");
        assert!(handled.is_err());
    }

    /// The phone may send keep-alives while it thinks; only `MAX_HANDSHAKE_KEEP_ALIVES` are accepted.
    #[tokio::test]
    async fn handshake_keep_alives_are_capped() {
        let (mut peer, ours) = tokio::io::duplex(4096);
        let (bus, _rx) = tokio::sync::broadcast::channel(8);
        let mut req = InboundRequest::new_over(MigratableStream::Duplex(ours), "id".into(), bus);
        req.state.state = TransferState::ReceivedUkeyClientFinish;

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
}
