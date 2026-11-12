use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use once_cell::sync::Lazy;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::channel::{self, ChannelMessage, TransferAction};
use crate::hdl::{
    InboundRequest, MigratableStream, OutboundPayload, OutboundRequest, TransferState,
};
use crate::location_nearby_connections::KeepAliveFrame;
use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::upgrade_path_info::WifiDirectCredentials;
use crate::manager::{SendInfo, TcpServer};
use crate::utils::RemoteDeviceInfo;

fn creds() -> WifiCredentials {
    WifiCredentials {
        ssid: "DIRECT-ab".into(),
        password: "secret123".into(),
        port: 40000,
        frequency_mhz: 5180,
        gateway: Ipv4Addr::new(192, 168, 49, 1),
    }
}

#[test]
fn credentials_survive_the_wire() {
    let frame = path_available_frame(&creds(), false);
    let decoded = OfflineFrame::decode(&*frame.encode_to_vec()).unwrap();
    let bwu = parse_bwu(&decoded).unwrap();
    assert_eq!(bwu.event_type(), EventType::UpgradePathAvailable);
    let info = bwu.upgrade_path_info.as_ref().unwrap();
    assert_eq!(info.medium(), Medium::WifiHotspot);
    assert!(info.supports_client_introduction_ack());
    assert_eq!(info_to_credentials(info).unwrap(), creds());
}

#[test]
fn offers_that_cannot_be_followed_are_rejected() {
    let mut info = credentials_to_info(&creds());
    info.medium = Some(Medium::WifiLan.into());
    assert!(info_to_credentials(&info).is_err());

    let mut info = credentials_to_info(&creds());
    info.wifi_hotspot_credentials = None;
    assert!(info_to_credentials(&info).is_err());

    for bad in [0, -1, 70000] {
        let mut info = credentials_to_info(&creds());
        info.wifi_hotspot_credentials.as_mut().unwrap().port = Some(bad);
        assert!(info_to_credentials(&info).is_err(), "port {bad}");
    }

    let mut info = credentials_to_info(&creds());
    info.wifi_hotspot_credentials.as_mut().unwrap().gateway = Some("not-an-ip".into());
    assert!(info_to_credentials(&info).is_err());
}

#[test]
fn unknown_frequency_maps_to_zero() {
    let mut info = credentials_to_info(&creds());
    info.wifi_hotspot_credentials.as_mut().unwrap().frequency = Some(-1);
    assert_eq!(info_to_credentials(&info).unwrap().frequency_mhz, 0);
}

#[test]
fn plain_frames_have_the_expected_shape() {
    let intro = client_introduction_frame("ABCD", false);
    let bwu = parse_bwu(&intro).unwrap();
    assert_eq!(bwu.event_type(), EventType::ClientIntroduction);
    assert_eq!(
        bwu.client_introduction.as_ref().unwrap().endpoint_id(),
        "ABCD"
    );

    let ack = client_introduction_ack_frame();
    assert_eq!(
        parse_bwu(&ack).unwrap().event_type(),
        EventType::ClientIntroductionAck
    );
    assert!(parse_bwu(&plain_disconnection_frame()).is_none());
    assert_eq!(
        parse_bwu(&event_frame(EventType::SafeToClosePriorChannel))
            .unwrap()
            .event_type(),
        EventType::SafeToClosePriorChannel
    );
}

#[test]
fn offers_and_introductions_carry_the_encryption_flag() {
    let wire = |f: &OfflineFrame| parse_bwu(f).unwrap().clone();
    let default_offer = wire(&path_available_frame(&creds(), false));
    let info = default_offer.upgrade_path_info.unwrap();
    assert_eq!(info.supports_disabling_encryption, None);
    assert!(!info.supports_disabling_encryption());

    let offer = wire(&path_available_frame(&creds(), true));
    assert_eq!(
        offer
            .upgrade_path_info
            .unwrap()
            .supports_disabling_encryption,
        Some(true)
    );

    for flag in [false, true] {
        let intro = wire(&client_introduction_frame("ABCD", flag));
        assert_eq!(
            intro
                .client_introduction
                .unwrap()
                .supports_disabling_encryption,
            Some(flag)
        );
    }
}

#[test]
fn hotspot_is_only_skipped_when_the_peer_lists_other_mediums() {
    use crate::location_nearby_connections::connection_request_frame::Medium as M;
    assert!(peer_allows_hotspot(&[]));
    assert!(peer_allows_hotspot(&[
        M::WifiLan as i32,
        M::WifiHotspot as i32
    ]));
    assert!(!peer_allows_hotspot(&[
        M::WifiLan as i32,
        M::Bluetooth as i32
    ]));
}

#[test]
fn lan_offer_carries_an_ipv4_address_and_ephemeral_port() {
    let ip = Ipv4Addr::new(192, 168, 12, 5);
    let frame = lan_path_available_frame(ip, 45678, false);
    let bwu = parse_bwu(&frame).unwrap();
    assert_eq!(bwu.event_type(), EventType::UpgradePathAvailable);
    let info = bwu.upgrade_path_info.as_ref().unwrap();
    assert_eq!(info.medium(), Medium::WifiLan);
    assert!(info.supports_client_introduction_ack());
    assert_eq!(
        info.wifi_lan_socket.as_ref().unwrap().ip_address.as_deref(),
        Some(ip.octets().as_slice())
    );
    assert_eq!(
        info.wifi_lan_socket.as_ref().unwrap().wifi_port,
        Some(45678)
    );
    assert_eq!(info.supports_disabling_encryption, None);
    assert_eq!(
        parse_bwu(&lan_path_available_frame(ip, 45678, true))
            .unwrap()
            .upgrade_path_info
            .as_ref()
            .unwrap()
            .supports_disabling_encryption,
        Some(true)
    );
}

