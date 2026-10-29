//! Interface between the Nearby bandwidth-upgrade protocol and the host's Wi-Fi.
//!
//! The protocol code in `hdl` only talks to this trait; the application
//! (KuickShare's core) implements it on top of NetworkManager.

use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use prost::Message;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, timeout};

use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::upgrade_path_info::{
    Medium, WifiHotspotCredentials,
};
use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::{
    ClientIntroduction, ClientIntroductionAck, EventType, UpgradePathInfo,
};
use crate::location_nearby_connections::{
    BandwidthUpgradeNegotiationFrame, DisconnectionFrame, OfflineFrame, V1Frame, offline_frame,
    v1_frame,
};
use crate::utils::stream_read_exact;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What a peer needs to join the Wi-Fi network (`WifiHotspotCredentials` or
/// `WifiDirectCredentials` on the wire).
/// `Debug` hides the password.
#[derive(Clone, PartialEq, Eq)]
pub struct WifiCredentials {
    pub ssid: String,
    pub password: String,
    /// TCP port of the upgrade server on the hosting side.
    pub port: u16,
    pub frequency_mhz: u32,
    /// Address of the hosting side on the network.
    pub gateway: Ipv4Addr,
}

impl std::fmt::Debug for WifiCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WifiCredentials")
            .field("ssid", &self.ssid)
            .field("password", &"<redacted>")
            .field("port", &self.port)
            .field("frequency_mhz", &self.frequency_mhz)
            .field("gateway", &self.gateway)
            .finish()
    }
}

pub trait WifiLink: Send + Sync {
    /// Receiving: bring up an access point with DHCP and return its credentials.
    /// `port` is the TCP port the caller will listen on; it is copied into the result.
    fn host(&self, port: u16) -> BoxFuture<'_, io::Result<WifiCredentials>>;

    /// Sending: join the network the phone hosts. Returns our IPv4 address on it,
    /// so the TCP connection can be bound to that interface.
    fn join<'a>(&'a self, creds: &'a WifiCredentials) -> BoxFuture<'a, io::Result<Ipv4Addr>>;

    /// Tear down whatever `host` or `join` set up. Must be safe to call twice.
    fn release(&self) -> BoxFuture<'_, ()>;
}

// ---------------------------------------------------------------------------
// Frame building and parsing
// ---------------------------------------------------------------------------

/// How long the host waits for the peer to join and connect (the peer has to join a Wi-Fi first).
pub const ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);
/// Host side: bound for `WifiLink::host`.
pub const HOST_TIMEOUT: Duration = Duration::from_secs(30);
/// Client side: bound for `WifiLink::join`.
pub const JOIN_TIMEOUT: Duration = Duration::from_secs(30);
/// Time the host gives a new connection to send its CLIENT_INTRODUCTION.
pub const INTRO_TIMEOUT: Duration = Duration::from_secs(5);
/// Client side: TCP connect timeout.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Client side: wait for CLIENT_INTRODUCTION_ACK.
pub const ACK_TIMEOUT: Duration = Duration::from_secs(10);
/// Sender side: how long the payload is held back waiting for the receiver's offer.
pub const OFFER_WAIT: Duration = Duration::from_secs(10);
/// Both sides: bound for the LAST_WRITE / SAFE_TO_CLOSE exchange.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(15);
/// Lets the peer read our DISCONNECTION before the old socket is dropped.
pub const CLOSE_GRACE: Duration = Duration::from_millis(500);

/// Plaintext upgrade frames are tiny; anything bigger is not from a Nearby peer.
const MAX_PLAIN_FRAME: usize = 64 * 1024;

fn bwu_offline_frame(frame: BandwidthUpgradeNegotiationFrame) -> OfflineFrame {
    OfflineFrame {
        version: Some(offline_frame::Version::V1.into()),
        v1: Some(V1Frame {
            r#type: Some(v1_frame::FrameType::BandwidthUpgradeNegotiation.into()),
            bandwidth_upgrade_negotiation: Some(frame),
            ..Default::default()
        }),
    }
}

