use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::sync::broadcast::{Receiver as BusReceiver, Sender};
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::channel::{self, ChannelMessage, MessageClient, TransferKind};
use crate::errors::AppError;
use crate::hdl::{
    BleTarget, InboundRequest, OutboundPayload, OutboundRequest, TransferState, wait_for_cancel,
};
use crate::safety::Limits;
use crate::utils::RemoteDeviceInfo;

const INNER_NAME: &str = "TcpServer";

/// How long a stopping service waits for its sessions to wind down (each one tells its peer
/// and closes, see `Limits::stop_close` and `inbound::FIN_TIMEOUT`). `RQS::stop` waits for this, so it
/// must stay well under what the caller allows for the whole stop. A session still busy after
/// it goes on alone: it is already cancelled and ends by itself. Public so that the caller can
/// keep its own bound for `RQS::stop` above it (KuickShare asserts that in a test).
pub const STOP_WIND_DOWN: Duration = Duration::from_millis(400);

/// Waits for the tasks of `tracker` for at most `STOP_WIND_DOWN`; the tracker takes no more.
pub(crate) async fn wind_down(tracker: &TaskTracker) {
    tracker.close();
    if tokio::time::timeout(STOP_WIND_DOWN, tracker.wait())
        .await
        .is_err()
    {
        debug!("{INNER_NAME}: sessions still winding down after the stop wait");
    }
}

/// First and longest wait before `accept()` is tried again after it failed (typically no
/// file descriptors left); the wait doubles on each failure in a row.
const ACCEPT_RETRY_MIN: Duration = Duration::from_millis(50);
const ACCEPT_RETRY_MAX: Duration = Duration::from_secs(1);

/// Where inbound connections come from: a `TcpListener`, or a stand-in in the tests.
pub(crate) trait Acceptor: Send + Sync {
    fn poll_accept(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<(TcpStream, SocketAddr)>>;
}

impl Acceptor for TcpListener {
    fn poll_accept(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<(TcpStream, SocketAddr)>> {
        TcpListener::poll_accept(self, cx)
    }
}

#[derive(Debug, Clone)]
pub struct SendInfo {
    pub id: String,
    pub name: String,
    pub addr: String,
    pub ob: OutboundPayload,
}

impl SendInfo {
    /// The BLE target when `id` is a `ble:` id (`BleTarget::id`); `addr` is then ignored.
    pub fn ble_target(&self) -> Option<BleTarget> {
        BleTarget::parse(&self.id)
    }
}

/// Runs an inbound session until it ends; an end before `Finished` is reported on the bus.
pub(crate) async fn drive_inbound(mut ir: InboundRequest, sender: Sender<ChannelMessage>) {
    let id = ir.state.id.clone();
    loop {
        match ir.handle().await {
            Ok(_) => {}
            Err(e) => match e.downcast_ref() {
                Some(AppError::NotAnError) => break,
                None => {
                    if ir.state.state == TransferState::Initial {
                        break;
                    }

                    if ir.state.state != TransferState::Finished {
                        let _ = sender.send(ChannelMessage {
                            id,
                            msg: channel::Message::Client(MessageClient {
                                kind: TransferKind::Inbound,
                                state: Some(TransferState::Disconnected),
                                metadata: Default::default(),
                            }),
                        });
                    }
                    error!(
                        "{INNER_NAME}: error while handling client: {e} ({:?})",
                        ir.state.state
                    );
                    break;
                }
            },
        }
    }
}

/// Sessions open per remote address; a session counts until its guard is dropped.
type AddrCounts = Arc<Mutex<HashMap<IpAddr, usize>>>;

struct AddrGuard {
    counts: AddrCounts,
    ip: IpAddr,
}

impl AddrGuard {
    /// `None` if `ip` already has `max` sessions open.
    fn take(counts: &AddrCounts, ip: IpAddr, max: usize) -> Option<Self> {
        let mut map = counts.lock().unwrap_or_else(|e| e.into_inner());
        let n = map.entry(ip).or_insert(0);
        if *n >= max {
            return None;
        }
        *n += 1;
        Some(Self {
            counts: Arc::clone(counts),
            ip,
        })
    }
}

impl Drop for AddrGuard {
    fn drop(&mut self) {
        let mut map = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = map.get_mut(&self.ip) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                map.remove(&self.ip);
            }
        }
    }
}