#[tokio::test]
async fn inbound_prefers_lan_then_hotspot_then_ble() {
    let with_lan = Arc::new(FakeLink {
        lan_address: Some(Ipv4Addr::LOCALHOST),
        ..Default::default()
    });
    let result = prepare_inbound(with_lan.clone(), true, true, Some(false))
        .await
        .unwrap();
    let Prepared::HostedLan(Ipv4Addr::LOCALHOST, listener) = result else {
        panic!("expected a LAN offer");
    };
    // The listener is bound to the offered address and the link was asked for its port.
    let bound_to = listener.local_addr().unwrap();
    assert_eq!(bound_to.ip(), std::net::IpAddr::from(Ipv4Addr::LOCALHOST));
    assert_eq!(*with_lan.lan_ports.lock().unwrap(), [bound_to.port()]);
    assert_eq!(with_lan.hosts.load(Ordering::SeqCst), 0);
    assert_eq!(with_lan.releases.load(Ordering::SeqCst), 0);

    // No LAN address: the hotspot, 2.4 GHz for a phone without 5 GHz.
    let hotspot = Arc::new(FakeLink::default());
    let result = prepare_inbound(hotspot.clone(), true, true, Some(false))
        .await
        .unwrap();
    assert!(matches!(result, Prepared::Hosted(_, _)));
    assert_eq!(hotspot.lan_hosts.load(Ordering::SeqCst), 0);
    assert_eq!(hotspot.hosts.load(Ordering::SeqCst), 1);
    assert!(hotspot.hosted_2ghz.load(Ordering::SeqCst));

    // Neither: stays on BLE.
    assert!(
        prepare_inbound(hotspot.clone(), true, false, None)
            .await
            .is_err()
    );
    assert_eq!(hotspot.hosts.load(Ordering::SeqCst), 1);
    // The peer does not list WIFI_LAN: the LAN is not even asked about.
    assert!(matches!(
        prepare_inbound(with_lan.clone(), false, true, None)
            .await
            .unwrap(),
        Prepared::Hosted(..)
    ));
    assert_eq!(with_lan.lan_hosts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_refused_or_failed_lan_port_goes_to_the_hotspot() {
    // The zone refuses: nothing was changed, so nothing is released.
    let refusing = Arc::new(FakeLink {
        lan_address: Some(Ipv4Addr::LOCALHOST),
        lan_refuses: true,
        ..Default::default()
    });
    let result = prepare_inbound(refusing.clone(), true, true, None)
        .await
        .unwrap();
    assert!(matches!(result, Prepared::Hosted(..)));
    assert_eq!(refusing.hosts.load(Ordering::SeqCst), 1);
    assert_eq!(refusing.releases.load(Ordering::SeqCst), 0);

    // An error: whatever was half done is released before the hotspot.
    let failing = Arc::new(FakeLink {
        lan_address: Some(Ipv4Addr::LOCALHOST),
        lan_fails: true,
        ..Default::default()
    });
    let result = prepare_inbound(failing.clone(), true, true, None)
        .await
        .unwrap();
    assert!(matches!(result, Prepared::Hosted(..)));
    assert_eq!(failing.hosts.load(Ordering::SeqCst), 1);
    assert_eq!(failing.releases.load(Ordering::SeqCst), 1);

    // Without the hotspot the session stays on BLE.
    let stay = Arc::new(FakeLink {
        lan_address: Some(Ipv4Addr::LOCALHOST),
        lan_fails: true,
        ..Default::default()
    });
    assert!(
        prepare_inbound(stay.clone(), true, false, None)
            .await
            .is_err()
    );
    assert_eq!(stay.hosts.load(Ordering::SeqCst), 0);
}

/// A receiver state that has prepared a LAN offer and waits for the peer on it.
async fn waiting_on_lan(link: &Arc<FakeLink>, mediums: &[i32]) -> UpgradeState {
    let mut state = UpgradeState::new(Some(link.clone()));
    state.set_peer_mediums(mediums);
    let (lan, hotspot, peer_5ghz) = state.peer_upgrade_options();
    state.begin_preparing(tokio::spawn(prepare_inbound(
        link.clone(),
        lan,
        hotspot,
        peer_5ghz,
    )));
    let UpgradeEvent::Prepared(Ok(Prepared::HostedLan(ip, listener))) = state.next_event().await
    else {
        panic!("expected a LAN offer");
    };
    state.begin_waiting_lan(tokio::spawn(accept_introduced(
        listener,
        "WXYZ".into(),
        ip,
        false,
    )));
    state
}

fn lan_and_hotspot() -> [i32; 2] {
    use crate::location_nearby_connections::connection_request_frame::Medium as M;
    [M::WifiLan as i32, M::WifiHotspot as i32]
}

#[tokio::test]
async fn the_lan_accept_window_is_short() {
    let link = Arc::new(FakeLink {
        lan_address: Some(Ipv4Addr::LOCALHOST),
        ..Default::default()
    });
    let state = waiting_on_lan(&link, &lan_and_hotspot()).await;
    let left = state.deadline.unwrap() - Instant::now();
    assert!(left <= LAN_ACCEPT_TIMEOUT && LAN_ACCEPT_TIMEOUT < ACCEPT_TIMEOUT);
    assert!(left > LAN_ACCEPT_TIMEOUT - Duration::from_secs(2));
}

#[tokio::test]
async fn a_lan_accept_timeout_offers_the_hotspot_once() {
    let link = Arc::new(FakeLink {
        lan_address: Some(Ipv4Addr::LOCALHOST),
        ..Default::default()
    });
    let mut state = waiting_on_lan(&link, &lan_and_hotspot()).await;
    assert_eq!(link.hosts.load(Ordering::SeqCst), 0);

    state.deadline = Some(Instant::now());
    assert!(matches!(
        state.next_event().await,
        UpgradeEvent::WaitTimedOut
    ));
    state.fail().await;
    // The LAN lease is released, then the hotspot is prepared.
    assert_eq!(link.releases.load(Ordering::SeqCst), 1);
    assert!(state.in_progress());
    let UpgradeEvent::Prepared(Ok(Prepared::Hosted(_, listener))) = state.next_event().await else {
        panic!("expected a hotspot offer");
    };
    assert_eq!(link.hosts.load(Ordering::SeqCst), 1);
    assert_eq!(link.lan_hosts.load(Ordering::SeqCst), 1);
    state.begin_waiting(tokio::spawn(accept_introduced(
        listener,
        "WXYZ".into(),
        LO,
        false,
    )));
    assert!(
        state
            .deadline
            .is_some_and(|d| d > Instant::now() + LAN_ACCEPT_TIMEOUT)
    );

    // The hotspot failing in turn ends the upgrade: no third offer.
    state.fail().await;
    assert_eq!(link.releases.load(Ordering::SeqCst), 2);
    assert!(!state.in_progress());
    assert!(!state.can_start());
    assert_eq!(link.hosts.load(Ordering::SeqCst), 1);
    assert_eq!(link.lan_hosts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_peer_failure_after_the_lan_offer_offers_the_hotspot() {
    let link = Arc::new(FakeLink {
        lan_address: Some(Ipv4Addr::LOCALHOST),
        ..Default::default()
    });
    let mut state = waiting_on_lan(&link, &lan_and_hotspot()).await;
    assert!(state.is_waiting());
    state.fail().await;
    assert!(matches!(
        state.next_event().await,
        UpgradeEvent::Prepared(Ok(Prepared::Hosted(..)))
    ));
    assert_eq!(link.hosts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_failed_lan_offer_stays_on_ble_when_the_hotspot_is_not_allowed() {
    use crate::location_nearby_connections::connection_request_frame::Medium as M;
    let link = Arc::new(FakeLink {
        lan_address: Some(Ipv4Addr::LOCALHOST),
        ..Default::default()
    });
    let mut state = waiting_on_lan(&link, &[M::WifiLan as i32]).await;
    state.fail().await;
    assert!(!state.in_progress());
    assert!(!state.can_start());
    assert_eq!(link.hosts.load(Ordering::SeqCst), 0);
    assert_eq!(link.releases.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_connected_lan_peer_closes_the_port_early_and_a_hotspot_never_does() {
    let link = Arc::new(FakeLink {
        lan_address: Some(Ipv4Addr::LOCALHOST),
        ..Default::default()
    });
    let mut state = waiting_on_lan(&link, &lan_and_hotspot()).await;
    state.close_lan_port().await;
    assert_eq!(link.lan_closes.load(Ordering::SeqCst), 1);

    let hotspot = Arc::new(FakeLink::default());
    let mut state = UpgradeState::new(Some(hotspot.clone()));
    state.close_lan_port().await;
    assert_eq!(hotspot.lan_closes.load(Ordering::SeqCst), 0);
}

#[test]
fn lan_is_only_offered_for_explicit_peer_capability() {
    use crate::location_nearby_connections::connection_request_frame::Medium as M;
    assert!(!peer_allows_lan(&[]));
    assert!(!peer_allows_lan(&[M::WifiHotspot as i32]));
    assert!(peer_allows_lan(&[M::WifiLan as i32]));
    let mut state = UpgradeState::new(Some(Arc::new(FakeLink::default())));
    state.set_peer_mediums(&[M::WifiLan as i32]);
    state.set_peer_5ghz(Some(false));
    assert!(state.can_start());
    assert_eq!(state.peer_upgrade_options(), (true, false, Some(false)));
    state.set_peer_mediums(&[M::Bluetooth as i32]);
    assert!(!state.can_start());
}

#[test]
fn handover_needs_both_peer_events() {
    let mut h = Handover::default();
    assert!(!h.is_complete());
    // SAFE_TO_CLOSE alone must not allow closing: the peer may still be writing.
    assert_eq!(
        h.on_peer_event(EventType::SafeToClosePriorChannel),
        Reply::Nothing
    );
    assert!(!h.is_complete());
    assert!(!h.peer_last_write());
    assert_eq!(
        h.on_peer_event(EventType::LastWriteToPriorChannel),
        Reply::SendSafeToClose
    );
    assert!(h.is_complete());
}

#[tokio::test]
async fn peer_events_before_the_new_stream_is_ready_are_kept() {
    let mut state = UpgradeState::new(None);
    state.begin_preparing(tokio::spawn(std::future::pending()));
    assert_eq!(
        state.on_peer_event(EventType::LastWriteToPriorChannel),
        Some(Reply::SendSafeToClose)
    );
    state.on_peer_event(EventType::SafeToClosePriorChannel);
    let (listener, port) = bound().await;
    let stream = TcpStream::connect((LO, port)).await.unwrap();
    drop(listener);
    state.begin_draining(stream);
    assert!(state.is_complete());
}

#[test]
fn handover_in_the_usual_order() {
    let mut h = Handover::default();
    assert_eq!(
        h.on_peer_event(EventType::LastWriteToPriorChannel),
        Reply::SendSafeToClose
    );
    assert!(!h.is_complete());
    h.on_peer_event(EventType::SafeToClosePriorChannel);
    assert!(h.is_complete());
    // Unrelated events change nothing.
    assert_eq!(h.on_peer_event(EventType::UpgradeFailure), Reply::Nothing);
}

/// Bytes seen by a `FakeLink` tap: (client to host, host to client).
pub(crate) type Tap = Arc<std::sync::Mutex<(Vec<u8>, Vec<u8>)>>;

/// Sends a plain keep-alive (no ack) to the handler under test.
pub(crate) async fn write_plain_keep_alive<W: AsyncWrite + Unpin>(peer: &mut W) {
    let frame = OfflineFrame {
        version: Some(offline_frame::Version::V1.into()),
        v1: Some(V1Frame {
            r#type: Some(v1_frame::FrameType::KeepAlive.into()),
            keep_alive: Some(KeepAliveFrame { ack: Some(false) }),
            ..Default::default()
        }),
    };
    let data = frame.encode_to_vec();
    peer.write_all(&(data.len() as u32).to_be_bytes())
        .await
        .unwrap();
    peer.write_all(&data).await.unwrap();
}

/// Asserts that `frame` is a plain keep-alive acknowledgement.
pub(crate) fn assert_keep_alive_ack(frame: &OfflineFrame) {
    let v1 = frame.v1.as_ref().unwrap();
    assert_eq!(v1.r#type(), v1_frame::FrameType::KeepAlive);
    assert!(v1.keep_alive.as_ref().unwrap().ack());
}

#[derive(Default)]
pub(crate) struct FakeLink {
    /// `host` advertises a forwarder in front of the real port that records the traffic.
    pub tap: Option<Tap>,
    pub host_fails: bool,
    /// `host` answers like a link that serves another session already.
    pub host_busy: bool,
    pub join_fails: bool,
    pub host_delay: Duration,
    pub lan_address: Option<Ipv4Addr>,
    /// `host_lan` answers `Ok(false)`: the zone refuses the port.
    pub lan_refuses: bool,
    pub lan_fails: bool,
    pub lan_hosts: AtomicUsize,
    /// The ports `host_lan` was asked to open.
    pub lan_ports: std::sync::Mutex<Vec<u16>>,
    pub lan_closes: AtomicUsize,
    pub hosted_2ghz: AtomicBool,
    pub capability: Option<bool>,
    pub hosts: AtomicUsize,
    pub joins: AtomicUsize,
    pub releases: AtomicUsize,
}

impl WifiLink for FakeLink {
    fn supports_5ghz(&self) -> BoxFuture<'_, Option<bool>> {
        Box::pin(async move { self.capability })
    }

    fn lan_address(&self) -> BoxFuture<'_, Option<Ipv4Addr>> {
        Box::pin(async move { self.lan_address })
    }

    fn host_lan(&self, _ip: Ipv4Addr, port: u16) -> BoxFuture<'_, io::Result<bool>> {
        Box::pin(async move {
            self.lan_hosts.fetch_add(1, Ordering::SeqCst);
            self.lan_ports.lock().unwrap().push(port);
            if self.lan_fails {
                return Err(io::Error::other("no LAN port"));
            }
            Ok(!self.lan_refuses)
        })
    }

    fn close_lan(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.lan_closes.fetch_add(1, Ordering::SeqCst);
        })
    }

    fn host(
        &self,
        port: u16,
        peer_5ghz: Option<bool>,
    ) -> BoxFuture<'_, io::Result<WifiCredentials>> {
        Box::pin(async move {
            self.hosted_2ghz
                .store(peer_5ghz == Some(false), Ordering::SeqCst);
            self.hosts.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.host_delay).await;
            if self.host_busy {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            if self.host_fails {
                return Err(io::Error::other("no hotspot"));
            }
            let port = match &self.tap {
                Some(tap) => spawn_tap(port, tap.clone()).await,
                None => port,
            };
            Ok(WifiCredentials {
                ssid: "loopback".into(),
                password: "password123".into(),
                port,
                frequency_mhz: 5180,
                gateway: Ipv4Addr::LOCALHOST,
            })
        })
    }

    fn join<'a>(&'a self, _creds: &'a WifiCredentials) -> BoxFuture<'a, io::Result<Ipv4Addr>> {
        Box::pin(async move {
            self.joins.fetch_add(1, Ordering::SeqCst);
            if self.join_fails {
                return Err(io::Error::other("no such network"));
            }
            Ok(Ipv4Addr::LOCALHOST)
        })
    }

    fn release(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.releases.fetch_add(1, Ordering::SeqCst);
        })
    }
}

const LO: Ipv4Addr = Ipv4Addr::LOCALHOST;

/// Forwards one connection to `target` and records both directions; returns the listen port.
async fn spawn_tap(target: u16, tap: Tap) -> u16 {
    async fn pump(mut from: OwnedReadHalf, mut to: OwnedWriteHalf, tap: Tap, up: bool) {
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n) = from.read(&mut buf).await {
            if n == 0 {
                break;
            }
            {
                let mut t = tap.lock().unwrap();
                let side = if up { &mut t.0 } else { &mut t.1 };
                side.extend_from_slice(&buf[..n]);
            }
            if to.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
        let _ = to.shutdown().await;
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (a, _) = listener.accept().await.unwrap();
        let b = TcpStream::connect(("127.0.0.1", target)).await.unwrap();
        let (ar, aw) = a.into_split();
        let (br, bw) = b.into_split();
        tokio::join!(
            pump(ar, bw, tap.clone(), true),
            pump(br, aw, tap.clone(), false)
        );
    });
    port
}