/// A frame that carries only an event type (LAST_WRITE, SAFE_TO_CLOSE, ...).
pub fn event_frame(event: EventType) -> OfflineFrame {
    bwu_offline_frame(BandwidthUpgradeNegotiationFrame {
        event_type: Some(event.into()),
        ..Default::default()
    })
}

pub fn credentials_to_info(creds: &WifiCredentials) -> UpgradePathInfo {
    UpgradePathInfo {
        medium: Some(Medium::WifiHotspot.into()),
        wifi_hotspot_credentials: Some(WifiHotspotCredentials {
            ssid: Some(creds.ssid.clone()),
            password: Some(creds.password.clone()),
            port: Some(i32::from(creds.port)),
            gateway: Some(creds.gateway.to_string()),
            frequency: Some(i32::try_from(creds.frequency_mhz).unwrap_or(-1)),
        }),
        supports_client_introduction_ack: Some(true),
        ..Default::default()
    }
}

/// The gateway must be a LAN address; loopback is only accepted by the tests.
fn gateway_is_acceptable(ip: Ipv4Addr, allow_loopback: bool) -> bool {
    (ip.is_private() || ip.is_link_local() || (allow_loopback && ip.is_loopback()))
        && !ip.is_unspecified()
}

/// Reads a WIFI_HOTSPOT or WIFI_DIRECT offer; any other medium, missing field or implausible
/// value (it comes from the peer) is an error.
pub fn info_to_credentials(info: &UpgradePathInfo) -> anyhow::Result<WifiCredentials> {
    let (ssid, password, port, frequency, gateway) = match info.medium() {
        Medium::WifiHotspot => {
            let c = info
                .wifi_hotspot_credentials
                .as_ref()
                .ok_or_else(|| anyhow!("WIFI_HOTSPOT offer without credentials"))?;
            (c.ssid(), c.password(), c.port(), c.frequency(), c.gateway())
        }
        Medium::WifiDirect => {
            let c = info
                .wifi_direct_credentials
                .as_ref()
                .ok_or_else(|| anyhow!("WIFI_DIRECT offer without credentials"))?;
            (c.ssid(), c.password(), c.port(), c.frequency(), c.gateway())
        }
        other => bail!("unsupported upgrade medium {other:?}"),
    };
    let port = u16::try_from(port).ok().filter(|p| *p != 0);
    if ssid.is_empty()
        || ssid.len() > 32
        || ssid.starts_with('-')
        || ssid.contains(char::is_control)
    {
        bail!("bad SSID");
    }
    if !(8..=63).contains(&password.chars().count()) {
        bail!("bad password length");
    }
    let gateway: Ipv4Addr = gateway
        .parse()
        .with_context(|| format!("bad gateway {gateway:?}"))?;
    if !gateway_is_acceptable(gateway, cfg!(test)) {
        bail!("gateway {gateway} is not a LAN address");
    }
    Ok(WifiCredentials {
        ssid: ssid.to_owned(),
        password: password.to_owned(),
        port: port.ok_or_else(|| anyhow!("bad port"))?,
        // The wire default is -1 (unknown).
        frequency_mhz: u32::try_from(frequency).unwrap_or(0),
        gateway,
    })
}

/// With `disable_encryption` the offer declares that the new channel carries plain
/// OfflineFrames; otherwise the flag is left out of the wire message.
pub fn path_available_frame(creds: &WifiCredentials, disable_encryption: bool) -> OfflineFrame {
    let mut info = credentials_to_info(creds);
    info.supports_disabling_encryption = disable_encryption.then_some(true);
    bwu_offline_frame(BandwidthUpgradeNegotiationFrame {
        event_type: Some(EventType::UpgradePathAvailable.into()),
        upgrade_path_info: Some(info),
        ..Default::default()
    })
}