/// At most one warning per this long about connections dropped over a limit.
const DROP_WARN_EVERY: Duration = Duration::from_secs(10);

pub struct TcpServer {
    endpoint_id: [u8; 4],
    acceptor: Box<dyn Acceptor>,
    sender: Sender<ChannelMessage>,
    connect_receiver: Receiver<SendInfo>,
    limits: Limits,
    /// One permit per inbound session, held until the session ends: a flood of silent
    /// connections cannot use up the file descriptors the sends and the accept need.
    sessions: Arc<Semaphore>,
    per_addr: AddrCounts,
    /// The tasks of the sessions and the sends, so that `run` can wait for them at the stop.
    tasks: TaskTracker,
}

impl TcpServer {
    pub fn new(
        endpoint_id: [u8; 4],
        tcp_listener: TcpListener,
        sender: Sender<ChannelMessage>,
        connect_receiver: Receiver<SendInfo>,
    ) -> Result<Self, anyhow::Error> {
        Ok(Self::from_acceptor(
            endpoint_id,
            Box::new(tcp_listener),
            sender,
            connect_receiver,
        ))
    }

    fn from_acceptor(
        endpoint_id: [u8; 4],
        acceptor: Box<dyn Acceptor>,
        sender: Sender<ChannelMessage>,
        connect_receiver: Receiver<SendInfo>,
    ) -> Self {
        Self {
            endpoint_id,
            acceptor,
            sender,
            connect_receiver,
            limits: Limits::default(),
            sessions: Arc::new(Semaphore::new(Limits::default().max_sessions)),
            per_addr: AddrCounts::default(),
            tasks: TaskTracker::new(),
        }
    }

