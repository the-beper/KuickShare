//! GATT dial with BlueZ: connect, find the 0xFEF3 service, subscribe, expose a `GattLink`.
//!
//! The phone has two 0xFEF3 services: one with the slot characteristics, one with the
//! ToPeripheral/FromPeripheral pair the weave packets travel on. The second one is used.
//! The connect quirks are those of [`dial_l2cap`](super::dial::dial_l2cap): the scan is
//! dropped by the caller, a stale ACL link is disconnected, then the same pause and retry rules.

use std::time::Duration;

use bluer::gatt::remote::Characteristic;
use bluer::{
    Adapter, AdapterEvent, Address, Device, DiscoveryFilter, DiscoveryTransport, ErrorKind, Uuid,
    UuidExt,
};
use futures::{Stream, StreamExt, pin_mut};
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep, timeout};

use super::dial::{Next, SETTLE, drop_stale_link, next_step_within};
use super::gatt_bridge::GattLink;
use crate::hdl::ble_disconnect::{DISCONNECT_TIMEOUT, DisconnectOnDrop, disconnect};
use crate::hdl::ble_receiver::gatt::{FROM_PERIPHERAL_UUID, TO_PERIPHERAL_UUID};
use crate::hdl::ble_receiver::service_data::SERVICE_UUID;
use crate::hdl::ble_scan::{ScanResult, wait_discovery_stopped};

const INNER_NAME: &str = "GattDial";
/// Whole dial, settle and service discovery included. A send scans for at most 12 s and the
/// app gives up on a send that has not started after 40 s (`BLE_START_TIMEOUT`), so this
/// leaves the rest for the first frame.
pub(crate) const GATT_DIAL_TIMEOUT: Duration = Duration::from_secs(22);
/// One `connect` call. The Galaxy A55 took 5.8 s once (0.8 s on other runs).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for BlueZ to list a device it dropped, once the phone is heard again.
const REDISCOVER_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the last queued writes get to go out once the bridge is done.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(3);
/// Writes waiting for the pump; a full queue blocks the bridge.
const WRITES_QUEUE: usize = 4;
const NOTIFY_QUEUE: usize = 16;
/// Weave packet size when BlueZ does not tell the MTU: the 23 byte default ATT MTU minus 3.
const DEFAULT_PACKET: usize = 20;
/// Largest weave packet.
const MAX_PACKET: usize = 509;

/// Weave packet size for the MTU `bluer` reports. `bluer` already takes the ATT header
/// (and 2 bytes more, a BlueZ quirk) off the MTU, so the value is used as is: at worst two
/// bytes under the ATT limit.
fn packet_size(mtu: Option<usize>) -> usize {
    mtu.map_or(DEFAULT_PACKET, |m| m.clamp(DEFAULT_PACKET, MAX_PACKET))
}

/// Index of the first service whose characteristic UUIDs include both Nearby ones.
fn pick_service(services: &[Vec<Uuid>]) -> Option<usize> {
    services
        .iter()
        .position(|c| c.contains(&TO_PERIPHERAL_UUID) && c.contains(&FROM_PERIPHERAL_UUID))
}

/// Connects to the phone in `scan` as GATT central. The scan stream must be dropped already.
pub async fn dial_gatt(adapter: &Adapter, scan: &ScanResult) -> anyhow::Result<GattLink> {
    let started = Instant::now();
    let device = adapter.device(scan.address)?;
    // If this future is dropped (send cancelled), the connect BlueZ may still hold goes away.
    let mut guard = DisconnectOnDrop(Some(device.clone()));
    let mut disconnected = false;
    let result = timeout(
        GATT_DIAL_TIMEOUT,
        dial(adapter, scan, &device, started, &mut disconnected),
    )
    .await
    .unwrap_or_else(|_| Err(anyhow::anyhow!("no GATT link within {GATT_DIAL_TIMEOUT:?}")));
    if result.is_err() && !disconnected {
        // Still armed while this runs: a cancellation here spawns the disconnect.
        disconnect(&device).await;
    }
    guard.0 = None;
    result
}