pub fn upgrade_failure_frame(info: Option<UpgradePathInfo>) -> OfflineFrame {
    bwu_offline_frame(BandwidthUpgradeNegotiationFrame {
        event_type: Some(EventType::UpgradeFailure.into()),
        upgrade_path_info: info,
        ..Default::default()
    })
}

/// `disable_encryption` echoes the offer's `supports_disabling_encryption`.
pub fn client_introduction_frame(endpoint_id: &str, disable_encryption: bool) -> OfflineFrame {
    bwu_offline_frame(BandwidthUpgradeNegotiationFrame {
        event_type: Some(EventType::ClientIntroduction.into()),
        client_introduction: Some(ClientIntroduction {
            endpoint_id: Some(endpoint_id.to_owned()),
            supports_disabling_encryption: Some(disable_encryption),
        }),
        ..Default::default()
    })
}

pub fn client_introduction_ack_frame() -> OfflineFrame {
    bwu_offline_frame(BandwidthUpgradeNegotiationFrame {
        event_type: Some(EventType::ClientIntroductionAck.into()),
        client_introduction_ack: Some(ClientIntroductionAck {}),
        ..Default::default()
    })
}

/// Sent in plaintext on the prior channel so the peer stops waiting on it.
pub fn plain_disconnection_frame() -> OfflineFrame {
    OfflineFrame {
        version: Some(offline_frame::Version::V1.into()),
        v1: Some(V1Frame {
            r#type: Some(v1_frame::FrameType::Disconnection.into()),
            disconnection: Some(DisconnectionFrame {
                request_safe_to_disconnect: Some(false),
                ack_safe_to_disconnect: Some(false),
            }),
            ..Default::default()
        }),
    }
}

/// The upgrade negotiation payload of a frame, if it is one.
pub fn parse_bwu(frame: &OfflineFrame) -> Option<&BandwidthUpgradeNegotiationFrame> {
    frame
        .v1
        .as_ref()
        .and_then(|v| v.bandwidth_upgrade_negotiation.as_ref())
}

/// Whether the peer's ConnectionRequest allows a hotspot upgrade. An empty list is
/// treated as "unknown" and does not forbid it.
pub fn peer_allows_hotspot(mediums: &[i32]) -> bool {
    mediums.is_empty()
        || mediums.contains(
            &(crate::location_nearby_connections::connection_request_frame::Medium::WifiHotspot
                as i32),
        )
}

pub async fn write_plain_frame(stream: &mut TcpStream, frame: &OfflineFrame) -> io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let data = frame.encode_to_vec();
    let mut buf = Vec::with_capacity(data.len() + 4);
    buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
    buf.extend_from_slice(&data);
    stream.write_all(&buf).await?;
    stream.flush().await
}