    /// Replaces the default limits of the inbound sessions.
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self.sessions = Arc::new(Semaphore::new(limits.max_sessions));
        self
    }

    pub async fn run(&mut self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        info!("{INNER_NAME}: service starting");

        let mut retry = ACCEPT_RETRY_MIN;
        let mut dropped: u32 = 0;
        let mut last_warn: Option<tokio::time::Instant> = None;

        loop {
            let cctk = ctk.clone();

            tokio::select! {
                _ = ctk.cancelled() => {
                    info!("{INNER_NAME}: tracker cancelled, breaking");
                    break;
                }
                Some(i) = self.connect_receiver.recv() => {
                    debug!("{INNER_NAME}: connect_receiver: got {:?}", i);
                    // Own task: a slow or silent peer must not block the other sends. The
                    // subscription is made here so a cancel sent right after is not missed.
                    let endpoint_id = self.endpoint_id;
                    let sender = self.sender.clone();
                    let cancel_rx = sender.subscribe();
                    self.tasks.spawn(async move {
                        if let Err(e) = connect_send(endpoint_id, sender, cancel_rx, cctk, i, Timeouts::default()).await {
                            error!("{INNER_NAME}: error sending: {}", e);
                        }
                    });
                }
                r = std::future::poll_fn(|cx| self.acceptor.poll_accept(cx)) => {
                    match r {
                        Ok((socket, remote_addr)) => {
                            retry = ACCEPT_RETRY_MIN;
                            let ip = remote_addr.ip();
                            let permit = self.sessions.clone().try_acquire_owned().ok();
                            let guard = AddrGuard::take(&self.per_addr, ip, self.limits.max_per_addr);
                            let (Some(permit), Some(guard)) = (permit, guard) else {
                                dropped += 1;
                                if last_warn.is_none_or(|t| t.elapsed() >= DROP_WARN_EVERY) {
                                    warn!("{INNER_NAME}: too many sessions, dropped {dropped} connection(s)");
                                    dropped = 0;
                                    last_warn = Some(tokio::time::Instant::now());
                                }
                                continue;
                            };
                            trace!("{INNER_NAME}: new client: {remote_addr}");
                            // The session ends with the service: it is told when `ctk` is cancelled.
                            let ir = InboundRequest::new(socket, remote_addr.to_string(), self.sender.clone())
                                .with_limits(self.limits)
                                .with_stop(ctk.clone());
                            let sender = self.sender.clone();
                            self.tasks.spawn(async move {
                                let _held = (permit, guard);
                                drive_inbound(ir, sender).await;
                            });
                        },
                        Err(err) => {
                            // Not fatal: a lack of descriptors passes once sessions end. The
                            // server must stay up for the sends and for the next peer.
                            error!("{INNER_NAME}: error accepting: {}, retrying in {} ms", err, retry.as_millis());
                            tokio::select! {
                                _ = ctk.cancelled() => {
                                    info!("{INNER_NAME}: tracker cancelled, breaking");
                                    break;
                                }
                                _ = tokio::time::sleep(retry) => {}
                            }
                            retry = (retry * 2).min(ACCEPT_RETRY_MAX);
                        }
                    }
                }
            }
        }

        // The sessions were told to end by the same token: let them tell their peers.
        wind_down(&self.tasks).await;
        Ok(())
    }

    /// Runs one send to the end. `run` spawns it per send; a failure before the transfer
    /// started is also reported on the bus (`Disconnected`), a cancel as `Cancelled`.
    pub async fn connect(&self, ctk: CancellationToken, si: SendInfo) -> Result<(), anyhow::Error> {
        let cancel_rx = self.sender.subscribe();
        connect_send(
            self.endpoint_id,
            self.sender.clone(),
            cancel_rx,
            ctk,
            si,
            Timeouts::default(),
        )
        .await
    }
}

/// How long the TCP connect to the peer may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// How long the peer has, from our first request, to answer the UKEY2 init and the connection
/// request, so that the transfer can start.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy)]
struct Timeouts {
    connect: Duration,
    handshake: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: CONNECT_TIMEOUT,
            handshake: HANDSHAKE_TIMEOUT,
        }
    }
}

/// Tells the app how a transfer that has no session of its own (yet) ended.
fn report(sender: &Sender<ChannelMessage>, id: &str, state: TransferState) {
    let _ = sender.send(ChannelMessage {
        id: id.to_string(),
        msg: channel::Message::Client(MessageClient {
            kind: TransferKind::Outbound,
            state: Some(state),
            metadata: Default::default(),
        }),
    });
}

/// `cancel_rx` must be subscribed before the call, so a cancel during the connect is seen.
async fn connect_send(
    endpoint_id: [u8; 4],
    sender: Sender<ChannelMessage>,
    cancel_rx: BusReceiver<ChannelMessage>,
    ctk: CancellationToken,
    si: SendInfo,
    timeouts: Timeouts,
) -> Result<(), anyhow::Error> {
    let id = si.id.clone();
    let result = if let Some(target) = si.ble_target() {
        connect_ble(endpoint_id, &sender, cancel_rx, ctk, si, target).await
    } else {
        connect_lan(endpoint_id, &sender, cancel_rx, ctk, si, timeouts).await
    };
    if result.is_err() {
        // The app waits for a first event of this transfer: tell it now that none will come.
        report(&sender, &id, TransferState::Disconnected);
    }
    result
}