async fn dial(
    adapter: &Adapter,
    scan: &ScanResult,
    device: &Device,
    started: Instant,
    disconnected: &mut bool,
) -> anyhow::Result<GattLink> {
    drop_stale_link(adapter, scan, DISCONNECT_TIMEOUT).await?;
    sleep(SETTLE).await;

    let mut failed = 0;
    loop {
        *disconnected = false;
        let mut gone = false;
        let last = match timeout(CONNECT_TIMEOUT, device.connect()).await {
            Ok(Ok(())) => break,
            Ok(Err(e)) => {
                gone = device_gone(&e);
                format!("connect: {e}")
            }
            Err(_) => {
                // The call was dropped, but BlueZ keeps its LE connect pending, and the next
                // attempt (or scan) would fail with "operation in progress".
                disconnect(device).await;
                *disconnected = true;
                "connect timed out".to_string()
            }
        };
        failed += 1;
        debug!("{INNER_NAME}: attempt {failed} failed: {last}");
        match next_step_within(failed, started.elapsed(), GATT_DIAL_TIMEOUT) {
            Next::Retry(wait) => {
                if gone {
                    // BlueZ dropped the object (a non-paired device does not outlive a
                    // cancelled connect): the next connect needs it back, not a pause.
                    rediscover(adapter, scan.address).await;
                } else {
                    sleep(wait).await;
                }
            }
            Next::GiveUp => anyhow::bail!("no GATT connection after {failed} attempts: {last}"),
        }
    }
    debug!("{INNER_NAME}: connected after {:?}", started.elapsed());

    // `services` waits for BlueZ to resolve them.
    let (to_phone, from_phone) = find_characteristics(device).await?;
    let notifications = from_phone.notify().await?;
    let mtu = match to_phone.mtu().await {
        Ok(m) => Some(m),
        Err(e) => {
            debug!("{INNER_NAME}: no MTU from BlueZ ({e}), using {DEFAULT_PACKET}");
            None
        }
    };
    let max_packet = packet_size(mtu);
    info!(
        "{INNER_NAME}: link up in {:?}, MTU {mtu:?}, packets of {max_packet} bytes",
        started.elapsed()
    );

    let (writes_tx, writes_rx) = mpsc::channel(WRITES_QUEUE);
    let (notify_tx, notify_rx) = mpsc::channel(NOTIFY_QUEUE);
    let device = device.clone();
    tokio::spawn(async move {
        let writer = forward_writes(writes_rx, to_phone);
        tokio::pin!(writer);
        tokio::select! {
            () = forward_notifications(notifications, notify_tx) => {
                // The bridge is done or the link is lost. The bridge drops both channel ends
                // together, so the writer ends once its queue is empty: let the last packets
                // (final data, socket DISCONNECTION) go out before the link is closed.
                if timeout(DRAIN_TIMEOUT, &mut writer).await.is_err() {
                    debug!("{INNER_NAME}: queued writes not sent within {DRAIN_TIMEOUT:?}");
                }
            }
            () = &mut writer => {}
        }
        disconnect(&device).await;
    });
    Ok(GattLink {
        writes: writes_tx,
        notifications: notify_rx,
        max_packet,
    })
}

/// The ToPeripheral and FromPeripheral characteristics of the right 0xFEF3 service.
async fn find_characteristics(device: &Device) -> anyhow::Result<(Characteristic, Characteristic)> {
    let service_uuid = Uuid::from_u16(SERVICE_UUID);
    let mut candidates = Vec::new();
    for service in device.services().await? {
        if service.uuid().await? == service_uuid {
            let chars = service.characteristics().await?;
            let mut uuids = Vec::new();
            for c in &chars {
                uuids.push(c.uuid().await?);
            }
            candidates.push((chars, uuids));
        }
    }
    let uuids: Vec<_> = candidates.iter().map(|(_, u)| u.clone()).collect();
    let Some(i) = pick_service(&uuids) else {
        anyhow::bail!(
            "the phone has no 0xFEF3 service with the Nearby characteristics ({} 0xFEF3 services seen)",
            candidates.len()
        );
    };
    let (chars, uuids) = candidates.swap_remove(i);
    let by_uuid = |wanted: Uuid| {
        uuids
            .iter()
            .position(|u| *u == wanted)
            .map(|p| chars[p].clone())
    };
    match (by_uuid(TO_PERIPHERAL_UUID), by_uuid(FROM_PERIPHERAL_UUID)) {
        (Some(to), Some(from)) => Ok((to, from)),
        _ => anyhow::bail!("the Nearby characteristics vanished"),
    }
}

/// Ends when the stream ends (link lost) or the bridge dropped its receiver.
async fn forward_notifications(stream: impl Stream<Item = Vec<u8>>, tx: mpsc::Sender<Vec<u8>>) {
    pin_mut!(stream);
    loop {
        tokio::select! {
            () = tx.closed() => return,
            value = stream.next() => match value {
                Some(v) => {
                    if tx.send(v).await.is_err() {
                        return;
                    }
                }
                None => return,
            },
        }
    }
}

/// Ends when the bridge dropped its sender or a write fails.
async fn forward_writes(mut rx: mpsc::Receiver<Vec<u8>>, to_phone: Characteristic) {
    while let Some(packet) = rx.recv().await {
        if let Err(e) = to_phone.write(&packet).await {
            debug!("{INNER_NAME}: write failed: {e}");
            return;
        }
    }
}

/// BlueZ no longer has the device object ("the target object was either not present or
/// removed", or `DoesNotExist`).
fn device_gone(e: &bluer::Error) -> bool {
    matches!(e.kind, ErrorKind::NotFound | ErrorKind::DoesNotExist)
}