pub async fn read_plain_frame<R: AsyncRead + Unpin>(
    stream: &mut R,
) -> anyhow::Result<OfflineFrame> {
    let mut len = [0u8; 4];
    stream_read_exact(stream, &mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_PLAIN_FRAME {
        bail!("bad plaintext frame length {len}");
    }
    let mut buf = vec![0u8; len];
    stream_read_exact(stream, &mut buf).await?;
    Ok(OfflineFrame::decode(&*buf)?)
}

fn has_event(frame: &OfflineFrame, event: EventType) -> bool {
    parse_bwu(frame).is_some_and(|b| b.event_type() == event)
}

/// Connections read concurrently while waiting for the peer; more are dropped.
const MAX_PENDING_INTRO: usize = 16;

/// A new connection is the peer only if it reached the advertised address and sends a
/// plaintext CLIENT_INTRODUCTION carrying the endpoint id from its ConnectionRequest.
/// A different `supports_disabling_encryption` than we offered is logged; ours wins.
async fn introduce(
    mut stream: TcpStream,
    peer: SocketAddr,
    endpoint_id: &str,
    gateway: Ipv4Addr,
    offered_disabling: bool,
) -> Option<TcpStream> {
    let reached = stream.local_addr().ok().map(|a| a.ip());
    if !gateway.is_unspecified() && reached != Some(gateway.into()) {
        warn!("upgrade: dropping connection from {peer} (wrong local address)");
        return None;
    }
    let frame = match timeout(INTRO_TIMEOUT, read_plain_frame(&mut stream)).await {
        Ok(Ok(f)) => f,
        _ => {
            warn!("upgrade: dropping connection from {peer} (no valid frame)");
            return None;
        }
    };
    let intro = parse_bwu(&frame).filter(|b| b.event_type() == EventType::ClientIntroduction);
    let intro = intro.and_then(|b| b.client_introduction.as_ref());
    if intro.and_then(|i| i.endpoint_id.as_deref()) != Some(endpoint_id) {
        warn!("upgrade: dropping connection from {peer} (not the expected peer)");
        return None;
    }
    let stated = intro.is_some_and(|i| i.supports_disabling_encryption());
    if stated != offered_disabling {
        warn!(
            "upgrade: peer {peer} states supports_disabling_encryption={stated}, \
             we offered {offered_disabling}; using ours"
        );
    }
    stream.set_nodelay(true).ok()?;
    info!("upgrade: peer {peer} introduced itself");
    Some(stream)
}

/// Host side: accepts connections until one is the expected peer (see `introduce`).
/// Connections are read concurrently, so a silent one does not delay the real peer; the
/// first valid one wins and the others are dropped. The caller bounds the total wait and
/// sends the ACK.
pub async fn accept_introduced(
    listener: TcpListener,
    endpoint_id: String,
    gateway: Ipv4Addr,
    offered_disabling: bool,
) -> io::Result<TcpStream> {
    let mut pending = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
                if pending.len() >= MAX_PENDING_INTRO {
                    warn!("upgrade: dropping connection from {peer} (too many pending)");
                    continue;
                }
                let endpoint_id = endpoint_id.clone();
                pending.spawn(async move { introduce(stream, peer, &endpoint_id, gateway, offered_disabling).await });
            }
            Some(done) = pending.join_next() => {
                if let Ok(Some(stream)) = done {
                    return Ok(stream);
                }
            }
        }
    }
}

/// Closes a prior channel without discarding what the peer has not read yet: half-close,
/// then drain until the peer closes or `CLOSE_GRACE` passes.
pub async fn close_gracefully<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let _ = stream.shutdown().await;
    let mut buf = [0u8; 1024];
    let _ = timeout(CLOSE_GRACE, async {
        while matches!(stream.read(&mut buf).await, Ok(n) if n > 0) {}
    })
    .await;
}

/// Client side: connects to the host from `bind` (the address `WifiLink::join` returned),
/// sends the plaintext CLIENT_INTRODUCTION and, if the host announced it, waits for the ACK.
pub async fn connect_introduced(
    addr: SocketAddrV4,
    bind: Ipv4Addr,
    endpoint_id: &str,
    expect_ack: bool,
    disable_encryption: bool,
) -> anyhow::Result<TcpStream> {
    let socket = TcpSocket::new_v4()?;
    socket.bind(SocketAddr::from((bind, 0)))?;
    let mut stream = timeout(CONNECT_TIMEOUT, socket.connect(addr.into()))
        .await
        .context("connect timed out")?
        .with_context(|| format!("connect to {addr}"))?;
    stream.set_nodelay(true)?;
    write_plain_frame(
        &mut stream,
        &client_introduction_frame(endpoint_id, disable_encryption),
    )
    .await?;
    if expect_ack {
        let ack = timeout(ACK_TIMEOUT, read_plain_frame(&mut stream))
            .await
            .context("no CLIENT_INTRODUCTION_ACK")??;
        if !has_event(&ack, EventType::ClientIntroductionAck) {
            bail!("expected CLIENT_INTRODUCTION_ACK");
        }
    }
    Ok(stream)
}