/// Splits `[4-byte BE length][frame]` records; panics on a truncated tail.
fn split_frames(mut data: &[u8]) -> Vec<&[u8]> {
    let mut frames = Vec::new();
    while !data.is_empty() {
        let len = u32::from_be_bytes(data[..4].try_into().unwrap()) as usize;
        frames.push(&data[4..4 + len]);
        data = &data[4 + len..];
    }
    frames
}

async fn bound() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

fn client(port: u16, endpoint: &'static str) -> JoinHandle<anyhow::Result<TcpStream>> {
    tokio::spawn(connect_introduced(
        SocketAddrV4::new(LO, port),
        LO,
        endpoint,
        true,
        false,
    ))
}

#[tokio::test]
async fn introduction_over_loopback_ignores_strangers() {
    let (listener, port) = bound().await;
    let server = tokio::spawn(accept_introduced(listener, "WXYZ".into(), LO, false));

    // Junk must not consume the upgrade.
    let mut junk = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    junk.write_all(&[0, 0, 0, 3, 1, 2, 3]).await.unwrap();

    // A well-formed introduction from the wrong endpoint must not either.
    let intruder = client(port, "EVIL");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!server.is_finished());

    let client = client(port, "WXYZ");
    let mut accepted = server.await.unwrap().unwrap();
    assert!(!client.is_finished(), "client must wait for the ACK");
    write_plain_frame(&mut accepted, &client_introduction_ack_frame())
        .await
        .unwrap();
    client.await.unwrap().unwrap();
    // The intruder never gets an ACK; its connection is closed.
    assert!(intruder.await.unwrap().is_err());
}

