//! Windowed BLE scan for phones in Receive mode (Everyone visibility), found by their 0xFEF3
//! service data (see `ble_receiver::service_data`), and the send to one of them.
//!
//! [`BleScanner::scan`] runs one [`scan_window`] per user action (discovery start, refresh)
//! and turns each [`ScanEvent`] into an `EndpointInfo` with id `ble:<endpoint_id>` and
//! `present` set from `Found`/`Gone`. One [`Dedup`] lives as long as the process, so devices
//! that vanish between two windows are reported. [`BleScanner::send`] scans again for the
//! target (its address and PSM rotate at every scan), dials it (L2CAP, or GATT when the phone
//! advertises no PSM) and runs the send session.
//!
//! A phone that advertises only a legacy header (its full record is in GATT slot 0) is read
//! over GATT by a send alone, never by a list scan: see [`LegacyReads`].
//!
//! The scan must NEVER run continuously: a permanent BlueZ discovery blocks the reconnection
//! of bonded LE devices (rquickshare #429). Hence a bounded window, a minimum interval, and
//! one lock shared with the send: no scan while a dial or a transfer is going on.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use bluer::gatt::remote::CharacteristicReadRequest;
use bluer::{Adapter, AdapterEvent, Address, AddressType, DiscoveryFilter, DiscoveryTransport};
use bluer::{Uuid, UuidExt};
use futures::StreamExt;
use once_cell::sync::Lazy;
use tokio::sync::{MutexGuard, broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use super::ble_disconnect::{DisconnectOnDrop, disconnect};
use super::ble_receiver::gatt::SLOT0_UUID;
use super::ble_receiver::service_data::{
    ReceiverAdvertisement, SERVICE_UUID, parse_legacy_header, regular_length,
};
use super::ble_send::dial::{DIAL_TIMEOUT, dial_l2cap};
use super::ble_send::gatt_dial::{GATT_DIAL_TIMEOUT, dial_gatt};
use super::ble_send::{send_over_gatt, send_over_l2cap};
use super::{BleTarget, EndpointInfo, OutboundPayload};
use crate::DeviceType;
use crate::channel::ChannelMessage;

/// Long enough to catch a phone that advertises its full record only every 10 s (the Pixel does,
/// seen in a capture), short enough for a refresh button. The window is never cut short: a phone can show up at any moment of it.
pub const SCAN_WINDOW: Duration = Duration::from_secs(15);

/// Minimum time between the starts of two discovery windows.
pub const MIN_SCAN_INTERVAL: Duration = Duration::from_secs(4);

/// How long a send looks for its target again before giving up.
pub const RESCAN_WINDOW: Duration = Duration::from_secs(12);

/// The whole start of a BLE send (scan, dials) has to end within this, below the daemon's own
/// watchdog, which uses this value (`BLE_START_TIMEOUT` in the app).
pub const BLE_START_BUDGET: Duration = Duration::from_secs(40);
/// Left for the phone's first frame after the dial.
const FIRST_FRAME_RESERVE: Duration = Duration::from_secs(2);

/// What the L2CAP dial may take: all of `DIAL_TIMEOUT` when there is time, as before the GATT
/// fallback existed.
fn l2cap_budget(remaining: Duration) -> Duration {
    DIAL_TIMEOUT.min(remaining.saturating_sub(FIRST_FRAME_RESERVE))
}

/// A GATT dial after a failed L2CAP one runs only when it fits whole.
fn gatt_fits(remaining: Duration) -> bool {
    remaining >= GATT_DIAL_TIMEOUT + FIRST_FRAME_RESERVE
}

/// A successful L2CAP dial is committed. Only failure *before* any bytes are sent may retry
/// over GATT, never a failure from `send_over_l2cap` (that could duplicate a partial transfer).
enum Dialed<L, G> {
    L2cap(L),
    Gatt(G),
}

/// L2CAP when the phone advertises a PSM, else GATT; `remaining` is what is left of
/// [`BLE_START_BUDGET`]. A failed L2CAP dial falls back to GATT only if [`gatt_fits`]; otherwise
/// its error is returned.
async fn dial_with_fallback<L, G>(
    psm: bool,
    l2cap: impl std::future::Future<Output = anyhow::Result<L>>,
    gatt: impl std::future::Future<Output = anyhow::Result<G>>,
    remaining: Duration,
    ctk: &CancellationToken,
) -> anyhow::Result<Dialed<L, G>> {
    let started = tokio::time::Instant::now();
    if psm {
        let budget = l2cap_budget(remaining);
        if budget.is_zero() {
            warn!(
                "the phone advertises a PSM but only {remaining:?} of the BLE start budget is left: no L2CAP dial"
            );
        } else {
            let attempt = tokio::select! {
                outcome = tokio::time::timeout(budget, l2cap) => outcome,
                () = ctk.cancelled() => bail!("cancelled while dialing L2CAP"),
            };
            let failure = match attempt {
                Ok(Ok(link)) => return Ok(Dialed::L2cap(link)),
                Ok(Err(e)) => e,
                Err(_) => anyhow::anyhow!("no L2CAP channel within {budget:?}"),
            };
            let left = remaining.saturating_sub(started.elapsed());
            if !gatt_fits(left) {
                warn!(
                    "L2CAP dial failed ({failure:#}); GATT fallback skipped: {left:?} of the BLE start budget left, it needs {:?}",
                    GATT_DIAL_TIMEOUT + FIRST_FRAME_RESERVE
                );
                return Err(failure);
            }
            warn!("L2CAP dial failed ({failure:#}); falling back to GATT");
        }
    }
    let gatt_bound = remaining.saturating_sub(started.elapsed() + FIRST_FRAME_RESERVE);
    if gatt_bound.is_zero() {
        bail!("no time remains for a GATT dial under the BLE start watchdog");
    }
    let link = tokio::select! {
        link = tokio::time::timeout(gatt_bound, gatt) => link.context("GATT dial exceeded the BLE start budget")??,
        () = ctk.cancelled() => bail!("cancelled while connecting over GATT"),
    };
    Ok(Dialed::Gatt(link))
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScanResult {
    /// Stable identity of the phone; the address rotates.
    pub endpoint_id: [u8; 4],
    pub name: String,
    pub device_type: DeviceType,
    pub address: Address,
    pub address_type: AddressType,
    pub psm: Option<u16>,
    pub rssi: Option<i16>,
}

impl ScanResult {
    /// Equal in everything a caller can act on; the RSSI jitters on every packet.
    fn same_target(&self, other: &Self) -> bool {
        Self {
            rssi: None,
            ..self.clone()
        } == Self {
            rssi: None,
            ..other.clone()
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ScanEvent {
    /// First sighting of a phone, or a change of its address, name or PSM.
    Found(ScanResult),
    /// Seen in the previous window, not in the last one.
    Gone([u8; 4]),
}

/// Turns what BlueZ knows about a device into a result. `None` for anything that is not a
/// full 0xFEF3 advertisement of a receiver visible to everyone.
pub fn scan_result(
    address: Address,
    address_type: AddressType,
    rssi: Option<i16>,
    service_data: &[u8],
) -> Option<ScanResult> {
    let adv = ReceiverAdvertisement::parse(service_data)?;
    Some(ScanResult {
        endpoint_id: adv.endpoint_id,
        name: adv.name,
        device_type: DeviceType::from_raw_value(adv.device_type),
        address,
        address_type,
        psm: adv.psm,
        rssi,
    })
}

/// A Pixel sends its full advertisement, with the PSM, only about every 10 s (observed). A
/// phone first heard less than this ago may simply not have sent it yet.
const FULL_ADVERT_INTERVAL: Duration = Duration::from_secs(10);

/// Longest GATT slot-0 read for a legacy-only BLE receiver; the time left in the window cuts it
/// shorter.
const LEGACY_GATT_TIMEOUT: Duration = Duration::from_secs(7);
const MAX_SLOT0: usize = 512;

/// Whether a window may connect over GATT to read slot 0 of a phone that advertises only a
/// legacy header. Any neighbour can make us do it, and a connection disturbs BlueZ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyReads {
    /// A list scan only fills the device list: a legacy-only phone is not listed.
    Off,
    /// A send looks for one phone: at most `max` reads per window.
    Send { max: usize },
}

pub const SEND_LEGACY_READS: LegacyReads = LegacyReads::Send { max: 2 };

/// What one window decides about slot-0 reads.
#[derive(Debug)]
struct LegacyGate {
    policy: LegacyReads,
    /// Gave a full 0x48 advertisement in this window: nothing to read from it.
    full: HashSet<Address>,
    /// Read, or being read, in this window. A legacy header identifies only a record in GATT,
    /// not an endpoint, so a changing device would be read again and again.
    tried: HashSet<Address>,
}

impl LegacyGate {
    fn new(policy: LegacyReads) -> Self {
        Self {
            policy,
            full: HashSet::new(),
            tried: HashSet::new(),
        }
    }

    fn saw_full(&mut self, address: Address) {
        self.full.insert(address);
    }

    /// True, and counted, when `address` may be read now.
    fn admit(&mut self, address: Address) -> bool {
        let LegacyReads::Send { max } = self.policy else {
            return false;
        };
        if self.full.contains(&address) || self.tried.contains(&address) || self.tried.len() >= max
        {
            return false;
        }
        self.tried.insert(address)
    }
}

/// How long a "this phone has no PSM" verdict is trusted by a send.
const NO_PSM_TTL: Duration = Duration::from_secs(300);

/// What the windows so far showed of one phone.
#[derive(Debug)]
struct Seen {
    first: Instant,
    last: Instant,
    /// A PSM showed up at least once.
    psm: bool,
}

impl Seen {
    /// Long enough on the air for a full advertisement to have come, and never with a PSM.
    fn no_psm(&self, now: Instant) -> bool {
        !self.psm
            && now.saturating_duration_since(self.first) >= FULL_ADVERT_INTERVAL
            && now.saturating_duration_since(self.last) < NO_PSM_TTL
    }
}

/// Keeps one entry per endpoint id (the newest sighting, hence the newest address wins) and
/// tracks which phones disappeared from one window to the next.
#[derive(Debug, Default)]
pub struct Dedup {
    /// Reported by the previous windows.
    known: HashMap<[u8; 4], ScanResult>,
    current: HashMap<[u8; 4], ScanResult>,
    /// Sightings of the phones in `known` and `current`, kept across windows (a cancelled
    /// window leaves them alone). A phone absent from a completed window is dropped.
    seen: HashMap<[u8; 4], Seen>,
    /// Phones judged to advertise no PSM at the end of a window, with the time of the verdict.
    /// A Samsung advertisement ends after the token (no mask byte) while a Pixel sends its PSM
    /// only every [`FULL_ADVERT_INTERVAL`], so a sighting without PSM proves nothing: see
    /// [`Self::no_psm_at`]. A sighting with a PSM withdraws the verdict.
    no_psm: HashMap<[u8; 4], Instant>,
}

impl Dedup {
    /// Forgets what a cancelled window had reported; the sightings and verdicts stay.
    pub fn begin_window(&mut self) {
        self.current.clear();
    }

    /// True when the phone advertises no PSM: a window ended with a verdict in the last
    /// [`NO_PSM_TTL`], or, without waiting for the end of a window, it was first heard at least
    /// [`FULL_ADVERT_INTERVAL`] ago in the last window or the one under way and never with a
    /// PSM. A PSM always wins.
    pub fn advertises_no_psm(&self, id: &[u8; 4]) -> bool {
        self.no_psm_at(id, Instant::now())
    }

    fn no_psm_at(&self, id: &[u8; 4], now: Instant) -> bool {
        let seen = self.seen.get(id);
        if seen.is_some_and(|s| s.psm) {
            return false;
        }
        self.no_psm
            .get(id)
            .is_some_and(|at| now.saturating_duration_since(*at) < NO_PSM_TTL)
            || (seen.is_some_and(|s| s.no_psm(now))
                && (self.current.contains_key(id) || self.known.contains_key(id)))
    }

    /// The device type last seen for the phone, if the windows still remember it.
    pub fn device_type(&self, id: &[u8; 4]) -> Option<DeviceType> {
        self.current
            .get(id)
            .or_else(|| self.known.get(id))
            .map(|r| r.device_type.clone())
    }

    /// Withdraws the verdict, e.g. after a send scan saw a PSM.
    pub fn forget_no_psm(&mut self, id: &[u8; 4]) {
        self.no_psm.remove(id);
        if let Some(seen) = self.seen.get_mut(id) {
            seen.psm = true;
        }
    }

    /// Returns the result when the caller has to be told about it.
    pub fn observe(&mut self, result: ScanResult) -> Option<ScanResult> {
        self.observe_at(result, Instant::now())
    }

    fn observe_at(&mut self, result: ScanResult, now: Instant) -> Option<ScanResult> {
        let id = result.endpoint_id;
        let seen = self.seen.entry(id).or_insert(Seen {
            first: now,
            last: now,
            psm: false,
        });
        if now.saturating_duration_since(seen.last) >= NO_PSM_TTL {
            // Gone for so long that the old sightings say nothing about it any more.
            *seen = Seen {
                first: now,
                last: now,
                psm: false,
            };
        }
        seen.last = now;
        if result.psm.is_some() {
            seen.psm = true;
            self.no_psm.remove(&id);
        }
        let changed = self
            .current
            .get(&id)
            .or_else(|| self.known.get(&id))
            .is_none_or(|prev| !prev.same_target(&result));
        self.current.insert(id, result.clone());
        changed.then_some(result)
    }

    /// Ids seen in the previous windows but not in this one; the window becomes the baseline.
    pub fn end_window(&mut self) -> Vec<[u8; 4]> {
        self.end_window_at(Instant::now())
    }

    fn end_window_at(&mut self, now: Instant) -> Vec<[u8; 4]> {
        let mut gone: Vec<_> = self
            .known
            .keys()
            .filter(|id| !self.current.contains_key(*id))
            .copied()
            .collect();
        gone.sort_unstable();
        for (id, seen) in &self.seen {
            if self.current.contains_key(id) && seen.no_psm(now) {
                self.no_psm.insert(*id, now);
            }
        }
        self.no_psm
            .retain(|_, at| now.saturating_duration_since(*at) < NO_PSM_TTL);
        self.known = std::mem::take(&mut self.current);
        self.seen.retain(|id, _| self.known.contains_key(id));
        gone
    }
}

/// Scans for `duration`, then reports the phones that are gone. The discovery is stopped on
/// every exit path, including cancellation of the future, by dropping the stream: bluer ends
/// the BlueZ `StartDiscovery` session when it sees the stream closed, a moment later.
pub async fn scan_window(
    adapter: &Adapter,
    duration: Duration,
    legacy: LegacyReads,
    dedup: &mut Dedup,
    mut sink: impl FnMut(ScanEvent),
) -> anyhow::Result<()> {
    dedup.begin_window();
    debug!("scan window of {duration:?} starts");
    adapter
        .set_discovery_filter(DiscoveryFilter {
            transport: DiscoveryTransport::Le,
            duplicate_data: true,
            ..Default::default()
        })
        .await
        .context("set the discovery filter")?;
    let collected = collect(adapter, duration, legacy, dedup, &mut sink).await;
    // Also after a failure: the next window must not start under a late stop of this one.
    wait_discovery_stopped(adapter).await;
    collected?;
    for id in dedup.end_window() {
        sink(ScanEvent::Gone(id));
    }
    Ok(())
}

/// bluer ends the BlueZ discovery a moment after its stream is dropped. A new discovery started
/// before that is cancelled by the late stop and then sees nothing, which left a rescan that
/// followed a scan at once deaf (a send started right after the list was filled never found its
/// phone). Waits, for a short while at most, until BlueZ reports the discovery as stopped.
pub(crate) async fn wait_discovery_stopped(adapter: &Adapter) {
    for _ in 0..40 {
        if !adapter.is_discovering().await.unwrap_or(false) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    debug!("BlueZ still reports the discovery as running");
}

/// The BlueZ discovery stream closed before the window was over. Seen on hardware (a session
/// log): `bluetoothd` itself sent the mgmt Stop Discovery, 0.8 s to 11.6 s into a window, and
/// bluer ends its stream when `Discovering` turns false. The cause is not known; a send scans
/// once more when it happens.
#[derive(Debug)]
struct DiscoveryEnded;

impl std::fmt::Display for DiscoveryEnded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the discovery ended before the window did")
    }
}

impl std::error::Error for DiscoveryEnded {}

async fn collect(
    adapter: &Adapter,
    duration: Duration,
    legacy: LegacyReads,
    dedup: &mut Dedup,
    sink: &mut impl FnMut(ScanEvent),
) -> anyhow::Result<()> {
    let mut gate = LegacyGate::new(legacy);
    let stream = adapter
        .discover_devices_with_changes()
        .await
        .context("start the discovery")?;
    futures::pin_mut!(stream);
    let deadline = tokio::time::sleep(duration);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => return Ok(()),
            event = stream.next() => match event {
                Some(AdapterEvent::DeviceAdded(address)) => {
                    let left = deadline
                        .deadline()
                        .saturating_duration_since(tokio::time::Instant::now());
                    let found = tokio::select! {
                        // A stalled GATT connect cannot extend the scan beyond its deadline.
                        () = &mut deadline => return Ok(()),
                        found = read_device(adapter, address, &mut gate, LEGACY_GATT_TIMEOUT.min(left)) => found,
                    };
                    if let Some(found) = found
                        && let Some(new) = dedup.observe(found)
                    {
                        debug!(
                            "scan sees {} (PSM {:?}, {} dBm)",
                            BleTarget { endpoint_id: new.endpoint_id }.id(),
                            new.psm,
                            new.rssi.unwrap_or_default()
                        );
                        sink(ScanEvent::Found(new));
                    }
                }
                Some(_) => {}
                None => return Err(DiscoveryEnded.into()),
            },
        }
    }
}

/// BlueZ keeps devices it saw long ago: only one with an RSSI is on the air now.
async fn read_device(
    adapter: &Adapter,
    address: Address,
    gate: &mut LegacyGate,
    legacy_bound: Duration,
) -> Option<ScanResult> {
    let device = adapter.device(address).ok()?;
    let data = device.service_data().await.ok()??;
    let bytes = data.get(&Uuid::from_u16(SERVICE_UUID))?;
    let Some(rssi) = device.rssi().await.ok()? else {
        trace!("{address}: 0xFEF3 data but no RSSI, so not on the air now");
        return None;
    };
    let address_type = device.address_type().await.ok()?;
    let result = if let Some(header) = parse_legacy_header(bytes) {
        if !gate.admit(address) {
            trace!("{address}: legacy Nearby BLE header, slot-0 read not allowed now");
            return None;
        }
        debug!(
            "{address}: legacy Nearby BLE header ({:?}), reading GATT slot 0",
            header.advert_hash
        );
        match read_legacy_slot0(adapter, address, legacy_bound).await {
            Ok(full) => scan_result(address, address_type, Some(rssi), &full),
            Err(e) => {
                debug!("{address}: legacy BLE slot-0 read failed ({e:#})");
                None
            }
        }
    } else {
        // Fast 0x4a remains deliberately invisible (no name); other junk is ignored.
        let full = scan_result(address, address_type, Some(rssi), bytes);
        if full.is_some() {
            gate.saw_full(address);
        }
        full
    };
    if result.is_none() {
        trace!("{address}: unusable 0xFEF3 data ({} bytes)", bytes.len());
    }
    result
}

/// Fetch the complete regular advertisement from the Nearby service's slot-0
/// characteristic. BlueZ may return only the first ATT MTU, so offset reads
/// continue while the encoded record's length requires more data.
async fn read_legacy_slot0(
    adapter: &Adapter,
    address: Address,
    bound: Duration,
) -> anyhow::Result<Vec<u8>> {
    let device = adapter.device(address)?;
    let was_connected = device.is_connected().await.unwrap_or(false);
    // We must not disconnect an existing connection belonging to another user. The guard also
    // acts when the window deadline drops this future mid-connect.
    let mut guard = DisconnectOnDrop((!was_connected).then(|| device.clone()));
    let result = tokio::time::timeout(bound, async {
        if !was_connected {
            device.connect().await?;
        }
        while !device.is_services_resolved().await? {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        for service in device.services().await? {
            if service.uuid().await? != Uuid::from_u16(SERVICE_UUID) {
                continue;
            }
            for characteristic in service.characteristics().await? {
                if characteristic.uuid().await? != SLOT0_UUID {
                    continue;
                }
                let mut full = characteristic.read().await?;
                while let Some(len) = regular_length(&full)
                    && full.len() < len
                    && full.len() < MAX_SLOT0
                {
                    let more = characteristic
                        .read_ext(&CharacteristicReadRequest {
                            offset: u16::try_from(full.len())?,
                            ..Default::default()
                        })
                        .await?;
                    if more.is_empty() {
                        break;
                    }
                    full.extend_from_slice(&more);
                }
                // The GATT record, not the bloom header, is the authority for
                // endpoint ID, cleartext name and L2CAP PSM.
                anyhow::ensure!(full.len() <= MAX_SLOT0, "oversized GATT advertisement");
                anyhow::ensure!(
                    ReceiverAdvertisement::parse(&full).is_some(),
                    "invalid GATT advertisement"
                );
                return Ok::<Vec<u8>, anyhow::Error>(full);
            }
        }
        anyhow::bail!("missing Nearby GATT slot-0 characteristic");
    })
    .await;
    // Awaited, so the dial that follows does not race the disconnect. If this future is dropped
    // meanwhile, the guard is still armed and disconnects again.
    if let Some(device) = &guard.0 {
        disconnect(device).await;
    }
    guard.0 = None;
    result.context("legacy GATT slot-0 read timed out")?
}

/// What the `EndpointInfo` bus is told about a scan event.
pub fn endpoint_info(event: &ScanEvent) -> EndpointInfo {
    let (target, name, rtype, present) = match event {
        ScanEvent::Found(r) => (
            BleTarget {
                endpoint_id: r.endpoint_id,
            },
            Some(r.name.clone()),
            Some(r.device_type.clone()),
            true,
        ),
        ScanEvent::Gone(id) => (BleTarget { endpoint_id: *id }, None, None, false),
    };
    let id = target.id();
    EndpointInfo {
        fullname: format!("{id}._ble"),
        id,
        name,
        ip: None,
        port: None,
        rtype,
        present: Some(present),
        ble: Some(target),
    }
}

/// The sighting a send waits for: its phone, with a PSM to dial, or without one when the phone
/// is known to advertise none (`no_psm_known`, see [`Dedup::advertises_no_psm`]).
fn target_found(event: ScanEvent, target: BleTarget, no_psm_known: bool) -> Option<ScanResult> {
    match event {
        ScanEvent::Found(r)
            if r.endpoint_id == target.endpoint_id && (r.psm.is_some() || no_psm_known) =>
        {
            Some(r)
        }
        _ => None,
    }
}

/// What a send finds when the id it was given is not on the air.
#[derive(Debug, PartialEq)]
enum Renamed {
    /// Nobody else has the phone's name.
    No,
    /// The one other phone with its name (and device type, when that is known).
    Found(ScanResult),
    /// Several do: which one is the phone cannot be told.
    Ambiguous(Vec<[u8; 4]>),
}

/// A phone can draw a new endpoint id (Samsung does after the screen is locked and unlocked)
/// while the list still shows the old one. Among the sightings of a window, looks for the
/// phone `wanted` by its advertised name; an empty name or a name shared by several phones
/// proves nothing.
fn renamed_sighting(
    seen: &HashMap<[u8; 4], ScanResult>,
    wanted: [u8; 4],
    name: &str,
    device_type: Option<&DeviceType>,
) -> Renamed {
    if name.is_empty() {
        return Renamed::No;
    }
    let mut same: Vec<&ScanResult> = seen
        .values()
        .filter(|r| {
            r.endpoint_id != wanted
                && r.name == name
                && device_type.is_none_or(|t| *t == r.device_type)
        })
        .collect();
    same.sort_by_key(|r| r.endpoint_id);
    match same.as_slice() {
        [] => Renamed::No,
        [one] => Renamed::Found((*one).clone()),
        many => Renamed::Ambiguous(many.iter().map(|r| r.endpoint_id).collect()),
    }
}

fn interval_elapsed(last_start: Option<Instant>, now: Instant) -> bool {
    last_start.is_none_or(|l| now.saturating_duration_since(l) >= MIN_SCAN_INTERVAL)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanOutcome {
    Done,
    /// No Wi-Fi link: a BLE-only send would be uselessly slow, so nothing is scanned.
    NoLink,
    /// A window, a dial or a transfer is going on.
    Busy,
    /// The previous window started less than [`MIN_SCAN_INTERVAL`] ago.
    TooSoon,
    NoAdapter,
    Failed,
    /// A send took the adapter before the window was over.
    Interrupted,
}

#[derive(Debug, Default)]
struct ScanState {
    dedup: Dedup,
    last_start: Option<Instant>,
}

/// Owner of the adapter's discovery: scans and sends exclude each other through `state`.
#[derive(Debug, Default)]
pub struct BleScanner {
    state: tokio::sync::Mutex<ScanState>,
    /// Cancels the list window under way; a send does it to get the slot at once.
    window: Mutex<CancellationToken>,
}

pub static SCANNER: Lazy<BleScanner> = Lazy::new(BleScanner::default);

async fn open_adapter() -> anyhow::Result<(bluer::Session, Adapter)> {
    let session = bluer::Session::new().await.context("connect to BlueZ")?;
    let adapter = session
        .default_adapter()
        .await
        .context("no Bluetooth adapter")?;
    if !adapter.is_powered().await? {
        adapter
            .set_powered(true)
            .await
            .context("power the adapter")?;
    }
    Ok((session, adapter))
}

/// Scans until `target` shows up with a PSM, which ends the scan at once. A sighting without a
/// PSM is kept and returned at the end of the window (the phone is then reached over GATT),
/// unless `no_psm_known`: the phone is known to have none, so the first sighting ends the scan.
/// When the id is not seen at all but exactly one other phone advertises the same `name` (and
/// `device_type`, when known), that phone is returned: it drew a new id. The scan is over, and
/// BlueZ's discovery stopped, when this returns.
async fn find_target(
    adapter: &Adapter,
    target: BleTarget,
    no_psm_known: bool,
    name: &str,
    device_type: Option<DeviceType>,
    window: Duration,
    ctk: &CancellationToken,
) -> anyhow::Result<ScanResult> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut dedup = Dedup::default();
    let without_psm: Mutex<Option<ScanResult>> = Mutex::new(None);
    // The latest sighting of every phone, for the rename fallback.
    let sightings: Mutex<HashMap<[u8; 4], ScanResult>> = Mutex::new(HashMap::new());
    let mut on_event = |event: ScanEvent| {
        if let ScanEvent::Found(r) = &event {
            if r.endpoint_id == target.endpoint_id
                && r.psm.is_none()
                && let Ok(mut kept) = without_psm.lock()
            {
                *kept = Some(r.clone());
            }
            if let Ok(mut all) = sightings.lock() {
                // A sighting with a PSM is not replaced by one without (a Pixel sends it rarely).
                let keep = r.psm.is_none()
                    && all
                        .get(&r.endpoint_id)
                        .is_some_and(|old| old.psm.is_some() && old.address == r.address);
                if !keep {
                    all.insert(r.endpoint_id, r.clone());
                }
            }
            debug!(
                "rescan sees {} (PSM {:?}, {} dBm), wanted {}",
                BleTarget {
                    endpoint_id: r.endpoint_id
                }
                .id(),
                r.psm,
                r.rssi.unwrap_or_default(),
                target.id()
            );
        }
        if let Some(r) = target_found(event, target, no_psm_known) {
            let _ = tx.send(r);
        }
    };
    let mut retried = false;
    let outcome = loop {
        let started = Instant::now();
        let outcome = {
            let scan = scan_window(
                adapter,
                window,
                SEND_LEGACY_READS,
                &mut dedup,
                &mut on_event,
            );
            tokio::pin!(scan);
            tokio::select! {
                Some(found) = rx.recv() => Ok(found),
                r = &mut scan => match r {
                    Err(e) => Err(e),
                    Ok(()) => match rx
                        .try_recv()
                        .ok()
                        .or_else(|| without_psm.lock().ok().and_then(|kept| kept.clone()))
                    {
                        Some(found) => Ok(found),
                        None => {
                            let seen = sightings.lock().map(|s| s.clone()).unwrap_or_default();
                            not_seen(&seen, target, name, device_type.as_ref(), window)
                        }
                    },
                },
                () = ctk.cancelled() => Err(anyhow::anyhow!("cancelled while looking for {}", target.id())),
            }
        };
        // The scan future was dropped with the block above, which is what stops the discovery.
        match outcome {
            Err(e) if !retried && e.downcast_ref::<DiscoveryEnded>().is_some() => {
                retried = true;
                debug!(
                    "rescan for {}: {e:#} after {:?}, scanning once more",
                    target.id(),
                    started.elapsed()
                );
                // The new discovery must not start under the late stop of this one.
                wait_discovery_stopped(adapter).await;
            }
            outcome => break outcome,
        }
    };
    // Dialing while BlueZ still stops the discovery made the dial fail ("operation in progress").
    wait_discovery_stopped(adapter).await;
    outcome
}

/// The end of a window without `target`: a renamed phone, or the error to show.
fn not_seen(
    seen: &HashMap<[u8; 4], ScanResult>,
    target: BleTarget,
    name: &str,
    device_type: Option<&DeviceType>,
    window: Duration,
) -> anyhow::Result<ScanResult> {
    match renamed_sighting(seen, target.endpoint_id, name, device_type) {
        Renamed::Found(r) => {
            info!(
                "{} is now {} (same name)",
                target.id(),
                BleTarget {
                    endpoint_id: r.endpoint_id
                }
                .id()
            );
            Ok(r)
        }
        Renamed::Ambiguous(ids) => {
            let ids: Vec<String> = ids
                .into_iter()
                .map(|endpoint_id| BleTarget { endpoint_id }.id())
                .collect();
            Err(anyhow::anyhow!(
                "{} not seen within {window:?}, and several phones are named \"{name}\" ({}): not guessing which one it is",
                target.id(),
                ids.join(", ")
            ))
        }
        Renamed::No => Err(anyhow::anyhow!(
            "{} not seen within {window:?}: is Quick Share open on it, visible to Everyone?",
            target.id()
        )),
    }
}

impl BleScanner {
    /// Takes the scan slot, or says why not.
    fn admit(&self, now: Instant) -> Result<MutexGuard<'_, ScanState>, ScanOutcome> {
        let mut state = self.state.try_lock().map_err(|_| ScanOutcome::Busy)?;
        if !interval_elapsed(state.last_start, now) {
            return Err(ScanOutcome::TooSoon);
        }
        state.last_start = Some(now);
        Ok(state)
    }

    /// Runs one window and reports to `sender`. Never fails: whatever keeps it from running
    /// is in the outcome and the debug log.
    pub async fn scan(&self, sender: &broadcast::Sender<EndpointInfo>) -> ScanOutcome {
        if crate::wifi_link().is_none() {
            debug!("BLE scan skipped: no Wi-Fi link, a BLE-only send would be too slow");
            return ScanOutcome::NoLink;
        }
        let mut state = match self.admit(Instant::now()) {
            Ok(state) => state,
            Err(outcome) => {
                debug!("BLE scan skipped: {outcome:?}");
                return outcome;
            }
        };
        let (_session, adapter) = match open_adapter().await {
            Ok(a) => a,
            Err(e) => {
                debug!("BLE scan skipped: {e:#}");
                return ScanOutcome::NoAdapter;
            }
        };
        let interrupt = CancellationToken::new();
        if let Ok(mut window) = self.window.lock() {
            *window = interrupt.clone();
        }
        let result = tokio::select! {
            r = scan_window(&adapter, SCAN_WINDOW, LegacyReads::Off, &mut state.dedup, |event| {
                let _ = sender.send(endpoint_info(&event));
            }) => r,
            // The window is dropped here, which stops the discovery; the sightings it made stay
            // in the dedup for the send.
            () = interrupt.cancelled() => {
                wait_discovery_stopped(&adapter).await;
                debug!("BLE scan interrupted by a send");
                return ScanOutcome::Interrupted;
            }
        };
        match result {
            Ok(()) => ScanOutcome::Done,
            Err(e) => {
                warn!("BLE scan failed: {e:#}");
                ScanOutcome::Failed
            }
        }
    }

    /// Scans for `target` again (its address and PSM changed since the last window), stops the
    /// scan, dials it (L2CAP when it advertises a PSM, GATT otherwise) and runs the send
    /// session, holding the scan slot all along.
    #[allow(clippy::too_many_arguments)]
    pub async fn send(
        &self,
        target: BleTarget,
        local_endpoint_id: [u8; 4],
        id: String,
        name: String,
        payload: OutboundPayload,
        sender: broadcast::Sender<ChannelMessage>,
        ctk: CancellationToken,
    ) -> anyhow::Result<()> {
        // The sender watchdog starts before waiting for the adapter (which may still be scanning).
        let started = Instant::now();
        // A list window under way would keep the slot for up to SCAN_WINDOW.
        if let Ok(window) = self.window.lock() {
            window.cancel();
        }
        let mut slot = tokio::select! {
            slot = self.state.lock() => slot,
            () = ctk.cancelled() => bail!("cancelled before the Bluetooth adapter was free"),
        };
        let (_session, adapter) = open_adapter().await?;
        let no_psm_known = slot.dedup.advertises_no_psm(&target.endpoint_id);
        let device_type = slot.dedup.device_type(&target.endpoint_id);
        let scan = find_target(
            &adapter,
            target,
            no_psm_known,
            &name,
            device_type,
            RESCAN_WINDOW,
            &ctk,
        )
        .await?;
        // The phone may have drawn a new id; `id` stays the one the app knows the transfer by.
        let target = BleTarget {
            endpoint_id: scan.endpoint_id,
        };
        if scan.psm.is_some() {
            slot.dedup.forget_no_psm(&target.endpoint_id);
        } else {
            debug!("{} advertises no PSM: sending over GATT", target.id());
        }
        match dial_with_fallback(
            scan.psm.is_some(),
            dial_l2cap(&adapter, &scan),
            dial_gatt(&adapter, &scan),
            BLE_START_BUDGET.saturating_sub(started.elapsed()),
            &ctk,
        )
        .await?
        {
            Dialed::L2cap(stream) => {
                // A failed send on this stream is terminal; never start GATT afterwards.
                send_over_l2cap(stream, local_endpoint_id, id, name, payload, sender, ctk).await?;
            }
            Dialed::Gatt(link) => {
                send_over_gatt(link, local_endpoint_id, id, name, payload, sender, ctk).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod dial_fallback_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    const fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    async fn never<T>() -> anyhow::Result<T> {
        panic!("this dial must not run")
    }

    #[test]
    fn l2cap_gets_the_whole_dial_timeout_when_there_is_time() {
        // The scan of a send ends at 12 s at the latest.
        assert_eq!(l2cap_budget(BLE_START_BUDGET - RESCAN_WINDOW), DIAL_TIMEOUT);
        assert_eq!(l2cap_budget(BLE_START_BUDGET), DIAL_TIMEOUT);
        assert_eq!(l2cap_budget(secs(10)), secs(8));
        assert_eq!(l2cap_budget(FIRST_FRAME_RESERVE), Duration::ZERO);
        assert_eq!(l2cap_budget(Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn the_gatt_fallback_needs_its_whole_dial() {
        let needed = GATT_DIAL_TIMEOUT + FIRST_FRAME_RESERVE;
        assert!(gatt_fits(needed));
        assert!(!gatt_fits(needed - Duration::from_millis(1)));
    }

    #[test]
    fn a_full_l2cap_dial_after_the_scan_fits_the_ble_start_watchdog() {
        assert!(RESCAN_WINDOW + DIAL_TIMEOUT + FIRST_FRAME_RESERVE <= BLE_START_BUDGET);
        // After the longest scan and a failed dial, GATT no longer fits.
        assert!(!gatt_fits(BLE_START_BUDGET - RESCAN_WINDOW - DIAL_TIMEOUT));
    }

    #[tokio::test]
    async fn a_failed_l2cap_dial_falls_back_to_gatt() {
        let called = AtomicBool::new(false);
        let selected = dial_with_fallback(
            true,
            async { Err::<u8, _>(anyhow::anyhow!("L2CAP failed")) },
            async {
                called.store(true, Ordering::SeqCst);
                Ok::<&str, anyhow::Error>("GATT")
            },
            BLE_START_BUDGET,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(selected, Dialed::Gatt("GATT")));
        assert!(called.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn an_l2cap_timeout_falls_back_to_gatt() {
        let started = tokio::time::Instant::now();
        let selected = dial_with_fallback(
            true,
            std::future::pending::<anyhow::Result<u8>>(),
            async { Ok::<&str, anyhow::Error>("GATT") },
            BLE_START_BUDGET,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(selected, Dialed::Gatt("GATT")));
        assert_eq!(started.elapsed(), DIAL_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn the_gatt_fallback_is_skipped_when_it_no_longer_fits() {
        // A scan that ended at 12 s leaves 28 s; the dial fails after 5 s.
        let outcome = dial_with_fallback(
            true,
            async {
                tokio::time::sleep(secs(5)).await;
                Err::<u8, _>(anyhow::anyhow!("connection refused"))
            },
            never::<&str>(),
            BLE_START_BUDGET - RESCAN_WINDOW,
            &CancellationToken::new(),
        )
        .await;
        let error = outcome.err().unwrap();
        assert!(error.to_string().contains("connection refused"), "{error}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_l2cap_dial_after_a_long_scan_gets_all_of_its_timeout() {
        let started = tokio::time::Instant::now();
        let outcome = dial_with_fallback(
            true,
            async {
                tokio::time::sleep(DIAL_TIMEOUT - secs(1)).await;
                Ok::<u8, anyhow::Error>(7)
            },
            never::<&str>(),
            BLE_START_BUDGET - RESCAN_WINDOW,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, Dialed::L2cap(7)));
        assert_eq!(started.elapsed(), DIAL_TIMEOUT - secs(1));
    }

    #[tokio::test]
    async fn without_a_psm_the_dial_goes_straight_to_gatt() {
        let selected = dial_with_fallback(
            false,
            never::<u8>(),
            async { Ok::<&str, anyhow::Error>("GATT") },
            BLE_START_BUDGET - RESCAN_WINDOW,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(selected, Dialed::Gatt("GATT")));
    }

    #[tokio::test]
    async fn no_time_left_dials_nothing() {
        let outcome = dial_with_fallback(
            true,
            never::<u8>(),
            never::<&str>(),
            FIRST_FRAME_RESERVE,
            &CancellationToken::new(),
        )
        .await;
        let error = outcome.err().unwrap();
        assert!(error.to_string().contains("no time remains"), "{error}");
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_while_dialing_l2cap_stops_the_dial() {
        let ctk = CancellationToken::new();
        let canceller = ctk.clone();
        tokio::spawn(async move {
            tokio::time::sleep(secs(3)).await;
            canceller.cancel();
        });
        let started = tokio::time::Instant::now();
        let outcome = dial_with_fallback(
            true,
            std::future::pending::<anyhow::Result<u8>>(),
            never::<&str>(),
            BLE_START_BUDGET,
            &ctk,
        )
        .await;
        let error = outcome.err().unwrap();
        assert!(
            error.to_string().contains("cancelled while dialing L2CAP"),
            "{error}"
        );
        assert_eq!(started.elapsed(), secs(3));
    }

    #[tokio::test]
    async fn successful_l2cap_never_dials_gatt() {
        let selected = dial_with_fallback(
            true,
            async { Ok::<u8, anyhow::Error>(17) },
            async { panic!("must not dial GATT after L2CAP succeeds") as anyhow::Result<()> },
            BLE_START_BUDGET,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(selected, Dialed::L2cap(17)));
    }
}

#[cfg(test)]
mod hardware_probe {
    use super::*;

    /// Hand test on a real adapter: open Quick Share on a phone (Everyone), then run
    /// `cargo test -p rqs_lib --no-run`, and execute the printed test binary on the host with
    /// `--ignored --nocapture probe_nearby_phones`.
    #[tokio::test]
    #[ignore = "needs a BlueZ adapter and a phone in Receive mode"]
    async fn probe_nearby_phones() {
        let session = bluer::Session::new().await.expect("BlueZ session");
        let adapter = session.default_adapter().await.expect("adapter");
        adapter.set_powered(true).await.expect("power on");
        let mut dedup = Dedup::default();
        let started = std::time::Instant::now();
        scan_window(
            &adapter,
            SCAN_WINDOW,
            SEND_LEGACY_READS,
            &mut dedup,
            |event| {
                println!("{:>5.1}s {event:?}", started.elapsed().as_secs_f32());
            },
        )
        .await
        .expect("scan window");
        println!("window over after {:.1}s", started.elapsed().as_secs_f32());
    }

    /// End to end through the public `RQS` API, no app: `scan_ble`, pick the phone from the
    /// `EndpointInfo` bus, send a file as `SendInfo { id: "ble:..." }` with a `WifiLink` that
    /// only prints the offer and fails. Env: `KS_ENDPOINT` (4 character id), `KS_SIZE_KB`
    /// (default 3072). Run the test binary on the host like `probe_nearby_phones`, with
    /// `--ignored --nocapture send_through_rqs_api`; press Accept on the phone.
    #[tokio::test]
    #[ignore = "needs a BlueZ adapter and a phone in Receive mode"]
    async fn send_through_rqs_api() {
        use crate::channel::{ChannelMessage, Message};
        use crate::hdl::TransferState;
        use crate::hdl::ble_send::dial::hardware_probe::ObserverLink;
        use crate::upgrade::WifiLink;
        use crate::{OutboundPayload, RQS, SendInfo, Visibility};

        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "rqs_lib=info".into()),
            )
            .try_init();
        let _guard = crate::upgrade::tests::SERIAL.lock().await;
        let kb: usize = std::env::var("KS_SIZE_KB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3072);
        let path = std::env::temp_dir().join(format!("kuickshare-rqs-{}.bin", std::process::id()));
        let mut data = vec![0u8; kb * 1024];
        rand::Rng::fill_bytes(&mut rand::rng(), &mut data);
        std::fs::write(&path, data).expect("write the temp file");

        let (offer_tx, mut offer_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut rqs = RQS::new(Visibility::Invisible, None, None, Some("KS probe".into()));
        let link: std::sync::Arc<dyn WifiLink> = std::sync::Arc::new(ObserverLink(offer_tx));
        rqs.set_wifi_link(Some(link));
        let (send_tx, _) = rqs.run().await.expect("run");
        let mut bus = rqs.message_sender.subscribe();

        let (found_tx, mut found_rx) = broadcast::channel(16);
        rqs.scan_ble(found_tx).expect("scan_ble");
        let wanted = std::env::var("KS_ENDPOINT").ok();
        let phone = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let ei = found_rx.recv().await.expect("endpoint bus");
                println!("endpoint {ei:?}");
                let ok = ei.present == Some(true)
                    && ei.ble.is_some_and(|b| {
                        wanted
                            .as_deref()
                            .is_none_or(|w| b.endpoint_id.as_slice() == w.as_bytes())
                    });
                if ok {
                    return ei;
                }
            }
        })
        .await
        .expect("no phone seen in 20 s: is Quick Share open on it, visible to Everyone?");

        send_tx
            .send(SendInfo {
                id: phone.id.clone(),
                name: phone.name.clone().unwrap_or_default(),
                addr: String::new(),
                ob: OutboundPayload::Files(vec![path.to_string_lossy().into_owned()]),
            })
            .await
            .expect("queue the send");
        println!("sending {kb} KB to {}, press Accept on the phone", phone.id);

        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(Duration::from_secs(300), async {
            loop {
                tokio::select! {
                    offer = offer_rx.recv() => println!("OFFER OBSERVED: {offer:?}"),
                    m = bus.recv() => match m {
                        Ok(ChannelMessage { id, msg: Message::Client(c) }) => {
                            println!("{:>6.1}s {id} {:?}", started.elapsed().as_secs_f32(), c.state);
                            assert_eq!(id, phone.id, "bus id must equal SendInfo.id");
                            if matches!(c.state, Some(TransferState::Finished | TransferState::Disconnected)) {
                                return c.state;
                            }
                        }
                        Ok(_) => {}
                        Err(e) => panic!("bus: {e}"),
                    },
                }
            }
        })
        .await;
        rqs.set_wifi_link(None);
        rqs.stop().await;
        let _ = std::fs::remove_file(&path);
        assert_eq!(outcome.expect("timed out"), Some(TransferState::Finished));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hdl::ble_receiver::service_data::DEVICE_TYPE_LAPTOP;

    fn adv(id: &[u8; 4], name: &str) -> Vec<u8> {
        let mut a = ReceiverAdvertisement::new(*id, 1, [9; 16], name);
        a.psm = Some(161);
        a.to_bytes()
    }

    fn addr(last: u8) -> Address {
        Address::new([0x4a, 0, 0, 0, 0, last])
    }

    fn result(id: &[u8; 4], last: u8, rssi: i16) -> ScanResult {
        scan_result(
            addr(last),
            AddressType::LeRandom,
            Some(rssi),
            &adv(id, "Pixel"),
        )
        .unwrap()
    }

    #[test]
    fn a_phone_becomes_a_result() {
        let r = scan_result(
            addr(1),
            AddressType::LeRandom,
            Some(-60),
            &adv(b"ABCD", "Pixel"),
        );
        assert_eq!(
            r,
            Some(ScanResult {
                endpoint_id: *b"ABCD",
                name: "Pixel".into(),
                device_type: DeviceType::Phone,
                address: addr(1),
                address_type: AddressType::LeRandom,
                psm: Some(161),
                rssi: Some(-60),
            })
        );
    }

    #[test]
    fn other_service_data_is_not_a_result() {
        let fast = [0x4a, 0x03, 1, 2, 3, 0x62, 0xf1];
        assert_eq!(
            scan_result(addr(1), AddressType::LeRandom, None, &fast),
            None
        );
        assert_eq!(scan_result(addr(1), AddressType::LeRandom, None, &[]), None);
        let mut hidden = adv(b"ABCD", "Pixel");
        hidden[17] |= 0x10;
        assert_eq!(
            scan_result(addr(1), AddressType::LeRandom, None, &hidden),
            None
        );
        let laptop = ReceiverAdvertisement::new(*b"ABCD", DEVICE_TYPE_LAPTOP, [0; 16], "L");
        let r = scan_result(addr(1), AddressType::LePublic, None, &laptop.to_bytes()).unwrap();
        assert_eq!(
            (r.device_type, r.psm, r.rssi),
            (DeviceType::Laptop, None, None)
        );
    }

    #[test]
    fn rssi_jitter_is_not_reported_but_a_new_address_is() {
        let mut d = Dedup::default();
        assert!(d.observe(result(b"ABCD", 1, -60)).is_some());
        assert_eq!(d.observe(result(b"ABCD", 1, -70)), None);
        let moved = d.observe(result(b"ABCD", 2, -70)).unwrap();
        assert_eq!(moved.address, addr(2));
    }

    #[test]
    fn a_name_change_is_reported() {
        let mut d = Dedup::default();
        d.observe(result(b"ABCD", 1, -60));
        let renamed = scan_result(
            addr(1),
            AddressType::LeRandom,
            Some(-60),
            &adv(b"ABCD", "New"),
        )
        .unwrap();
        assert!(d.observe(renamed).is_some());
    }

    #[test]
    fn devices_missing_from_a_window_are_gone_once() {
        let mut d = Dedup::default();
        d.begin_window();
        d.observe(result(b"AAAA", 1, -60));
        d.observe(result(b"BBBB", 2, -60));
        assert!(d.end_window().is_empty());

        d.begin_window();
        // Same phone again: nothing to report; the other one stays silent.
        assert_eq!(d.observe(result(b"AAAA", 1, -61)), None);
        assert_eq!(d.end_window(), vec![*b"BBBB"]);

        d.begin_window();
        assert_eq!(d.end_window(), vec![*b"AAAA"]);
        d.begin_window();
        assert!(d.end_window().is_empty());
    }

    #[test]
    fn a_phone_that_comes_back_is_reported_again() {
        let mut d = Dedup::default();
        d.observe(result(b"AAAA", 1, -60));
        d.end_window();
        d.begin_window();
        d.end_window();
        d.begin_window();
        assert!(d.observe(result(b"AAAA", 1, -60)).is_some());
    }

    #[test]
    fn a_cancelled_window_leaves_no_residue() {
        let mut d = Dedup::default();
        d.observe(result(b"AAAA", 1, -60));
        d.end_window();
        d.begin_window();
        d.observe(result(b"BBBB", 2, -60));
        // Cancelled here; the next window sees only AAAA.
        d.begin_window();
        d.observe(result(b"AAAA", 1, -60));
        assert!(d.end_window().is_empty());
    }

    #[test]
    fn found_becomes_a_present_ble_endpoint() {
        let ei = endpoint_info(&ScanEvent::Found(result(b"G483", 1, -60)));
        assert_eq!(ei.id, "ble:G483");
        assert_eq!(ei.fullname, "ble:G483._ble");
        assert_eq!(ei.name.as_deref(), Some("Pixel"));
        assert!(matches!(ei.rtype, Some(DeviceType::Phone)));
        assert_eq!((ei.ip, ei.port), (None, None));
        assert_eq!(ei.present, Some(true));
        assert_eq!(
            ei.ble,
            Some(BleTarget {
                endpoint_id: *b"G483"
            })
        );
    }

    #[test]
    fn gone_becomes_an_absent_ble_endpoint() {
        let ei = endpoint_info(&ScanEvent::Gone(*b"G483"));
        assert_eq!(ei.id, "ble:G483");
        assert_eq!(ei.present, Some(false));
        assert_eq!(ei.name, None);
        assert!(ei.ble.is_some());
        let raw = endpoint_info(&ScanEvent::Gone([0, 1, 2, 3]));
        assert_eq!(raw.id, "ble:00010203");
    }

    fn without(id: &[u8; 4]) -> ScanResult {
        ScanResult {
            psm: None,
            ..result(id, 2, -60)
        }
    }

    #[test]
    fn no_psm_needs_an_early_sighting_and_goes_stale() {
        let t0 = Instant::now();
        let secs = Duration::from_secs;
        let with = result(b"PIXL", 1, -60);
        let mut d = Dedup::default();
        d.begin_window();
        d.observe_at(with.clone(), t0 + secs(1));
        d.observe_at(
            ScanResult {
                psm: None,
                address: addr(9),
                ..with.clone()
            },
            t0 + secs(2),
        );
        d.observe_at(without(b"SAMS"), t0 + secs(3));
        // First heard in the last seconds: its full advertisement may not have come yet.
        d.observe_at(without(b"LATE"), t0 + secs(14));
        d.end_window_at(t0 + secs(15));
        let end = t0 + secs(15);
        assert!(!d.no_psm_at(b"PIXL", end));
        assert!(d.no_psm_at(b"SAMS", end));
        assert!(!d.no_psm_at(b"LATE", end));
        assert!(!d.no_psm_at(b"NONE", end));
        assert!(d.no_psm_at(b"SAMS", end + NO_PSM_TTL - secs(1)));
        assert!(!d.no_psm_at(b"SAMS", end + NO_PSM_TTL));
        // A later sighting with a PSM withdraws the verdict.
        d.observe_at(
            ScanResult {
                psm: Some(133),
                ..without(b"SAMS")
            },
            end + secs(20),
        );
        assert!(!d.no_psm_at(b"SAMS", end + secs(20)));
    }

    #[test]
    fn no_psm_holds_while_the_window_is_still_running() {
        let t0 = Instant::now();
        let secs = Duration::from_secs;
        let mut d = Dedup::default();
        d.begin_window();
        d.observe_at(without(b"SAMS"), t0 + secs(2));
        d.observe_at(
            ScanResult {
                psm: Some(133),
                ..without(b"PIXL")
            },
            t0 + secs(2),
        );
        assert!(!d.no_psm_at(b"SAMS", t0 + secs(11)));
        assert!(d.no_psm_at(b"SAMS", t0 + secs(12)));
        assert!(!d.no_psm_at(b"PIXL", t0 + secs(12)));
        // A PSM that shows up after the 10 s mark wins at once.
        d.observe_at(
            ScanResult {
                psm: Some(133),
                ..without(b"SAMS")
            },
            t0 + secs(13),
        );
        assert!(!d.no_psm_at(b"SAMS", t0 + secs(13)));
    }

    #[test]
    fn sightings_and_verdicts_survive_the_next_window() {
        let t0 = Instant::now();
        let secs = Duration::from_secs;
        let mut d = Dedup::default();
        d.begin_window();
        d.observe_at(without(b"SAMS"), t0 + secs(1));
        d.end_window_at(t0 + secs(15));
        // A new window, phone not heard yet: the verdict and the sighting still count.
        d.begin_window();
        assert!(d.no_psm_at(b"SAMS", t0 + secs(16)));
        // Heard again: the first sighting is still the old one.
        d.observe_at(without(b"SAMS"), t0 + secs(17));
        assert!(d.no_psm_at(b"SAMS", t0 + secs(17)));
        // A cancelled window (no end_window) keeps them too.
        d.begin_window();
        assert!(d.no_psm_at(b"SAMS", t0 + secs(18)));
        // Missing from a completed window drops the sighting, not the verdict until its TTL.
        d.end_window_at(t0 + secs(30));
        assert!(d.seen.is_empty());
        assert!(d.no_psm_at(b"SAMS", t0 + secs(31)));
        assert!(!d.no_psm_at(b"SAMS", t0 + secs(30) + NO_PSM_TTL));
    }

    #[test]
    fn forget_no_psm_sticks_until_the_phone_is_gone() {
        let t0 = Instant::now();
        let secs = Duration::from_secs;
        let mut d = Dedup::default();
        d.begin_window();
        d.observe_at(without(b"SAMS"), t0);
        d.end_window_at(t0 + secs(15));
        assert!(d.no_psm_at(b"SAMS", t0 + secs(15)));
        d.forget_no_psm(b"SAMS");
        assert!(!d.no_psm_at(b"SAMS", t0 + secs(16)));
    }

    #[test]
    fn a_send_waits_only_for_its_phone_with_a_psm() {
        let t = BleTarget {
            endpoint_id: *b"ABCD",
        };
        let hit = result(b"ABCD", 1, -60);
        assert_eq!(
            target_found(ScanEvent::Found(hit.clone()), t, false),
            Some(hit.clone())
        );
        assert_eq!(
            target_found(ScanEvent::Found(result(b"WXYZ", 1, -60)), t, false),
            None
        );
        assert_eq!(target_found(ScanEvent::Gone(*b"ABCD"), t, true), None);
        let no_psm = ScanResult { psm: None, ..hit };
        assert_eq!(
            target_found(ScanEvent::Found(no_psm.clone()), t, false),
            None
        );
        assert_eq!(
            target_found(ScanEvent::Found(no_psm.clone()), t, true),
            Some(no_psm)
        );
    }

    #[test]
    fn windows_start_at_least_the_interval_apart() {
        let t0 = Instant::now();
        assert!(interval_elapsed(None, t0));
        assert!(!interval_elapsed(Some(t0), t0));
        assert!(!interval_elapsed(
            Some(t0),
            t0 + MIN_SCAN_INTERVAL - Duration::from_millis(1)
        ));
        assert!(interval_elapsed(Some(t0), t0 + MIN_SCAN_INTERVAL));
        // A clock that looks backwards never lets a window through by accident.
        assert!(!interval_elapsed(Some(t0 + MIN_SCAN_INTERVAL), t0));
    }

    #[test]
    fn overlapping_and_early_windows_are_refused() {
        let s = BleScanner::default();
        let t0 = Instant::now();
        let slot = s.admit(t0).unwrap();
        assert_eq!(
            s.admit(t0 + MIN_SCAN_INTERVAL).err(),
            Some(ScanOutcome::Busy)
        );
        drop(slot);
        assert_eq!(
            s.admit(t0 + Duration::from_secs(1)).err(),
            Some(ScanOutcome::TooSoon)
        );
        assert!(s.admit(t0 + MIN_SCAN_INTERVAL).is_ok());
    }

    #[tokio::test]
    async fn no_scan_without_a_wifi_link() {
        let _guard = crate::upgrade::tests::SERIAL.lock().await;
        *crate::WIFI_LINK.write().unwrap() = None;
        let s = BleScanner::default();
        let (tx, mut rx) = broadcast::channel(4);
        assert_eq!(s.scan(&tx).await, ScanOutcome::NoLink);
        assert!(rx.try_recv().is_err());
        // The refused call did not use up the rate limit.
        assert!(s.admit(Instant::now()).is_ok());
    }

    fn named(id: &[u8; 4], name: &str, device_type: DeviceType) -> ScanResult {
        ScanResult {
            name: name.into(),
            device_type,
            psm: None,
            ..result(id, 1, -60)
        }
    }

    fn sightings(list: &[ScanResult]) -> HashMap<[u8; 4], ScanResult> {
        list.iter().map(|r| (r.endpoint_id, r.clone())).collect()
    }

    #[test]
    fn a_phone_with_a_new_id_is_found_by_its_name() {
        let samsung = named(b"4IX7", "Galaxy A55 5G", DeviceType::Phone);
        let seen = sightings(&[
            named(b"HNBG", "Pixel 7", DeviceType::Phone),
            samsung.clone(),
        ]);
        assert_eq!(
            renamed_sighting(&seen, *b"ARWA", "Galaxy A55 5G", Some(&DeviceType::Phone)),
            Renamed::Found(samsung.clone())
        );
        // The device type is only checked when it is known.
        assert_eq!(
            renamed_sighting(&seen, *b"ARWA", "Galaxy A55 5G", None),
            Renamed::Found(samsung)
        );
        assert_eq!(
            renamed_sighting(&seen, *b"ARWA", "Galaxy A55 5G", Some(&DeviceType::Laptop)),
            Renamed::No
        );
    }

    #[test]
    fn a_rename_is_not_guessed() {
        let a = named(b"4IX7", "Galaxy A55 5G", DeviceType::Phone);
        let b = named(b"Q2ZK", "Galaxy A55 5G", DeviceType::Phone);
        let seen = sightings(&[a, b]);
        assert_eq!(
            renamed_sighting(&seen, *b"ARWA", "Galaxy A55 5G", None),
            Renamed::Ambiguous(vec![*b"4IX7", *b"Q2ZK"])
        );
        // No name to go by, or no phone with it.
        assert_eq!(renamed_sighting(&seen, *b"ARWA", "", None), Renamed::No);
        assert_eq!(
            renamed_sighting(&seen, *b"ARWA", "Pixel 7", None),
            Renamed::No
        );
        // The wanted id itself never counts as another phone.
        assert_eq!(
            renamed_sighting(&seen, *b"4IX7", "Galaxy A55 5G", None),
            Renamed::Found(named(b"Q2ZK", "Galaxy A55 5G", DeviceType::Phone))
        );
    }

    #[test]
    fn the_error_names_the_ambiguity() {
        let t = BleTarget {
            endpoint_id: *b"ARWA",
        };
        let seen = sightings(&[
            named(b"4IX7", "Galaxy A55 5G", DeviceType::Phone),
            named(b"Q2ZK", "Galaxy A55 5G", DeviceType::Phone),
        ]);
        let secs = Duration::from_secs(12);
        let e = not_seen(&seen, t, "Galaxy A55 5G", None, secs).unwrap_err();
        assert!(e.to_string().contains("ble:4IX7, ble:Q2ZK"), "{e}");
        let e = not_seen(&seen, t, "Pixel 7", None, secs).unwrap_err();
        assert!(e.to_string().contains("Quick Share open"), "{e}");
        let one = sightings(&[named(b"4IX7", "Galaxy A55 5G", DeviceType::Phone)]);
        assert_eq!(
            not_seen(&one, t, "Galaxy A55 5G", None, secs)
                .unwrap()
                .endpoint_id,
            *b"4IX7"
        );
    }

    #[test]
    fn dedup_remembers_the_device_type() {
        let mut d = Dedup::default();
        d.begin_window();
        d.observe(named(b"ARWA", "Galaxy A55 5G", DeviceType::Phone));
        assert_eq!(d.device_type(b"ARWA"), Some(DeviceType::Phone));
        assert_eq!(d.device_type(b"4IX7"), None);
        d.end_window();
        assert_eq!(d.device_type(b"ARWA"), Some(DeviceType::Phone));
    }

    #[test]
    fn a_list_scan_never_reads_slot_0() {
        let mut gate = LegacyGate::new(LegacyReads::Off);
        assert!(!gate.admit(addr(1)));
        assert!(!gate.admit(addr(2)));
    }

    #[test]
    fn a_send_reads_at_most_twice_per_window_and_a_device_once() {
        assert_eq!(SEND_LEGACY_READS, LegacyReads::Send { max: 2 });
        let mut gate = LegacyGate::new(SEND_LEGACY_READS);
        assert!(gate.admit(addr(1)));
        assert!(!gate.admit(addr(1)));
        assert!(gate.admit(addr(2)));
        assert!(!gate.admit(addr(3)));
        // A new window starts with a new gate.
        assert!(LegacyGate::new(SEND_LEGACY_READS).admit(addr(3)));
    }

    #[test]
    fn a_device_that_gave_a_full_advertisement_is_not_read() {
        let mut gate = LegacyGate::new(SEND_LEGACY_READS);
        gate.saw_full(addr(1));
        assert!(!gate.admit(addr(1)));
        // It did not use up a read.
        assert!(gate.admit(addr(2)));
        assert!(gate.admit(addr(3)));
    }

    #[test]
    fn an_ended_discovery_is_recognised_through_anyhow() {
        let e: anyhow::Error = DiscoveryEnded.into();
        assert!(e.downcast_ref::<DiscoveryEnded>().is_some());
        assert!(
            anyhow::anyhow!("other")
                .downcast_ref::<DiscoveryEnded>()
                .is_none()
        );
    }
}