// ---------------------------------------------------------------------------
// Hand-over state machine
// ---------------------------------------------------------------------------

/// What to do after a peer event on the prior channel.
#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
    Nothing,
    SendSafeToClose,
}

/// Tracks the LAST_WRITE / SAFE_TO_CLOSE exchange. The prior channel may only be
/// closed once the peer's LAST_WRITE (nothing more is coming on it) and its
/// SAFE_TO_CLOSE (it has our LAST_WRITE) have both arrived.
#[derive(Debug, Default)]
pub struct Handover {
    peer_last_write: bool,
    peer_safe_to_close: bool,
}

impl Handover {
    pub fn on_peer_event(&mut self, event: EventType) -> Reply {
        match event {
            EventType::LastWriteToPriorChannel => {
                self.peer_last_write = true;
                Reply::SendSafeToClose
            }
            EventType::SafeToClosePriorChannel => {
                self.peer_safe_to_close = true;
                Reply::Nothing
            }
            _ => Reply::Nothing,
        }
    }

    pub fn is_complete(&self) -> bool {
        self.peer_last_write && self.peer_safe_to_close
    }

    pub fn peer_last_write(&self) -> bool {
        self.peer_last_write
    }
}

// ---------------------------------------------------------------------------
// Per-session upgrade state, embedded in the inbound and outbound handlers
// ---------------------------------------------------------------------------

enum Phase {
    Idle,
    /// Bringing the link up (`host` or `join` plus connect) in a background task.
    Preparing(JoinHandle<anyhow::Result<Prepared>>),
    /// Host: waiting for the peer to connect and introduce itself.
    Waiting(JoinHandle<io::Result<TcpStream>>),
    /// Both: new stream is up and introduced; the prior channel is being drained.
    Draining(TcpStream, Handover),
}

/// Result of the background link set-up.
#[derive(Debug)]
pub enum Prepared {
    /// Receiver: hotspot is up, listener is bound.
    Hosted(WifiCredentials, TcpListener),
    /// Sender: joined, connected and introduced.
    Joined(TcpStream),
}

/// `WifiLink` refuses a second session with `ErrorKind::WouldBlock`.
fn is_link_busy(e: &anyhow::Error) -> bool {
    e.root_cause()
        .downcast_ref::<io::Error>()
        .is_some_and(|e| e.kind() == io::ErrorKind::WouldBlock)
}

/// Receiver: brings the hotspot up for `listener`'s port.
pub async fn prepare_host(
    link: Arc<dyn WifiLink>,
    listener: TcpListener,
) -> anyhow::Result<Prepared> {
    let port = listener.local_addr()?.port();
    let creds = timeout(HOST_TIMEOUT, link.host(port))
        .await
        .context("hosting the hotspot timed out")??;
    Ok(Prepared::Hosted(creds, listener))
}

/// Sender: joins the offered network and introduces itself to the host.
pub async fn prepare_join(
    link: Arc<dyn WifiLink>,
    creds: WifiCredentials,
    endpoint_id: String,
    expect_ack: bool,
    disable_encryption: bool,
) -> anyhow::Result<Prepared> {
    let local = timeout(JOIN_TIMEOUT, link.join(&creds))
        .await
        .context("joining the hotspot timed out")??;
    let stream = connect_introduced(
        SocketAddrV4::new(creds.gateway, creds.port),
        local,
        &endpoint_id,
        expect_ack,
        disable_encryption,
    )
    .await?;
    Ok(Prepared::Joined(stream))
}

#[derive(Debug)]
pub enum UpgradeEvent {
    /// The background set-up finished.
    Prepared(anyhow::Result<Prepared>),
    Connected(io::Result<TcpStream>),
    WaitTimedOut,
    DrainTimedOut,
    /// Sender: the receiver made no offer in time.
    OfferTimedOut,
}