#[tokio::test]
async fn three_wrong_introductions_end_the_wait() {
    let (listener, port) = bound().await;
    let server = tokio::spawn(accept_introduced(listener, "WXYZ".into(), LO, false));
    let intruders: Vec<_> = (0..MAX_WRONG_INTRO).map(|_| client(port, "EVIL")).collect();
    let err = server.await.unwrap().unwrap_err();
    assert!(
        err.to_string().contains("too many wrong introductions"),
        "{err}"
    );
    for intruder in intruders {
        assert!(intruder.await.unwrap().is_err());
    }
}

#[tokio::test]
async fn the_right_peer_still_wins_after_two_wrong_introductions() {
    let (listener, port) = bound().await;
    let server = tokio::spawn(accept_introduced(listener, "WXYZ".into(), LO, false));
    for _ in 0..MAX_WRONG_INTRO - 1 {
        // The server closes a wrong connection without an ACK: the client's connect fails.
        assert!(client(port, "EVIL").await.unwrap().is_err());
    }
    let client = client(port, "WXYZ");
    let mut accepted = server.await.unwrap().unwrap();
    write_plain_frame(&mut accepted, &client_introduction_ack_frame())
        .await
        .unwrap();
    client.await.unwrap().unwrap();
}

#[tokio::test]
async fn junk_and_silent_connections_do_not_count_as_wrong_introductions() {
    let (listener, port) = bound().await;
    let server = tokio::spawn(accept_introduced(listener, "WXYZ".into(), LO, false));
    let mut junk = Vec::new();
    for _ in 0..MAX_WRONG_INTRO + 1 {
        let mut c = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        c.write_all(&[0, 0, 0, 3, 1, 2, 3]).await.unwrap();
        junk.push(c);
    }
    let client = client(port, "WXYZ");
    let mut accepted = server.await.unwrap().unwrap();
    write_plain_frame(&mut accepted, &client_introduction_ack_frame())
        .await
        .unwrap();
    client.await.unwrap().unwrap();
}