async fn connect_lan(
    endpoint_id: [u8; 4],
    sender: &Sender<ChannelMessage>,
    mut cancel_rx: BusReceiver<ChannelMessage>,
    ctk: CancellationToken,
    si: SendInfo,
    timeouts: Timeouts,
) -> Result<(), anyhow::Error> {
    let id = si.id.clone();
    debug!("{INNER_NAME}: Connecting to: {}", si.addr);
    let socket = tokio::select! {
        _ = ctk.cancelled() => return Ok(()),
        _ = wait_for_cancel(&mut cancel_rx, &id) => {
            info!("{INNER_NAME}: {id}: cancelled while connecting");
            report(sender, &id, TransferState::Cancelled);
            return Ok(());
        }
        r = tokio::time::timeout(timeouts.connect, TcpStream::connect(si.addr.clone())) => match r {
            Ok(r) => r?,
            Err(_) => anyhow::bail!(
                "connecting to {} timed out after {} s",
                si.addr,
                timeouts.connect.as_secs_f32()
            ),
        },
    };

    let mut or = OutboundRequest::new(
        endpoint_id,
        socket,
        si.id,
        sender.clone(),
        si.ob,
        RemoteDeviceInfo {
            device_type: crate::DeviceType::Unknown,
            name: si.name,
        },
    );

    // Send connection request
    or.send_connection_request().await?;
    // Send UKEY init
    or.send_ukey2_client_init().await?;
    let handshake_deadline = tokio::time::Instant::now() + timeouts.handshake;

    loop {
        // Until the connection response is in, the peer is only being waited for.
        let handshaking = matches!(
            or.state.state,
            TransferState::Initial
                | TransferState::SentUkeyClientInit
                | TransferState::SentUkeyClientFinish
        );
        tokio::select! {
            biased;
            _ = ctk.cancelled() => {
                info!("{INNER_NAME}: tracker cancelled, breaking");
                break;
            },
            _ = wait_for_cancel(&mut cancel_rx, &id), if handshaking => {
                info!("{INNER_NAME}: {id}: cancelled while handshaking");
                // With encryption up the peer is told; before that there is nothing to say.
                if or.state.state == TransferState::SentUkeyClientFinish {
                    or.cancel_peer(&[]).await;
                }
                report(sender, &id, TransferState::Cancelled);
                break;
            },
            _ = tokio::time::sleep_until(handshake_deadline), if handshaking => {
                error!("{INNER_NAME}: {id}: no answer from the peer within {} s ({:?})",
                    timeouts.handshake.as_secs_f32(), or.state.state);
                if or.state.state == TransferState::SentUkeyClientFinish {
                    or.cancel_peer(&[]).await;
                }
                report(sender, &id, TransferState::Disconnected);
                break;
            },
            r = or.handle() => {
                if let Err(e) = r {
                    match e.downcast_ref() {
                        Some(AppError::NotAnError) => break,
                        None => {
                            if or.state.state != TransferState::Finished && or.state.state != TransferState::Cancelled {
                                report(sender, &id, TransferState::Disconnected);
                            }
                            error!("{INNER_NAME}: error while handling client: {e} ({:?})", or.state.state);
                            break;
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Sends over BLE L2CAP: a fresh scan for the phone, a dial, then the same session and bus
/// messages as the TCP path, under the transfer id `si.id`.
async fn connect_ble(
    endpoint_id: [u8; 4],
    sender: &Sender<ChannelMessage>,
    #[cfg_attr(
        not(all(feature = "experimental", target_os = "linux")),
        allow(unused_mut)
    )]
    mut cancel_rx: BusReceiver<ChannelMessage>,
    ctk: CancellationToken,
    si: SendInfo,
    target: BleTarget,
) -> Result<(), anyhow::Error> {
    if crate::wifi_link().is_none() {
        anyhow::bail!(
            "sending to {} over Bluetooth needs the Wi-Fi upgrade, which is off",
            si.id
        );
    }
    #[cfg(all(feature = "experimental", target_os = "linux"))]
    {
        debug!("{INNER_NAME}: Connecting over BLE to: {}", si.id);
        let id = si.id.clone();
        let send = crate::hdl::ble_scan::SCANNER.send(
            target,
            endpoint_id,
            si.id,
            si.name,
            si.ob,
            sender.clone(),
            ctk,
        );
        // The scanner only watches `ctk`: a user cancel during the scan, the dial or the wait
        // for the scanner is caught here, and dropping the future abandons the send.
        tokio::select! {
            r = send => r,
            _ = wait_for_cancel(&mut cancel_rx, &id) => {
                info!("{INNER_NAME}: {id}: cancelled before the Bluetooth session started");
                report(sender, &id, TransferState::Cancelled);
                Ok(())
            }
        }
    }
    #[cfg(not(all(feature = "experimental", target_os = "linux")))]
    {
        let _ = (ctk, target, endpoint_id, sender, cancel_rx);
        anyhow::bail!("sending over Bluetooth is not supported in this build")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upgrade::tests::SERIAL;

    fn ble_info(id: &str) -> SendInfo {
        SendInfo {
            id: id.into(),
            name: "Pixel".into(),
            addr: ":".into(),
            ob: OutboundPayload::Files(vec![]),
        }
    }

    #[test]
    fn only_ble_ids_are_ble_targets() {
        assert_eq!(
            ble_info("ble:G483").ble_target(),
            Some(BleTarget {
                endpoint_id: *b"G483"
            })
        );
        assert_eq!(ble_info("10.0.0.2:4000").ble_target(), None);
    }

    #[tokio::test]
    async fn a_ble_send_without_a_wifi_link_fails_fast() {
        let _guard = SERIAL.lock().await;
        *crate::WIFI_LINK.write().unwrap() = None;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (bus, mut rx) = tokio::sync::broadcast::channel(4);
        let (_tx, unused) = tokio::sync::mpsc::channel(1);
        let server = TcpServer::new(*b"TEST", listener, bus, unused).unwrap();
        let err = server
            .connect(CancellationToken::new(), ble_info("ble:G483"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Wi-Fi upgrade"), "{err}");
        // The failure is also reported on the bus, so the app need not wait for its watchdog.
        let message = rx.try_recv().unwrap();
        assert_eq!(message.id, "ble:G483");
        let channel::Message::Client(client) = message.msg else {
            panic!("expected a client message");
        };
        assert_eq!(client.state, Some(TransferState::Disconnected));
    }

    #[tokio::test]
    async fn a_tcp_send_does_not_need_the_link() {
        let _guard = SERIAL.lock().await;
        *crate::WIFI_LINK.write().unwrap() = None;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (bus, _rx) = tokio::sync::broadcast::channel(4);
        let (_tx, unused) = tokio::sync::mpsc::channel(1);
        let server = TcpServer::new(*b"TEST", listener, bus, unused).unwrap();
        let mut si = ble_info("127.0.0.1:1");
        si.addr = "127.0.0.1:1".into();
        // Nothing listens there: the failure is a TCP one, not the BLE message.
        let err = server
            .connect(CancellationToken::new(), si)
            .await
            .unwrap_err();
        assert!(!err.to_string().contains("Wi-Fi upgrade"), "{err}");
    }

    /// Binds a peer that accepts and then never answers; returns it (kept open) and the send.
    async fn silent_peer() -> (TcpListener, SendInfo) {
        let peer = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = peer.local_addr().unwrap().to_string();
        let mut si = ble_info(&addr);
        si.addr = addr;
        (peer, si)
    }

    fn state_of(message: ChannelMessage) -> Option<TransferState> {
        match message.msg {
            channel::Message::Client(client) => client.state,
            _ => None,
        }
    }

    #[tokio::test]
    async fn a_refused_tcp_send_is_reported_as_disconnected() {
        let _guard = SERIAL.lock().await;
        let (bus, mut rx) = tokio::sync::broadcast::channel(8);
        let cancel_rx = bus.subscribe();
        let mut si = ble_info("127.0.0.1:1");
        si.addr = "127.0.0.1:1".into();
        let result = connect_send(
            *b"TEST",
            bus,
            cancel_rx,
            CancellationToken::new(),
            si,
            Timeouts::default(),
        )
        .await;
        assert!(result.is_err());
        let message = rx.try_recv().unwrap();
        assert_eq!(message.id, "127.0.0.1:1");
        assert_eq!(state_of(message), Some(TransferState::Disconnected));
    }

    #[tokio::test]
    async fn a_silent_peer_ends_the_handshake_with_disconnected() {
        let _guard = SERIAL.lock().await;
        let (peer, si) = silent_peer().await;
        let id = si.id.clone();
        let (bus, mut rx) = tokio::sync::broadcast::channel(8);
        let cancel_rx = bus.subscribe();
        let timeouts = Timeouts {
            connect: Duration::from_secs(5),
            handshake: Duration::from_millis(200),
        };
        let send = connect_send(
            *b"TEST",
            bus,
            cancel_rx,
            CancellationToken::new(),
            si,
            timeouts,
        );
        let (result, _accepted) = tokio::join!(send, peer.accept());
        assert!(result.is_ok());
        let mut states = vec![];
        while let Ok(message) = rx.try_recv() {
            assert_eq!(message.id, id);
            states.push(state_of(message));
        }
        assert_eq!(states, vec![Some(TransferState::Disconnected)]);
    }

    #[tokio::test]
    async fn a_cancel_during_the_handshake_ends_the_send_as_cancelled() {
        let _guard = SERIAL.lock().await;
        let (peer, si) = silent_peer().await;
        let id = si.id.clone();
        let (bus, mut rx) = tokio::sync::broadcast::channel(8);
        let cancel_rx = bus.subscribe();
        let timeouts = Timeouts {
            connect: Duration::from_secs(5),
            handshake: Duration::from_secs(30),
        };
        let send = connect_send(
            *b"TEST",
            bus.clone(),
            cancel_rx,
            CancellationToken::new(),
            si,
            timeouts,
        );
        let cancel = async {
            // Let the peer accept and the handshake start, then cancel.
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = bus.send(ChannelMessage {
                id: id.clone(),
                msg: channel::Message::Lib {
                    action: channel::TransferAction::TransferCancel,
                },
            });
        };
        let started = tokio::time::Instant::now();
        let (result, _, _accepted) = tokio::join!(send, cancel, peer.accept());
        assert!(result.is_ok());
        assert!(started.elapsed() < Duration::from_secs(5));
        let mut states = vec![];
        while let Ok(message) = rx.try_recv() {
            states.push(state_of(message));
        }
        assert!(
            states.contains(&Some(TransferState::Cancelled)),
            "{states:?}"
        );
        assert!(
            !states.contains(&Some(TransferState::Disconnected)),
            "{states:?}"
        );
    }

    /// The text of an outgoing send must not be written to the log at any level.
    #[tokio::test]
    async fn send_text_is_not_logged() {
        const SECRET: &str = "TOPSECRET-BODY-4f2a9c";
        crate::hdl::capture_log::install();
        let mark = crate::hdl::capture_log::mark();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (bus, _keep) = tokio::sync::broadcast::channel(16);
        let mut watch = bus.subscribe();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let mut server = TcpServer::new(*b"TEST", listener, bus, rx).unwrap();
        let ctk = CancellationToken::new();
        let run = tokio::spawn({
            let ctk = ctk.clone();
            async move { server.run(ctk).await }
        });
        let mut si = ble_info("127.0.0.1:1");
        si.addr = "127.0.0.1:1".into();
        si.ob = OutboundPayload::Text {
            text: SECRET.into(),
            text_type: crate::TextPayloadType::Text,
        };
        tx.send(si).await.unwrap();
        // Nothing listens on port 1: the refused send is reported once `run` has logged it.
        while state_of(watch.recv().await.unwrap()) != Some(TransferState::Disconnected) {}
        ctk.cancel();
        run.await.unwrap().unwrap();

        let leaked: Vec<String> = crate::hdl::capture_log::since(mark)
            .into_iter()
            .filter(|l| l.contains(SECRET))
            .collect();
        assert!(leaked.is_empty(), "the text is in the log: {leaked:?}");
    }

    /// Short limits so the tests wait for a fraction of a second.
    fn quick_limits() -> Limits {
        Limits {
            handshake: Duration::from_millis(300),
            idle: Duration::from_millis(300),
            ..Limits::default()
        }
    }

    /// A stand-in for the listener that fails its first `failures` accepts.
    struct FlakyAcceptor {
        inner: TcpListener,
        failures: u32,
    }

    impl Acceptor for FlakyAcceptor {
        fn poll_accept(
            &mut self,
            cx: &mut Context<'_>,
        ) -> Poll<io::Result<(TcpStream, SocketAddr)>> {
            if self.failures > 0 {
                self.failures -= 1;
                return Poll::Ready(Err(io::Error::other("Too many open files (injected)")));
            }
            TcpListener::poll_accept(&self.inner, cx)
        }
    }

    /// D12: a connection that sends nothing must be dropped after the idle time (60 s by
    /// default; the limit is injected here).
    #[tokio::test]
    async fn idle_inbound_session_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (bus, _keep) = tokio::sync::broadcast::channel(16);
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let mut server = TcpServer::new(*b"TEST", listener, bus, rx)
            .unwrap()
            .with_limits(quick_limits());
        let ctk = CancellationToken::new();
        tokio::spawn({
            let ctk = ctk.clone();
            async move { server.run(ctk).await }
        });
        let mut idle = TcpStream::connect(addr).await.unwrap();
        let mut buf = [0u8; 1];
        let closed = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::io::AsyncReadExt::read(&mut idle, &mut buf),
        )
        .await
        .expect("the server kept a silent connection open");
        assert!(matches!(closed, Ok(0) | Err(_)));
        ctk.cancel();
    }

    /// D12: an `accept()` error (file descriptors used up) must not end the server loop.
    #[tokio::test]
    async fn accept_error_keeps_server_running() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (bus, _keep) = tokio::sync::broadcast::channel(16);
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let flaky = FlakyAcceptor {
            inner: listener,
            failures: 3,
        };
        let mut server = TcpServer::from_acceptor(*b"TEST", Box::new(flaky), bus, rx)
            .with_limits(quick_limits());
        let ctk = CancellationToken::new();
        let run = tokio::spawn({
            let ctk = ctk.clone();
            async move { server.run(ctk).await }
        });
        // The connection waits in the backlog while accept fails; once it is accepted the
        // idle timeout closes it, which only a running server does (a stopped one resets it).
        let mut client = TcpStream::connect(addr).await.unwrap();
        let mut buf = [0u8; 1];
        let closed = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::io::AsyncReadExt::read(&mut client, &mut buf),
        )
        .await
        .expect("the connection was never served");
        assert!(matches!(closed, Ok(0)), "{closed:?}");
        assert!(
            !run.is_finished(),
            "the server loop ended after accept errors"
        );
        ctk.cancel();
        run.await.unwrap().unwrap();
    }

    /// Waits until the server holds `taken` of its permits (the connection was accepted).
    async fn wait_for_permits(sessions: &Semaphore, taken: usize, total: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while sessions.available_permits() + taken != total {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the server never took the connection");
    }

    /// D12: more connections than `max_sessions` are dropped, the ones served stay.
    #[tokio::test]
    async fn sessions_are_capped() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (bus, _keep) = tokio::sync::broadcast::channel(16);
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let limits = Limits {
            max_sessions: 1,
            handshake: Duration::from_secs(30),
            idle: Duration::from_secs(30),
            ..Limits::default()
        };
        let mut server = TcpServer::new(*b"TEST", listener, bus, rx)
            .unwrap()
            .with_limits(limits);
        let sessions = server.sessions.clone();
        let ctk = CancellationToken::new();
        tokio::spawn({
            let ctk = ctk.clone();
            async move { server.run(ctk).await }
        });
        let _first = TcpStream::connect(addr).await.unwrap();
        wait_for_permits(&sessions, 1, 1).await;
        let mut second = TcpStream::connect(addr).await.unwrap();
        let mut buf = [0u8; 1];
        let closed = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::io::AsyncReadExt::read(&mut second, &mut buf),
        )
        .await
        .expect("a connection over the cap was kept");
        assert!(matches!(closed, Ok(0) | Err(_)));
        ctk.cancel();
    }

    /// One address cannot hold more than `max_per_addr` sessions, however many are free.
    #[tokio::test]
    async fn sessions_per_address_are_capped() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (bus, _keep) = tokio::sync::broadcast::channel(16);
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let limits = Limits {
            max_per_addr: 1,
            handshake: Duration::from_secs(30),
            idle: Duration::from_secs(30),
            ..Limits::default()
        };
        let total = limits.max_sessions;
        let mut server = TcpServer::new(*b"TEST", listener, bus, rx)
            .unwrap()
            .with_limits(limits);
        let sessions = server.sessions.clone();
        let ctk = CancellationToken::new();
        tokio::spawn({
            let ctk = ctk.clone();
            async move { server.run(ctk).await }
        });
        let _first = TcpStream::connect(addr).await.unwrap();
        wait_for_permits(&sessions, 1, total).await;
        let mut second = TcpStream::connect(addr).await.unwrap();
        let mut buf = [0u8; 1];
        let closed = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::io::AsyncReadExt::read(&mut second, &mut buf),
        )
        .await
        .expect("a second connection from one address was kept");
        assert!(matches!(closed, Ok(0) | Err(_)));
        ctk.cancel();
    }

    /// The service stops while a session waits for the user: the session ends as `Cancelled`
    /// (the app sees it), its permit is back, and `run` is over within the wind-down bound.
    #[tokio::test]
    async fn stop_ends_a_lan_session_waiting_for_consent() {
        let _guard = SERIAL.lock().await;
        *crate::WIFI_LINK.write().unwrap() = None;
        // The receiver.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (bus, mut watch) = tokio::sync::broadcast::channel(256);
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let mut server = TcpServer::new(*b"TEST", listener, bus, rx).unwrap();
        let sessions = server.sessions.clone();
        let total = sessions.available_permits();
        let ctk = CancellationToken::new();
        let run = tokio::spawn({
            let ctk = ctk.clone();
            async move { server.run(ctk).await }
        });

        // A sender with its own bus offers a text to it.
        let sender_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (sender_bus, _keep) = tokio::sync::broadcast::channel(256);
        let (_stx, srx) = tokio::sync::mpsc::channel(1);
        let sender = TcpServer::new(*b"SEND", sender_listener, sender_bus, srx).unwrap();
        let mut si = ble_info(&addr.to_string());
        si.addr = addr.to_string();
        si.ob = OutboundPayload::Text {
            text: "hello".into(),
            text_type: crate::TextPayloadType::Text,
        };
        let send = tokio::spawn(async move { sender.connect(CancellationToken::new(), si).await });

        let mut states = vec![];
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let message = watch.recv().await.unwrap();
                if state_of(message) == Some(TransferState::WaitingForUserConsent) {
                    break;
                }
            }
        })
        .await
        .expect("the session never asked for consent");
        assert_eq!(sessions.available_permits() + 1, total);

        ctk.cancel();
        tokio::time::timeout(STOP_WIND_DOWN + Duration::from_secs(2), run)
            .await
            .expect("run did not end after the stop")
            .unwrap()
            .unwrap();
        assert_eq!(sessions.available_permits(), total, "the permit is held");
        while let Ok(message) = watch.try_recv() {
            states.push(state_of(message));
        }
        assert!(
            states.contains(&Some(TransferState::Cancelled)),
            "{states:?}"
        );
        send.abort();
    }
}