pub struct UpgradeState {
    link: Option<Arc<dyn WifiLink>>,
    phase: Phase,
    deadline: Option<Instant>,
    /// Receiver: endpoint id from the peer's ConnectionRequest.
    peer_endpoint_id: Option<String>,
    /// An upgrade was started or given up on; no second try in the same session.
    attempted: bool,
    /// Sender: payload is held back until the upgrade is settled.
    holding: bool,
    /// `host` or `join` was called and `release` is still due.
    link_used: bool,
    hotspot_allowed: bool,
    /// Peer hand-over events that arrived before our own new stream was ready (the peer
    /// can be faster than the background `join`, most of all on a fast first channel).
    early: Handover,
    /// The new channel carries plain OfflineFrames: what the host offered, which the client
    /// follows. Off unless the host option is set (tests) or the peer's offer says so.
    disable_encryption: bool,
}

impl std::fmt::Debug for UpgradeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpgradeState")
            .field("enabled", &self.link.is_some())
            .field("attempted", &self.attempted)
            .finish()
    }
}

impl UpgradeState {
    pub fn new(link: Option<Arc<dyn WifiLink>>) -> Self {
        Self {
            link,
            peer_endpoint_id: None,
            phase: Phase::Idle,
            deadline: None,
            attempted: false,
            holding: false,
            link_used: false,
            hotspot_allowed: true,
            early: Handover::default(),
            disable_encryption: false,
        }
    }

    pub fn set_disable_encryption(&mut self, on: bool) {
        self.disable_encryption = on;
    }

    pub fn disable_encryption(&self) -> bool {
        self.disable_encryption
    }

    pub fn enabled(&self) -> bool {
        self.link.is_some()
    }

    pub fn link(&self) -> Option<Arc<dyn WifiLink>> {
        self.link.clone()
    }

    pub fn set_peer_mediums(&mut self, mediums: &[i32]) {
        self.hotspot_allowed = peer_allows_hotspot(mediums);
    }

    pub fn set_peer_endpoint_id(&mut self, id: &str) {
        self.peer_endpoint_id = Some(id.to_owned());
    }

    pub fn peer_endpoint_id(&self) -> Option<&str> {
        self.peer_endpoint_id.as_deref()
    }

    /// Whether a new upgrade may start now.
    pub fn can_start(&self) -> bool {
        self.link.is_some()
            && self.hotspot_allowed
            && !self.attempted
            && matches!(self.phase, Phase::Idle)
    }

    pub fn is_waiting(&self) -> bool {
        matches!(self.phase, Phase::Waiting(_))
    }

    /// Whether an upgrade is under way (hosting, waiting for the peer, or draining). Each of
    /// these has its own deadline, so the prior channel may be quiet meanwhile.
    pub fn in_progress(&self) -> bool {
        !matches!(self.phase, Phase::Idle)
    }

    /// Call right before `host` / `join` so a failure there still triggers `release`.
    pub fn mark_link_used(&mut self) {
        self.attempted = true;
        self.link_used = true;
    }

    /// Runs `task` in the background; its outcome comes back through `next_event`.
    pub fn begin_preparing(&mut self, task: JoinHandle<anyhow::Result<Prepared>>) {
        self.mark_link_used();
        self.phase = Phase::Preparing(task);
        self.deadline = None;
    }

    pub fn begin_waiting(&mut self, task: JoinHandle<io::Result<TcpStream>>) {
        self.phase = Phase::Waiting(task);
        self.deadline = Some(Instant::now() + ACCEPT_TIMEOUT);
    }

    pub fn begin_draining(&mut self, stream: TcpStream) {
        self.attempted = true;
        self.phase = Phase::Draining(stream, std::mem::take(&mut self.early));
        self.deadline = Some(Instant::now() + DRAIN_TIMEOUT);
    }

    /// Sender: hold the payload until the offer was handled or `OFFER_WAIT` elapsed.
    pub fn hold_payload(&mut self) {
        self.holding = true;
        if self.can_start() {
            self.deadline = Some(Instant::now() + OFFER_WAIT);
        }
    }

