//! Opens the L2CAP CoC channel to a phone found by [`scan_window`](crate::hdl::ble_scan).
//!
//! BlueZ quirks this works around (all seen in other Quick Share clients, none verified on
//! this machine yet):
//! - a running scan breaks the connection, so the caller drops the scan first and we wait
//!   [`SETTLE`] before connecting;
//! - an ACL link BlueZ still holds from the scan makes `connect` return at once on a channel
//!   that is not up, so it is disconnected first;
//! - even then `connect` can return early: the channel is used only when it has a send MTU,
//!   otherwise the socket is dropped and the connect is retried.
//!
//! The phone takes an unbonded channel, so the security level is Low.

use std::fmt;
use std::time::Duration;

use bluer::l2cap::{Security, SecurityLevel, Socket, SocketAddr, Stream};
use bluer::{Adapter, AddressType};
use tokio::time::{Instant, sleep, timeout};

use crate::hdl::ble_scan::ScanResult;

const INNER_NAME: &str = "L2capDial";
/// Wait between the end of the scan and the first connect.
pub const SETTLE: Duration = Duration::from_millis(500);
/// Whole dial, settle included.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(15);
pub const MAX_ATTEMPTS: u32 = 5;
/// One `connect` call.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
/// How long a connected socket gets to report a send MTU.
const READY_TIMEOUT: Duration = Duration::from_secs(3);
const READY_POLL: Duration = Duration::from_millis(20);
/// How long the L2CAP dial waits for BlueZ to drop a stale link (the GATT path passes its own).
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const RECV_MTU: u16 = 4096;
const ENOTCONN: i32 = 107;

#[derive(Debug)]
pub enum DialError {
    /// The phone's advertisement carried no PSM.
    NoPsm,
    /// BlueZ refused to describe the adapter or the device.
    Adapter(String),
    /// The dial did not finish in [`DIAL_TIMEOUT`].
    Timeout,
    /// Every attempt failed; `last` is the reason of the final one.
    Failed { attempts: u32, last: String },
}

impl fmt::Display for DialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoPsm => f.write_str("the phone advertises no L2CAP PSM"),
            Self::Adapter(e) => write!(f, "BlueZ: {e}"),
            Self::Timeout => write!(f, "no L2CAP channel within {DIAL_TIMEOUT:?}"),
            Self::Failed { attempts, last } => {
                write!(f, "no L2CAP channel after {attempts} attempts: {last}")
            }
        }
    }
}

impl std::error::Error for DialError {}

/// What to do after a failed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    Retry(Duration),
    GiveUp,
}

/// `failed` attempts have failed `elapsed` after the dial started.
pub fn next_step(failed: u32, elapsed: Duration) -> Next {
    next_step_within(failed, elapsed, DIAL_TIMEOUT)
}

/// [`next_step`] for a dial that has `budget` in all.
pub fn next_step_within(failed: u32, elapsed: Duration, budget: Duration) -> Next {
    let backoff = Duration::from_millis(500 * u64::from(failed)).min(Duration::from_secs(2));
    if failed >= MAX_ATTEMPTS || elapsed + backoff + SETTLE >= budget {
        Next::GiveUp
    } else {
        Next::Retry(backoff)
    }
}

/// Connects to the PSM in `scan`. The scan stream must be dropped already.
pub async fn dial_l2cap(adapter: &Adapter, scan: &ScanResult) -> anyhow::Result<Stream> {
    timeout(DIAL_TIMEOUT, dial(adapter, scan))
        .await
        .map_err(|_| DialError::Timeout)?
        .map_err(Into::into)
}