#[tokio::test]
async fn the_host_keeps_its_own_encryption_choice_when_the_client_differs() {
    for offered in [true, false] {
        let (listener, port) = bound().await;
        let server = tokio::spawn(accept_introduced(listener, "WXYZ".into(), LO, offered));
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        write_plain_frame(&mut stream, &client_introduction_frame("WXYZ", !offered))
            .await
            .unwrap();
        // Accepted anyway; the caller keeps what it offered.
        assert!(server.await.unwrap().is_ok());
    }
}

#[tokio::test]
async fn connections_to_another_address_are_ignored() {
    let (listener, port) = bound().await;
    let server = tokio::spawn(accept_introduced(
        listener,
        "WXYZ".into(),
        Ipv4Addr::new(192, 168, 49, 1),
        false,
    ));
    let _client = client(port, "WXYZ");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!server.is_finished());
}

#[tokio::test]
async fn a_silent_connection_does_not_delay_the_peer() {
    let (listener, port) = bound().await;
    let server = tokio::spawn(accept_introduced(listener, "WXYZ".into(), LO, false));
    let _silent = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let started = std::time::Instant::now();
    let _client = client(port, "WXYZ");
    server.await.unwrap().unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn closing_gracefully_keeps_unread_data() {
    let (listener, port) = bound().await;
    let mut peer = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let (mut ours, _) = listener.accept().await.unwrap();
    ours.write_all(&[7u8; 100_000]).await.unwrap();
    let closing = tokio::spawn(async move { close_gracefully(&mut ours).await });
    let mut got = Vec::new();
    peer.read_to_end(&mut got).await.unwrap();
    assert_eq!(got.len(), 100_000);
    drop(peer);
    closing.await.unwrap();
}

#[tokio::test]
async fn pending_setup_does_not_block_the_event_loop() {
    let link = Arc::new(FakeLink {
        host_delay: Duration::from_secs(5),
        ..Default::default()
    });
    let mut state = UpgradeState::new(Some(link.clone()));
    let (listener, _) = bound().await;
    state.begin_preparing(tokio::spawn(prepare_host(link.clone(), listener, None)));
    assert!(!state.can_start());
    assert!(
        timeout(Duration::from_millis(200), state.next_event())
            .await
            .is_err()
    );
    // Giving up cancels the task and releases the link.
    state.fail().await;
    assert_eq!(link.releases.load(Ordering::SeqCst), 1);
}

#[test]
fn offers_with_implausible_credentials_are_rejected() {
    let reject = |edit: &dyn Fn(&mut WifiHotspotCredentials)| {
        let mut info = credentials_to_info(&creds());
        edit(info.wifi_hotspot_credentials.as_mut().unwrap());
        assert!(info_to_credentials(&info).is_err());
    };
    reject(&|c| c.ssid = Some(String::new()));
    reject(&|c| c.ssid = Some("x".repeat(33)));
    reject(&|c| c.ssid = Some("-oProxyCommand".into()));
    reject(&|c| c.password = Some("short".into()));
    reject(&|c| c.password = Some("p".repeat(64)));
    reject(&|c| c.gateway = Some("8.8.8.8".into()));
    reject(&|c| c.gateway = Some("0.0.0.0".into()));

    // Outside tests loopback is refused too.
    assert!(!gateway_is_acceptable(Ipv4Addr::LOCALHOST, false));
    assert!(gateway_is_acceptable(Ipv4Addr::new(10, 0, 0, 1), false));
    assert!(gateway_is_acceptable(Ipv4Addr::new(169, 254, 1, 1), false));
}

fn direct_info() -> UpgradePathInfo {
    UpgradePathInfo {
        medium: Some(Medium::WifiDirect.into()),
        wifi_direct_credentials: Some(WifiDirectCredentials {
            ssid: Some("DIRECT-ab".into()),
            password: Some("secret123".into()),
            port: Some(40000),
            frequency: Some(5220),
            gateway: Some("192.168.49.1".into()),
        }),
        ..Default::default()
    }
}

#[test]
fn wifi_direct_offers_map_to_the_same_credentials() {
    let c = info_to_credentials(&direct_info()).unwrap();
    assert_eq!(c.ssid, "DIRECT-ab");
    assert_eq!(c.password, "secret123");
    assert_eq!((c.port, c.frequency_mhz), (40000, 5220));
    assert_eq!(c.gateway, Ipv4Addr::new(192, 168, 49, 1));

    let mut info = direct_info();
    info.wifi_direct_credentials.as_mut().unwrap().frequency = Some(-1);
    assert_eq!(info_to_credentials(&info).unwrap().frequency_mhz, 0);
}

#[test]
fn implausible_wifi_direct_offers_are_rejected() {
    let reject = |edit: &dyn Fn(&mut WifiDirectCredentials)| {
        let mut info = direct_info();
        edit(info.wifi_direct_credentials.as_mut().unwrap());
        assert!(info_to_credentials(&info).is_err());
    };
    reject(&|c| c.ssid = Some(String::new()));
    reject(&|c| c.ssid = Some("x".repeat(33)));
    reject(&|c| c.ssid = Some("-oProxyCommand".into()));
    reject(&|c| c.password = Some("short".into()));
    reject(&|c| c.password = Some("p".repeat(64)));
    reject(&|c| c.gateway = Some("8.8.8.8".into()));
    reject(&|c| c.gateway = Some("0.0.0.0".into()));
    reject(&|c| c.gateway = None);
    reject(&|c| c.port = Some(0));
    reject(&|c| c.port = Some(70000));
    reject(&|c| c.port = None);

    let mut info = direct_info();
    info.wifi_direct_credentials = None;
    assert!(info_to_credentials(&info).is_err());
    // Credentials of the wrong kind do not count.
    let mut info = direct_info();
    info.medium = Some(Medium::WifiHotspot.into());
    assert!(info_to_credentials(&info).is_err());
}

#[test]
fn credentials_debug_hides_the_password() {
    let shown = format!("{:?}", creds());
    assert!(!shown.contains("secret123"));
    assert!(shown.contains("DIRECT-ab"));
}

#[tokio::test]
async fn payload_is_released_when_no_offer_arrives() {
    let link: Arc<dyn WifiLink> = Arc::new(FakeLink::default());
    let mut state = UpgradeState::new(Some(link));
    assert!(state.needs_hold());
    state.hold_payload();
    assert!(!state.take_hold_released());
    assert!(
        state
            .deadline
            .is_some_and(|d| d > Instant::now() + Duration::from_secs(5))
    );
    state.deadline = Some(Instant::now());
    assert!(matches!(
        state.next_event().await,
        UpgradeEvent::OfferTimedOut
    ));
    assert!(state.take_hold_released());
    assert!(!state.take_hold_released());
    // Late offers are ignored.
    assert!(!state.can_start());
}

#[tokio::test]
async fn disabled_upgrade_never_holds_or_starts() {
    let state = UpgradeState::new(None);
    assert!(!state.enabled());
    assert!(!state.needs_hold());
    assert!(!state.can_start());
}

#[tokio::test]
async fn failure_releases_the_link_once_used() {
    let link = Arc::new(FakeLink::default());
    let mut state = UpgradeState::new(Some(link.clone()));
    state.fail().await;
    assert_eq!(link.releases.load(Ordering::SeqCst), 0);

    let mut state = UpgradeState::new(Some(link.clone()));
    state.mark_link_used();
    state.fail().await;
    assert_eq!(link.releases.load(Ordering::SeqCst), 1);
    state.fail().await;
    assert_eq!(link.releases.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_refused_session_does_not_release_the_other_sessions_link() {
    for (busy, expected_releases) in [(true, 0), (false, 1)] {
        let link = Arc::new(FakeLink {
            host_busy: busy,
            host_fails: !busy,
            ..Default::default()
        });
        let mut state = UpgradeState::new(Some(link.clone()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        state.begin_preparing(tokio::spawn(prepare_host(link.clone(), listener, None)));
        assert!(matches!(
            state.next_event().await,
            UpgradeEvent::Prepared(Err(_))
        ));
        state.fail().await;
        assert_eq!(link.releases.load(Ordering::SeqCst), expected_releases);
    }
}

// --- Whole transfer between our own receiver and sender over loopback ------------------

/// The upgrade link and download directory are process-wide.
pub(crate) static SERIAL: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

const FILE_SIZE: usize = 3 * 1024 * 1024;

/// Forwards one connection and reports the bytes (sender to receiver, receiver to sender).
/// Errors such as resets at close only end the copy; the bytes counted so far stay valid.
fn spawn_proxy(target: SocketAddr, listener: TcpListener) -> JoinHandle<(u64, u64)> {
    async fn pump(mut from: OwnedReadHalf, mut to: OwnedWriteHalf) -> u64 {
        let mut total = 0;
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n) = from.read(&mut buf).await {
            if n == 0 || to.write_all(&buf[..n]).await.is_err() {
                break;
            }
            total += n as u64;
        }
        let _ = to.shutdown().await;
        total
    }

    tokio::spawn(async move {
        let (a, _) = listener.accept().await.unwrap();
        let b = TcpStream::connect(target).await.unwrap();
        let (ar, aw) = a.into_split();
        let (br, bw) = b.into_split();
        tokio::join!(pump(ar, bw), pump(br, aw))
    })
}

type Outcome = ((u64, u64), Vec<u8>, Vec<u8>);

/// Runs `try_transfer` again when the UKEY2 handshake fails. That happens for about 1 in 70
/// sessions because the lib mis-encodes public keys with a leading zero byte (upstream bug,
/// unrelated to the upgrade).
async fn transfer(link: Option<Arc<FakeLink>>) -> Outcome {
    for _ in 0..5 {
        if let Some(outcome) = try_transfer(link.clone()).await {
            return outcome;
        }
    }
    panic!("transfer kept aborting");
}

/// Sends a file through a proxy and returns what the proxy saw and the received file.
async fn try_transfer(link: Option<Arc<FakeLink>>) -> Option<Outcome> {
    let _guard = SERIAL.lock().await;
    let dyn_link: Option<Arc<dyn WifiLink>> = link.map(|l| l as Arc<dyn WifiLink>);
    *crate::WIFI_LINK.write().unwrap() = dyn_link;

    let dir = std::env::temp_dir().join(format!("rqs_upgrade_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (src_dir, dst_dir) = (dir.join("src"), dir.join("dst"));
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::create_dir_all(&dst_dir).unwrap();
    *crate::CUSTOM_DOWNLOAD.write().unwrap() = Some(dst_dir.clone());

    let data: Vec<u8> = (0..FILE_SIZE).map(|i| (i * 31 % 251) as u8).collect();
    let src = src_dir.join("payload.bin");
    std::fs::write(&src, &data).unwrap();

    // Receiver
    let (rx_bus, mut rx_events) = broadcast::channel::<ChannelMessage>(256);
    let rx_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rx_addr = rx_listener.local_addr().unwrap();
    let (_unused_tx, unused_rx) = mpsc::channel(1);
    let mut receiver = TcpServer::new(*b"RECV", rx_listener, rx_bus.clone(), unused_rx).unwrap();
    let ctk = CancellationToken::new();
    let rctk = ctk.clone();
    tokio::spawn(async move { receiver.run(rctk).await });

    // Sender, reaching the receiver through a counting proxy
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let proxy = spawn_proxy(rx_addr, proxy_listener);
    let (tx_bus, _tx_events) = broadcast::channel::<ChannelMessage>(256);
    let (_unused_tx2, unused_rx2) = mpsc::channel(1);
    let tx_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sender = TcpServer::new(*b"SEND", tx_listener, tx_bus, unused_rx2).unwrap();
    let sctk = ctk.clone();
    let send = tokio::spawn(async move {
        sender
            .connect(
                sctk,
                SendInfo {
                    id: "sender".into(),
                    name: "test".into(),
                    addr: proxy_addr.to_string(),
                    ob: OutboundPayload::Files(vec![src.to_string_lossy().into_owned()]),
                },
            )
            .await
    });

    // Accept on the receiving side, then wait for the end of the transfer.
    let run = async {
        loop {
            let msg = rx_events.recv().await.unwrap();
            let Some(client) = msg.msg.as_client() else {
                continue;
            };
            match client.state {
                Some(TransferState::WaitingForUserConsent) => {
                    rx_bus
                        .send(ChannelMessage {
                            id: msg.id.clone(),
                            msg: channel::Message::Lib {
                                action: TransferAction::ConsentAccept,
                            },
                        })
                        .unwrap();
                }
                Some(TransferState::Finished) => break true,
                Some(TransferState::Disconnected | TransferState::Cancelled) => break false,
                _ => {}
            }
        }
    };
    let finished = tokio::time::timeout(Duration::from_secs(60), run)
        .await
        .expect("transfer did not finish");
    if !finished {
        ctk.cancel();
        *crate::WIFI_LINK.write().unwrap() = None;
        return None;
    }
    tokio::time::timeout(Duration::from_secs(20), send)
        .await
        .expect("sender did not finish")
        .unwrap()
        .unwrap();
    let seen = tokio::time::timeout(Duration::from_secs(20), proxy)
        .await
        .expect("old channel was not closed")
        .unwrap();
    ctk.cancel();

    let received = std::fs::read(dst_dir.join("payload.bin")).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    *crate::WIFI_LINK.write().unwrap() = None;
    Some((seen, data, received))
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_link_everything_stays_on_the_first_channel() {
    let ((to_rx, _), sent, received) = transfer(None).await;
    assert_eq!(sent, received);
    assert!(to_rx >= FILE_SIZE as u64);
}

/// The peer of a LAN session is already on a network: even with a link set, nothing is hosted or
/// joined and the payload stays on the first channel. The upgrade is tested with in-memory
/// first channels below (the Bluetooth case).
#[tokio::test(flavor = "multi_thread")]
async fn a_lan_session_never_upgrades_even_with_a_link() {
    let link = Arc::new(FakeLink::default());
    let ((to_rx, _), sent, received) = transfer(Some(link.clone())).await;
    assert_eq!(sent, received);
    assert_eq!(link.hosts.load(Ordering::SeqCst), 0);
    assert_eq!(link.joins.load(Ordering::SeqCst), 0);
    assert!(
        to_rx >= FILE_SIZE as u64,
        "payload must use the first channel"
    );
}

// --- Whole transfer where the first channel is an in-memory stream (BLE bridge) ----------

/// Sends a file from an `OutboundRequest` to an `InboundRequest` whose first channel is a
/// duplex stream (through a byte counter), then returns the bytes that crossed the duplex
/// (sender to receiver, receiver to sender), the source and the received file.
/// With `plain` the receiver offers to disable encryption on the upgraded channel.
async fn try_duplex_transfer(
    link: Option<Arc<FakeLink>>,
    size: usize,
    plain: bool,
) -> Option<Outcome> {
    let _guard = SERIAL.lock().await;
    let dyn_link: Option<Arc<dyn WifiLink>> = link.map(|l| l as Arc<dyn WifiLink>);
    *crate::WIFI_LINK.write().unwrap() = dyn_link;

    let dir = std::env::temp_dir().join(format!("rqs_duplex_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (src_dir, dst_dir) = (dir.join("src"), dir.join("dst"));
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::create_dir_all(&dst_dir).unwrap();
    *crate::CUSTOM_DOWNLOAD.write().unwrap() = Some(dst_dir.clone());

    let data: Vec<u8> = (0..size).map(|i| (i * 31 % 251) as u8).collect();
    let src = src_dir.join("payload.bin");
    std::fs::write(&src, &data).unwrap();

    let (tx_end, mid_a) = tokio::io::duplex(8 * 1024);
    let (mid_b, rx_end) = tokio::io::duplex(8 * 1024);
    // Forwards in both directions and counts the bytes; errors only end the copy.
    async fn pump<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
        mut from: R,
        mut to: W,
        total: Arc<AtomicUsize>,
    ) {
        let mut buf = vec![0u8; 16 * 1024];
        while let Ok(n) = from.read(&mut buf).await {
            if n == 0 || to.write_all(&buf[..n]).await.is_err() {
                break;
            }
            total.fetch_add(n, Ordering::SeqCst);
        }
        let _ = to.shutdown().await;
    }
    let (to_rx_count, to_tx_count) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (a_r, a_w) = tokio::io::split(mid_a);
    let (b_r, b_w) = tokio::io::split(mid_b);
    tokio::spawn(pump(a_r, b_w, to_rx_count.clone()));
    tokio::spawn(pump(b_r, a_w, to_tx_count.clone()));

    let (rx_bus, mut rx_events) = broadcast::channel::<ChannelMessage>(256);
    let mut inbound = InboundRequest::new_over(
        MigratableStream::Duplex(rx_end),
        "ble-rx".into(),
        rx_bus.clone(),
    );
    if plain {
        inbound.offer_disabling_encryption();
    }
    // Like the TCP server, so that an aborted handshake is reported and the test retries.
    let rx_task = tokio::spawn(crate::manager::drive_inbound(inbound, rx_bus.clone()));

    let (tx_bus, _tx_events) = broadcast::channel::<ChannelMessage>(256);
    let mut outbound = OutboundRequest::new_over(
        *b"SEND",
        MigratableStream::Duplex(tx_end),
        "ble-tx".into(),
        tx_bus,
        OutboundPayload::Files(vec![src.to_string_lossy().into_owned()]),
        RemoteDeviceInfo {
            device_type: crate::DeviceType::Unknown,
            name: "test".into(),
        },
    );
    let tx_task = tokio::spawn(async move {
        outbound.send_connection_request().await?;
        outbound.send_ukey2_client_init().await?;
        while outbound.handle().await.is_ok() {}
        anyhow::Ok(())
    });

    let run = async {
        loop {
            let msg = rx_events.recv().await.unwrap();
            let Some(client) = msg.msg.as_client() else {
                continue;
            };
            match client.state {
                Some(TransferState::WaitingForUserConsent) => {
                    rx_bus
                        .send(ChannelMessage {
                            id: msg.id.clone(),
                            msg: channel::Message::Lib {
                                action: TransferAction::ConsentAccept,
                            },
                        })
                        .unwrap();
                }
                Some(TransferState::Finished) => break true,
                Some(TransferState::Disconnected | TransferState::Cancelled) => break false,
                _ => {}
            }
        }
    };
    let finished = tokio::time::timeout(Duration::from_secs(60), run)
        .await
        .expect("transfer did not finish");
    let _ = tokio::time::timeout(Duration::from_secs(20), tx_task).await;
    let _ = tokio::time::timeout(Duration::from_secs(20), rx_task).await;
    let seen = (
        to_rx_count.load(Ordering::SeqCst) as u64,
        to_tx_count.load(Ordering::SeqCst) as u64,
    );

    let received = std::fs::read(dst_dir.join("payload.bin")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    *crate::WIFI_LINK.write().unwrap() = None;
    finished.then_some((seen, data, received))
}

async fn duplex_transfer(link: Option<Arc<FakeLink>>, size: usize, plain: bool) -> Outcome {
    for _ in 0..5 {
        if let Some(outcome) = try_duplex_transfer(link.clone(), size, plain).await {
            return outcome;
        }
    }
    panic!("transfer kept aborting");
}

#[tokio::test(flavor = "multi_thread")]
async fn session_over_a_duplex_stream_without_a_link() {
    let size = 100 * 1024;
    let ((to_rx, _), sent, received) = duplex_transfer(None, size, false).await;
    assert_eq!(sent, received);
    assert!(to_rx >= size as u64);
}

#[tokio::test(flavor = "multi_thread")]
async fn duplex_session_upgrades_to_tcp_and_delivers_the_file() {
    let link = Arc::new(FakeLink::default());
    let ((to_rx, to_tx), sent, received) =
        duplex_transfer(Some(link.clone()), FILE_SIZE, false).await;
    assert_eq!(sent, received);
    assert_eq!(link.hosts.load(Ordering::SeqCst), 1);
    assert_eq!(link.joins.load(Ordering::SeqCst), 1);
    assert!(
        to_rx < (FILE_SIZE / 4) as u64,
        "{to_rx} bytes on the duplex"
    );
    assert!(to_tx < 64 * 1024, "{to_tx} bytes back on the duplex");
}

fn decode_plain(frames: &[&[u8]]) -> Vec<OfflineFrame> {
    frames
        .iter()
        .map(|f| {
            let frame = OfflineFrame::decode(*f).expect("not an OfflineFrame");
            assert!(frame.v1.is_some(), "not an OfflineFrame");
            frame
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn duplex_session_upgrades_to_plain_frames_and_delivers_the_file() {
    let tap = Tap::default();
    let link = Arc::new(FakeLink {
        tap: Some(tap.clone()),
        ..Default::default()
    });
    let ((to_rx, to_tx), sent, received) =
        duplex_transfer(Some(link.clone()), FILE_SIZE, true).await;
    assert_eq!(sent, received);
    assert_eq!(link.hosts.load(Ordering::SeqCst), 1);
    assert_eq!(link.joins.load(Ordering::SeqCst), 1);
    assert!(
        to_rx < (FILE_SIZE / 4) as u64,
        "{to_rx} bytes on the duplex"
    );
    assert!(to_tx < 64 * 1024, "{to_tx} bytes back on the duplex");

    // The upgraded channel carries bare OfflineFrames, no SecureMessage.
    let (up, down) = tap.lock().unwrap().clone();
    let up = decode_plain(&split_frames(&up));
    let down = decode_plain(&split_frames(&down));
    let intro = parse_bwu(&up[0])
        .unwrap()
        .client_introduction
        .as_ref()
        .unwrap();
    assert_eq!(intro.supports_disabling_encryption, Some(true));
    assert_eq!(
        parse_bwu(&down[0]).unwrap().event_type(),
        EventType::ClientIntroductionAck
    );
    let body: usize = up[1..]
        .iter()
        .filter_map(|f| {
            f.v1.as_ref()?
                .payload_transfer
                .as_ref()?
                .payload_chunk
                .as_ref()
        })
        .map(|c| c.body.as_ref().map_or(0, Vec::len))
        .sum();
    assert!(
        body >= FILE_SIZE,
        "only {body} payload bytes in plain frames"
    );
}