    pub fn needs_hold(&self) -> bool {
        self.link.is_some() && (self.can_start() || !matches!(self.phase, Phase::Idle))
    }

    /// True once, when the held payload may be sent.
    pub fn take_hold_released(&mut self) -> bool {
        if self.holding && self.attempted && matches!(self.phase, Phase::Idle) {
            self.holding = false;
            return true;
        }
        false
    }

    /// Feeds a peer event to the hand-over. `None` if no hand-over is running.
    pub fn on_peer_event(&mut self, event: EventType) -> Option<Reply> {
        match &mut self.phase {
            Phase::Draining(_, h) => Some(h.on_peer_event(event)),
            Phase::Preparing(_) => Some(self.early.on_peer_event(event)),
            _ => None,
        }
    }

    pub fn is_complete(&self) -> bool {
        matches!(&self.phase, Phase::Draining(_, h) if h.is_complete())
    }

    /// Ends the hand-over and returns the new stream.
    pub fn take_stream(&mut self) -> Option<TcpStream> {
        match std::mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Draining(s, _) => {
                self.deadline = None;
                Some(s)
            }
            other => {
                self.phase = other;
                None
            }
        }
    }

    /// Gives up (or ends a failed attempt): the session stays on the prior channel.
    pub async fn fail(&mut self) {
        match std::mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Waiting(task) => task.abort(),
            Phase::Preparing(task) => task.abort(),
            Phase::Idle | Phase::Draining(..) => {}
        }
        self.attempted = true;
        self.deadline = None;
        self.early = Handover::default();
        if self.link_used
            && let Some(link) = &self.link
        {
            link.release().await;
        }
        self.link_used = false;
    }

    /// Next timer or connection event of the current phase; pending if there is none.
    /// Cancel-safe.
    pub async fn next_event(&mut self) -> UpgradeEvent {
        if let Phase::Preparing(task) = &mut self.phase {
            let r = task.await;
            self.phase = Phase::Idle;
            let r = r.unwrap_or_else(|e| Err(e.into()));
            // The link is serving another session: releasing it would tear that one down.
            if r.as_ref().is_err_and(is_link_busy) {
                self.link_used = false;
            }
            return UpgradeEvent::Prepared(r);
        }
        let Some(deadline) = self.deadline else {
            return std::future::pending().await;
        };
        let event = match &mut self.phase {
            Phase::Waiting(task) => {
                tokio::select! {
                    r = task => {
                        UpgradeEvent::Connected(r.unwrap_or_else(|e| Err(io::Error::other(e))))
                    }
                    _ = tokio::time::sleep_until(deadline) => UpgradeEvent::WaitTimedOut,
                }
            }
            Phase::Draining(..) => {
                tokio::time::sleep_until(deadline).await;
                UpgradeEvent::DrainTimedOut
            }
            Phase::Idle | Phase::Preparing(_) => {
                tokio::time::sleep_until(deadline).await;
                UpgradeEvent::OfferTimedOut
            }
        };
        self.deadline = None;
        if matches!(event, UpgradeEvent::Connected(_)) {
            self.phase = Phase::Idle;
        }
        if matches!(event, UpgradeEvent::OfferTimedOut) {
            self.attempted = true;
        }
        event
    }

    /// Whether the peer's LAST_WRITE was seen (used when the drain times out).
    pub fn peer_last_write_seen(&self) -> bool {
        matches!(&self.phase, Phase::Draining(_, h) if h.peer_last_write())
    }
}

impl Drop for UpgradeState {
    fn drop(&mut self) {
        match &self.phase {
            Phase::Waiting(task) => task.abort(),
            Phase::Preparing(task) => task.abort(),
            Phase::Idle | Phase::Draining(..) => {}
        }
        if self.link_used
            && let Some(link) = self.link.clone()
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            rt.spawn(async move { link.release().await });
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