async fn dial(adapter: &Adapter, scan: &ScanResult) -> Result<Stream, DialError> {
    let started = Instant::now();
    let psm = scan.psm.ok_or(DialError::NoPsm)?;
    let local = adapter
        .address()
        .await
        .map_err(|e| DialError::Adapter(e.to_string()))?;
    drop_stale_link(adapter, scan, DISCONNECT_TIMEOUT).await?;
    sleep(SETTLE).await;

    let target = SocketAddr::new(scan.address, scan.address_type, psm);
    let mut failed = 0;
    loop {
        let remaining = DIAL_TIMEOUT.saturating_sub(started.elapsed());
        let last = match attempt(local, target, remaining).await {
            Ok(stream) => {
                info!(
                    "{INNER_NAME}: channel to PSM {psm} up in {:?} ({} attempts)",
                    started.elapsed(),
                    failed + 1
                );
                return Ok(stream);
            }
            Err(e) => e,
        };
        failed += 1;
        debug!("{INNER_NAME}: attempt {failed} failed: {last}");
        match next_step(failed, started.elapsed()) {
            Next::Retry(wait) => sleep(wait).await,
            Next::GiveUp => {
                return Err(DialError::Failed {
                    attempts: failed,
                    last,
                });
            }
        }
    }
}

/// Disconnects the ACL link BlueZ holds to the phone, if any, waiting `disconnect_timeout`.
pub(super) async fn drop_stale_link(
    adapter: &Adapter,
    scan: &ScanResult,
    disconnect_timeout: Duration,
) -> Result<(), DialError> {
    let device = adapter
        .device(scan.address)
        .map_err(|e| DialError::Adapter(e.to_string()))?;
    if !device.is_connected().await.unwrap_or(false) {
        return Ok(());
    }
    debug!("{INNER_NAME}: dropping a stale ACL link");
    match timeout(disconnect_timeout, device.disconnect()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => debug!("{INNER_NAME}: disconnect failed: {e}"),
        Err(_) => debug!(
            "{INNER_NAME}: no answer to the disconnect within {disconnect_timeout:?}; BlueZ closes the link later"
        ),
    }
    Ok(())
}

