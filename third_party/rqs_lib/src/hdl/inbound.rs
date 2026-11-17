use std::collections::VecDeque;
use std::path::PathBuf;
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
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::outbound::{
    CANCEL_TIMEOUT, HandshakeStep, disconnection_frame, handshake_step, is_safe_disconnect_request,
    safe_disconnect_ack_frame, sharing_cancel_frame,
};
use super::{InnerState, TransferState};
use crate::channel::{self, ChannelMessage, MessageClient, TransferAction, TransferKind};
use crate::hdl::info::{TransferMetadata, TransferPayload, TransferPayloadKind};
use crate::hdl::{MigratableStream, TextPayloadInfo};
use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::EventType;
use crate::location_nearby_connections::payload_transfer_frame::{
    PacketType, PayloadChunk, PayloadHeader, payload_header,
};
use crate::location_nearby_connections::{KeepAliveFrame, OfflineFrame, PayloadTransferFrame};
use crate::safety::{self, DestDir, Limits, Refusal};
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
    get_download_dir, hkdf_extract_expand, read_length_prefix, to_four_digit_string,
};
use crate::{location_nearby_connections, sharing_nearby};

type HmacSha256 = Hmac<Sha256>;

const SANE_FRAME_LENGTH: i32 = 5 * 1024 * 1024;
const SANITY_DURATION: Duration = Duration::from_micros(10);
/// How long looking at the download folder (space, names) may take at the offer.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// Bound on the FIN after the Cancel frames at a stop (the close itself is `Limits::stop_close`).
pub(crate) const FIN_TIMEOUT: Duration = Duration::from_millis(50);
/// How long a session stopped by the service waits for its peer to close after the Cancel
/// (a user cancel waits `CANCEL_TIMEOUT`): the stop of the service waits for the sessions.

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
    limits: Limits,
    /// When the session began (the handshake deadline counts from here).
    started: Instant,
    /// The last time the peer sent something (or an upgrade step happened).
    last_rx: Instant,
    /// When the session began to wait for the user (the ceiling counts from here).
    consent_since: Instant,
    /// A write failed or timed out: a half frame may be on the wire, so nothing more is
    /// written and the session ends.
    poisoned: bool,
    /// A frame body is being read: the stream is not at a frame boundary.
    in_body: bool,
    /// Our Disconnection asked for the safe-to-disconnect handshake: its ack is awaited.
    disconnect_requested: bool,
    /// Ids of the last `Bytes` payloads completed, to ignore a repeated empty last chunk.
    completed: VecDeque<i64>,
    /// Cancelled when the service stops: the session then ends like a user cancel (see
    /// `on_stop`). Never cancelled unless the owner of the session sets it (`with_stop`).
    stop: CancellationToken,
    /// Tests only: where files go, instead of the process-wide download directory.
    #[cfg(test)]
    download_dir: Option<PathBuf>,
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
            limits: Limits::default(),
            started: Instant::now(),
            last_rx: Instant::now(),
            consent_since: Instant::now(),
            poisoned: false,
            in_body: false,
            disconnect_requested: false,
            completed: VecDeque::new(),
            stop: CancellationToken::new(),
            #[cfg(test)]
            download_dir: None,
        }
    }

    /// Replaces the default limits (tests, or a caller with other needs).
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Ends the session when `stop` is cancelled (the service shutting down), like a user cancel
    /// where the stream can still be written, else by dropping it. The partial files are removed
    /// and the app gets the final state.
    pub fn with_stop(mut self, stop: CancellationToken) -> Self {
        self.stop = stop;
        self
    }

    fn dest_dir(&self) -> PathBuf {
        #[cfg(test)]
        if let Some(dir) = &self.download_dir {
            return dir.clone();
        }
        get_download_dir()
    }

    /// Offer to disable encryption on the upgraded channel (never done outside tests).
    #[cfg(test)]
    pub(crate) fn offer_disabling_encryption(&mut self) {
        self.upgrade.set_disable_encryption(true);
    }

    /// Handles the next event of the session: a bus message, a frame, an upgrade step or a
    /// timeout. An error ends the session; the files not complete are removed first. Only the
    /// wait for the length prefix is cancel-safe: once it is complete the frame is read to
    /// its end (serving the bus meanwhile), so do not drop the future after that point.
    pub async fn handle(&mut self) -> Result<(), anyhow::Error> {
        let result = self.handle_event().await;
        if result.is_err() {
            self.state.inbound.discard();
        }
        result
    }

    /// The last moment the peer may still be silent, if it may be silent at all. Not while
    /// the user decides (the core's own timer applies) nor while an upgrade runs (each of its
    /// steps has a deadline).
    fn idle_deadline(&self) -> Option<Instant> {
        idle_deadline_for(
            &self.state.state,
            self.upgrade.in_progress(),
            self.last_rx,
            self.started,
            self.consent_since,
            &self.limits,
        )
    }

    async fn handle_event(&mut self) -> Result<(), anyhow::Error> {
        // `select!` picks at random among the branches that are ready, so a stop could lose
        // against a queued bus message (or a frame) every time it is looked at; check it first.
        if self.stop.is_cancelled() {
            return Err(self.on_stop().await);
        }
        let idle_deadline = self.idle_deadline();
        tokio::select! {
            _ = tokio::time::sleep_until(idle_deadline.unwrap_or_else(Instant::now)), if idle_deadline.is_some() => {
                if self.state.state == TransferState::WaitingForUserConsent {
                    // The core's own timer did not fire (it may have missed the event).
                    warn!("The user did not answer in time; declining");
                    return self.decline().await;
                }
                return Err(anyhow!("no data from the peer within the time allowed"));
            },
            () = self.stop.cancelled() => {
                return Err(self.on_stop().await);
            },
            i = self.receiver.recv() => {
                self.on_bus(i).await?;
            },
            h = read_length_prefix(&mut self.socket, &mut self.length_buf, &mut self.length_filled) => {
                let prefix = h?;
                self.last_rx = Instant::now();
                self.handle_frame(prefix).await?
            },
            ev = self.upgrade.next_event() => {
                self.on_upgrade_event(ev).await?;
                self.last_rx = Instant::now();
            }
        }

        Ok(())
    }

    /// A message from the bus (the app's accept, decline or cancel for this transfer).
    async fn on_bus(
        &mut self,
        i: Result<ChannelMessage, tokio::sync::broadcast::error::RecvError>,
    ) -> Result<(), anyhow::Error> {
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
                        }
                        TransferAction::ConsentDecline => {
                            return self.decline().await;
                        }
                        TransferAction::TransferCancel => {
                            self.update_state(
                                |e| {
                                    e.state = TransferState::Cancelled;
                                },
                                true,
                            )
                            .await;
                            self.cancel_peer().await;
                            return Err(anyhow!(crate::errors::AppError::NotAnError));
                        }
                    }
                }
            }
            Err(e) => {
                error!("inbound: channel error: {}", e);
            }
        }
        Ok(())
    }

    /// Handles one frame whose length prefix was read. An error ends the session; the files
    /// not complete are removed first.
    #[cfg(test)]
    pub(crate) async fn _handle(&mut self, length_buf: [u8; 4]) -> Result<(), anyhow::Error> {
        let result = self.handle_frame(length_buf).await;
        if result.is_err() {
            self.state.inbound.discard();
        }
        result
    }

    /// Reads a frame body without sizing a buffer by what the peer announced: the buffer
    /// grows as bytes arrive. Every read must make progress within the idle time, the whole
    /// frame has a deadline (a slow link gets `MIN_RATE` bytes per second on top of the idle
    /// time; before the offer the handshake deadline caps it), and the bus is served while
    /// waiting, so a decline or a cancel never waits for a slow peer.
    async fn read_body(&mut self, len: usize) -> Result<Vec<u8>, anyhow::Error> {
        const STEP: usize = 64 * 1024;
        const MIN_RATE: u64 = 2048;
        let mut hard =
            Instant::now() + self.limits.idle + Duration::from_secs(len as u64 / MIN_RATE);
        if self.state.state == TransferState::WaitingForUserConsent {
            hard = hard.min(self.consent_since + self.limits.consent);
        }
        let mut frame = Vec::with_capacity(len.min(STEP));
        while frame.len() < len {
            let start = frame.len();
            let end = len.min(start + STEP);
            frame.resize(end, 0);
            let mut filled = start;
            while filled < end {
                // Not left to the `select!` below, which may keep picking the bus or the read.
                if self.stop.is_cancelled() {
                    return Err(self.on_stop().await);
                }
                if is_handshaking(&self.state.state) {
                    hard = hard.min(self.started + self.limits.handshake);
                }
                // From the last byte, not from now: bus traffic must not push it back.
                let deadline = read_deadline(self.last_rx, self.limits.idle, hard);
                let n = tokio::select! {
                    r = tokio::time::timeout_at(deadline, self.socket.read(&mut frame[filled..end])) => {
                        r.map_err(|_| anyhow!("the peer stopped in the middle of a frame"))??
                    }
                    () = self.stop.cancelled() => {
                        return Err(self.on_stop().await);
                    }
                    i = self.receiver.recv() => {
                        self.on_bus(i).await?;
                        continue;
                    }
                };
                if n == 0 {
                    return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
                }
                filled += n;
                self.last_rx = Instant::now();
            }
        }
        Ok(frame)
    }

    async fn handle_frame(&mut self, length_buf: [u8; 4]) -> Result<(), anyhow::Error> {
        let msg_length = u32::from_be_bytes(length_buf) as usize;
        // Ensure the message length is not unreasonably big to avoid allocation attacks
        if msg_length > SANE_FRAME_LENGTH as usize {
            error!("Message length too big");
            return Err(anyhow!("value"));
        }

        self.in_body = true;
        let frame_data = self.read_body(msg_length).await;
        self.in_body = false;
        let frame_data = frame_data?;

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
                    self.upgrade
                        .set_peer_5ghz(cr.medium_metadata.as_ref().and_then(|m| m.supports_5_ghz));
                    self.upgrade.set_peer_endpoint_id(cr.endpoint_id());
                    info!(
                        "upgrade: peer lists LAN={}, hotspot={} (mediums {:?})",
                        upgrade::peer_allows_lan(&cr.mediums),
                        upgrade::peer_allows_hotspot(&cr.mediums),
                        cr.mediums
                    );
                }
                debug!(
                    "Peer is a {:?}, its name has {} bytes",
                    rdi.device_type,
                    rdi.name.len()
                );

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

        // The peer's safe-to-disconnect level, from its ConnectionResponse: six or more means
        // it expects our encrypted PAYLOAD_ACK for each received FILE payload.
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

        let response = location_nearby_connections::OfflineFrame {
			version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
			v1: Some(location_nearby_connections::V1Frame {
				r#type: Some(location_nearby_connections::v1_frame::FrameType::ConnectionResponse.into()),
				connection_response: Some(location_nearby_connections::ConnectionResponseFrame {
					response: Some(location_nearby_connections::connection_response_frame::ResponseStatus::Accept.into()),
					// The level this receiver answers at: 6 means the peer may expect
					// one encrypted PAYLOAD_ACK per received FILE payload.
					safe_to_disconnect_version: Some(6),
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
        let offline = self.decrypt_secure_message(smsg).await?;
        self.process_offline_frame(&offline).await
    }

    /// The offline frame inside a secure message, with its signature and sequence checked.
    async fn decrypt_secure_message(
        &mut self,
        smsg: &SecureMessage,
    ) -> Result<OfflineFrame, anyhow::Error> {
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

                // We send no files from here, only Bytes, which our level 6 does not ask the
                // peer to ack; an ack that comes anyway carries no chunk and is ignored.
                if payload_transfer.packet_type()
                    == location_nearby_connections::payload_transfer_frame::PacketType::PayloadAck
                {
                    debug!("Ignoring a payload ack from the sender");
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
                        let payload_id = header.id();
                        // The id of an offered file is for its FILE payload, not for bytes
                        // that would be decoded as a sharing frame.
                        if self.state.inbound.position(payload_id).is_some() {
                            return Err(anyhow!("a bytes payload with the id of an offered file"));
                        }
                        let is_text = self
                            .state
                            .text_payload
                            .as_ref()
                            .is_some_and(|t| t.get_i64_value() == payload_id);
                        // A text, URL or Wi-Fi payload is only taken once the user accepted.
                        if is_text && self.state.state != TransferState::ReceivingFiles {
                            self.state.payload_buffers.remove(&payload_id);
                            return Err(anyhow!("a text payload arrived before the user accepted"));
                        }

                        // A peer may repeat the empty last chunk of a payload that is complete.
                        if chunk.body().is_empty()
                            && chunk.flags() & 1 == 1
                            && !self.state.payload_buffers.contains_key(&payload_id)
                            && self.completed.contains(&payload_id)
                        {
                            return Ok(());
                        }
                        // Before the accept only the sharing frames come, one at a time.
                        let mut limits = self.limits;
                        if self.state.state != TransferState::ReceivingFiles {
                            limits.max_open_payloads = 1;
                        }
                        // `None` until the last chunk; the payload is then forgotten here.
                        let Some(buffer) = safety::assemble(
                            &mut self.state.payload_buffers,
                            header,
                            chunk,
                            &limits,
                        )?
                        else {
                            return Ok(());
                        };
                        self.completed.push_back(payload_id);
                        if self.completed.len() > 16 {
                            self.completed.pop_front();
                        }

                        {
                            debug!("Bytes payload complete");

                            if is_text {
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
                                        // The BYTE payload is the serialized `WifiCredentials`
                                        // message (password = field 1, hidden_ssid = field 2). The
                                        // sender may leave out `hidden_ssid`, order the fields
                                        // differently or add fields in a later version, so decode
                                        // the message instead of looking for the 0x10 tag by hand.
                                        let payload = match security_type {
                                            SecurityType::Open => String::new(),
                                            SecurityType::WpaPsk
                                            | SecurityType::Wep
                                            | SecurityType::Sae => wifi_password(&buffer)
                                                .inspect_err(|err| error!("{err:#}"))
                                                .unwrap_or_default(),
                                            SecurityType::UnknownSecurityType => String::new(),
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
                                self.linger().await;
                                return Err(anyhow!(crate::errors::AppError::NotAnError));
                            } else {
                                let inner_frame = sharing_nearby::Frame::decode(buffer.as_slice())?;
                                self.process_transfer_setup(&inner_frame).await?;
                            }
                        }
                    }
                    payload_header::PayloadType::File => {
                        trace!("Processing PayloadType::File");
                        // Nothing is written before the user accepted.
                        if self.state.state != TransferState::ReceivingFiles {
                            return Err(anyhow!("file data arrived before the user accepted"));
                        }
                        let payload_id = header.id();

                        let file = self.state.inbound.get_active(payload_id).ok_or_else(|| {
                            anyhow!("File payload ID ({}) is not known", payload_id)
                        })?;
                        if file.finished {
                            // Some peers repeat the empty last chunk of a file.
                            if chunk.body().is_empty() {
                                return Ok(());
                            }
                            return Err(anyhow!("data for a file that is already complete"));
                        }
                        let partial = file.partial.as_mut().ok_or_else(|| {
                            anyhow!("File payload ID ({}) is not receiving", payload_id)
                        })?;

                        let body = chunk.body();
                        partial.write_at(header.total_size(), chunk.offset(), body)?;

                        if !body.is_empty() {
                            let chunk_size = body.len() as u64;
                            self.update_state(
                                |e| {
                                    if let Some(tmd) = e.transfer_metadata.as_mut() {
                                        tmd.ack_bytes += chunk_size;
                                    }
                                },
                                true,
                            )
                            .await;
                        }
                        if (chunk.flags() & 1) == 1 {
                            self.finish_file(payload_id).await?;
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
            location_nearby_connections::v1_frame::FrameType::Disconnection => {
                if is_safe_disconnect_request(offline) {
                    self.encrypt_and_send(&safe_disconnect_ack_frame()).await?;
                }
                debug!("The peer is disconnecting");
            }
            other => {
                error!("Unhandled offline frame encrypted: {:?}", other);
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
            self.linger().await;
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
        // A Pixel 7 sends its own Response next to the Introduction, as two payloads written
        // at the same time, so it can come first (seen in a hand test): it says nothing about
        // the offer, which is still to come.
        if v1_frame.r#type() == sharing_nearby::v1_frame::FrameType::Response {
            debug!("A Response from the sender before its Introduction is ignored");
            return Ok(());
        }
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
        self.consent_since = Instant::now();

        if !introduction.file_metadata.is_empty() && introduction.text_metadata.is_empty() {
            trace!("process_introduction: handling file_metadata");
            let offered = match safety::check_offer(&introduction.file_metadata, &self.limits) {
                Ok(offered) => offered,
                Err(refusal) => return self.refuse(refusal).await,
            };
            let total_bytes = offered
                .iter()
                .fold(0u64, |sum, o| sum.saturating_add(o.size));
            debug!("Offer of {} file(s), {} bytes", offered.len(), total_bytes);

            // Where the files would go and whether they fit, without creating anything yet.
            let dir = self.dest_dir();
            let names: Vec<String> = offered.iter().map(|o| o.name.clone()).collect();
            let need = total_bytes.saturating_add(self.limits.space_reserve);
            // Only looks: the folder is not created before the user accepted.
            let task = {
                let dir = dir.clone();
                tokio::task::spawn_blocking(move || {
                    let dest = match DestDir::open_existing(&dir) {
                        Ok(d) => Some(d),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                        Err(e) => return Err(e),
                    };
                    // A file system that does not report its size is not a reason to refuse.
                    let free_now = |dest: &Option<std::sync::Arc<DestDir>>| {
                        match dest {
                            Some(d) => d.free_bytes(),
                            None => DestDir::free_bytes_near(&dir),
                        }
                        .unwrap_or(u64::MAX)
                    };
                    let mut free = free_now(&dest);
                    // A crashed session's staging folder must not make every offer too big:
                    // the only thing an offer removes, and only when space is short.
                    if free < need
                        && let Some(d) = &dest
                    {
                        d.sweep_stale();
                        free = free_now(&dest);
                    }
                    let planned = match &dest {
                        Some(d) => d.predict(&names),
                        None => names,
                    };
                    Ok((dest, free, planned))
                })
            };
            // A hung network mount must not hold the offer for ever.
            let probe = match tokio::time::timeout(PROBE_TIMEOUT, task).await {
                Ok(joined) => joined.context("inspecting the download folder")?,
                Err(_) => Err(std::io::ErrorKind::TimedOut.into()),
            };
            let (dest, free, planned) = match probe {
                Ok(probe) => probe,
                Err(e) => {
                    debug!("The download folder cannot be used: {e}");
                    return self.refuse(Refusal::Destination).await;
                }
            };
            // The core starts its own consent timer when it sees the offer, after this probe:
            // count the library's ceiling from here so it stays well above the core's.
            self.consent_since = Instant::now();
            if free < need {
                return self.refuse(Refusal::NotEnoughSpace { need, free }).await;
            }

            let saved_paths = planned
                .iter()
                .map(|n| dir.join(n).to_string_lossy().into_owned())
                .collect();
            let files_name = offered.iter().map(|o| o.name.clone()).collect();
            self.state.inbound.set(offered);
            self.state.inbound.dest = dest;

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
                refusal: None,
            };

            info!("Asking for user consent: files");
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
                        refusal: None,
                    };

                    info!("Asking for user consent: URL");
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
                        refusal: None,
                    };

                    info!("Asking for user consent: text");
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
                    // Reject transfer; nobody is asked, so the idle timeout applies again
                    self.update_state(|e| e.state = TransferState::Rejected, false)
                        .await;
                    self.reject_transfer(Some(
                        sharing_nearby::connection_response_frame::Status::UnsupportedAttachmentType,
                    ))
                    .await?;
                    self.linger().await;
                    return Err(anyhow!(crate::errors::AppError::NotAnError));
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
                refusal: None,
            };

            info!("Asking for user consent: Wi-Fi credentials");
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
            // Reject transfer; nobody is asked, so the idle timeout applies again
            self.update_state(|e| e.state = TransferState::Rejected, false)
                .await;
            self.reject_transfer(Some(
                sharing_nearby::connection_response_frame::Status::UnsupportedAttachmentType,
            ))
            .await?;
            self.linger().await;
            return Err(anyhow!(crate::errors::AppError::NotAnError));
        }

        // A text or link is sent as soon as the user accepts, too soon for an upgrade started
        // then. Offer the existing LAN now, while the user is still reading the notice.
        if introduction.file_metadata.is_empty() && !introduction.text_metadata.is_empty() {
            self.start_lan_upgrade_early().await;
        }

        Ok(())
    }

    /// The user declined (or did not answer): tells the app and the peer and ends the session.
    async fn decline(&mut self) -> Result<(), anyhow::Error> {
        self.update_state(
            |e| {
                e.state = TransferState::Rejected;
            },
            true,
        )
        .await;
        self.reject_transfer(Some(
            sharing_nearby::connection_response_frame::Status::Reject,
        ))
        .await?;
        self.linger().await;
        Err(anyhow!(crate::errors::AppError::NotAnError))
    }

    /// After a reject frame or the closing frames: closes our side and gives the peer a moment to read the frame
    /// and close, so the socket going away does not turn into a reset that drops it.
    async fn linger(&mut self) {
        let deadline = tokio::time::Instant::now() + CANCEL_TIMEOUT;
        if self.disconnect_requested {
            self.finish_safe_disconnect(deadline).await;
        }
        let closing = async {
            if let Err(e) = self.socket.shutdown().await {
                debug!("Closing the write half failed: {e}");
            }
            let mut sink = [0u8; 4096];
            while matches!(self.socket.read(&mut sink).await, Ok(n) if n > 0) {}
        };
        let _ = tokio::time::timeout_at(deadline, closing).await;
    }

    /// Whether this session ends with the safe-to-disconnect handshake: the peer reads it from
    /// level 1 on, and a phone that is not answered takes the end for a lost link (it logs
    /// "unsafe disconnection" and tries to reconnect, for seconds on a cancel).
    fn asks_safe_disconnect(&self) -> bool {
        self.state.peer_disconnect_version >= 1 && !self.poisoned
    }

    /// The rest of the handshake after our request (upstream `EndpointManager`): reads the
    /// peer's frames until `deadline` (see `handshake_step`). Not in the middle of a frame body
    /// (a cancel while one is read): the rest of it cannot be told from the next frame, and the
    /// peer has our request anyway.
    async fn finish_safe_disconnect(&mut self, deadline: tokio::time::Instant) {
        if self.in_body {
            return;
        }
        loop {
            let Ok(Ok(length)) = tokio::time::timeout_at(
                deadline,
                read_length_prefix(
                    &mut self.socket,
                    &mut self.length_buf,
                    &mut self.length_filled,
                ),
            )
            .await
            else {
                return;
            };
            let len = u32::from_be_bytes(length) as usize;
            if len > SANE_FRAME_LENGTH as usize {
                return;
            }
            let mut body = vec![0u8; len];
            let Ok(Ok(_)) =
                tokio::time::timeout_at(deadline, self.socket.read_exact(&mut body)).await
            else {
                return;
            };
            let offline = if self.plain_frames {
                OfflineFrame::decode(&*body).map_err(anyhow::Error::from)
            } else {
                match SecureMessage::decode(&*body) {
                    Ok(smsg) => self.decrypt_secure_message(&smsg).await,
                    Err(e) => Err(e.into()),
                }
            };
            let Ok(offline) = offline else {
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

    /// Declines an offer the library cannot take: the app is told (`Rejected`, with the
    /// reason), the peer gets an existing status, and the session ends.
    async fn refuse(&mut self, refusal: Refusal) -> Result<(), anyhow::Error> {
        warn!("Offer refused: {}", refusal.key());
        let metadata = TransferMetadata {
            id: self.state.id.clone(),
            source: self.state.remote_device_info.clone(),
            payload_kind: TransferPayloadKind::Files,
            payload_preview: Default::default(),
            payload: None,
            pin_code: self.state.pin_code.clone(),
            total_bytes: Default::default(),
            ack_bytes: Default::default(),
            saved_paths: Vec::new(),
            refusal: Some(refusal.clone()),
        };
        self.update_state(
            |e| {
                e.state = TransferState::Rejected;
                e.transfer_metadata = Some(metadata);
            },
            true,
        )
        .await;
        self.reject_transfer(Some(refusal.status())).await?;
        self.linger().await;
        Err(anyhow!(crate::errors::AppError::NotAnError))
    }

    /// Completes one file (all bytes there and on disk). The transfer is `Finished` when
    /// every file of the offer is.
    async fn finish_file(&mut self, payload_id: i64) -> Result<(), anyhow::Error> {
        let partial = self
            .state
            .inbound
            .get_mut(payload_id)
            .and_then(|f| f.partial.take())
            .ok_or_else(|| anyhow!("File payload ID ({}) is not receiving", payload_id))?;
        // Blocking: fsync and the rename into the download folder. If it fails the staged file
        // is removed when `partial` is dropped there.
        let published = tokio::task::spawn_blocking(move || partial.finish())
            .await
            .context("finishing a received file")?
            .context("the received file is not complete")?;
        debug!("A received file is complete");
        if let Some(file) = self.state.inbound.get_mut(payload_id) {
            file.finished = true;
        }
        // The real path, before the ack and before `Finished` carry it to the app.
        if let Some(slot) = self
            .state
            .inbound
            .position(payload_id)
            .zip(self.state.transfer_metadata.as_mut())
            .and_then(|(i, tmd)| tmd.saved_paths.get_mut(i))
        {
            *slot = published.to_string_lossy().into_owned();
        }

        if !self.state.inbound.all_finished() {
            // One encrypted ACK per completed FILE payload (crQuickShare `AckFile`, matching
            // Nearby PayloadManager), now that the file is durable. A failed ack is a failed
            // transfer: the phone keeps "sending" until it knows the file is here.
            return self.send_payload_ack(payload_id).await;
        }
        // The last file of the batch: the ack goes out before the polite close, and its
        // failure is only logged — the files are received either way (crQuickShare defers the
        // final ACK until the batch is committed and does not fail the session on it).
        if let Err(e) = self.send_payload_ack(payload_id).await {
            debug!("the final payload ack could not be sent: {e:#}");
        }
        info!("Transfer finished");
        self.update_state(
            |e| {
                e.state = TransferState::Finished;
            },
            true,
        )
        .await;
        self.disconnection().await?;
        self.linger().await;
        Err(anyhow!(crate::errors::AppError::NotAnError))
    }

    /// One encrypted `PAYLOAD_ACK` per completed FILE payload (crQuickShare `AckFile`,
    /// matching Nearby PayloadManager): a `PayloadTransfer` frame with the payload's id and
    /// `total_size = -1`, no chunk. Only a peer at safe-to-disconnect level 6 or more reads
    /// it, older peers keep their old behaviour without it.
    async fn send_payload_ack(&mut self, payload_id: i64) -> Result<(), anyhow::Error> {
        if self.state.peer_disconnect_version < 6 {
            return Ok(());
        }
        debug!("Sending the payload ack of a completed file");
        let frame = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::PayloadTransfer.into(),
                ),
                payload_transfer: Some(PayloadTransferFrame {
                    packet_type: Some(PacketType::PayloadAck.into()),
                    payload_header: Some(PayloadHeader {
                        id: Some(payload_id),
                        total_size: Some(-1),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };
        self.encrypt_and_send(&frame).await
    }

    /// Tells the peer that the user cancelled and closes our side, as the outbound side does:
    /// the sharing Cancel, the Disconnection, a FIN, then a wait for the peer to close, all
    /// within `CANCEL_TIMEOUT`. Failures are only logged. Called from a bus message, handled
    /// between two frames or while a frame body is read: nothing else writes meanwhile.
    async fn cancel_peer(&mut self) {
        info!("Transfer canceled by the user");
        self.close_with_cancel(CANCEL_TIMEOUT).await;
    }

    /// The service is stopping. A session the app knows about ends as `Cancelled` (the partial
    /// files go with the session), and the peer is told when a frame can still be written:
    /// between frames or while a body is read, nothing writes meanwhile, and a stream with a
    /// failed write is never written again. Always returns the error that ends the session.
    async fn on_stop(&mut self) -> anyhow::Error {
        let state = self.state.state.clone();
        let known = !matches!(
            state,
            TransferState::Initial
                | TransferState::Disconnected
                | TransferState::Rejected
                | TransferState::Cancelled
                | TransferState::Finished
        );
        if known {
            info!("Transfer ended: the service is stopping");
            self.update_state(
                |e| {
                    e.state = TransferState::Cancelled;
                },
                true,
            )
            .await;
            if !self.poisoned && (self.plain_frames || self.state.encrypt_key.is_some()) {
                self.close_with_cancel(self.limits.stop_close).await;
            }
        }
        anyhow!(crate::errors::AppError::NotAnError)
    }

    /// The sharing Cancel, the Disconnection, a FIN and a wait for the peer to close, within
    /// `limit`.
    async fn close_with_cancel(&mut self, limit: Duration) {
        let deadline = tokio::time::Instant::now() + limit;
        let sent = tokio::time::timeout_at(deadline, async {
            self.send_encrypted_frame(&sharing_cancel_frame()).await?;
            self.disconnection().await
        })
        .await;
        match sent {
            Ok(Ok(())) => {
                if self.disconnect_requested {
                    self.finish_safe_disconnect(deadline).await;
                }
            }
            Ok(Err(e)) => debug!("Cancel frames not sent: {e}"),
            Err(_) => debug!("Cancel frames not sent in time"),
        }
        // The FIN goes out even when the frames were late, so the peer always sees the end; a
        // frame cut off by the deadline is never followed by another one (no write but this).
        match tokio::time::timeout(FIN_TIMEOUT, self.socket.shutdown()).await {
            Ok(Err(e)) => debug!("Closing the write half failed: {e}"),
            Err(_) => debug!("Closing the write half timed out"),
            Ok(Ok(())) => {}
        }
        let mut sink = [0u8; 4096];
        let drained = tokio::time::timeout_at(deadline, async {
            while matches!(self.socket.read(&mut sink).await, Ok(n) if n > 0) {}
        })
        .await;
        if drained.is_err() {
            debug!("The peer did not close after the cancel in time");
        }
    }

    /// Our Disconnection: a safe-to-disconnect request to a peer that reads one (level 1 or
    /// more), answered in `finish_safe_disconnect`; a plain one otherwise.
    async fn disconnection(&mut self) -> Result<(), anyhow::Error> {
        let request = self.asks_safe_disconnect();
        let frame = disconnection_frame(request);
        self.disconnect_requested = request;

        if self.state.encryption_done {
            self.encrypt_and_send(&frame).await
        } else {
            self.send_frame(frame.encode_to_vec()).await
        }
    }

    /// The user accepted: the staging folder and the files in it are created now, never before.
    /// Only taken once, and only while the user is being asked.
    async fn accept_transfer(&mut self) -> Result<(), anyhow::Error> {
        if self.state.state != TransferState::WaitingForUserConsent {
            debug!("Ignoring an accept in state {:?}", self.state.state);
            return Ok(());
        }

        if !self.state.inbound.is_empty() {
            let dir = self.dest_dir();
            let kept = self.state.inbound.dest.take();
            let plan = self.state.inbound.plan();
            let need = self
                .state
                .inbound
                .total_bytes()
                .saturating_add(self.limits.space_reserve);
            // Blocking calls. The files are reserved here in the hidden staging folder, one by
            // one, each with an exclusive create; if one fails, the ones before it and the
            // folder are removed again as `created` drops.
            let created = tokio::task::spawn_blocking(move || {
                // The folder looked at when the offer came, if it is still the one set and
                // still where it was. A changed setting opens the new folder (created if need be).
                let dest = match kept {
                    Some(d) if d.path() == dir.as_path() => {
                        if !d.is_still_at_path() {
                            return Err(Refusal::Destination);
                        }
                        d
                    }
                    _ => DestDir::open(&dir).map_err(|e| {
                        warn!("The download folder cannot be opened: {e}");
                        Refusal::Destination
                    })?,
                };
                if let Ok(free) = dest.free_bytes()
                    && free < need
                {
                    return Err(Refusal::NotEnoughSpace { need, free });
                }
                // What the files are called when they are complete, if no name is taken
                // meanwhile; a completed file reports its real path.
                let names: Vec<String> = plan.iter().map(|(name, _)| name.clone()).collect();
                let planned = dest.predict(&names);
                let saved_paths: Vec<String> = planned
                    .iter()
                    .map(|name| dest.path().join(name).to_string_lossy().into_owned())
                    .collect();
                let staging = dest.staging().map_err(|e| {
                    warn!("The staging folder cannot be made: {e}");
                    Refusal::Destination
                })?;
                let mut created = Vec::with_capacity(plan.len());
                for ((name, size), planned) in plan.iter().zip(planned) {
                    let mut partial = staging.create(name, *size).map_err(|e| {
                        warn!("A received file cannot be reserved: {e}");
                        Refusal::Destination
                    })?;
                    partial.plan(planned);
                    created.push(partial);
                }
                Ok((created, saved_paths))
            })
            .await
            .context("creating the files")?;
            let (created, saved_paths) = match created {
                Ok(created) => created,
                Err(refusal) => return self.refuse(refusal).await,
            };
            // The service may have stopped while the folder was prepared: no Accept, and no
            // upgrade, for a session that is ending.
            if self.stop.is_cancelled() {
                return Err(self.on_stop().await);
            }
            debug!("Reserved {} file(s)", created.len());

            // The names may differ from the preview (a name taken since the offer).
            for (file, partial) in self.state.inbound.iter_mut().zip(created) {
                file.partial = Some(partial);
            }
            if let Some(tmd) = self.state.transfer_metadata.as_mut() {
                tmd.saved_paths = saved_paths;
            }
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
        // The user may have taken minutes: the idle time counts from now.
        self.last_rx = Instant::now();

        // Text and URLs are not worth a hotspot.
        if !self.state.inbound.is_empty() {
            self.start_upgrade().await;
        }

        Ok(())
    }

    /// Offers the existing LAN while the user is still deciding. The payload of a text or link
    /// follows the accept at once: an upgrade started then would come too late, while one
    /// already offered can be in place. Only the LAN is prepared (it leaves the network alone);
    /// a hotspot is not worth it for a payload this small.
    async fn start_lan_upgrade_early(&mut self) {
        let Some(link) = self.upgrade.link().filter(|_| self.upgrade.can_start()) else {
            return;
        };
        if self.upgrade.peer_endpoint_id().is_none() {
            return;
        }
        let (lan, _, peer_5ghz) = self.upgrade.peer_upgrade_options();
        if !lan {
            return;
        }
        info!("upgrade: preparing the LAN before the user decides");
        self.upgrade.set_lan_only();
        let task = tokio::spawn(upgrade::prepare_inbound(link, true, false, peer_5ghz));
        self.upgrade.begin_preparing(task);
    }

    /// Starts preparing the upgrade in the background: the existing LAN if the peer lists
    /// WIFI_LAN and a LAN can be offered, otherwise (or if that fails) a hotspot. The offer goes
    /// out once it is ready. Any failure leaves the session on the current channel.
    async fn start_upgrade(&mut self) {
        let Some(link) = self.upgrade.link().filter(|_| self.upgrade.can_start()) else {
            return;
        };
        if self.upgrade.peer_endpoint_id().is_none() {
            return;
        }
        let (lan, hotspot, peer_5ghz) = self.upgrade.peer_upgrade_options();
        info!("upgrade: preparing (LAN allowed: {lan}, hotspot allowed: {hotspot})");
        let task = tokio::spawn(upgrade::prepare_inbound(link, lan, hotspot, peer_5ghz));
        self.upgrade.begin_preparing(task);
    }

    async fn offer_lan_upgrade(
        &mut self,
        ip: std::net::Ipv4Addr,
        listener: TcpListener,
    ) -> Result<(), anyhow::Error> {
        let endpoint_id = self
            .upgrade
            .peer_endpoint_id()
            .ok_or_else(|| anyhow!("unknown peer endpoint id"))?
            .to_owned();
        let port = listener.local_addr()?.port();
        info!("upgrade: offering WIFI_LAN on {ip}:{port}");
        self.encrypt_and_send(&upgrade::lan_path_available_frame(
            ip,
            port,
            self.upgrade.disable_encryption(),
        ))
        .await?;
        self.upgrade
            .begin_waiting_lan(tokio::spawn(upgrade::accept_introduced(
                listener,
                endpoint_id,
                ip,
                self.upgrade.disable_encryption(),
            )));
        Ok(())
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
            "upgrade: offering a hotspot ({}:{})",
            creds.gateway, creds.port
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
            UpgradeEvent::Prepared(Ok(upgrade::Prepared::HostedLan(ip, listener))) => {
                if let Err(e) = self.offer_lan_upgrade(ip, listener).await {
                    warn!("upgrade: LAN offer failed ({e:#})");
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
                self.upgrade.close_lan_port().await;
                match acked {
                    Ok(()) => {
                        self.encrypt_and_send(&upgrade::event_frame(
                            EventType::LastWriteToPriorChannel,
                        ))
                        .await?;
                        self.upgrade.begin_draining(stream);
                    }
                    Err(e) => {
                        warn!("upgrade: ACK failed ({e:#})");
                        self.upgrade.fail().await;
                    }
                }
            }
            UpgradeEvent::Connected(Err(e)) => {
                warn!("upgrade: accept failed ({e})");
                self.upgrade.fail().await;
            }
            UpgradeEvent::WaitTimedOut => {
                warn!("upgrade: peer did not connect in time");
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
                // The phone may report the failed LAN offer after the hotspot was offered.
                let failed_lan = bwu.upgrade_path_info.as_ref().map(|i| i.medium())
                    == Some(
                        location_nearby_connections::bandwidth_upgrade_negotiation_frame::upgrade_path_info::Medium::WifiLan,
                    );
                if failed_lan && !self.upgrade.lan_offer_pending() {
                    debug!("upgrade: ignoring the failure of the LAN offer already given up");
                } else {
                    warn!("upgrade: peer reported a failure");
                    self.upgrade.fail().await;
                }
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
        // A prefix half read on the prior channel does not belong to the new one, and neither
        // does a write that failed there (the phone may drop Bluetooth at any point now).
        self.length_filled = 0;
        self.poisoned = false;
        self.plain_frames = self.upgrade.disable_encryption();
        if self.plain_frames {
            info!("upgrade: now on the new channel, plain frames");
        } else {
            info!("upgrade: now on the new channel");
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

        if self.poisoned {
            return Err(anyhow!(
                "an earlier write failed; the stream is not used again"
            ));
        }
        // A peer that never reads must not hold the session (and its files) for ever.
        let write = async {
            self.socket.write_all(&prefixed_length).await?;
            self.socket.flush().await
        };
        let written = tokio::time::timeout(self.limits.idle, write).await;
        match written {
            Ok(Ok(())) => Ok(()),
            // Part of the frame may be on the wire: a later frame would corrupt the stream.
            Ok(Err(e)) => {
                self.poisoned = true;
                Err(e.into())
            }
            Err(_) => {
                self.poisoned = true;
                Err(anyhow!("the peer did not read our frame in time"))
            }
        }
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

/// Whether the session is still before the transfer proper (handshake and offer).
fn is_handshaking(state: &TransferState) -> bool {
    !matches!(
        state,
        TransferState::WaitingForUserConsent
            | TransferState::ReceivingFiles
            | TransferState::Rejected
            | TransferState::Cancelled
            | TransferState::Finished
            | TransferState::Disconnected
    )
}

/// When a read must make progress by: `idle` after the last byte received, and never later
/// than `hard`. Other traffic (the bus) does not move it.
fn read_deadline(last_rx: Instant, idle: Duration, hard: Instant) -> Instant {
    (last_rx + idle).min(hard)
}

/// The last moment the peer may still be silent: while the user decides, the consent ceiling
/// (the core's own timer is shorter and normally comes first); never while an upgrade runs
/// (its steps have deadlines); otherwise `idle` after the last byte, and within `handshake`
/// of the start until the offer is in.
fn idle_deadline_for(
    state: &TransferState,
    upgrade_active: bool,
    last_rx: Instant,
    started: Instant,
    consent_since: Instant,
    l: &Limits,
) -> Option<Instant> {
    if *state == TransferState::WaitingForUserConsent {
        return Some(consent_since + l.consent);
    }
    if upgrade_active {
        return None;
    }
    let idle = last_rx + l.idle;
    Some(if is_handshaking(state) {
        idle.min(started + l.handshake)
    } else {
        idle
    })
}

/// The password of the `WifiCredentials` message carried by the follow-up BYTE payload of a
/// shared network. Decoding the protobuf accepts the fields in any order, an absent
/// `hidden_ssid`, and fields added by a newer sender. The error never quotes the payload.
fn wifi_password(buffer: &[u8]) -> anyhow::Result<String> {
    let credentials = sharing_nearby::WifiCredentials::decode(buffer)
        .context("the Wi-Fi credentials payload is not a WifiCredentials message")?;
    credentials
        .password
        .filter(|password| !password.is_empty())
        .context("the Wi-Fi credentials payload has no password")
}

/// Test-only capture of `log` output, per thread (each test runs on its own thread), so a
/// test can check what a code path writes to the log at any level.
#[cfg(test)]
pub mod capture_log {
    use std::sync::{Mutex, Once};
    use std::thread::ThreadId;

    static LINES: Mutex<Vec<(ThreadId, String)>> = Mutex::new(Vec::new());

    struct Capture;

    impl log::Log for Capture {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }

        fn log(&self, record: &log::Record) {
            LINES.lock().unwrap_or_else(|e| e.into_inner()).push((
                std::thread::current().id(),
                format!("{} {}", record.level(), record.args()),
            ));
        }

        fn flush(&self) {}
    }

    /// Installs the capturing logger at TRACE (once per process).
    pub fn install() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            log::set_logger(&Capture).expect("another logger is already installed");
            log::set_max_level(log::LevelFilter::Trace);
        });
    }

    /// A position in the captured lines; pass it to `since`.
    pub fn mark() -> usize {
        LINES.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// The lines logged by the calling thread after `mark`.
    pub fn since(mark: usize) -> Vec<String> {
        let me = std::thread::current().id();
        LINES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .skip(mark)
            .filter(|(t, _)| *t == me)
            .map(|(_, l)| l.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;
    use tokio::sync::broadcast;

    use super::*;
    use crate::upgrade::tests::FakeLink;

    /// The password payload a current Android sender produces: `WifiCredentials` with only the
    /// `password` field, so no `hidden_ssid` tag at the end.
    #[test]
    fn wifi_password_without_hidden_ssid() {
        let mut payload = vec![0x0A, 14];
        payload.extend_from_slice(b"hunter2-secret");
        assert_eq!(wifi_password(&payload).unwrap(), "hunter2-secret");
    }

    /// Older senders append `hidden_ssid`, and field order is not guaranteed.
    #[test]
    fn wifi_password_with_any_field_order() {
        let mut payload = vec![0x0A, 14];
        payload.extend_from_slice(b"hunter2-secret");
        payload.extend([0x10, 0x00]);
        assert_eq!(wifi_password(&payload).unwrap(), "hunter2-secret");

        let mut payload = vec![0x10, 0x01, 0x0A, 14];
        payload.extend_from_slice(b"hunter2-secret");
        assert_eq!(wifi_password(&payload).unwrap(), "hunter2-secret");

        // A field added by a newer sender is skipped.
        let mut payload = vec![0x18, 0x01, 0x0A, 14];
        payload.extend_from_slice(b"hunter2-secret");
        assert_eq!(wifi_password(&payload).unwrap(), "hunter2-secret");
    }

    #[test]
    fn wifi_password_rejects_a_missing_or_broken_payload_without_quoting_it() {
        for payload in [&[][..], &[0x10, 0x00][..]] {
            assert!(wifi_password(payload).is_err());
        }
        let broken = [0x0A, 0x7F, b'h', b'u', b'n', b't', b'e', b'r'];
        let shown = format!("{:#}", wifi_password(&broken).unwrap_err());
        assert!(!shown.contains("hunter"));
    }

    /// A file that is already there is never opened for writing nor replaced: the new file
    /// gets a number.
    #[test]
    fn publish_does_not_truncate_existing_file() {
        let tmp = TempDir::new("keep");
        let f = tmp.0.join("x.txt");
        std::fs::write(&f, b"keep").unwrap();
        let dest = DestDir::open(&tmp.0).unwrap();
        let staging = dest.staging().unwrap();
        let mut partial = staging.create("x.txt", 4).unwrap();
        partial.write_at(4, 0, b"new!").unwrap();
        assert_eq!(partial.finish().unwrap(), tmp.0.join("x (1).txt"));
        assert_eq!(std::fs::read(&f).unwrap(), b"keep");
        assert_eq!(std::fs::read(tmp.0.join("x (1).txt")).unwrap(), b"new!");
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
            .begin_preparing(tokio::spawn(upgrade::prepare_host(
                link.clone(),
                hosting,
                None,
            )));

        // An absurd frame length makes `handle` fail right away, which proves it ran.
        peer.write_all(&[0xff; 4]).await.unwrap();
        let handled = tokio::time::timeout(Duration::from_secs(1), req.handle())
            .await
            .expect("handle() was blocked by the pending hotspot");
        assert!(handled.is_err());
    }

    fn connection_request(mediums: &[i32], supports_5ghz: Option<bool>) -> OfflineFrame {
        OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::ConnectionRequest.into(),
                ),
                connection_request: Some(location_nearby_connections::ConnectionRequestFrame {
                    endpoint_id: Some("WXYZ".into()),
                    endpoint_info: Some(
                        RemoteDeviceInfo {
                            name: "Phone".into(),
                            device_type: DeviceType::Phone,
                        }
                        .serialize(),
                    ),
                    mediums: mediums.to_vec(),
                    medium_metadata: supports_5ghz.map(|v| {
                        location_nearby_connections::MediumMetadata {
                            supports_5_ghz: Some(v),
                            ..Default::default()
                        }
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        }
    }

    /// A session that received the phone's ConnectionRequest and may upgrade through `link`.
    async fn upgrade_session(
        dir: &Path,
        link: &Arc<FakeLink>,
        mediums: &[i32],
        supports_5ghz: Option<bool>,
    ) -> Session {
        let mut s = Session::new(dir);
        s.req.state.state = TransferState::Initial;
        s.req.upgrade = UpgradeState::new(Some(link.clone()));
        s.feed(&connection_request(mediums, supports_5ghz))
            .await
            .unwrap();
        s
    }

    /// Runs the next upgrade step of the session and returns the offer the phone receives.
    async fn next_offer(
        s: &mut Session,
    ) -> crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::UpgradePathInfo
    {
        let ev = s.req.upgrade.next_event().await;
        s.req.on_upgrade_event(ev).await.unwrap();
        let frame = upgrade::read_plain_frame(s.peer.as_mut().unwrap())
            .await
            .unwrap();
        upgrade::parse_bwu(&frame)
            .and_then(|b| b.upgrade_path_info.clone())
            .expect("not an upgrade offer")
    }

    fn medium(m: crate::location_nearby_connections::connection_request_frame::Medium) -> i32 {
        m as i32
    }

    /// The `supports_5_ghz` of the ConnectionRequest decides the band the hotspot is hosted on.
    #[tokio::test]
    async fn the_phones_5_ghz_support_reaches_the_hotspot_host() {
        use crate::location_nearby_connections::connection_request_frame::Medium as M;
        let tmp = TempDir::new("wiring-5ghz");
        for (reported, expect_2ghz) in [(Some(false), true), (Some(true), false), (None, false)] {
            let link = Arc::new(FakeLink::default());
            let mut s = upgrade_session(&tmp.0, &link, &[medium(M::WifiHotspot)], reported).await;
            s.req.start_upgrade().await;
            assert!(matches!(
                s.req.upgrade.next_event().await,
                UpgradeEvent::Prepared(Ok(upgrade::Prepared::Hosted(..)))
            ));
            assert_eq!(link.hosts.load(Ordering::SeqCst), 1);
            assert_eq!(link.hosted_2ghz.load(Ordering::SeqCst), expect_2ghz);
        }
    }

    /// A phone that lists WIFI_LAN gets the LAN offer, with the address the listener is bound to
    /// and the port the link was asked to open.
    #[tokio::test]
    async fn the_lan_offer_carries_the_offered_address_and_port() {
        use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::upgrade_path_info::Medium;
        use crate::location_nearby_connections::connection_request_frame::Medium as M;
        let tmp = TempDir::new("wiring-lan");
        let link = Arc::new(FakeLink {
            lan_address: Some(std::net::Ipv4Addr::LOCALHOST),
            ..Default::default()
        });
        let mut s = upgrade_session(
            &tmp.0,
            &link,
            &[medium(M::WifiLan), medium(M::WifiHotspot)],
            Some(true),
        )
        .await;
        s.req.start_upgrade().await;
        let offer = next_offer(&mut s).await;
        assert_eq!(offer.medium(), Medium::WifiLan);
        let socket = offer.wifi_lan_socket.unwrap();
        assert_eq!(socket.ip_address(), [127, 0, 0, 1]);
        let ports = link.lan_ports.lock().unwrap().clone();
        assert_eq!(ports.len(), 1);
        assert_eq!(socket.wifi_port(), i32::from(ports[0]));
        assert!(s.req.upgrade.is_waiting());
        assert_eq!(link.hosts.load(Ordering::SeqCst), 0);
    }

    /// A text or link follows the accept at once, so the LAN is prepared while the user is
    /// still reading the notice: an upgrade started after the accept would come too late.
    #[tokio::test]
    async fn a_text_offer_prepares_the_lan_before_consent() {
        use crate::location_nearby_connections::connection_request_frame::Medium as M;
        let tmp = TempDir::new("early-lan-text");
        let link = Arc::new(FakeLink {
            lan_address: Some(std::net::Ipv4Addr::LOCALHOST),
            ..Default::default()
        });
        let mut s = Session::new(&tmp.0);
        s.req.upgrade = UpgradeState::new(Some(link.clone()));
        s.req.upgrade.set_peer_mediums(&[M::WifiLan as i32]);
        s.req.upgrade.set_peer_endpoint_id("WXYZ");

        s.offer(vec![], vec![text_meta(INTRO_ID, 4)]).await.unwrap();
        assert_eq!(s.req.state.state, TransferState::WaitingForUserConsent);
        assert!(s.req.upgrade.in_progress(), "no LAN preparation started");
        assert!(matches!(
            s.req.upgrade.next_event().await,
            UpgradeEvent::Prepared(Ok(upgrade::Prepared::HostedLan(..)))
        ));
        assert_eq!(link.lan_hosts.load(Ordering::SeqCst), 1);
    }

    /// A text is too small to pay for a hotspot: when the early LAN offer fails, nothing else
    /// is offered, even though the phone allows WIFI_HOTSPOT.
    #[tokio::test]
    async fn a_failed_early_lan_offer_is_not_followed_by_a_hotspot() {
        use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::upgrade_path_info::Medium;
        use crate::location_nearby_connections::connection_request_frame::Medium as M;
        let tmp = TempDir::new("early-lan-text-failure");
        let link = Arc::new(FakeLink {
            lan_address: Some(std::net::Ipv4Addr::LOCALHOST),
            ..Default::default()
        });
        let mut s = Session::new(&tmp.0);
        s.req.upgrade = UpgradeState::new(Some(link.clone()));
        s.req
            .upgrade
            .set_peer_mediums(&[M::WifiLan as i32, M::WifiHotspot as i32]);
        s.req.upgrade.set_peer_endpoint_id("WXYZ");

        s.offer(vec![], vec![text_meta(INTRO_ID, 4)]).await.unwrap();
        assert_eq!(next_offer(&mut s).await.medium(), Medium::WifiLan);

        s.feed(&upgrade::upgrade_failure_frame(None)).await.unwrap();
        assert!(
            !s.req.upgrade.in_progress(),
            "a hotspot was offered for a text"
        );
        assert_eq!(link.hosts.load(Ordering::SeqCst), 0);
    }

    /// The phone answers the LAN offer with UPGRADE_FAILURE (not on the same network): the
    /// hotspot is offered next.
    #[tokio::test]
    async fn a_declined_lan_offer_is_followed_by_the_hotspot_offer() {
        use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::upgrade_path_info::Medium;
        use crate::location_nearby_connections::connection_request_frame::Medium as M;
        let tmp = TempDir::new("wiring-fallback");
        let link = Arc::new(FakeLink {
            lan_address: Some(std::net::Ipv4Addr::LOCALHOST),
            ..Default::default()
        });
        let mut s = upgrade_session(
            &tmp.0,
            &link,
            &[medium(M::WifiLan), medium(M::WifiHotspot)],
            Some(true),
        )
        .await;
        s.req.start_upgrade().await;
        assert_eq!(next_offer(&mut s).await.medium(), Medium::WifiLan);

        // The upgrade starts after the handshake: later frames are not UKey2 messages.
        s.req.state.state = TransferState::ReceivedPairedKeyResult;
        s.feed(&upgrade::upgrade_failure_frame(None)).await.unwrap();
        assert_eq!(next_offer(&mut s).await.medium(), Medium::WifiHotspot);
        assert_eq!(link.hosts.load(Ordering::SeqCst), 1);
        assert_eq!(link.lan_hosts.load(Ordering::SeqCst), 1);

        // A second refusal ends it: nothing more is offered.
        s.feed(&upgrade::upgrade_failure_frame(None)).await.unwrap();
        assert!(!s.req.upgrade.in_progress());
        assert_eq!(link.hosts.load(Ordering::SeqCst), 1);
    }

    /// The phone reports the failed LAN offer only after the hotspot was offered: that report is
    /// not a refusal of the hotspot.
    #[tokio::test]
    async fn a_late_failure_of_the_lan_offer_does_not_end_the_hotspot_offer() {
        use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::UpgradePathInfo;
        use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::upgrade_path_info::Medium;
        use crate::location_nearby_connections::connection_request_frame::Medium as M;
        let tmp = TempDir::new("wiring-late-failure");
        let link = Arc::new(FakeLink {
            lan_address: Some(std::net::Ipv4Addr::LOCALHOST),
            ..Default::default()
        });
        let mut s = upgrade_session(
            &tmp.0,
            &link,
            &[medium(M::WifiLan), medium(M::WifiHotspot)],
            Some(true),
        )
        .await;
        s.req.start_upgrade().await;
        assert_eq!(next_offer(&mut s).await.medium(), Medium::WifiLan);
        s.req.state.state = TransferState::ReceivedPairedKeyResult;
        s.feed(&upgrade::upgrade_failure_frame(None)).await.unwrap();
        assert_eq!(next_offer(&mut s).await.medium(), Medium::WifiHotspot);

        let lan = UpgradePathInfo {
            medium: Some(Medium::WifiLan.into()),
            ..Default::default()
        };
        s.feed(&upgrade::upgrade_failure_frame(Some(lan)))
            .await
            .unwrap();
        assert!(s.req.upgrade.is_waiting(), "the hotspot offer was dropped");
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

    // ---- Inbound file safety: repro tests (written before the fix; most fail on purpose) ----

    use tokio::io::DuplexStream;
    use tokio::sync::broadcast::error::TryRecvError;

    use crate::hdl::capture_log;

    /// A directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("rqs-p0a-{}-{tag}", std::process::id()));
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

    const INTRO_ID: i64 = 9_000_001;

    /// An inbound session over a duplex pipe, in plain-frame mode and already past the
    /// handshake, saving into its own directory. The test plays the phone.
    struct Session {
        req: InboundRequest,
        peer: Option<DuplexStream>,
        bus: broadcast::Sender<ChannelMessage>,
        rx: broadcast::Receiver<ChannelMessage>,
    }

    impl Session {
        fn new(dir: &Path) -> Self {
            let (peer, ours) = tokio::io::duplex(8 * 1024 * 1024);
            let (bus, rx) = broadcast::channel(4096);
            let mut req = InboundRequest::new_over(
                MigratableStream::Duplex(ours),
                "peer".into(),
                bus.clone(),
            );
            req.plain_frames = true;
            // No Wi-Fi link: another test may have set the global one.
            req.upgrade = UpgradeState::new(None);
            req.state.state = TransferState::ReceivedPairedKeyResult;
            req.download_dir = Some(dir.to_owned());
            Self {
                req,
                peer: Some(peer),
                bus,
                rx,
            }
        }

        /// The peer sends one frame and the session handles exactly that frame.
        async fn feed(&mut self, frame: &OfflineFrame) -> Result<(), anyhow::Error> {
            let data = frame.encode_to_vec();
            self.peer
                .as_mut()
                .expect("peer is closed")
                .write_all(&data)
                .await
                .unwrap();
            self.req._handle((data.len() as u32).to_be_bytes()).await
        }

        async fn offer(
            &mut self,
            files: Vec<sharing_nearby::FileMetadata>,
            texts: Vec<sharing_nearby::TextMetadata>,
        ) -> Result<(), anyhow::Error> {
            let body = introduction(files, texts);
            let frame = payload_frame(
                payload_header::PayloadType::Bytes,
                INTRO_ID,
                body.len() as i64,
                0,
                1,
                &body,
            );
            self.feed(&frame).await
        }

        async fn file_chunk(
            &mut self,
            id: i64,
            total: i64,
            offset: i64,
            flags: i32,
            body: &[u8],
        ) -> Result<(), anyhow::Error> {
            let frame = payload_frame(
                payload_header::PayloadType::File,
                id,
                total,
                offset,
                flags,
                body,
            );
            self.feed(&frame).await
        }

        /// The peer sends the empty last chunk of a file and closes once the session has shut
        /// down its side, so the end of the session does not wait for it.
        async fn last_chunk_and_close(
            &mut self,
            id: i64,
            total: i64,
            offset: i64,
        ) -> Result<(), anyhow::Error> {
            let data = payload_frame(payload_header::PayloadType::File, id, total, offset, 1, b"")
                .encode_to_vec();
            let mut peer = self.peer.take().expect("peer is closed");
            peer.write_all(&data).await.unwrap();
            let closer = async move {
                let mut sink = Vec::new();
                let _ = peer.read_to_end(&mut sink).await;
            };
            let (result, ()) =
                tokio::join!(self.req._handle((data.len() as u32).to_be_bytes()), closer);
            result
        }

        /// What the core would send on the bus; handles every message queued for the session.
        async fn deliver(&mut self, action: TransferAction) -> Result<(), anyhow::Error> {
            let _ = self.bus.send(ChannelMessage {
                id: "peer".into(),
                msg: channel::Message::Lib { action },
            });
            while !self.req.receiver.is_empty() {
                self.req.handle().await?;
            }
            Ok(())
        }

        /// A user cancel; the peer closes when the session shuts down its side.
        async fn cancel(&mut self) -> Result<(), anyhow::Error> {
            let mut peer = self.peer.take().expect("peer is closed");
            let closer = async move {
                let mut sink = Vec::new();
                let _ = peer.read_to_end(&mut sink).await;
            };
            let (result, ()) = tokio::join!(self.deliver(TransferAction::TransferCancel), closer);
            result
        }

        /// The peer vanishes; returns the error that ends the session.
        async fn disconnect(&mut self) -> anyhow::Error {
            drop(self.peer.take());
            loop {
                if let Err(e) = self.req.handle().await {
                    return e;
                }
            }
        }

        fn events(&mut self) -> Vec<MessageClient> {
            let mut out = Vec::new();
            loop {
                match self.rx.try_recv() {
                    Ok(m) => {
                        if let channel::Message::Client(c) = m.msg {
                            out.push(c);
                        }
                    }
                    Err(TryRecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
            out
        }

        fn finished(&mut self) -> bool {
            self.events()
                .iter()
                .any(|e| e.state == Some(TransferState::Finished))
        }
    }

    fn payload_frame(
        kind: payload_header::PayloadType,
        id: i64,
        total: i64,
        offset: i64,
        flags: i32,
        body: &[u8],
    ) -> OfflineFrame {
        OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::PayloadTransfer.into(),
                ),
                payload_transfer: Some(PayloadTransferFrame {
                    packet_type: Some(PacketType::Data.into()),
                    payload_header: Some(PayloadHeader {
                        id: Some(id),
                        r#type: Some(kind.into()),
                        total_size: Some(total),
                        is_sensitive: Some(false),
                        ..Default::default()
                    }),
                    payload_chunk: Some(PayloadChunk {
                        offset: Some(offset),
                        flags: Some(flags),
                        body: Some(body.to_vec()),
                        index: None,
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        }
    }

    fn introduction(
        files: Vec<sharing_nearby::FileMetadata>,
        texts: Vec<sharing_nearby::TextMetadata>,
    ) -> Vec<u8> {
        sharing_nearby::Frame {
            version: Some(sharing_nearby::frame::Version::V1.into()),
            v1: Some(sharing_nearby::V1Frame {
                r#type: Some(sharing_nearby::v1_frame::FrameType::Introduction.into()),
                introduction: Some(sharing_nearby::IntroductionFrame {
                    file_metadata: files,
                    text_metadata: texts,
                    ..Default::default()
                }),
                ..Default::default()
            }),
        }
        .encode_to_vec()
    }

    fn file_meta(name: &str, id: i64, size: i64) -> sharing_nearby::FileMetadata {
        sharing_nearby::FileMetadata {
            name: Some(name.into()),
            payload_id: Some(id),
            size: Some(size),
            mime_type: Some("text/plain".into()),
            ..Default::default()
        }
    }

    fn text_meta(id: i64, size: i64) -> sharing_nearby::TextMetadata {
        sharing_nearby::TextMetadata {
            text_title: Some("note".into()),
            r#type: Some(text_metadata::Type::Text.into()),
            payload_id: Some(id),
            size: Some(size),
            ..Default::default()
        }
    }

    fn dir_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// The hidden folders receiving sessions keep their files in, inside `dir`.
    fn staging_dirs(dir: &Path) -> Vec<PathBuf> {
        dir_names(dir)
            .into_iter()
            .filter(|name| name.starts_with(".kuickshare-incoming-"))
            .map(|name| dir.join(name))
            .collect()
    }

    /// The one hidden folder of the session receiving into `dir`.
    fn staging_dir(dir: &Path) -> PathBuf {
        let mut all = staging_dirs(dir);
        assert_eq!(all.len(), 1, "{all:?}");
        all.remove(0)
    }

    /// A stopping session closes (frames, FIN, the peer's close) within the wind-down that
    /// `RQS::stop` waits for.
    #[test]
    fn a_stopping_session_closes_within_the_wind_down() {
        let close = Limits::default().stop_close + FIN_TIMEOUT;
        let wind_down = crate::manager::STOP_WIND_DOWN;
        assert!(close < wind_down, "{close:?} vs {wind_down:?}");
    }

    /// The service stops: the session ends like a user cancel (the peer is told and closes),
    /// the app gets `Cancelled`, and what was received so far is removed.
    async fn stop_session(s: &mut Session) -> (anyhow::Error, Vec<MessageClient>) {
        let stop = CancellationToken::new();
        s.req.stop = stop.clone();
        // Generous on purpose: a loaded test machine must not turn the bounded close into a
        // missing Cancel (the production bound is checked against STOP_WIND_DOWN in manager.rs).
        s.req.limits.stop_close = Duration::from_secs(5);
        let mut peer = s.peer.take().expect("peer is closed");
        let closer = async move {
            let mut sink = Vec::new();
            let _ = peer.read_to_end(&mut sink).await;
            sink
        };
        stop.cancel();
        let (ended, sink) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(s.req.handle(), closer)
        })
        .await
        .expect("the stop did not end the session");
        assert!(!sink.is_empty(), "the peer was not told");
        (ended.unwrap_err(), s.events())
    }

    /// A stop wins against what is ready at the same time (the session's own events queued on
    /// the bus): `handle` used to return after the bus message about half of the time.
    #[tokio::test]
    async fn stop_wins_against_queued_bus_messages() {
        for round in 0..50 {
            let tmp = TempDir::new("stop-queued");
            let mut s = Session::new(&tmp.0);
            s.offer(vec![file_meta("a.txt", 1, 10)], vec![])
                .await
                .unwrap();
            // Nobody reads the other end: the Cancel goes nowhere and the close is short.
            drop(s.peer.take());
            s.req.limits.stop_close = Duration::from_millis(200);
            let stop = CancellationToken::new();
            s.req.stop = stop.clone();
            stop.cancel();
            let err = tokio::time::timeout(Duration::from_secs(10), s.req.handle())
                .await
                .expect("the stop did not end the session")
                .expect_err(&format!("round {round}: handle returned without the stop"));
            assert!(is_end_of_session(&Err(err)));
            assert_eq!(
                s.events().last().unwrap().state,
                Some(TransferState::Cancelled)
            );
        }
    }

    #[tokio::test]
    async fn stop_ends_a_session_waiting_for_consent() {
        let tmp = TempDir::new("stop-consent");
        let mut s = Session::new(&tmp.0);
        s.offer(vec![file_meta("a.txt", 1, 10)], vec![])
            .await
            .unwrap();
        let (err, events) = stop_session(&mut s).await;
        assert!(is_end_of_session(&Err(err)));
        assert_eq!(events.last().unwrap().state, Some(TransferState::Cancelled));
        assert!(dir_names(&tmp.0).is_empty());
    }

    #[tokio::test]
    async fn stop_ends_a_receiving_session_and_removes_the_partial_file() {
        let tmp = TempDir::new("stop-receiving");
        let mut s = Session::new(&tmp.0);
        s.offer(vec![file_meta("a.txt", 1, 10)], vec![])
            .await
            .unwrap();
        s.deliver(TransferAction::ConsentAccept).await.unwrap();
        s.file_chunk(1, 10, 0, 0, b"12345").await.unwrap();
        assert!(staging_dir(&tmp.0).join("a.txt").exists(), "precondition");
        let (err, events) = stop_session(&mut s).await;
        assert!(is_end_of_session(&Err(err)));
        assert_eq!(events.last().unwrap().state, Some(TransferState::Cancelled));
        assert!(dir_names(&tmp.0).is_empty(), "the partial file is left");
    }

    /// Before the handshake the app does not know the session: it ends silently.
    #[tokio::test]
    async fn stop_ends_a_session_before_the_handshake_without_an_event() {
        let (_peer, ours) = tokio::io::duplex(1024);
        let (bus, mut rx) = broadcast::channel(16);
        let stop = CancellationToken::new();
        let mut req = InboundRequest::new_over(MigratableStream::Duplex(ours), "id".into(), bus)
            .with_stop(stop.clone());
        stop.cancel();
        let err = tokio::time::timeout(Duration::from_secs(10), req.handle())
            .await
            .expect("the stop did not end the session")
            .unwrap_err();
        assert!(is_end_of_session(&Err(err)));
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    /// D1: a user cancel while receiving must not leave the half file behind.
    #[tokio::test]
    async fn cancel_removes_partial_file() {
        let tmp = TempDir::new("cancel");
        let mut s = Session::new(&tmp.0);
        s.offer(vec![file_meta("a.txt", 1, 10)], vec![])
            .await
            .unwrap();
        s.deliver(TransferAction::ConsentAccept).await.unwrap();
        s.file_chunk(1, 10, 0, 0, b"12345").await.unwrap();
        let path = staging_dir(&tmp.0).join("a.txt");
        assert_eq!(std::fs::read(&path).unwrap(), b"12345", "precondition");

        assert!(s.cancel().await.is_err(), "a cancel ends the session");
        assert!(
            !path.exists(),
            "the partial file is still there after a cancel"
        );
        assert!(
            dir_names(&tmp.0).is_empty(),
            "the staging folder is still there after a cancel"
        );
    }

    /// D1: the peer vanishing mid-file must not leave the half file behind.
    #[tokio::test]
    async fn disconnect_removes_partial_file() {
        let tmp = TempDir::new("disconnect");
        let mut s = Session::new(&tmp.0);
        s.offer(vec![file_meta("a.txt", 1, 10)], vec![])
            .await
            .unwrap();
        s.deliver(TransferAction::ConsentAccept).await.unwrap();
        s.file_chunk(1, 10, 0, 0, b"12345").await.unwrap();
        let path = staging_dir(&tmp.0).join("a.txt");
        assert!(path.exists(), "precondition");

        let _ = s.disconnect().await;
        assert!(
            !path.exists(),
            "the partial file is still there after an EOF"
        );
        assert!(
            dir_names(&tmp.0).is_empty(),
            "the staging folder is still there after an EOF"
        );
    }

    /// D2: 10 bytes of a declared 1000 plus the last-chunk flag is not a finished transfer.
    #[tokio::test]
    async fn short_file_is_not_finished() {
        let tmp = TempDir::new("short");
        let mut s = Session::new(&tmp.0);
        s.offer(vec![file_meta("a.txt", 1, 1000)], vec![])
            .await
            .unwrap();
        s.deliver(TransferAction::ConsentAccept).await.unwrap();
        s.file_chunk(1, 1000, 0, 0, b"0123456789").await.unwrap();
        let _ = s.file_chunk(1, 1000, 10, 1, b"").await;
        assert!(
            !s.finished(),
            "a 10 byte file declared as 1000 reached Finished"
        );
    }

    /// A chunk past the declared size is refused (it is today) and the partial file goes too.
    #[tokio::test]
    async fn chunk_beyond_declared_size_is_refused_and_removed() {
        let tmp = TempDir::new("beyond");
        let mut s = Session::new(&tmp.0);
        s.offer(vec![file_meta("a.txt", 1, 4)], vec![])
            .await
            .unwrap();
        s.deliver(TransferAction::ConsentAccept).await.unwrap();
        s.file_chunk(1, 4, 0, 0, b"ab").await.unwrap();
        let refused = s.file_chunk(1, 4, 2, 0, b"cdefgh").await;
        assert!(refused.is_err(), "a chunk past the declared size was taken");
        assert!(
            dir_names(&tmp.0).is_empty(),
            "the partial file is still there after the refusal"
        );
    }

    /// A chunk whose header declares another total than the offer did is refused like any
    /// bad chunk: the session ends and what was received goes.
    #[tokio::test]
    async fn file_chunk_with_a_changed_total_size_is_refused_and_removed() {
        let tmp = TempDir::new("total-changed");
        let mut s = Session::new(&tmp.0);
        s.offer(vec![file_meta("a.txt", 1, 4)], vec![])
            .await
            .unwrap();
        s.deliver(TransferAction::ConsentAccept).await.unwrap();
        s.file_chunk(1, 4, 0, 0, b"ab").await.unwrap();
        let err = s
            .file_chunk(1, 8, 2, 0, b"cd")
            .await
            .expect_err("a chunk with another total was taken");
        assert!(
            matches!(
                err.downcast_ref::<safety::ChunkError>(),
                Some(safety::ChunkError::SizeChanged {
                    got: 8,
                    declared: 4
                })
            ),
            "{err:#}"
        );
        assert!(
            dir_names(&tmp.0).is_empty(),
            "the partial file is still there after the refusal"
        );
    }

    /// The id of an offered file is not for a `Bytes` payload, which would be decoded as a
    /// sharing frame.
    #[tokio::test]
    async fn bytes_payload_with_an_offered_file_id_is_refused() {
        let tmp = TempDir::new("bytes-file-id");
        let mut s = Session::new(&tmp.0);
        s.offer(vec![file_meta("a.txt", 1, 4)], vec![])
            .await
            .unwrap();
        s.deliver(TransferAction::ConsentAccept).await.unwrap();
        let frame = payload_frame(payload_header::PayloadType::Bytes, 1, 2, 0, 0, b"ab");
        assert!(s.feed(&frame).await.is_err());
        assert!(
            s.req.state.payload_buffers.is_empty(),
            "the bytes were buffered"
        );
    }

    /// D5: file data before the user accepted is an error, not a panic.
    #[tokio::test]
    async fn file_chunk_before_consent_is_an_error() {
        let tmp = TempDir::new("noconsent");
        let mut s = Session::new(&tmp.0);
        s.offer(vec![file_meta("a.txt", 1, 10)], vec![])
            .await
            .unwrap();
        let r = s.file_chunk(1, 10, 0, 0, b"x").await;
        assert!(r.is_err(), "file data before consent was taken");
        assert!(
            dir_names(&tmp.0).is_empty(),
            "a file appeared before consent"
        );
    }

    /// D4: a text whose bytes arrive before the user accepted must not be reported as received.
    #[tokio::test]
    async fn text_before_consent_is_not_finished() {
        let tmp = TempDir::new("text");
        let mut s = Session::new(&tmp.0);
        s.offer(vec![], vec![text_meta(77, 5)]).await.unwrap();
        assert_eq!(s.req.state.state, TransferState::WaitingForUserConsent);
        let frame = payload_frame(payload_header::PayloadType::Bytes, 77, 5, 0, 1, b"hello");
        let _ = s.feed(&frame).await;
        assert!(!s.finished(), "a text reached Finished without consent");
    }

    /// D6: a negative `total_size` is refused, it must not panic.
    #[tokio::test]
    async fn negative_bytes_total_size_is_refused() {
        let tmp = TempDir::new("negative");
        let mut s = Session::new(&tmp.0);
        let frame = payload_frame(payload_header::PayloadType::Bytes, 5, -1, 0, 0, b"x");
        assert!(s.feed(&frame).await.is_err());
    }

    /// D7: chunks past the declared `total_size` must not keep growing the buffer.
    #[tokio::test]
    async fn bytes_payload_cannot_outgrow_total_size() {
        let tmp = TempDir::new("outgrow");
        let mut s = Session::new(&tmp.0);
        let first = payload_frame(payload_header::PayloadType::Bytes, 5, 10, 0, 0, &[1; 8]);
        s.feed(&first).await.unwrap();
        let second = payload_frame(payload_header::PayloadType::Bytes, 5, 10, 8, 0, &[2; 8]);
        let refused = s.feed(&second).await;
        let held = s
            .req
            .state
            .payload_buffers
            .get(&5)
            .map_or(0, |open| open.buffered());
        assert!(refused.is_err(), "16 bytes were taken for a payload of 10");
        assert!(
            held <= 10,
            "the buffer holds {held} bytes for a payload of 10"
        );
    }

    /// D10: a second ConsentAccept while receiving must not re-create or rename the files.
    #[tokio::test]
    async fn second_accept_is_ignored() {
        let tmp = TempDir::new("twice");
        let mut s = Session::new(&tmp.0);
        s.offer(
            vec![file_meta("a.txt", 1, 4), file_meta("b.txt", 2, 4)],
            vec![],
        )
        .await
        .unwrap();
        s.deliver(TransferAction::ConsentAccept).await.unwrap();
        s.file_chunk(1, 4, 0, 0, b"AA").await.unwrap();

        let _ = s.deliver(TransferAction::ConsentAccept).await;
        let _ = s.file_chunk(1, 4, 2, 0, b"AA").await;

        let staging = staging_dir(&tmp.0);
        assert_eq!(
            dir_names(&staging),
            ["a.txt", "b.txt"],
            "a second accept renamed files"
        );
        assert_eq!(std::fs::read(staging.join("a.txt")).unwrap(), b"AAAA");
    }

    /// The PAYLOAD_ACK the session sends for one completed FILE payload (crQuickShare
    /// `AckFile`): the payload's id, `total_size = -1`, no chunk.
    fn assert_payload_ack(frame: &OfflineFrame, id: i64) {
        let v1 = frame.v1.as_ref().expect("no v1 frame");
        assert_eq!(
            v1.r#type(),
            location_nearby_connections::v1_frame::FrameType::PayloadTransfer
        );
        let transfer = v1.payload_transfer.as_ref().expect("no payload transfer");
        assert_eq!(
            transfer.packet_type(),
            crate::location_nearby_connections::payload_transfer_frame::PacketType::PayloadAck
        );
        let header = transfer.payload_header.as_ref().expect("no payload header");
        assert_eq!(header.id(), id);
        assert_eq!(header.total_size(), -1);
        assert!(transfer.payload_chunk.is_none(), "an ack carries no chunk");
    }

    /// `Session::feed` without going through `Session.peer`: the test keeps the peer out to
    /// read what the session sends. Only the body goes on the stream, `_handle` takes the
    /// length prefix as its argument.
    async fn write_and_handle(
        peer: &mut DuplexStream,
        req: &mut InboundRequest,
        frame: &OfflineFrame,
    ) -> Result<(), anyhow::Error> {
        let data = frame.encode_to_vec();
        peer.write_all(&data).await.unwrap();
        req._handle((data.len() as u32).to_be_bytes()).await
    }

    /// A peer at safe-to-disconnect level 6: one ack per completed file, in order, and the
    /// polite close only after the last one. The files are received as before.
    #[tokio::test]
    async fn a_v6_peer_is_acked_per_completed_file() {
        let tmp = TempDir::new("ack");
        let mut s = Session::new(&tmp.0);
        s.req.state.peer_disconnect_version = 6;
        s.offer(
            vec![file_meta("a.txt", 1, 4), file_meta("b.txt", 2, 4)],
            vec![],
        )
        .await
        .unwrap();
        s.deliver(TransferAction::ConsentAccept).await.unwrap();

        let mut peer = s.peer.take().expect("peer is closed");
        // The reply to the accept is a status, not an ack.
        let reply = upgrade::read_plain_frame(&mut peer)
            .await
            .expect("the accept reply");
        let body = reply
            .v1
            .unwrap()
            .payload_transfer
            .unwrap()
            .payload_chunk
            .unwrap()
            .body
            .unwrap();
        let inner = sharing_nearby::Frame::decode(&*body).unwrap();
        assert_eq!(
            inner.v1.unwrap().connection_response.unwrap().status(),
            Status::Accept
        );
        // The reply's empty last-chunk marker follows, as with every Bytes payload.
        let _marker = upgrade::read_plain_frame(&mut peer)
            .await
            .expect("the marker of the accept reply");

        // a.txt: the bytes, then the empty last-chunk marker; its ack comes out at once.
        write_and_handle(
            &mut peer,
            &mut s.req,
            &payload_frame(payload_header::PayloadType::File, 1, 4, 0, 0, b"AA"),
        )
        .await
        .unwrap();
        write_and_handle(
            &mut peer,
            &mut s.req,
            &payload_frame(payload_header::PayloadType::File, 1, 4, 2, 0, b"AA"),
        )
        .await
        .unwrap();
        assert!(
            !tmp.0.join("a.txt").exists(),
            "an incomplete file is published"
        );
        write_and_handle(
            &mut peer,
            &mut s.req,
            &payload_frame(payload_header::PayloadType::File, 1, 4, 4, 1, b""),
        )
        .await
        .unwrap();
        let ack = upgrade::read_plain_frame(&mut peer)
            .await
            .expect("the ack of a.txt");
        assert_payload_ack(&ack, 1);
        // The ack is sent after the file was published, and b.txt is still staged.
        assert_eq!(std::fs::read(tmp.0.join("a.txt")).unwrap(), b"AAAA");
        assert!(!tmp.0.join("b.txt").exists());

        // b.txt completes the batch: its ack is followed by the polite close. The peer goes
        // away once it has read both, so the session's wind-down does not wait for it.
        write_and_handle(
            &mut peer,
            &mut s.req,
            &payload_frame(payload_header::PayloadType::File, 2, 4, 0, 0, b"BB"),
        )
        .await
        .unwrap();
        write_and_handle(
            &mut peer,
            &mut s.req,
            &payload_frame(payload_header::PayloadType::File, 2, 4, 2, 0, b"BB"),
        )
        .await
        .unwrap();
        let last = payload_frame(payload_header::PayloadType::File, 2, 4, 4, 1, b"");
        let data = last.encode_to_vec();
        peer.write_all(&data).await.unwrap();
        let reader = async move {
            let ack = upgrade::read_plain_frame(&mut peer)
                .await
                .expect("the ack of b.txt");
            assert_payload_ack(&ack, 2);
            let close = upgrade::read_plain_frame(&mut peer)
                .await
                .expect("the close after the last ack");
            assert_eq!(
                close.v1.unwrap().r#type(),
                location_nearby_connections::v1_frame::FrameType::Disconnection
            );
            // Dropping the peer at the end of the block lets the wind-down see EOF at once.
        };
        let (feed, ()) = tokio::join!(s.req._handle((data.len() as u32).to_be_bytes()), reader);
        assert!(is_end_of_session(&feed), "{feed:?}");
        assert!(s.finished(), "both files were received");
        assert_eq!(std::fs::read(tmp.0.join("a.txt")).unwrap(), b"AAAA");
        assert_eq!(std::fs::read(tmp.0.join("b.txt")).unwrap(), b"BBBB");
    }

    /// The Pixel 7 can send its own Response before the Introduction (two payloads written at
    /// once): the session waits for the offer instead of ending.
    #[tokio::test]
    async fn a_response_before_the_introduction_is_ignored() {
        let tmp = TempDir::new("early-response");
        let mut s = Session::new(&tmp.0);
        let response = sharing_nearby::Frame {
            version: Some(sharing_nearby::frame::Version::V1.into()),
            v1: Some(sharing_nearby::V1Frame {
                r#type: Some(sharing_nearby::v1_frame::FrameType::Response.into()),
                connection_response: Some(sharing_nearby::ConnectionResponseFrame {
                    status: Some(Status::Accept.into()),
                }),
                ..Default::default()
            }),
        }
        .encode_to_vec();
        let frame = payload_frame(
            payload_header::PayloadType::Bytes,
            INTRO_ID + 1,
            response.len() as i64,
            0,
            1,
            &response,
        );
        s.feed(&frame)
            .await
            .expect("an early Response ended the session");
        s.offer(vec![file_meta("a.txt", 1, 4)], vec![])
            .await
            .expect("the offer after it");
        assert_eq!(s.req.state.state, TransferState::WaitingForUserConsent);
    }

    /// The sender cancels (a Pixel 7 at level 4): our close asks for the handshake and the
    /// phone's own request gets our ack before the FIN, so the phone does not take the end for
    /// a lost link (it used to try to reconnect for about 5 s before showing the cancel).
    #[tokio::test]
    async fn a_peer_cancel_ends_with_the_safe_disconnect_handshake() {
        let tmp = TempDir::new("peer-cancel-safe");
        let mut s = Session::new(&tmp.0);
        s.req.state.peer_disconnect_version = 4;
        s.offer(vec![file_meta("a.txt", 1, 4)], vec![])
            .await
            .unwrap();
        s.deliver(TransferAction::ConsentAccept).await.unwrap();
        let mut peer = s.peer.take().expect("peer is closed");
        let _reply = upgrade::read_plain_frame(&mut peer).await.unwrap();
        let _marker = upgrade::read_plain_frame(&mut peer).await.unwrap();

        let cancel = sharing_nearby::Frame {
            version: Some(sharing_nearby::frame::Version::V1.into()),
            v1: Some(sharing_nearby::V1Frame {
                r#type: Some(sharing_nearby::v1_frame::FrameType::Cancel.into()),
                ..Default::default()
            }),
        }
        .encode_to_vec();
        let frame = payload_frame(
            payload_header::PayloadType::Bytes,
            INTRO_ID + 2,
            cancel.len() as i64,
            0,
            1,
            &cancel,
        );
        let data = frame.encode_to_vec();
        peer.write_all(&data).await.unwrap();
        let reader = async move {
            let ours = upgrade::read_plain_frame(&mut peer)
                .await
                .expect("our close");
            let ours = ours.v1.unwrap().disconnection.unwrap();
            assert_eq!(ours.request_safe_to_disconnect, Some(true));
            // The phone asks too: the requests cross.
            let request = disconnection_frame(true).encode_to_vec();
            peer.write_all(&(request.len() as u32).to_be_bytes())
                .await
                .unwrap();
            peer.write_all(&request).await.unwrap();
            let ack = upgrade::read_plain_frame(&mut peer).await.expect("our ack");
            let ack = ack.v1.unwrap().disconnection.unwrap();
            assert_eq!(
                (ack.request_safe_to_disconnect, ack.ack_safe_to_disconnect),
                (Some(true), Some(true))
            );
            assert!(
                upgrade::read_plain_frame(&mut peer).await.is_err(),
                "the FIN comes after the ack"
            );
        };
        let (fed, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(s.req._handle((data.len() as u32).to_be_bytes()), reader)
        })
        .await
        .expect("the handshake never ended");
        assert!(is_end_of_session(&fed), "{fed:?}");
        assert_eq!(s.req.state.state, TransferState::Cancelled);
    }

    /// A safe-to-disconnect request from the sender gets the ack with both flags; the session
    /// goes on until the peer closes, as before.
    #[tokio::test]
    async fn a_safe_to_disconnect_request_is_acknowledged() {
        let tmp = TempDir::new("safe-disc");
        let mut s = Session::new(&tmp.0);
        let request = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::Disconnection.into(),
                ),
                disconnection: Some(location_nearby_connections::DisconnectionFrame {
                    request_safe_to_disconnect: Some(true),
                    ack_safe_to_disconnect: Some(false),
                }),
                ..Default::default()
            }),
        };
        s.feed(&request).await.unwrap();
        let reply = upgrade::read_plain_frame(s.peer.as_mut().expect("peer is closed"))
            .await
            .expect("the ack of the request");
        let disconnection = reply.v1.unwrap().disconnection.unwrap();
        assert_eq!(disconnection.request_safe_to_disconnect, Some(true));
        assert_eq!(disconnection.ack_safe_to_disconnect, Some(true));
    }

    /// A payload ack from the sender (it read our level 6) has no chunk: it is ignored, not
    /// an error that ends the session.
    #[tokio::test]
    async fn a_payload_ack_from_the_sender_is_ignored() {
        let tmp = TempDir::new("ack-in");
        let mut s = Session::new(&tmp.0);
        let ack = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::PayloadTransfer.into(),
                ),
                payload_transfer: Some(PayloadTransferFrame {
                    packet_type: Some(PacketType::PayloadAck.into()),
                    payload_header: Some(PayloadHeader {
                        id: Some(7),
                        total_size: Some(-1),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };
        s.feed(&ack).await.expect("an ack ended the session");
    }

    /// A peer below level 6 is not acked: the close follows the last chunk as before.
    #[tokio::test]
    async fn an_old_peer_is_not_acked() {
        let tmp = TempDir::new("noack");
        let mut s = Session::new(&tmp.0);
        s.offer(vec![file_meta("a.txt", 1, 4)], vec![])
            .await
            .unwrap();
        s.deliver(TransferAction::ConsentAccept).await.unwrap();

        let mut peer = s.peer.take().expect("peer is closed");
        // The reply to the accept: the status and its empty last-chunk marker.
        let _reply = upgrade::read_plain_frame(&mut peer)
            .await
            .expect("the accept reply");
        let _marker = upgrade::read_plain_frame(&mut peer)
            .await
            .expect("the marker of the accept reply");

        write_and_handle(
            &mut peer,
            &mut s.req,
            &payload_frame(payload_header::PayloadType::File, 1, 4, 0, 0, b"AAAA"),
        )
        .await
        .unwrap();
        let marker = payload_frame(payload_header::PayloadType::File, 1, 4, 4, 1, b"");
        let data = marker.encode_to_vec();
        peer.write_all(&data).await.unwrap();
        let reader = async move {
            let close = upgrade::read_plain_frame(&mut peer)
                .await
                .expect("the close");
            assert_eq!(
                close.v1.unwrap().r#type(),
                location_nearby_connections::v1_frame::FrameType::Disconnection,
                "no ack is sent to a peer below level 6"
            );
        };
        let (feed, ()) = tokio::join!(s.req._handle((data.len() as u32).to_be_bytes()), reader);
        assert!(is_end_of_session(&feed), "{feed:?}");
        assert!(s.finished());
        assert_eq!(std::fs::read(tmp.0.join("a.txt")).unwrap(), b"AAAA");
    }

    /// A finished receive from a sender at level 1 or more (a Pixel 7 is at 4): our close asks
    /// for the handshake and the sender's ack ends it at once, well inside the 2 s wait.
    #[tokio::test]
    async fn a_finished_receive_ends_with_the_safe_disconnect_handshake() {
        let tmp = TempDir::new("finish-safe");
        let mut s = Session::new(&tmp.0);
        s.req.state.peer_disconnect_version = 4;
        s.offer(vec![file_meta("a.txt", 1, 4)], vec![])
            .await
            .unwrap();
        s.deliver(TransferAction::ConsentAccept).await.unwrap();
        let mut peer = s.peer.take().expect("peer is closed");
        let _reply = upgrade::read_plain_frame(&mut peer).await.unwrap();
        let _marker = upgrade::read_plain_frame(&mut peer).await.unwrap();
        write_and_handle(
            &mut peer,
            &mut s.req,
            &payload_frame(payload_header::PayloadType::File, 1, 4, 0, 0, b"AAAA"),
        )
        .await
        .unwrap();
        let last = payload_frame(payload_header::PayloadType::File, 1, 4, 4, 1, b"");
        let data = last.encode_to_vec();
        peer.write_all(&data).await.unwrap();
        let reader = async move {
            let close = upgrade::read_plain_frame(&mut peer)
                .await
                .expect("our close");
            let close = close.v1.unwrap().disconnection.unwrap();
            assert_eq!(close.request_safe_to_disconnect, Some(true));
            let ack = safe_disconnect_ack_frame().encode_to_vec();
            peer.write_all(&(ack.len() as u32).to_be_bytes())
                .await
                .unwrap();
            peer.write_all(&ack).await.unwrap();
            assert!(
                upgrade::read_plain_frame(&mut peer).await.is_err(),
                "the FIN comes after the ack"
            );
        };
        let started = std::time::Instant::now();
        let (fed, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(s.req._handle((data.len() as u32).to_be_bytes()), reader)
        })
        .await
        .expect("the handshake never ended");
        assert!(is_end_of_session(&fed), "{fed:?}");
        assert!(s.finished());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the session waited out the handshake bound instead of ending at the ack"
        );
    }

    /// A decline sends no request, so nothing is awaited: the FIN follows the refusal at once,
    /// as before the handshake.
    #[tokio::test]
    async fn a_decline_does_not_wait_for_a_handshake() {
        let tmp = TempDir::new("decline-fast");
        let mut s = Session::new(&tmp.0);
        s.req.state.peer_disconnect_version = 4;
        s.offer(vec![file_meta("a.txt", 1, 4)], vec![])
            .await
            .unwrap();
        let mut peer = s.peer.take().expect("peer is closed");
        let reader = async move {
            assert_eq!(read_reject_status(&mut peer).await, Status::Reject);
            assert!(
                upgrade::read_plain_frame(&mut peer).await.is_err(),
                "the FIN follows the refusal"
            );
        };
        let started = std::time::Instant::now();
        let (declined, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(s.deliver(TransferAction::ConsentDecline), reader)
        })
        .await
        .expect("the decline never ended");
        assert!(is_end_of_session(&declined), "{declined:?}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the decline waited"
        );
    }

    /// D11: when the name is taken between the offer and the accept, the paths reported to the
    /// app are the ones really written.
    #[tokio::test]
    async fn collision_after_offer_updates_saved_paths() {
        let tmp = TempDir::new("collision");
        let mut s = Session::new(&tmp.0);
        s.offer(vec![file_meta("a.txt", 1, 4)], vec![])
            .await
            .unwrap();
        std::fs::write(tmp.0.join("a.txt"), b"other").unwrap();
        s.deliver(TransferAction::ConsentAccept).await.unwrap();

        assert_eq!(std::fs::read(tmp.0.join("a.txt")).unwrap(), b"other");
        let saved = s
            .events()
            .into_iter()
            .rev()
            .find_map(|e| e.metadata)
            .expect("no metadata was reported")
            .saved_paths;
        let expected = tmp.0.join("a (1).txt").to_string_lossy().into_owned();
        assert_eq!(saved, [expected]);
    }

    /// A file has its final name only when it is complete; until then it is in the hidden
    /// folder, and the path the app is told about is the real one from then on.
    #[tokio::test]
    async fn a_file_appears_only_when_complete() {
        let tmp = TempDir::new("appears");
        let mut s = Session::new(&tmp.0);
        s.offer(
            vec![file_meta("a.txt", 1, 4), file_meta("b.txt", 2, 4)],
            vec![],
        )
        .await
        .unwrap();
        s.deliver(TransferAction::ConsentAccept).await.unwrap();
        let staging = staging_dir(&tmp.0);
        assert_eq!(dir_names(&staging), ["a.txt", "b.txt"]);
        assert_eq!(dir_names(&tmp.0).len(), 1, "a file has its name early");

        s.file_chunk(1, 4, 0, 0, b"ab").await.unwrap();
        s.file_chunk(1, 4, 2, 0, b"cd").await.unwrap();
        assert!(!tmp.0.join("a.txt").exists(), "an unfinished file is there");
        s.file_chunk(1, 4, 4, 1, b"").await.unwrap();
        assert_eq!(std::fs::read(tmp.0.join("a.txt")).unwrap(), b"abcd");
        assert_eq!(dir_names(&staging), ["b.txt"]);
        assert!(!tmp.0.join("b.txt").exists());

        let saved = s
            .req
            .state
            .transfer_metadata
            .as_ref()
            .expect("no metadata")
            .saved_paths
            .clone();
        let expected = ["a.txt", "b.txt"].map(|n| tmp.0.join(n).to_string_lossy().into_owned());
        assert_eq!(saved, expected);
    }

    /// A name taken while the transfer runs is not replaced: the file is numbered when it is
    /// published, and `Finished` carries the paths really written.
    #[tokio::test]
    async fn a_name_taken_during_the_transfer_gets_a_number() {
        let tmp = TempDir::new("taken-meanwhile");
        let mut s = Session::new(&tmp.0);
        s.offer(
            vec![file_meta("a.txt", 1, 2), file_meta("b.txt", 2, 2)],
            vec![],
        )
        .await
        .unwrap();
        s.deliver(TransferAction::ConsentAccept).await.unwrap();
        std::fs::write(tmp.0.join("a.txt"), b"other").unwrap();

        s.file_chunk(1, 2, 0, 0, b"hi").await.unwrap();
        s.file_chunk(1, 2, 2, 1, b"").await.unwrap();
        s.file_chunk(2, 2, 0, 0, b"yo").await.unwrap();
        let ended = s.last_chunk_and_close(2, 2, 2).await;
        assert!(is_end_of_session(&ended), "{ended:?}");

        assert_eq!(std::fs::read(tmp.0.join("a.txt")).unwrap(), b"other");
        assert_eq!(std::fs::read(tmp.0.join("a (1).txt")).unwrap(), b"hi");
        assert_eq!(std::fs::read(tmp.0.join("b.txt")).unwrap(), b"yo");
        assert_eq!(dir_names(&tmp.0), ["a (1).txt", "a.txt", "b.txt"]);
        let saved = s
            .events()
            .into_iter()
            .rev()
            .find(|e| e.state == Some(TransferState::Finished))
            .and_then(|e| e.metadata)
            .expect("no Finished event with metadata")
            .saved_paths;
        let expected = ["a (1).txt", "b.txt"].map(|n| tmp.0.join(n).to_string_lossy().into_owned());
        assert_eq!(saved, expected);
    }

    /// Two sessions into one folder, offered one name: each has its own hidden folder, and
    /// the files are numbered as they complete.
    #[tokio::test]
    async fn two_sessions_with_one_name_do_not_collide() {
        let tmp = TempDir::new("two-sessions");
        let mut first = Session::new(&tmp.0);
        let mut second = Session::new(&tmp.0);
        for s in [&mut first, &mut second] {
            s.offer(vec![file_meta("a.txt", 1, 4)], vec![])
                .await
                .unwrap();
        }
        for s in [&mut first, &mut second] {
            s.deliver(TransferAction::ConsentAccept).await.unwrap();
        }
        assert_eq!(staging_dirs(&tmp.0).len(), 2, "a session swept the other");

        first.file_chunk(1, 4, 0, 0, b"1111").await.unwrap();
        second.file_chunk(1, 4, 0, 0, b"2222").await.unwrap();
        let ended = second.last_chunk_and_close(1, 4, 4).await;
        assert!(is_end_of_session(&ended), "{ended:?}");
        let ended = first.last_chunk_and_close(1, 4, 4).await;
        assert!(is_end_of_session(&ended), "{ended:?}");

        assert_eq!(std::fs::read(tmp.0.join("a.txt")).unwrap(), b"2222");
        assert_eq!(std::fs::read(tmp.0.join("a (1).txt")).unwrap(), b"1111");
        assert_eq!(dir_names(&tmp.0), ["a (1).txt", "a.txt"]);
    }

    /// A staging folder a dead session left (nothing holds its lock) is removed when the next
    /// transfer is accepted into the folder, not at the offer.
    #[tokio::test]
    async fn stale_staging_is_swept_at_accept() {
        let tmp = TempDir::new("stale");
        let outside = TempDir::new("stale-outside");
        let target = outside.0.join("target.txt");
        std::fs::write(&target, b"keep").unwrap();
        let stale = tmp.0.join(".kuickshare-incoming-0123456789abcdef");
        std::fs::create_dir(&stale).unwrap();
        std::fs::write(stale.join("half.bin"), b"half").unwrap();
        std::os::unix::fs::symlink(&target, stale.join("link")).unwrap();
        crate::safety::backdate(&stale.join("half.bin"));
        crate::safety::backdate(&stale.join("link"));
        crate::safety::backdate(&stale);

        let mut s = Session::new(&tmp.0);
        s.offer(vec![file_meta("a.txt", 1, 4)], vec![])
            .await
            .unwrap();
        assert!(stale.exists(), "the offer swept");
        s.deliver(TransferAction::ConsentAccept).await.unwrap();
        assert!(!stale.exists(), "the stale folder is still there");
        assert_eq!(staging_dirs(&tmp.0).len(), 1);
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
    }

    /// An offer that does not fit sweeps a dead session's staging folder before it is
    /// refused, so a crash's leftovers cannot make every later offer too big.
    #[tokio::test]
    async fn an_offer_short_of_space_sweeps_stale_staging_first() {
        let tmp = TempDir::new("stale-space");
        let stale = tmp.0.join(".kuickshare-incoming-0123456789abcdef");
        std::fs::create_dir(&stale).unwrap();
        std::fs::write(stale.join("half.bin"), b"half").unwrap();
        crate::safety::backdate(&stale.join("half.bin"));
        crate::safety::backdate(&stale);

        let mut s = Session::new(&tmp.0);
        // No disk is this big: the offer is short of space whatever the sweep frees.
        let files = vec![file_meta("big.bin", 1, i64::MAX / 2)];
        let (result, status) = s.offer_and_read_reply(files).await;
        assert!(is_end_of_session(&result), "{result:?}");
        assert_eq!(status, Status::NotEnoughSpace);
        assert!(matches!(
            rejected_with(&s.events()),
            Some(Refusal::NotEnoughSpace { .. })
        ));
        assert!(!stale.exists(), "the offer short of space did not sweep");
    }

    /// D9: two dangling symlinks used to make `accept_transfer` spin forever. It runs in a
    /// thread because a busy loop cannot be stopped by a tokio timeout.
    #[test]
    fn dangling_symlinks_do_not_hang_accept() {
        let tmp = TempDir::new("dangling");
        std::os::unix::fs::symlink(tmp.0.join("gone-1"), tmp.0.join("a.jpg")).unwrap();
        std::os::unix::fs::symlink(tmp.0.join("gone-2"), tmp.0.join("a (1).jpg")).unwrap();
        let dir = tmp.0.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let mut s = Session::new(&dir);
                let _ = s.offer(vec![file_meta("a.jpg", 1, 4)], vec![]).await;
                let _ = s.deliver(TransferAction::ConsentAccept).await;
            });
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(20))
            .expect("accept_transfer is stuck in a loop on dangling symlinks");
        assert!(
            !tmp.0.join("gone-1").exists(),
            "a dangling symlink was followed"
        );
        assert!(
            !tmp.0.join("gone-2").exists(),
            "a dangling symlink was followed"
        );
    }

    /// D8: an offer of many files with one name must be handled in bounded time (today the
    /// cost grows with the cube of the count). Thread plus timeout, as above.
    #[test]
    fn many_identical_names_offer_is_bounded() {
        let tmp = TempDir::new("identical");
        let dir = tmp.0.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let mut s = Session::new(&dir);
                let files = (1..=2000).map(|i| file_meta("a.txt", i, 1)).collect();
                let _ = s.offer(files, vec![]).await;
            });
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(10))
            .expect("an offer of 2000 files named a.txt takes far too long");
    }

    /// Regression guard (passes today): a symlink put at the planned name after the offer is
    /// never written through.
    #[tokio::test]
    async fn symlink_swapped_in_after_offer_is_not_followed() {
        let tmp = TempDir::new("swap");
        let outside = TempDir::new("swap-outside");
        let target = outside.0.join("target.txt");
        std::fs::write(&target, b"keep").unwrap();

        let mut s = Session::new(&tmp.0);
        s.offer(vec![file_meta("a.txt", 1, 4)], vec![])
            .await
            .unwrap();
        std::os::unix::fs::symlink(&target, tmp.0.join("a.txt")).unwrap();
        let _ = s.deliver(TransferAction::ConsentAccept).await;
        let _ = s.file_chunk(1, 4, 0, 0, b"EVIL").await;

        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
    }

    /// The pairing PIN must not be written to the log at any level.
    #[tokio::test]
    async fn pin_is_not_logged() {
        capture_log::install();
        let mark = capture_log::mark();
        let (_peer, ours) = tokio::io::duplex(1024);
        let (bus, _rx) = broadcast::channel(8);
        let mut req = InboundRequest::new_over(MigratableStream::Duplex(ours), "id".into(), bus);
        let (secret, _public) = gen_ecdsa_keypair();
        req.state.private_key = Some(secret);
        req.state.client_init_msg_data = Some(vec![1, 2, 3]);
        req.state.server_init_data = Some(vec![4, 5, 6]);
        let (_peer_secret, peer_public) = gen_ecdsa_keypair();
        let point = peer_public.to_sec1_point(false);
        let key = GenericPublicKey {
            r#type: PublicKeyType::EcP256.into(),
            ec_p256_public_key: Some(EcP256PublicKey {
                x: point.x().unwrap().to_vec(),
                y: point.y().unwrap().to_vec(),
            }),
            ..Default::default()
        };
        req.finalize_key_exchange(key).await.unwrap();

        let pin = req.state.pin_code.clone().expect("no PIN was derived");
        let leaked: Vec<String> = capture_log::since(mark)
            .into_iter()
            .filter(|l| l.contains(&pin))
            .collect();
        assert!(leaked.is_empty(), "the PIN {pin} is in the log: {leaked:?}");
    }

    // ---- Fix round: limits, refusals, deadlines ----

    use crate::sharing_nearby::connection_response_frame::Status;

    /// The status in the reject frame the peer reads from the session.
    async fn read_reject_status(peer: &mut DuplexStream) -> Status {
        let frame = upgrade::read_plain_frame(peer).await.unwrap();
        let body = frame
            .v1
            .unwrap()
            .payload_transfer
            .unwrap()
            .payload_chunk
            .unwrap()
            .body
            .unwrap();
        let inner = sharing_nearby::Frame::decode(&*body).unwrap();
        let status = inner.v1.unwrap().connection_response.unwrap().status();
        // The empty last chunk follows; read it so the session's write does not meet a closed pipe.
        upgrade::read_plain_frame(peer).await.unwrap();
        status
    }

    fn is_end_of_session(r: &Result<(), anyhow::Error>) -> bool {
        matches!(
            r.as_ref().unwrap_err().downcast_ref(),
            Some(crate::errors::AppError::NotAnError)
        )
    }

    fn rejected_with(events: &[MessageClient]) -> Option<Refusal> {
        events
            .iter()
            .rev()
            .find(|e| e.state == Some(TransferState::Rejected))
            .and_then(|e| e.metadata.as_ref())
            .and_then(|m| m.refusal.clone())
    }

    impl Session {
        /// Offers `files` and plays a peer that reads the reply and closes; returns what the
        /// session ended with and the status it sent.
        async fn offer_and_read_reply(
            &mut self,
            files: Vec<sharing_nearby::FileMetadata>,
        ) -> (Result<(), anyhow::Error>, Status) {
            let body = introduction(files, vec![]);
            let data = payload_frame(
                payload_header::PayloadType::Bytes,
                INTRO_ID,
                body.len() as i64,
                0,
                1,
                &body,
            )
            .encode_to_vec();
            let mut peer = self.peer.take().expect("peer is closed");
            peer.write_all(&data).await.unwrap();
            let reader = async move {
                let status = read_reject_status(&mut peer).await;
                drop(peer);
                status
            };
            tokio::join!(self.req._handle((data.len() as u32).to_be_bytes()), reader)
        }
    }

    #[test]
    fn idle_deadline_rules() {
        let l = Limits {
            idle: Duration::from_secs(60),
            handshake: Duration::from_secs(90),
            ..Limits::default()
        };
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        // While the user decides only the ceiling counts, however silent the peer is.
        assert_eq!(
            idle_deadline_for(
                &TransferState::WaitingForUserConsent,
                false,
                t0,
                t0,
                at(5),
                &l
            ),
            Some(at(5) + l.consent)
        );
        // Not while an upgrade runs.
        assert_eq!(
            idle_deadline_for(&TransferState::ReceivingFiles, true, t0, t0, t0, &l),
            None
        );
        // Handshake: the earlier of idle and the handshake deadline.
        assert_eq!(
            idle_deadline_for(&TransferState::SentUkeyServerInit, false, at(0), t0, t0, &l),
            Some(at(60))
        );
        assert_eq!(
            idle_deadline_for(
                &TransferState::SentUkeyServerInit,
                false,
                at(50),
                t0,
                t0,
                &l
            ),
            Some(at(90))
        );
        // Receiving: idle after the last byte, the handshake deadline no longer counts.
        assert_eq!(
            idle_deadline_for(&TransferState::ReceivingFiles, false, at(100), t0, t0, &l),
            Some(at(160))
        );
    }

    /// B4: the idle time counts from the accept, not from the last byte before the user decided.
    #[tokio::test]
    async fn accept_refreshes_the_idle_clock() {
        let tmp = TempDir::new("refresh");
        let mut s = Session::new(&tmp.0);
        s.offer(vec![file_meta("a.txt", 1, 4)], vec![])
            .await
            .unwrap();
        // `checked_sub`: an `Instant` close to the boot time cannot go back 30 s.
        let before = Instant::now()
            .checked_sub(Duration::from_secs(30))
            .unwrap_or_else(Instant::now);
        s.req.last_rx = before;
        s.deliver(TransferAction::ConsentAccept).await.unwrap();
        assert!(s.req.last_rx > before);
    }

    /// An offer over the limits is refused: the peer gets a status, the app a reason.
    #[tokio::test]
    async fn too_many_files_is_refused_end_to_end() {
        let tmp = TempDir::new("refuse");
        let mut s = Session::new(&tmp.0);
        s.req.limits.max_files = 1;
        let files = vec![file_meta("a.txt", 1, 1), file_meta("b.txt", 2, 1)];
        let (result, status) = s.offer_and_read_reply(files).await;
        assert!(is_end_of_session(&result), "{result:?}");
        assert_eq!(status, Status::UnsupportedAttachmentType);
        assert_eq!(rejected_with(&s.events()), Some(Refusal::TooManyFiles));
        assert!(dir_names(&tmp.0).is_empty());
    }

    #[tokio::test]
    async fn not_enough_space_is_refused_at_the_offer() {
        let tmp = TempDir::new("space-offer");
        let mut s = Session::new(&tmp.0);
        let files = vec![file_meta("big.bin", 1, i64::MAX / 2)];
        let (result, status) = s.offer_and_read_reply(files).await;
        assert!(is_end_of_session(&result), "{result:?}");
        assert_eq!(status, Status::NotEnoughSpace);
        assert!(matches!(
            rejected_with(&s.events()),
            Some(Refusal::NotEnoughSpace { .. })
        ));
    }

    /// The space is checked again when the user accepts, and nothing is left behind.
    #[tokio::test]
    async fn not_enough_space_is_refused_at_the_accept() {
        let tmp = TempDir::new("space-accept");
        let mut s = Session::new(&tmp.0);
        s.offer(vec![file_meta("a.txt", 1, 4)], vec![])
            .await
            .unwrap();
        s.req.limits.space_reserve = u64::MAX;
        let mut peer = s.peer.take().unwrap();
        let reader = async move {
            let status = read_reject_status(&mut peer).await;
            drop(peer);
            status
        };
        let (result, status) = tokio::join!(s.deliver(TransferAction::ConsentAccept), reader);
        assert!(is_end_of_session(&result), "{result:?}");
        assert_eq!(status, Status::NotEnoughSpace);
        assert!(matches!(
            rejected_with(&s.events()),
            Some(Refusal::NotEnoughSpace { .. })
        ));
        assert!(dir_names(&tmp.0).is_empty());
    }

    /// An offer for a folder that does not exist creates nothing until the accept.
    #[tokio::test]
    async fn offer_does_not_create_the_download_folder() {
        let tmp = TempDir::new("nofolder");
        let target = tmp.0.join("not/yet");
        let mut s = Session::new(&target);
        s.offer(vec![file_meta("a.txt", 1, 4)], vec![])
            .await
            .unwrap();
        assert!(!target.exists(), "the offer created the folder");
        s.deliver(TransferAction::ConsentAccept).await.unwrap();
        assert!(staging_dir(&target).join("a.txt").exists());
        assert!(!target.join("a.txt").exists());
    }

    /// A folder swapped between the offer and the accept is refused, not followed.
    #[tokio::test]
    async fn folder_replaced_after_the_offer_is_refused() {
        let tmp = TempDir::new("replaced-dir");
        let target = tmp.0.join("dl");
        std::fs::create_dir(&target).unwrap();
        let mut s = Session::new(&target);
        s.offer(vec![file_meta("a.txt", 1, 4)], vec![])
            .await
            .unwrap();
        std::fs::rename(&target, tmp.0.join("old")).unwrap();
        std::fs::create_dir(&target).unwrap();
        let mut peer = s.peer.take().unwrap();
        let reader = async move {
            let status = read_reject_status(&mut peer).await;
            drop(peer);
            status
        };
        let (result, _) = tokio::join!(s.deliver(TransferAction::ConsentAccept), reader);
        assert!(is_end_of_session(&result), "{result:?}");
        assert_eq!(rejected_with(&s.events()), Some(Refusal::Destination));
        assert!(dir_names(&target).is_empty());
    }

    /// B6: a peer that repeats the empty last chunk of a finished file is tolerated.
    #[tokio::test]
    async fn repeated_last_chunk_is_ignored() {
        let tmp = TempDir::new("repeat");
        let mut s = Session::new(&tmp.0);
        s.offer(
            vec![file_meta("a.txt", 1, 2), file_meta("b.txt", 2, 2)],
            vec![],
        )
        .await
        .unwrap();
        s.deliver(TransferAction::ConsentAccept).await.unwrap();
        s.file_chunk(1, 2, 0, 0, b"hi").await.unwrap();
        s.file_chunk(1, 2, 2, 1, b"").await.unwrap();
        s.file_chunk(1, 2, 2, 1, b"").await.unwrap();
        assert_eq!(std::fs::read(tmp.0.join("a.txt")).unwrap(), b"hi");
        assert!(!s.finished());
    }

    /// A3: a frame that arrives a byte at a time does not make a decline wait.
    #[tokio::test]
    async fn decline_is_served_while_a_frame_trickles_in() {
        let tmp = TempDir::new("trickle");
        let mut s = Session::new(&tmp.0);
        s.offer(vec![file_meta("a.txt", 1, 4)], vec![])
            .await
            .unwrap();
        let mut peer = s.peer.take().unwrap();
        // The peer announces 1000 bytes and sends one.
        peer.write_all(&[7]).await.unwrap();
        let _ = s.bus.send(ChannelMessage {
            id: "peer".into(),
            msg: channel::Message::Lib {
                action: TransferAction::ConsentDecline,
            },
        });
        let closer = async move {
            let mut sink = Vec::new();
            let _ = peer.read_to_end(&mut sink).await;
        };
        let (result, ()) = tokio::join!(
            tokio::time::timeout(
                Duration::from_secs(10),
                s.req._handle(1000u32.to_be_bytes())
            ),
            closer
        );
        let result = result.expect("the decline waited for the slow frame");
        assert!(is_end_of_session(&result), "{result:?}");
    }

    /// A cancel from the app while a frame body is half read (the common case: chunks are
    /// 512 KiB): our request still goes out, but the rest of the stream cannot be told apart
    /// from the next frame, so nothing is read as a frame and the FIN follows at once.
    #[tokio::test]
    async fn a_cancel_in_the_middle_of_a_frame_does_not_read_past_it() {
        let tmp = TempDir::new("cancel-mid");
        let mut s = Session::new(&tmp.0);
        s.req.state.peer_disconnect_version = 4;
        s.offer(vec![file_meta("a.txt", 1, 4)], vec![])
            .await
            .unwrap();
        s.deliver(TransferAction::ConsentAccept).await.unwrap();
        let mut peer = s.peer.take().unwrap();
        let _reply = upgrade::read_plain_frame(&mut peer).await.unwrap();
        let _marker = upgrade::read_plain_frame(&mut peer).await.unwrap();
        // The peer announces 1000 bytes and sends one.
        peer.write_all(&[7]).await.unwrap();
        let _ = s.bus.send(ChannelMessage {
            id: "peer".into(),
            msg: channel::Message::Lib {
                action: TransferAction::TransferCancel,
            },
        });
        let closer = async move {
            let mut sink = Vec::new();
            let _ = peer.read_to_end(&mut sink).await;
            sink
        };
        let started = std::time::Instant::now();
        let (result, sink) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(5), s.req._handle(1000u32.to_be_bytes())),
            closer
        );
        let result = result.expect("the cancel never ended");
        assert!(is_end_of_session(&result), "{result:?}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the cancel waited"
        );
        let mut at = 0;
        let mut requested = false;
        while at + 4 <= sink.len() {
            let len = u32::from_be_bytes(sink[at..at + 4].try_into().unwrap()) as usize;
            let frame = OfflineFrame::decode(&sink[at + 4..at + 4 + len]).unwrap();
            requested |= frame
                .v1
                .and_then(|v1| v1.disconnection)
                .is_some_and(|d| d.request_safe_to_disconnect == Some(true));
            at += 4 + len;
        }
        assert!(requested, "our request did not go out");
    }

    /// A3: a body that stalls ends the session at the idle limit.
    #[tokio::test]
    async fn stalled_frame_body_times_out() {
        let tmp = TempDir::new("stall");
        let mut s = Session::new(&tmp.0);
        s.req.limits.idle = Duration::from_millis(100);
        let _peer = s.peer.take().unwrap();
        let r = tokio::time::timeout(
            Duration::from_secs(10),
            s.req._handle(1000u32.to_be_bytes()),
        )
        .await
        .expect("a stalled body kept the session");
        assert!(r.is_err());
    }

    /// A4: a peer that never reads must not hold a write for ever.
    #[tokio::test]
    async fn write_to_a_peer_that_never_reads_times_out() {
        let (_peer, ours) = tokio::io::duplex(64);
        let (bus, _rx) = broadcast::channel(8);
        let mut req = InboundRequest::new_over(MigratableStream::Duplex(ours), "id".into(), bus)
            .with_limits(Limits {
                idle: Duration::from_millis(100),
                ..Limits::default()
            });
        let r = tokio::time::timeout(Duration::from_secs(10), req.send_frame(vec![0; 4096]))
            .await
            .expect("the write never ended");
        assert!(r.is_err());
        // Part of the frame may be on the wire: nothing is written after it.
        assert!(req.send_frame(vec![]).await.is_err());
        assert!(req.poisoned);
    }

    /// B3: before the accept only one `Bytes` payload may be open.
    #[tokio::test]
    async fn only_one_bytes_payload_before_the_accept() {
        let tmp = TempDir::new("one-open");
        let mut s = Session::new(&tmp.0);
        let first = payload_frame(payload_header::PayloadType::Bytes, 5, 10, 0, 0, b"a");
        s.feed(&first).await.unwrap();
        let second = payload_frame(payload_header::PayloadType::Bytes, 6, 10, 0, 0, b"b");
        assert!(s.feed(&second).await.is_err());
    }

    // ---- Final round ----

    #[test]
    fn bus_traffic_does_not_move_the_read_deadline() {
        let t0 = Instant::now();
        let idle = Duration::from_secs(60);
        let hard = t0 + Duration::from_secs(1000);
        // The deadline depends on the last byte only: however often the bus is served, a
        // later "now" gives the same answer.
        assert_eq!(read_deadline(t0, idle, hard), t0 + idle);
        let later = t0 + Duration::from_secs(50);
        assert_eq!(
            read_deadline(t0, idle, hard),
            read_deadline(t0, idle, hard.max(later))
        );
        // A byte received moves it; the frame deadline caps it.
        assert_eq!(read_deadline(later, idle, hard), later + idle);
        assert_eq!(read_deadline(later, idle, t0 + idle), t0 + idle);
    }

    /// An accept sent while a frame body is being read is served, and the frame completes.
    #[tokio::test]
    async fn accept_is_served_during_a_body_read() {
        let tmp = TempDir::new("accept-mid-frame");
        let mut s = Session::new(&tmp.0);
        s.offer(vec![file_meta("a.txt", 1, 4)], vec![])
            .await
            .unwrap();
        let keep_alive = OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(location_nearby_connections::v1_frame::FrameType::KeepAlive.into()),
                keep_alive: Some(KeepAliveFrame { ack: Some(false) }),
                ..Default::default()
            }),
        }
        .encode_to_vec();
        let (first, second) = keep_alive.split_at(keep_alive.len() / 2);
        let mut peer = s.peer.take().unwrap();
        peer.write_all(first).await.unwrap();
        let _ = s.bus.send(ChannelMessage {
            id: "peer".into(),
            msg: channel::Message::Lib {
                action: TransferAction::ConsentAccept,
            },
        });
        let second = second.to_vec();
        let dir = tmp.0.clone();
        let rest = async move {
            // The rest of the frame comes only after the accept was served.
            tokio::time::timeout(Duration::from_secs(5), async {
                while !staging_dirs(&dir).iter().any(|d| d.join("a.txt").exists()) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the accept never reserved the file");
            peer.write_all(&second).await.unwrap();
            peer
        };
        let prefix = (keep_alive.len() as u32).to_be_bytes();
        let (result, _peer) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(10), s.req._handle(prefix)),
            rest
        );
        result.expect("the accept waited for the frame").unwrap();
        assert_eq!(s.req.state.state, TransferState::ReceivingFiles);
    }

    /// Alternating chunks of two files never hold more than one descriptor.
    #[tokio::test]
    async fn one_descriptor_at_a_time_with_alternating_files() {
        let tmp = TempDir::new("alternate");
        let mut s = Session::new(&tmp.0);
        s.offer(
            vec![file_meta("a.txt", 1, 4), file_meta("b.txt", 2, 4)],
            vec![],
        )
        .await
        .unwrap();
        s.deliver(TransferAction::ConsentAccept).await.unwrap();
        let open = |s: &mut Session| {
            s.req
                .state
                .inbound
                .get_mut(1)
                .and_then(|f| f.partial.as_ref().map(|p| p.open_files()))
                .unwrap()
        };
        assert_eq!(open(&mut s), 0, "the reserved files hold no descriptor");
        s.file_chunk(1, 4, 0, 0, b"ab").await.unwrap();
        assert_eq!(open(&mut s), 1);
        s.file_chunk(2, 4, 0, 0, b"AB").await.unwrap();
        assert_eq!(open(&mut s), 1);
        s.file_chunk(1, 4, 2, 0, b"cd").await.unwrap();
        assert_eq!(open(&mut s), 1);
        s.file_chunk(2, 4, 2, 0, b"CD").await.unwrap();
        assert_eq!(open(&mut s), 1);
        let staging = staging_dir(&tmp.0);
        assert_eq!(std::fs::read(staging.join("a.txt")).unwrap(), b"abcd");
        assert_eq!(std::fs::read(staging.join("b.txt")).unwrap(), b"ABCD");
    }

    /// A user who never answers is declined by the library when the core misses the event.
    #[tokio::test]
    async fn unanswered_offer_is_declined_at_the_ceiling() {
        let tmp = TempDir::new("ceiling");
        let mut s = Session::new(&tmp.0);
        s.offer(vec![file_meta("a.txt", 1, 4)], vec![])
            .await
            .unwrap();
        s.req.limits.consent = Duration::from_millis(50);
        s.req.consent_since = Instant::now();
        let mut peer = s.peer.take().unwrap();
        let reader = async move {
            let status = read_reject_status(&mut peer).await;
            drop(peer);
            status
        };
        let drive = async {
            loop {
                if let Err(e) = s.req.handle().await {
                    break e;
                }
            }
        };
        let (ended, status) =
            tokio::join!(tokio::time::timeout(Duration::from_secs(10), drive), reader);
        let ended = ended.expect("the offer was never declined");
        assert!(
            matches!(
                ended.downcast_ref(),
                Some(crate::errors::AppError::NotAnError)
            ),
            "{ended:?}"
        );
        assert_eq!(status, Status::Reject);
        assert!(dir_names(&tmp.0).is_empty());
    }

    /// A repeated empty last chunk of a completed `Bytes` payload is ignored.
    #[tokio::test]
    async fn repeated_last_chunk_of_a_bytes_payload_is_ignored() {
        let tmp = TempDir::new("repeat-bytes");
        let mut s = Session::new(&tmp.0);
        s.offer(vec![file_meta("a.txt", 1, 4)], vec![])
            .await
            .unwrap();
        let len = introduction(vec![file_meta("a.txt", 1, 4)], vec![]).len() as i64;
        let again = payload_frame(
            payload_header::PayloadType::Bytes,
            INTRO_ID,
            len,
            len,
            1,
            b"",
        );
        s.feed(&again).await.unwrap();
    }
}