/// Brings the device object back: BlueZ has no per-address discovery filter, so this runs an
/// LE discovery until `address` is listed again, [`REDISCOVER_TIMEOUT`] at most. Best effort:
/// the next connect tells whether it worked.
async fn rediscover(adapter: &Adapter, address: Address) {
    debug!("{INNER_NAME}: device object gone, discovering {address} again");
    let filter = DiscoveryFilter {
        transport: DiscoveryTransport::Le,
        ..Default::default()
    };
    if let Err(e) = adapter.set_discovery_filter(filter).await {
        debug!("{INNER_NAME}: discovery filter failed: {e}");
    }
    let listed = async {
        let events = adapter.discover_devices().await.ok()?;
        pin_mut!(events);
        while let Some(event) = events.next().await {
            if matches!(event, AdapterEvent::DeviceAdded(a) if a == address) {
                return Some(());
            }
        }
        None
    };
    let found = timeout(REDISCOVER_TIMEOUT, listed).await.ok().flatten();
    // The stream is dropped: let BlueZ stop the discovery before the next connect.
    wait_discovery_stopped(adapter).await;
    debug!(
        "{INNER_NAME}: device {address} listed again: {}",
        found.is_some()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLOT: Uuid = Uuid::from_u128(0x0000_0000_0000_3000_8000_0000_0000_0001);

    #[test]
    fn packet_size_follows_the_mtu_within_limits() {
        assert_eq!(packet_size(None), 20);
        assert_eq!(packet_size(Some(0)), 20);
        assert_eq!(packet_size(Some(100)), 100);
        assert_eq!(packet_size(Some(512)), 509);
    }

    #[test]
    fn a_missing_device_object_is_told_from_other_errors() {
        let err = |kind| bluer::Error {
            kind,
            message: String::new(),
        };
        assert!(device_gone(&err(ErrorKind::NotFound)));
        assert!(device_gone(&err(ErrorKind::DoesNotExist)));
        assert!(!device_gone(&err(ErrorKind::Failed)));
        assert!(!device_gone(&err(ErrorKind::NotReady)));
    }

    #[test]
    fn picks_the_service_with_the_nearby_characteristics() {
        let slots = vec![SLOT];
        let nearby = vec![FROM_PERIPHERAL_UUID, TO_PERIPHERAL_UUID];
        assert_eq!(pick_service(&[slots.clone(), nearby.clone()]), Some(1));
        assert_eq!(pick_service(&[nearby, slots.clone()]), Some(0));
        assert_eq!(pick_service(&[slots, vec![TO_PERIPHERAL_UUID]]), None);
        assert_eq!(pick_service(&[]), None);
    }
}

#[cfg(test)]
pub(crate) mod hardware_probe {
    use super::*;
    use crate::hdl::ble_scan::{Dedup, SCAN_WINDOW, SEND_LEGACY_READS, ScanEvent, scan_window};

    /// Hand test on a real adapter and phone: open Quick Share on the phone (Everyone), then
    /// run `cargo test -p rqs_lib --no-run` and execute the printed test binary on the host
    /// with `--ignored --nocapture gatt_connection_confirm`. Env: `KS_ENDPOINT` (4 character
    /// endpoint id of the phone to use).
    #[tokio::test]
    #[ignore = "needs a BlueZ adapter and a phone in Receive mode"]
    async fn gatt_connection_confirm() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "rqs_lib=debug".into()),
            )
            .try_init();
        let session = bluer::Session::new().await.expect("BlueZ session");
        let adapter = session.default_adapter().await.expect("adapter");
        adapter.set_powered(true).await.expect("power on");

        let wanted = std::env::var("KS_ENDPOINT").ok();
        let mut found: Vec<ScanResult> = Vec::new();
        scan_window(
            &adapter,
            SCAN_WINDOW,
            SEND_LEGACY_READS,
            &mut Dedup::default(),
            |event| {
                if let ScanEvent::Found(r) = event {
                    println!("found {r:?}");
                    found.push(r);
                }
            },
        )
        .await
        .expect("scan window");
        let scan = found
            .into_iter()
            .find(|r| {
                wanted
                    .as_deref()
                    .is_none_or(|w| r.endpoint_id.as_slice() == w.as_bytes())
            })
            .expect("no phone found: is Quick Share open on it, visible to Everyone?");
        println!("target {}", scan.name);

        let mut link = dial_gatt(&adapter, &scan).await.expect("dial_gatt");
        println!("max_packet {}", link.max_packet);

        let mut request = vec![0x80, 0x00, 0x01, 0x00, 0x01];
        request.extend_from_slice(&(link.max_packet as u16).to_be_bytes());
        link.writes.send(request).await.expect("queue the request");
        let first = timeout(Duration::from_secs(10), link.notifications.recv())
            .await
            .expect("no packet within 10 s");
        println!("first packet {first:02x?}");
        assert_eq!(first.and_then(|p| p.first().copied()), Some(0x81));
        drop(link);
        sleep(Duration::from_secs(1)).await;
    }
}