/// One connect that has to end with a usable channel.
async fn attempt(
    local: bluer::Address,
    target: SocketAddr,
    budget: Duration,
) -> Result<Stream, String> {
    let started = Instant::now();
    let socket = Socket::<Stream>::new_stream().map_err(|e| format!("socket: {e}"))?;
    socket
        .set_security(Security {
            level: SecurityLevel::Low,
            key_size: 0,
        })
        .map_err(|e| format!("set security: {e}"))?;
    socket
        .set_recv_mtu(RECV_MTU)
        .map_err(|e| format!("set receive MTU: {e}"))?;
    socket
        .bind(SocketAddr::new(local, AddressType::LePublic, 0))
        .map_err(|e| format!("bind: {e}"))?;

    let stream = match timeout(CONNECT_TIMEOUT.min(budget), socket.connect(target)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => return Err(format!("connect: {e}")),
        Err(_) => return Err("connect timed out".into()),
    };
    let ready_by = Instant::now() + READY_TIMEOUT.min(budget.saturating_sub(started.elapsed()));
    loop {
        match stream.as_ref().send_mtu() {
            Ok(mtu) => {
                debug!("{INNER_NAME}: send MTU {mtu} after {:?}", started.elapsed());
                return Ok(stream);
            }
            Err(e) if e.raw_os_error() == Some(ENOTCONN) && Instant::now() < ready_by => {
                sleep(READY_POLL).await;
            }
            Err(e) => return Err(format!("channel not ready: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_back_off_and_stop_at_the_attempt_limit() {
        let waits: Vec<_> = (1..=MAX_ATTEMPTS)
            .map(|n| next_step(n, Duration::ZERO))
            .collect();
        assert_eq!(
            waits,
            [
                Next::Retry(Duration::from_millis(500)),
                Next::Retry(Duration::from_millis(1000)),
                Next::Retry(Duration::from_millis(1500)),
                Next::Retry(Duration::from_millis(2000)),
                Next::GiveUp,
            ]
        );
        assert_eq!(next_step(9, Duration::ZERO), Next::GiveUp);
    }

    #[test]
    fn no_retry_that_would_end_after_the_deadline() {
        let late = DIAL_TIMEOUT - Duration::from_millis(900);
        assert_eq!(next_step(1, late), Next::GiveUp);
        let ok = DIAL_TIMEOUT - Duration::from_millis(1100);
        assert_eq!(next_step(1, ok), Next::Retry(Duration::from_millis(500)));
    }

    #[test]
    fn errors_say_what_happened() {
        assert!(DialError::NoPsm.to_string().contains("PSM"));
        let e = DialError::Failed {
            attempts: 3,
            last: "connect: boom".into(),
        };
        assert_eq!(
            e.to_string(),
            "no L2CAP channel after 3 attempts: connect: boom"
        );
    }
}

#[cfg(test)]
pub(crate) mod hardware_probe {
    use std::io;
    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use std::time::Instant;

    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::channel::{ChannelMessage, Message};
    use crate::hdl::OutboundPayload;
    use crate::hdl::TransferState;
    use crate::hdl::ble_scan::{Dedup, SCAN_WINDOW, SEND_LEGACY_READS, ScanEvent, scan_window};
    use crate::hdl::ble_send::bridge::BridgeExit;
    use crate::hdl::ble_send::gatt_dial::dial_gatt;
    use crate::hdl::ble_send::{send_over_gatt, send_over_l2cap};
    use crate::upgrade::tests::SERIAL;
    use crate::upgrade::{BoxFuture, WifiCredentials, WifiLink};

    /// The phone's user has to press Accept in this time.
    const ACCEPT_TIME: Duration = Duration::from_secs(60);
    /// `observe_upgrade_offer` stops waiting for an offer this long after the send started.
    const OBSERVE_TIME: Duration = Duration::from_secs(30);

    /// Scans one window and picks the phone (`KS_ENDPOINT` narrows it down).
    async fn scan_target(adapter: &bluer::Adapter, step: &dyn Fn(&str)) -> ScanResult {
        step("scanning");
        let wanted = std::env::var("KS_ENDPOINT").ok();
        let mut found: Vec<ScanResult> = Vec::new();
        scan_window(
            adapter,
            SCAN_WINDOW,
            SEND_LEGACY_READS,
            &mut Dedup::default(),
            |event| {
                if let ScanEvent::Found(r) = event {
                    step(&format!("found {r:?}"));
                    found.push(r);
                }
            },
        )
        .await
        .expect("scan window");
        let scan = found
            .into_iter()
            .rev()
            .find(|r| {
                r.psm.is_some()
                    && wanted
                        .as_deref()
                        .is_none_or(|w| r.endpoint_id.as_slice() == w.as_bytes())
            })
            .expect("no phone with a PSM found: is Quick Share open on it, visible to Everyone?");
        step(&format!("target {} PSM {:?}", scan.name, scan.psm));
        scan
    }

    /// Prints the bus events of a transfer; returns whether it reached `Finished`.
    pub(crate) fn spawn_printer(
        mut rx: broadcast::Receiver<ChannelMessage>,
        started: Instant,
    ) -> tokio::task::JoinHandle<bool> {
        tokio::spawn(async move {
            let mut finished = false;
            loop {
                match rx.recv().await {
                    Ok(ChannelMessage {
                        msg: Message::Client(m),
                        ..
                    }) => {
                        let md = m.metadata.as_ref();
                        println!(
                            "{:>6.1}s state {:?} pin {:?} acked {:?}/{:?}",
                            started.elapsed().as_secs_f32(),
                            m.state,
                            md.and_then(|m| m.pin_code.as_deref()),
                            md.map(|m| m.ack_bytes),
                            md.map(|m| m.total_bytes),
                        );
                        finished |= m.state == Some(TransferState::Finished);
                    }
                    Ok(other) => {
                        println!("{:>6.1}s event {other:?}", started.elapsed().as_secs_f32())
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => println!("(lagged by {n})"),
                    Err(broadcast::error::RecvError::Closed) => return finished,
                }
            }
        })
    }

    /// Hand test on a real adapter and phone: open Quick Share on the phone (Everyone), then
    /// run `cargo test -p rqs_lib --no-run`, and execute the printed test binary on the host
    /// with `--ignored --nocapture send_small_file_to_phone`. Env: `KS_ENDPOINT` (4 character
    /// endpoint id of the phone to use), `KS_FILE` (file to send instead of 8 KB of text),
    /// `KS_TRANSPORT=gatt` (send over GATT even if the phone advertises a PSM).
    /// Press Accept on the phone when it asks.
    #[tokio::test]
    #[ignore = "needs a BlueZ adapter and a phone in Receive mode"]
    async fn send_small_file_to_phone() {
        let started = Instant::now();
        let step = |what: &str| println!("{:>6.1}s {what}", started.elapsed().as_secs_f32());

        let session = bluer::Session::new().await.expect("BlueZ session");
        let adapter = session.default_adapter().await.expect("adapter");
        adapter.set_powered(true).await.expect("power on");

        let scan = scan_target(&adapter, &step).await;

        let (path, temp) = match std::env::var("KS_FILE") {
            Ok(p) => (p, false),
            Err(_) => {
                let p = std::env::temp_dir()
                    .join(format!("kuickshare-probe-{}.txt", std::process::id()));
                let text = "KuickShare L2CAP probe line\n".repeat(8 * 1024 / 28);
                std::fs::write(&p, text).expect("write the temp file");
                (p.to_string_lossy().into_owned(), true)
            }
        };
        step(&format!("sending {path}"));

        step("dialing");
        let gatt = std::env::var("KS_TRANSPORT").is_ok_and(|v| v == "gatt");
        let (tx, rx) = broadcast::channel::<ChannelMessage>(256);
        let ctk = CancellationToken::new();
        let payload = OutboundPayload::Files(vec![path.clone()]);
        let send: std::pin::Pin<Box<dyn Future<Output = anyhow::Result<BridgeExit>> + Send>> =
            if gatt {
                let link = dial_gatt(&adapter, &scan).await.expect("dial GATT");
                step("GATT link up; waiting for the phone to accept");
                Box::pin(send_over_gatt(
                    link,
                    *b"KSPR",
                    "ks-probe".into(),
                    scan.name.clone(),
                    payload,
                    tx.clone(),
                    ctk.clone(),
                ))
            } else {
                let stream = dial_l2cap(&adapter, &scan).await.expect("dial");
                step("channel up; waiting for the phone to accept");
                Box::pin(send_over_l2cap(
                    stream,
                    *b"KSPR",
                    "ks-probe".into(),
                    scan.name.clone(),
                    payload,
                    tx.clone(),
                    ctk.clone(),
                ))
            };
        let printer = spawn_printer(rx, started);
        let result = tokio::time::timeout(ACCEPT_TIME + Duration::from_secs(60), send).await;
        ctk.cancel();
        drop(tx);
        let finished = printer.await.expect("printer task");
        if temp {
            let _ = std::fs::remove_file(&path);
        }
        let exit = match result {
            Ok(r) => r.expect("send"),
            Err(_) => panic!("timed out: did the phone user press Accept within {ACCEPT_TIME:?}?"),
        };
        step(&format!("bridge exit {exit:?}, finished {finished}"));
        assert!(finished, "the transfer did not reach Finished");
    }

    /// Same as `send_small_file_to_phone`, with a larger file and a fake `WifiLink` that only
    /// prints what the phone offers and then fails, so the sender answers UPGRADE_FAILURE and
    /// stays on BLE. Stops at the first offer, or 30 s after the send started. Env: `KS_ENDPOINT`,
    /// `KS_SIZE_KB` (size of the temp file, default 3072). Run it like the other test with
    /// `--ignored --nocapture observe_upgrade_offer`; `RUST_LOG=rqs_lib=debug` shows more.
    #[tokio::test]
    #[ignore = "needs a BlueZ adapter and a phone in Receive mode"]
    async fn observe_upgrade_offer() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "rqs_lib=info".into()),
            )
            .try_init();
        let started = Instant::now();
        let step = |what: &str| println!("{:>6.1}s {what}", started.elapsed().as_secs_f32());

        let session = bluer::Session::new().await.expect("BlueZ session");
        let adapter = session.default_adapter().await.expect("adapter");
        adapter.set_powered(true).await.expect("power on");
        let scan = scan_target(&adapter, &step).await;

        let kb: usize = std::env::var("KS_SIZE_KB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3072);
        let path =
            std::env::temp_dir().join(format!("kuickshare-offer-{}.bin", std::process::id()));
        let mut data = vec![0u8; kb * 1024];
        rand::Rng::fill_bytes(&mut rand::rng(), &mut data);
        std::fs::write(&path, data).expect("write the temp file");
        let path = path.to_string_lossy().into_owned();

        let _guard = SERIAL.lock().await;
        let (offer_tx, mut offer_rx) = tokio::sync::mpsc::unbounded_channel();
        let link: Arc<dyn WifiLink> = Arc::new(ObserverLink(offer_tx));
        *crate::WIFI_LINK.write().unwrap() = Some(link);

        step("dialing");
        let result = async {
            let stream = dial_l2cap(&adapter, &scan).await?;
            step(&format!(
                "channel up; sending {kb} KB, press Accept on the phone"
            ));
            let (tx, rx) = broadcast::channel::<ChannelMessage>(256);
            let printer = spawn_printer(rx, started);
            let ctk = CancellationToken::new();
            let send = tokio::spawn(send_over_l2cap(
                stream,
                *b"KSOF",
                "ks-offer".into(),
                scan.name.clone(),
                OutboundPayload::Files(vec![path.clone()]),
                tx,
                ctk.clone(),
            ));
            let outcome = tokio::select! {
                offer = offer_rx.recv() => offer,
                _ = tokio::time::sleep(OBSERVE_TIME) => None,
            };
            match &outcome {
                Some(o) => println!(
                    "OFFER OBSERVED: {o} (after {:.1}s)",
                    started.elapsed().as_secs_f32()
                ),
                None => println!("NO OFFER within {} s", OBSERVE_TIME.as_secs()),
            }
            // Let UPGRADE_FAILURE go out before the session is cancelled.
            tokio::time::sleep(Duration::from_secs(1)).await;
            ctk.cancel();
            let exit = tokio::time::timeout(Duration::from_secs(180), send).await;
            let finished = printer.await.unwrap_or(false);
            step(&format!("send ended: {exit:?}, finished {finished}"));
            Ok::<_, anyhow::Error>(outcome.is_some())
        }
        .await;

        *crate::WIFI_LINK.write().unwrap() = None;
        let _ = std::fs::remove_file(&path);
        let observed = result.expect("dial");
        println!(
            "SUMMARY: {}",
            if observed {
                "offer observed"
            } else {
                "no offer"
            }
        );
    }

    /// Never joins: reports the offer and fails.
    pub(crate) struct ObserverLink(pub(crate) tokio::sync::mpsc::UnboundedSender<String>);

    impl WifiLink for ObserverLink {
        fn host(
            &self,
            _port: u16,
            _peer_5ghz: Option<bool>,
        ) -> BoxFuture<'_, io::Result<WifiCredentials>> {
            Box::pin(async { Err(io::Error::other("observer link does not host")) })
        }

        fn join<'a>(&'a self, c: &'a WifiCredentials) -> BoxFuture<'a, io::Result<Ipv4Addr>> {
            Box::pin(async move {
                let _ = self.0.send(format!(
                    "ssid {:?}, {} MHz, gateway {}, port {}",
                    c.ssid, c.frequency_mhz, c.gateway, c.port
                ));
                Err(io::Error::other("observer link does not join"))
            })
        }

        fn release(&self) -> BoxFuture<'_, ()> {
            Box::pin(async {})
        }
    }
}
