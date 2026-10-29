//! Owns the Quick Share service and the user settings; rebuilds the service when needed.

use std::{
    collections::{HashMap, HashSet},
    fs,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex as StdMutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use rqs_lib::{
    BleTarget, EndpointInfo, OutboundPayload, RQS, SendInfo, TextPayloadType, Visibility, WifiLink,
    channel::{ChannelMessage, Message, TransferAction},
};
use tokio::{
    net::TcpStream,
    sync::{Mutex, broadcast, mpsc, watch},
    task::JoinHandle,
};
use zbus::Connection;

use crate::{
    allowed::{Allowed, AllowedDevice},
    consent::Consents,
    dbus::{self, TransferEvent},
    history::{self, History},
    link::{HotspotPolicy, LinkNotice, NetworkEvent, NmLink},
    requirements::{
        EnableVerdict, RequirementsEngine, RequirementsProbe, RuntimeEvidence, SystemEngine,
        enable_verdict,
    },
    settings::{self, Settings},
    trust::{self, Trust, TrustEvent},
    wifi_credentials,
};

/// Nearby devices found by discovery, keyed by endpoint id (`ip:port`, or `ble:<id>` over Bluetooth).
pub type Endpoints = Arc<StdMutex<HashMap<String, EndpointInfo>>>;
/// IPs of nearby computers that run KuickShare and are paired with this one.
pub type TrustedIps = Arc<StdMutex<HashSet<String>>>;
/// Transfers that have not reached a final state yet.
pub type ActiveTransfers = Arc<StdMutex<HashMap<String, TransferEvent>>>;
pub type SharedHistory = Arc<StdMutex<History>>;
/// Sends that went out under another endpoint id than the one the user picked (original id ->
/// id used with the library); the transfer events are reported under the original id.
pub type Redirects = Arc<StdMutex<HashMap<String, String>>>;

/// Where Accept, Decline and Cancel send their action: the library's event channel of the running
/// engine (`None` while it is stopped). It is shared with the D-Bus service, so these calls never
/// wait for the `Core` lock, which a restart can hold for seconds.
#[derive(Clone, Default)]
pub struct LibActions {
    sender: Arc<StdMutex<Option<broadcast::Sender<ChannelMessage>>>>,
    redirects: Redirects,
    sends: SendBook,
}

impl LibActions {
    pub(crate) fn set(&self, sender: Option<broadcast::Sender<ChannelMessage>>) {
        *self.sender.lock().unwrap() = sender;
    }

    /// Consent and cancel actions travel on the library's own event channel.
    pub fn send(&self, id_in: &str, action: TransferAction) -> Result<()> {
        let sender = self
            .sender
            .lock()
            .unwrap()
            .clone()
            .context("service is not running")?;
        let id = self.redirects.lock().unwrap().get(id_in).cloned();
        sender
            .send(ChannelMessage {
                id: id.unwrap_or_else(|| id_in.to_owned()),
                msg: Message::Lib { action },
            })
            .map(|_| ())
            .map_err(|e| anyhow!(e.to_string()))
    }

    /// Cancels the transfer `id`. A send that has not been handed to the library yet (it is
    /// still choosing its route) is marked and ends at once; its final `Cancelled` is returned to
    /// the caller, who emits it before answering, so it precedes the events of any later send
    /// under the same id (the shell applies a final event to the newest row of an id). A send
    /// that was handed over keeps its mark until the library takes it off its queue
    /// (`await_first_event` repeats the cancel then). The cancel is also broadcast, for a send
    /// the library already runs. Never takes the `Core` lock.
    pub fn cancel(&self, id: &str) -> Result<Vec<CancelReport>> {
        let (marked, due) = self.sends.cancel(id);
        match self.send(id, TransferAction::TransferCancel) {
            // The mark is what stops a send that was not queued: nothing more to tell the caller.
            Err(_) if marked => Ok(due),
            result => result.map(|()| due),
        }
    }

    /// The sends that are not with the library yet (see `SendBook`).
    pub fn sends(&self) -> SendBook {
        self.sends.clone()
    }
}

/// The final `Cancelled` of a send that ended before it went out, to be emitted exactly once by
/// whoever takes it from the `SendBook` (`LibActions::cancel` or the send itself).
#[derive(Debug)]
pub struct CancelReport {
    id: String,
    device: String,
    conn: Option<Connection>,
}

impl CancelReport {
    pub async fn emit(self) {
        if let Some(conn) = &self.conn {
            dbus::emit_cancelled_transfer(conn, &self.id, &self.device).await;
        }
    }
}

/// The sends that are not running in the library yet, keyed by the id the user picked. The
/// library hears a cancel only once it has taken the send off its queue, so `LibActions::cancel`
/// marks the send here. A send is in the book from before it looks for a route until the library
/// has reported its first event (the mark outlives the queue: see `await_first_event`). A std
/// mutex, never held across an await, separate from the `Core` lock.
#[derive(Clone, Default)]
pub struct SendBook(Arc<StdMutex<PendingSends>>);

#[derive(Default)]
struct PendingSends {
    next: u64,
    sends: HashMap<u64, Entry>,
}

struct Entry {
    /// The id the user picked.
    id: String,
    cancel: watch::Sender<bool>,
    /// What to emit when the send is cancelled while it chooses its route; taken by the first to
    /// emit it, and gone once the send is handed to the library (which reports its own end).
    report: Option<CancelReport>,
}

impl SendBook {
    /// Registers a send under the picked id; it is forgotten when the last copy of the guard is
    /// dropped.
    pub fn begin(&self, id: &str) -> PendingSend {
        let mut book = self.0.lock().unwrap();
        book.next += 1;
        let token = book.next;
        let (cancel, cancelled) = watch::channel(false);
        book.sends.insert(
            token,
            Entry {
                id: id.to_owned(),
                cancel,
                report: None,
            },
        );
        PendingSend {
            registration: Arc::new(Registration {
                book: self.clone(),
                token,
            }),
            cancelled,
        }
    }

    /// Marks every send registered under `id`. Returns whether there was one, and the final
    /// events of those that are still choosing their route (each is handed out once).
    fn cancel(&self, id: &str) -> (bool, Vec<CancelReport>) {
        let mut marked = false;
        let mut due = Vec::new();
        for entry in self.0.lock().unwrap().sends.values_mut() {
            if entry.id == id {
                entry.cancel.send_replace(true);
                marked = true;
                due.extend(entry.report.take());
            }
        }
        (marked, due)
    }

    /// True when no send is starting.
    pub fn is_empty(&self) -> bool {
        self.0.lock().unwrap().sends.is_empty()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.lock().unwrap().sends.len()
    }
}

/// A send that is registered in the `SendBook`. Copies share the registration.
#[derive(Clone)]
pub struct PendingSend {
    registration: Arc<Registration>,
    cancelled: watch::Receiver<bool>,
}

struct Registration {
    book: SendBook,
    token: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.book.0.lock().unwrap().sends.remove(&self.token);
    }
}

impl PendingSend {
    pub fn cancelled(&self) -> bool {
        *self.cancelled.borrow()
    }

    /// Resolves when the send is cancelled; never when it is not.
    async fn wait_cancelled(&self) {
        let mut cancelled = self.cancelled.clone();
        if cancelled.wait_for(|cancelled| *cancelled).await.is_err() {
            std::future::pending::<()>().await;
        }
    }

    /// Says what a cancel during the route phase reports (the send knows its device only after
    /// `Core::begin_send`).
    fn arm(&self, id: &str, device: String, conn: Option<Connection>) {
        if let Some(entry) = self
            .registration
            .book
            .0
            .lock()
            .unwrap()
            .sends
            .get_mut(&self.registration.token)
        {
            entry.report = Some(CancelReport {
                id: id.to_owned(),
                device,
                conn,
            });
        }
    }

    /// The final `Cancelled` of this send, unless a cancel already took it.
    fn claim_report(&self) -> Option<CancelReport> {
        self.registration
            .book
            .0
            .lock()
            .unwrap()
            .sends
            .get_mut(&self.registration.token)
            .and_then(|entry| entry.report.take())
    }

    /// The last look before the send goes to the library: true when it was not cancelled, and
    /// from then on a cancel is the library's to report (it goes out with the first event). One
    /// step under the book's lock, so a cancel is either seen here or leaves nothing to emit.
    fn hand_over(&self) -> bool {
        let mut book = self.registration.book.0.lock().unwrap();
        let Some(entry) = book.sends.get_mut(&self.registration.token) else {
            return false;
        };
        if *entry.cancel.borrow() {
            return false;
        }
        entry.report = None;
        true
    }
}

struct Engine {
    rqs: RQS,
    send_tx: mpsc::Sender<SendInfo>,
    forwarders: dbus::Forwarders,
    discovery_tx: broadcast::Sender<EndpointInfo>,
    discovery_on: bool,
    /// Tasks that restart the mDNS search for a phone seen only over Bluetooth, by device name.
    lan_retries: HashMap<String, JoinHandle<()>>,
}

impl Engine {
    fn stop_lan_retries(&mut self) {
        for (_, task) in self.lan_retries.drain() {
            task.abort();
        }
    }

    async fn stop(mut self, bounds: StopBounds) {
        self.stop_lan_retries();
        wind_down(self.rqs.stop(), bounds, self.forwarders).await;
    }
}

/// How long each step of stopping an engine may take.
#[derive(Clone, Copy)]
struct StopBounds {
    /// The library's tasks winding down (`RQS::stop` waits for all of them, without a limit).
    service: Duration,
    /// The transfer forwarder emitting what the library still had queued.
    drain: Duration,
}

/// A restart waits for the library to let go of its port before the new one binds it.
const RESTART_STOP: StopBounds = StopBounds {
    service: Duration::from_secs(3),
    drain: Duration::from_millis(500),
};
/// At exit everything has to fit in `SHUTDOWN_ENGINE_BOUND`.
const SHUTDOWN_STOP: StopBounds = StopBounds {
    service: Duration::from_millis(500),
    drain: Duration::from_millis(200),
};
/// Bound of the whole engine stop at exit, the wait for the `Core` lock included. With
/// `WIFI_STOP_CLEANUP_BOUND` it stays within the shell's 3 s wait for the daemon after SIGTERM.
pub const SHUTDOWN_ENGINE_BOUND: Duration = Duration::from_millis(800);

/// Stops an engine in the order that keeps its last events: the library first (a cancelled or
/// failed transfer reports its final state while it winds down), then the transfer forwarder gets
/// to emit what is still queued, then the other forwarders go. Every step is bounded: a step that
/// does not finish is abandoned, the next one still runs.
async fn wind_down(
    stop_service: impl Future<Output = ()>,
    bounds: StopBounds,
    forwarders: dbus::Forwarders,
) {
    if tokio::time::timeout(bounds.service, stop_service)
        .await
        .is_err()
    {
        tracing::warn!("the service did not stop in {:?}", bounds.service);
    }
    let dbus::Forwarders {
        stop,
        mut drain,
        abort,
    } = forwarders;
    // The forwarder empties its queue and ends.
    let _ = stop.send(true);
    if tokio::time::timeout(bounds.drain, &mut drain)
        .await
        .is_err()
    {
        tracing::warn!(
            "the transfer events were not all sent in {:?}",
            bounds.drain
        );
        drain.abort();
    }
    for task in abort {
        task.abort();
    }
}

/// The transfers that had no final event when their engine stopped, as final events: the shell
/// keeps no row alive for a service that is gone. `active` is left empty.
fn take_unfinished(active: &ActiveTransfers) -> Vec<TransferEvent> {
    let now = history::now();
    active
        .lock()
        .unwrap()
        .drain()
        .map(|(_, transfer)| transfer.disconnected(now))
        .collect()
}

pub struct Core {
    settings: Settings,
    settings_path: Option<PathBuf>,
    engine: Option<Engine>,
    endpoints: Endpoints,
    redirects: Redirects,
    lib_actions: LibActions,
    /// Who may answer which incoming transfer (see `consent`); it has a lock of its own.
    consents: Consents,
    active: ActiveTransfers,
    /// Wi-Fi networks received from phones, in memory only (see `wifi_credentials`).
    wifi_creds: wifi_credentials::Shared,
    /// The NetworkManager link, shared by the Wi-Fi upgrade and "Join this network".
    nm: Arc<NmLink>,
    /// What the link reports (a hotspot the firewall did not allow); forwarded as a D-Bus signal.
    link_notices: Option<mpsc::UnboundedReceiver<LinkNotice>>,
    /// The Wi-Fi requirements engine; it gets the evidence of the hotspot attempts and of the
    /// firewall consent (see `set_requirements`).
    requirements: Option<Arc<SystemEngine>>,
    history: SharedHistory,
    trust: Arc<Trust>,
    trust_events: Option<mpsc::UnboundedReceiver<TrustEvent>>,
    trusted_ips: TrustedIps,
    allowed: Allowed,
    /// Whether the allowed list is used; shared with the task that accepts transfers.
    trusted_enabled: Arc<AtomicBool>,
    conn: Option<Connection>,
    /// Temporary hiding (not persisted), e.g. while Do Not Disturb is on.
    suspended: bool,
    /// Temporary visibility (not persisted), set by `make_visible_for`.
    temp_visible: Option<TempVisible>,
    temp_visible_gen: u64,
    /// When the mDNS search was last restarted to look for a phone picked over Bluetooth.
    last_lan_lookup: Option<Instant>,
    /// The mutex this core lives in, for the tasks that lock it later (see `set_handle`).
    handle: Weak<Mutex<Core>>,
}

impl Core {
    pub fn new(
        settings: Settings,
        settings_path: Option<PathBuf>,
        history: History,
        trust: Arc<Trust>,
        trust_events: mpsc::UnboundedReceiver<TrustEvent>,
    ) -> Self {
        let mut history = history;
        history.set_policy(settings.history_enabled, settings.history_keep_days);
        let redirects = Redirects::default();
        let lib_actions = LibActions {
            redirects: redirects.clone(),
            ..LibActions::default()
        };
        let (notice_tx, link_notices) = mpsc::unbounded_channel();
        // The firewall consent (`HotspotPolicy`) is never stored: every start begins without it,
        // whatever `settings` hold, and only `request_wifi_upgrade` can grant it.
        Self {
            allowed: Allowed::new(StdMutex::new(settings.allowed_devices.clone())),
            trusted_enabled: Arc::new(AtomicBool::new(settings.trusted_devices)),
            settings,
            settings_path,
            engine: None,
            endpoints: Endpoints::default(),
            consents: Consents::new(lib_actions.clone()),
            lib_actions,
            redirects,
            active: ActiveTransfers::default(),
            wifi_creds: wifi_credentials::Shared::default(),
            nm: Arc::new(NmLink::new().with_notices(notice_tx)),
            link_notices: Some(link_notices),
            requirements: None,
            history: Arc::new(StdMutex::new(history)),
            trust,
            trust_events: Some(trust_events),
            trusted_ips: TrustedIps::default(),
            conn: None,
            suspended: false,
            temp_visible: None,
            temp_visible_gen: 0,
            last_lan_lookup: None,
            handle: Weak::new(),
        }
    }

    /// Gives the core the requirements engine, before `start`: the link's evidence goes to it, and
    /// so does every change of the firewall consent.
    pub fn set_requirements(&mut self, engine: Arc<SystemEngine>) {
        self.requirements = Some(engine);
    }

    /// Tells the core which mutex it lives in; needed for `watch_lan`.
    pub fn set_handle(&mut self, handle: Weak<Mutex<Core>>) {
        self.handle = handle;
    }

    pub async fn start(&mut self, conn: Connection) -> Result<()> {
        if let Some(events) = self.trust_events.take() {
            tokio::spawn(dbus::forward_trust_events(conn.clone(), events));
        }
        if let Some(notices) = self.link_notices.take() {
            tokio::spawn(dbus::forward_link_notices(
                conn.clone(),
                notices,
                self.requirements.clone(),
            ));
        }
        let any = SocketAddr::from(([0, 0, 0, 0], trust::DEFAULT_PORT));
        if let Err(e) = self.trust.listen(any).await {
            // Trusted devices are optional: sharing works without them.
            tracing::warn!("trusted devices unavailable: {e:#}");
        }
        self.conn = Some(conn);
        self.start_engine(false).await
    }

    /// Stops the service within `SHUTDOWN_ENGINE_BOUND` (the caller bounds the wait for the lock).
    /// The Wi-Fi link is released apart, without the `Core` lock (see `release_wifi`).
    pub async fn shutdown(&mut self) {
        let unfinished = self.stop_engine(SHUTDOWN_STOP).await;
        self.emit_unfinished(unfinished).await;
    }

    /// Stops the engine and forgets what belonged to it. Accept, Decline and Cancel fail at once:
    /// the old transfers end anyway, and their tokens must not answer a transfer that gets the
    /// same id later. The consents are forgotten (`Consents::clear`) only after the forwarders
    /// emitted the last events of the old engine, which that call would make them drop. Returns
    /// the transfers that never got a final event, as final events (not emitted here).
    async fn stop_engine(&mut self, bounds: StopBounds) -> Vec<TransferEvent> {
        self.lib_actions.set(None);
        if let Some(engine) = self.engine.take() {
            engine.stop(bounds).await;
        }
        self.consents.clear();
        self.redirects.lock().unwrap().clear();
        take_unfinished(&self.active)
    }

    async fn emit_unfinished(&self, unfinished: Vec<TransferEvent>) {
        if let Some(conn) = &self.conn {
            for event in &unfinished {
                dbus::emit_transfer(conn, event).await;
            }
        }
    }

    /// What `release_wifi` needs: the shared link.
    pub fn wifi_release(&self) -> WifiRelease {
        WifiRelease {
            nm: self.nm.clone(),
        }
    }

    pub fn trust(&self) -> Arc<Trust> {
        self.trust.clone()
    }

    /// Starts pairing with a nearby computer; the outcome arrives as trust events.
    pub fn pair_with(&self, endpoint_id: &str) -> Result<()> {
        let addr = self.control_addr(endpoint_id)?;
        let trust = self.trust.clone();
        tokio::spawn(async move {
            if let Err(e) = trust.pair(addr).await {
                tracing::info!("pairing failed: {e:#}");
            }
        });
        Ok(())
    }

    fn control_addr(&self, endpoint_id: &str) -> Result<SocketAddr> {
        control_addr(&self.endpoints, endpoint_id)
    }

    /// Remembers the sender of an incoming transfer: from now on its transfers are accepted
    /// without asking. Works only while the transfer waits for an answer, or within
    /// `consent::TRUST_GRACE` after the user accepted it, and binds the device to the MAC address
    /// read when the consent opened (same network only). Outbound and finished transfers are
    /// refused. `AnswerTransfer` with `accept_and_trust` is the way that cannot race with the
    /// accept.
    pub fn allow_device(&mut self, transfer_id: &str) -> Result<()> {
        let (name, mac) = self.consents.allow_trust(transfer_id, Instant::now())?;
        self.remember_device(name, mac);
        Ok(())
    }

    /// Adds a trusted device (name and the MAC address verified by the consent) and saves.
    pub fn remember_device(&mut self, name: String, mac: String) {
        let device = AllowedDevice { name, mac };
        let mut list = self.allowed.lock().unwrap();
        if !list.contains(&device) {
            list.push(device);
        }
        self.settings.allowed_devices = list.clone();
        drop(list);
        self.save();
    }

    /// The consent book, which the D-Bus service uses without taking the `Core` lock.
    pub fn consents(&self) -> Consents {
        self.consents.clone()
    }

    /// Whether the user still has to be told that the trusted devices of an older version were
    /// removed (see `Settings::allowed_reset_v1`).
    pub fn trusted_devices_reset(&self) -> bool {
        self.settings.trusted_devices_reset_notice
    }

    /// The user saw the notice. Returns false when there was none.
    pub fn acknowledge_trusted_devices_reset(&mut self) -> bool {
        let pending = self.settings.trusted_devices_reset_notice;
        if pending {
            self.settings.trusted_devices_reset_notice = false;
            self.save();
        }
        pending
    }

    pub fn forget_allowed(&mut self, name: &str, mac: &str) {
        let mut list = self.allowed.lock().unwrap();
        list.retain(|d| !(d.name == name && d.mac == mac));
        self.settings.allowed_devices = list.clone();
        drop(list);
        self.save();
    }

    pub fn allowed_json(&self) -> String {
        serde_json::to_string(&*self.allowed.lock().unwrap()).unwrap_or_else(|_| "[]".to_owned())
    }

    pub fn history_json(&self) -> String {
        self.history.lock().unwrap().json()
    }

    pub fn clear_history(&self) {
        self.history.lock().unwrap().clear();
    }

    pub fn history_size(&self) -> u64 {
        self.history.lock().unwrap().file_size()
    }

    pub fn remove_history(&self, uid: u64) {
        self.history.lock().unwrap().remove(uid);
    }

    pub fn set_history_enabled(&mut self, enabled: bool) {
        self.settings.history_enabled = enabled;
        self.apply_history_policy();
    }

    pub fn set_history_keep_days(&mut self, days: u32) {
        self.settings.history_keep_days = days;
        self.apply_history_policy();
    }

    /// Puts every setting back to its default. The list of trusted devices is kept: it is data the user
    /// built up, not a preference. Fails, leaving things as they are, while a transfer is running.
    pub async fn reset_settings(&mut self) -> Result<()> {
        self.ensure_idle()?;
        // Off during the restart, so the defaults apply; put back if the restart fails.
        let temp = self.temp_visible.take();
        let previous = self.settings.clone();
        self.settings = Settings {
            allowed_devices: previous.allowed_devices.clone(),
            trusted_devices_reset_notice: previous.trusted_devices_reset_notice,
            ..Settings::default()
        };
        self.trusted_enabled
            .store(self.settings.trusted_devices, Ordering::Relaxed);
        self.history.lock().unwrap().set_policy(
            self.settings.history_enabled,
            self.settings.history_keep_days,
        );
        // The name or the port may differ now: restart the service with the defaults.
        if let Err(e) = self.restart_or_revert(previous.clone()).await {
            // restart_or_revert put the settings back; what depends on them goes back too.
            self.trusted_enabled
                .store(previous.trusted_devices, Ordering::Relaxed);
            self.history
                .lock()
                .unwrap()
                .set_policy(previous.history_enabled, previous.history_keep_days);
            self.temp_visible = temp;
            return Err(e);
        }
        if let Some(t) = temp {
            t.task.abort();
        }
        // The defaults have the upgrade off, with its consent.
        self.set_consent(false);
        self.trust.set_name(self.device_name());
        Ok(())
    }

    pub fn set_trusted_devices(&mut self, enabled: bool) {
        self.settings.trusted_devices = enabled;
        self.trusted_enabled.store(enabled, Ordering::Relaxed);
        self.save();
    }

    /// The only writer of the Wi-Fi upgrade setting and of the firewall consent. `enabled` is
    /// saved; `allow_insecure` (host the hotspot in an open firewall zone when its port cannot be
    /// opened) is session-scoped: it lives in memory, and every write without it takes it back.
    /// Consent without the upgrade makes no sense and is refused, changing nothing.
    pub fn request_wifi_upgrade(
        &mut self,
        enabled: bool,
        allow_insecure: bool,
    ) -> Result<(), InsecureNeedsUpgrade> {
        if allow_insecure && !enabled {
            return Err(InsecureNeedsUpgrade);
        }
        self.settings.wifi_upgrade = enabled;
        self.set_consent(allow_insecure);
        if let Some(engine) = &self.engine {
            engine.rqs.set_wifi_link(wifi_link(enabled, &self.nm));
        }
        self.save();
        Ok(())
    }

    /// Sets the firewall consent. Taking it back also stops a hotspot that already runs in an open
    /// zone (in a task of its own: it waits for the link, and the `Core` lock is not held for it).
    fn set_consent(&self, allow: bool) {
        let had = self.insecure_hotspot_allowed();
        self.nm.set_hotspot_policy(if allow {
            HotspotPolicy::AllowOpenZone
        } else {
            HotspotPolicy::TargetedPortOnly
        });
        if had != allow
            && let Some(requirements) = &self.requirements
        {
            requirements.record(RuntimeEvidence::FirewallConsent(allow));
        }
        if had && !allow {
            let nm = self.nm.clone();
            tokio::spawn(async move { nm.drop_open_zone_hotspot().await });
        }
    }

    /// Whether the user allowed the hotspot to use an open firewall zone for this session.
    pub fn insecure_hotspot_allowed(&self) -> bool {
        self.nm.hotspot_policy() == HotspotPolicy::AllowOpenZone
    }

    pub fn set_tray_icon(&mut self, show: bool) {
        self.settings.tray_icon = show;
        self.save();
    }

    pub fn set_dolphin_integration(&mut self, enabled: bool) {
        self.settings.dolphin_integration = enabled;
        self.save();
    }

    fn apply_history_policy(&self) {
        self.history.lock().unwrap().set_policy(
            self.settings.history_enabled,
            self.settings.history_keep_days,
        );
        self.save();
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn device_name(&self) -> String {
        cap_device_name(
            &self
                .settings
                .device_name
                .clone()
                .unwrap_or_else(settings::default_device_name),
        )
    }

    pub async fn set_device_name(&mut self, name: &str) -> Result<()> {
        let name = Some(cap_device_name(name.trim())).filter(|n| !n.is_empty());
        if name == self.settings.device_name {
            return Ok(());
        }
        self.ensure_idle()?;
        let previous = self.settings.clone();
        self.settings.device_name = name;
        self.restart_or_revert(previous).await?;
        self.trust.set_name(self.device_name());
        Ok(())
    }

    pub async fn set_static_port(&mut self, port: u32) -> Result<()> {
        if port != 0 && !(1024..=65535).contains(&port) {
            bail!("Choose a port between 1024 and 65535");
        }
        let port = Some(port).filter(|&p| p != 0);
        if port == self.settings.static_port {
            return Ok(());
        }
        self.ensure_idle()?;
        let previous = self.settings.clone();
        self.settings.static_port = port;
        self.restart_or_revert(previous).await.map_err(|e| {
            anyhow!(
                "Port {} can't be used (another program may be using it): {e:#}",
                port.unwrap_or_default()
            )
        })
    }

    pub fn set_download_dir(&mut self, dir: &str) -> Result<()> {
        let dir = Some(PathBuf::from(dir.trim())).filter(|d| !d.as_os_str().is_empty());
        if let Some(dir) = &dir {
            fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        if let Some(engine) = &self.engine {
            engine.rqs.set_download_path(dir.clone());
        }
        self.settings.download_dir = dir;
        self.save();
        Ok(())
    }

    pub fn set_visible(&mut self, visible: bool) {
        self.clear_temp_visible();
        self.settings.visible = visible;
        self.apply_visibility();
        self.save();
    }

    /// Unix seconds when the temporary visibility ends; 0 when none is active.
    pub fn visible_until(&self) -> i64 {
        self.temp_visible.as_ref().map_or(0, |t| t.until)
    }

    /// Makes this computer visible for `seconds` (capped, 0 cancels), whatever `Visible` says.
    /// Visibility goes back to `Visible` when the time is up.
    pub fn make_visible_for(&mut self, seconds: u32) {
        self.clear_temp_visible();
        if let Some(window) = temp_window(seconds) {
            self.temp_visible_gen += 1;
            let id = self.temp_visible_gen;
            let until = unix_now() + window.as_secs() as i64;
            let task = tokio::spawn(expire_temp_visible(self.handle.clone(), id, until));
            self.temp_visible = Some(TempVisible { id, until, task });
        }
        self.apply_visibility();
    }

    fn clear_temp_visible(&mut self) {
        if let Some(t) = self.temp_visible.take() {
            t.task.abort();
        }
    }

    /// Ends the temporary visibility `id`; false when it was already replaced or cancelled.
    fn expire_temp_visible(&mut self, id: u64) -> bool {
        if self.temp_visible.as_ref().is_none_or(|t| t.id != id) {
            return false;
        }
        self.temp_visible = None;
        self.apply_visibility();
        true
    }

    pub fn connection(&self) -> Option<Connection> {
        self.conn.clone()
    }

    pub fn suspended(&self) -> bool {
        self.suspended
    }

    pub fn set_suspended(&mut self, suspended: bool) {
        self.suspended = suspended;
        self.apply_visibility();
    }

    pub fn set_hide_when_dnd(&mut self, hide: bool) {
        self.settings.hide_when_dnd = hide;
        self.save();
    }

    fn apply_visibility(&mut self) {
        let visible = self.effective_visible();
        if let Some(engine) = &mut self.engine {
            engine.rqs.change_visibility(visibility(visible));
        }
    }

    fn effective_visible(&self) -> bool {
        effective_visible(
            self.settings.visible,
            self.temp_visible.is_some(),
            self.suspended,
        )
    }

    /// The handle the D-Bus service uses for Accept and Decline (see `LibActions`).
    pub fn lib_actions(&self) -> LibActions {
        self.lib_actions.clone()
    }

    pub fn start_discovery(&mut self) -> Result<()> {
        let engine = self.engine.as_mut().context("service is not running")?;
        if !engine.discovery_on {
            engine.rqs.discovery(engine.discovery_tx.clone())?;
            engine.discovery_on = true;
        }
        Ok(())
    }

    /// Runs one more Bluetooth scan window; the devices arrive like the ones of `start_discovery`.
    pub fn scan_bluetooth(&self) -> Result<()> {
        let engine = self.engine.as_ref().context("service is not running")?;
        if !engine.discovery_on {
            bail!("discovery is not running");
        }
        engine.rqs.scan_ble(engine.discovery_tx.clone())
    }

    /// What `drop_unreachable_lan` needs, so that the caller can release the `Core` lock first.
    pub fn lan_handles(&self) -> (Endpoints, Option<Connection>) {
        (self.endpoints.clone(), self.conn.clone())
    }

    /// The last network connection is gone: the devices found over the LAN cannot be reached any
    /// more, so they are dropped now instead of when their mDNS records expire, and a Bluetooth
    /// scan looks for the phones that remain. Returns what the caller reports as `DeviceLost`,
    /// after it released the `Core` lock.
    pub fn network_lost(&mut self) -> Option<(Connection, Vec<String>)> {
        if !self.settings.wifi_upgrade {
            return None;
        }
        let gone = take_lan_endpoints(&mut self.endpoints.lock().unwrap());
        if let Err(e) = self.scan_bluetooth() {
            tracing::debug!("no Bluetooth scan after the network was lost: {e}");
        }
        self.conn.clone().map(|conn| (conn, gone))
    }

    /// A network is up again: the devices of the LAN would be noticed by the mDNS library only
    /// after up to 30 s, so the search starts over now. The list stays as it is.
    pub fn network_gained(&mut self) {
        if !self.settings.wifi_upgrade {
            return;
        }
        self.restart_discovery();
    }

    /// Starts the mDNS search over; the list of devices is not touched (unlike
    /// `stop_discovery`). Returns false when the discovery is not running or cannot restart.
    fn restart_discovery(&mut self) -> bool {
        let Some(engine) = self.engine.as_mut() else {
            return false;
        };
        if !engine.discovery_on {
            return false;
        }
        // Only the mDNS search: restarting the Bluetooth advertisement and scan with it stopped a
        // scan window in progress ("the discovery ended before the window did").
        if let Err(e) = engine
            .rqs
            .restart_mdns_discovery(engine.discovery_tx.clone())
        {
            tracing::warn!("could not restart the mDNS search: {e:#}");
            return false;
        }
        true
    }

    pub fn stop_discovery(&mut self) {
        if let Some(engine) = &mut self.engine {
            engine.stop_lan_retries();
        }
        if let Some(engine) = &mut self.engine
            && engine.discovery_on
        {
            engine.rqs.stop_discovery();
            engine.discovery_on = false;
        }
        self.endpoints.lock().unwrap().clear();
    }

    /// The first step of a send, under the `Core` lock: what the slow steps (route choice,
    /// announcement) need, taken as a snapshot so they run without the lock.
    fn begin_send(&self, endpoint_id: &str) -> Result<SendStart> {
        if self.engine.is_none() {
            bail!("service is not running");
        }
        let picked = self
            .endpoints
            .lock()
            .unwrap()
            .get(endpoint_id)
            .cloned()
            .with_context(|| format!("unknown device {endpoint_id}"))?;
        Ok(SendStart {
            picked,
            generation: self.consents.generation(),
            consents: self.consents.clone(),
            wifi_upgrade: self.settings.wifi_upgrade,
            endpoints: self.endpoints.clone(),
            conn: self.conn.clone(),
            trusted_ips: self.trusted_ips.clone(),
            trust: self.trust.clone(),
        })
    }

    /// The last step of a send, under the `Core` lock again: hands it to the library, unless the
    /// service stopped or restarted since `begin_send` (its generation changed) or the user
    /// cancelled it. Nothing here waits.
    fn queue_send(
        &self,
        start: &SendStart,
        pending: &PendingSend,
        endpoint_id: &str,
        endpoint: EndpointInfo,
        ob: OutboundPayload,
    ) -> Result<Queued> {
        let engine = self.engine.as_ref().context("the service is not running")?;
        if self.consents.generation() != start.generation {
            bail!("the service restarted, try again");
        }
        let ble = endpoint.ble.is_some();
        if ble && !self.settings.wifi_upgrade {
            bail!("Sending over Bluetooth needs the Wi-Fi upgrade, which is off (see Settings)");
        }
        let name = endpoint.name.clone().unwrap_or_default();
        let addr = if ble {
            String::new()
        } else {
            format!(
                "{}:{}",
                endpoint.ip.clone().unwrap_or_default(),
                endpoint.port.clone().unwrap_or_default()
            )
        };
        // Safety net: without any event in time, tell the user. The library reports a failed
        // scan, dial, connect or handshake itself, so this only covers a hang.
        let watch = self.conn.as_ref().map(|conn| {
            (
                conn.clone(),
                engine.rqs.message_sender.subscribe(),
                engine.rqs.message_sender.clone(),
            )
        });
        let send_info = SendInfo {
            id: endpoint.id.clone(),
            name: name.clone(),
            addr,
            ob,
        };
        // The last word on a cancel: from here on it is the library's to report.
        if !pending.hand_over() {
            return Ok(Queued::Cancelled);
        }
        // Only a send that is really going out is mapped, and no two picked ids share one
        // library id.
        {
            let mut redirects = self.redirects.lock().unwrap();
            redirects.retain(|_, sent_as| *sent_as != endpoint.id);
            redirects.remove(endpoint_id);
            if endpoint.id != endpoint_id {
                redirects.insert(endpoint_id.to_owned(), endpoint.id.clone());
            }
        }
        // `try_send`: the library takes sends off this queue at once, and the lock is held.
        if let Err(e) = engine.send_tx.try_send(send_info) {
            self.redirects.lock().unwrap().remove(endpoint_id);
            return Err(match e {
                mpsc::error::TrySendError::Full(_) => anyhow!("too many sends are starting"),
                mpsc::error::TrySendError::Closed(_) => anyhow!("service stopped while sending"),
            });
        }
        // A Cancel from now on is broadcast before the library can listen for it (it subscribes
        // when it takes the send off its queue): the mark stays until its first event, and
        // `await_first_event` says it again then.
        if let Some((conn, events, bus)) = watch {
            let within = if ble {
                BLE_START_TIMEOUT
            } else {
                LAN_START_TIMEOUT
            };
            tokio::spawn(report_silent_send(
                conn,
                events,
                bus,
                (endpoint.id, endpoint_id.to_owned()),
                name,
                within,
                pending.clone(),
            ));
        }
        Ok(Queued::Sent)
    }

    /// Restarts the search for the phone picked over Bluetooth, at most once per `LAN_LOOKUP_GAP`
    /// (the send waits for it without the lock: see `RouteSteps`). `generation` is the one the
    /// send started with: after a restart nothing is touched. False when nothing was restarted.
    fn restart_lan_search_for(&mut self, generation: u64) -> bool {
        if self.consents.generation() != generation {
            return false;
        }
        let now = Instant::now();
        if !lookup_due(self.last_lan_lookup, now) || !self.restart_discovery() {
            return false;
        }
        self.last_lan_lookup = Some(now);
        true
    }

    /// A phone was seen over Bluetooth only: restarts the mDNS search every `LAN_RETRY_EVERY`, at
    /// most `LAN_RETRIES` times, until its LAN entry is known (the phone answers the first
    /// queries late). One task per device name; it ends with the discovery or the engine.
    pub fn watch_lan(&mut self, generation: u64, ble: EndpointInfo) {
        let Some(name) = ble.name.clone() else {
            return;
        };
        // A sighting of an engine that was stopped since: the new one lists the phone again.
        if self.consents.generation() != generation
            || !self.settings.wifi_upgrade
            || !self.lan_retry_wanted(&ble)
        {
            return;
        }
        let handle = self.handle.clone();
        let Some(engine) = self.engine.as_mut().filter(|e| e.discovery_on) else {
            return;
        };
        if engine
            .lan_retries
            .get(&name)
            .is_some_and(|t| !t.is_finished())
        {
            return;
        }
        engine
            .lan_retries
            .insert(name, tokio::spawn(retry_lan(handle, ble)));
    }

    /// True while `ble` is still listed and has no LAN entry.
    fn lan_retry_wanted(&self, ble: &EndpointInfo) -> bool {
        let endpoints = self.endpoints.lock().unwrap();
        endpoints.contains_key(&ble.id) && needs_lan_retry(endpoints.values(), ble)
    }

    fn is_idle(&self) -> bool {
        let mut active = self.active.lock().unwrap();
        drop_stale(&mut active, history::now());
        active.is_empty()
    }

    /// Fails while a transfer runs or a send is still starting (choosing its route, or queued and
    /// not yet heard from): a restart would tear it down.
    fn ensure_idle(&self) -> Result<()> {
        if self.is_idle() && self.lib_actions.sends.is_empty() {
            Ok(())
        } else {
            bail!("Wait for the current transfer to finish before changing this setting");
        }
    }

    /// What `JoinNetwork` needs, taken under the core lock so the join itself (seconds) runs
    /// without it. Joining another network drops the connection a transfer may be using, so it
    /// waits for the transfers to end; a running Wi-Fi upgrade is refused by the link itself.
    pub fn wifi_join_parts(&self) -> Result<(Arc<NmLink>, wifi_credentials::Shared)> {
        if !self.is_idle() {
            bail!("Wait for the current transfer to finish before joining a network");
        }
        Ok((self.nm.clone(), self.wifi_creds.clone()))
    }

    /// Applies the new settings by restarting the service; if that fails, the previous settings
    /// are restored (and the service restarted) so sharing keeps working.
    async fn restart_or_revert(&mut self, previous: Settings) -> Result<()> {
        match self.restart().await {
            Ok(()) => {
                self.save();
                Ok(())
            }
            Err(e) => {
                tracing::warn!("restart failed, reverting settings: {e:#}");
                self.settings = previous;
                if let Err(e) = self.restart().await {
                    tracing::error!("cannot restart with the previous settings: {e:#}");
                }
                Err(e)
            }
        }
    }

    /// Rebuilds the service, keeping discovery on if it was on.
    async fn restart(&mut self) -> Result<()> {
        let discovery = self.engine.as_ref().is_some_and(|e| e.discovery_on);
        let unfinished = self.stop_engine(RESTART_STOP).await;
        self.emit_unfinished(unfinished).await;
        self.endpoints.lock().unwrap().clear();
        self.wifi_creds.lock().unwrap().clear();
        self.start_engine(discovery).await
    }

    async fn start_engine(&mut self, discovery: bool) -> Result<()> {
        let conn = self.conn.clone().context("not connected to the bus")?;
        let mut rqs = RQS::new(
            visibility(self.effective_visible()),
            self.settings.static_port,
            self.settings.download_dir.clone(),
            Some(self.device_name()),
        );
        rqs.set_wifi_link(wifi_link(self.settings.wifi_upgrade, &self.nm));
        let (send_tx, _) = rqs
            .run()
            .await
            .context("failed to start the Quick Share service")?;

        let (discovery_tx, _) = broadcast::channel(32);
        let forwarders = dbus::spawn_forwarders(
            dbus::Forwarding {
                connection: conn,
                endpoints: self.endpoints.clone(),
                redirects: self.redirects.clone(),
                active: self.active.clone(),
                wifi_creds: self.wifi_creds.clone(),
                history: self.history.clone(),
                trust: self.trust.clone(),
                trusted_ips: self.trusted_ips.clone(),
                allowed: self.allowed.clone(),
                trusted_enabled: self.trusted_enabled.clone(),
                consents: self.consents.clone(),
                generation: self.consents.generation(),
                core: self.handle.clone(),
            },
            rqs.message_sender.subscribe(),
            discovery_tx.subscribe(),
        );
        self.lib_actions.set(Some(rqs.message_sender.clone()));
        self.engine = Some(Engine {
            rqs,
            send_tx,
            forwarders,
            discovery_tx,
            discovery_on: false,
            lan_retries: HashMap::new(),
        });
        if discovery {
            self.start_discovery()?;
        }
        Ok(())
    }

    fn save(&self) {
        if let Some(path) = &self.settings_path
            && let Err(e) = self.settings.save(path)
        {
            tracing::warn!("cannot save settings: {e:#}");
        }
    }
}

/// The control address (`ip` and the pairing port) of a device found over the LAN.
fn control_addr(endpoints: &Endpoints, endpoint_id: &str) -> Result<SocketAddr> {
    if BleTarget::parse(endpoint_id).is_some() {
        bail!("Bluetooth devices cannot be paired");
    }
    let ip = endpoints
        .lock()
        .unwrap()
        .get(endpoint_id)
        .and_then(|e| e.ip.clone())
        .with_context(|| format!("unknown device {endpoint_id}"))?;
    Ok(SocketAddr::new(ip.parse()?, trust::DEFAULT_PORT))
}

/// What a send takes from the `Core` (under its lock) before the slow steps, which run without
/// it: a snapshot, valid for the engine of `generation` (`Consents::generation`).
struct SendStart {
    picked: EndpointInfo,
    generation: u64,
    /// To tell whether the engine of `generation` still runs.
    consents: Consents,
    wifi_upgrade: bool,
    endpoints: Endpoints,
    conn: Option<Connection>,
    trusted_ips: TrustedIps,
    trust: Arc<Trust>,
}

/// What `Core::queue_send` did.
enum Queued {
    Sent,
    /// The user cancelled the send before it was handed to the library: nothing was sent.
    Cancelled,
}

/// What a send came to.
#[derive(Debug)]
pub enum SendOutcome {
    /// With the library; its events come as `TransferUpdated` under the picked id.
    Queued,
    /// Cancelled before it went out. The final `Cancelled` event is the caller's to emit when
    /// this holds it; `None` when `LibActions::cancel` emitted it already.
    Cancelled(Option<CancelReport>),
}

/// The slow, outside-world parts of choosing a route, so that the choice runs without the `Core`
/// lock and a test can hold it still.
trait RouteSteps {
    /// True when `addr` accepts a TCP connection (`PROBE_TIMEOUT`).
    async fn probe(&self, addr: &str) -> bool;
    /// Restarts the mDNS search (see `Core::restart_lan_search_for`); false when it did not.
    async fn restart_lan_search(&self) -> bool;
    /// Starts a Bluetooth scan window; does nothing when one cannot start.
    async fn scan_bluetooth(&self);
    /// The device was dropped from the list: tell the shell.
    async fn device_lost(&self, id: &str);
    /// True while the engine the send began with is the one that runs: the list and the
    /// signals belong to the engine that found them.
    fn engine_is_current(&self) -> bool;

    /// `probe`, tried once more when it fails (a lost SYN is not a gone phone).
    async fn probe_twice(&self, addr: &str) -> bool {
        self.probe(addr).await || self.probe(addr).await
    }
}

/// `RouteSteps` on the running service: each step that needs the `Core` takes its lock for that
/// step only, and does nothing when the service restarted since the send began.
struct CoreSteps<'a> {
    core: &'a Mutex<Core>,
    generation: u64,
    consents: Consents,
    conn: Option<Connection>,
}

impl RouteSteps for CoreSteps<'_> {
    async fn probe(&self, addr: &str) -> bool {
        probe(addr).await
    }

    async fn restart_lan_search(&self) -> bool {
        self.core
            .lock()
            .await
            .restart_lan_search_for(self.generation)
    }

    async fn scan_bluetooth(&self) {
        let core = self.core.lock().await;
        if core.consents.generation() != self.generation {
            return;
        }
        if let Err(e) = core.scan_bluetooth() {
            tracing::debug!("Bluetooth lookup: no scan started: {e:#}");
        }
    }

    async fn device_lost(&self, id: &str) {
        if let Some(conn) = &self.conn {
            dbus::emit_device_lost(conn, id).await;
        }
    }

    fn engine_is_current(&self) -> bool {
        self.consents.generation() == self.generation
    }
}

/// Sends files to the device `endpoint_id` (see `send_payload`).
pub async fn send_files(
    core: &Mutex<Core>,
    sends: &SendBook,
    endpoint_id: &str,
    paths: Vec<String>,
) -> Result<()> {
    if paths.is_empty() {
        bail!("no files to send");
    }
    for path in &paths {
        fs::metadata(path).with_context(|| format!("cannot read {path}"))?;
    }
    send_payload(core, sends, endpoint_id, OutboundPayload::Files(paths)).await
}

/// Sends a text or a link to the device `endpoint_id` (see `send_payload`).
pub async fn send_text(
    core: &Mutex<Core>,
    sends: &SendBook,
    endpoint_id: &str,
    text: String,
    url: bool,
) -> Result<()> {
    if text.trim().is_empty() {
        bail!("nothing to send");
    }
    let text_type = if url {
        TextPayloadType::Url
    } else {
        TextPayloadType::Text
    };
    let ob = OutboundPayload::Text { text, text_type };
    send_payload(core, sends, endpoint_id, ob).await
}

/// Runs a send; a send the user cancelled before it went out ends with a final `Cancelled`
/// event, never with an error.
async fn send_payload(
    core: &Mutex<Core>,
    sends: &SendBook,
    endpoint_id: &str,
    ob: OutboundPayload,
) -> Result<()> {
    let steps = |start: &SendStart| CoreSteps {
        core,
        generation: start.generation,
        consents: start.consents.clone(),
        conn: start.conn.clone(),
    };
    if let SendOutcome::Cancelled(Some(report)) =
        run_send(core, sends, steps, endpoint_id, ob).await?
    {
        report.emit().await;
    }
    Ok(())
}

/// One send, in three steps. The `Core` lock is held for the first and the last only (neither
/// waits for anything slow); the route choice and the announcement to a paired computer, which
/// take seconds, run without it, so the properties, Cancel, the discovery and the rest of the
/// API stay responsive:
///
/// 1. under the lock, a snapshot (`Core::begin_send`);
/// 2. no lock: `choose_route`, then the announcement to a paired computer;
/// 3. under the lock, the engine must be the same one (else the send fails with "the service
///    restarted") and the send goes to the library (`Core::queue_send`).
///
/// The send is in the `SendBook` from before step 1, so a Cancel at any point stops it at once:
/// step 2 gives way to it wherever it waits (a probe, a lookup, the announcement), and the last
/// look is just before the queue. The final `Cancelled` is emitted by the Cancel call itself
/// (`LibActions::cancel`) when it comes during the route phase.
async fn run_send<S: RouteSteps>(
    core: &Mutex<Core>,
    sends: &SendBook,
    steps: impl FnOnce(&SendStart) -> S,
    endpoint_id: &str,
    ob: OutboundPayload,
) -> Result<SendOutcome> {
    let pending = sends.begin(endpoint_id);
    let start = core.lock().await.begin_send(endpoint_id)?;
    pending.arm(
        endpoint_id,
        start.picked.name.clone().unwrap_or_default(),
        start.conn.clone(),
    );
    let cancelled = || SendOutcome::Cancelled(pending.claim_report());
    let steps = steps(&start);
    let endpoint = tokio::select! {
        biased;
        () = pending.wait_cancelled() => return Ok(cancelled()),
        route = choose_route(&start, &steps) => route?,
    };
    if pending.cancelled() {
        return Ok(cancelled());
    }
    let ble = endpoint.ble.is_some();
    if ble && !start.wifi_upgrade {
        bail!("Sending over Bluetooth needs the Wi-Fi upgrade, which is off (see Settings)");
    }
    // A paired KuickShare computer accepts the transfer by itself if it is told in advance.
    let trusted = endpoint
        .ip
        .as_ref()
        .is_some_and(|ip| start.trusted_ips.lock().unwrap().contains(ip));
    if trusted
        && steps.engine_is_current()
        && let Ok(addr) = control_addr(&start.endpoints, &endpoint.id)
    {
        let announce = tokio::time::timeout(Duration::from_secs(2), start.trust.announce(addr));
        let announced = tokio::select! {
            biased;
            () = pending.wait_cancelled() => return Ok(cancelled()),
            announced = announce => announced,
        };
        if !matches!(announced, Ok(Ok(()))) {
            tracing::info!("could not announce the transfer to the paired computer");
        }
    }
    let queued = core
        .lock()
        .await
        .queue_send(&start, &pending, endpoint_id, endpoint, ob)?;
    Ok(match queued {
        Queued::Sent => SendOutcome::Queued,
        Queued::Cancelled => cancelled(),
    })
}

/// Decides which endpoint a send really goes to. A device found over the LAN is probed first:
/// a phone whose Wi-Fi went off sends no mDNS goodbye, so its entry would stay for minutes.
/// If it is gone the entry is dropped and the same phone is tried over Bluetooth; the other
/// way round, a Bluetooth phone that answers on the LAN is reached there. When no LAN entry
/// is known for a Bluetooth phone, the mDNS search is restarted once and given
/// `LAN_LOOKUP_TIMEOUT` to find it (see `look_up_lan`). The other way round, when a LAN phone
/// is gone and no Bluetooth entry is known, a Bluetooth scan window is started and given
/// `BLE_LOOKUP_TIMEOUT` to see it (see `look_up_bluetooth`). The `Core` lock is not held
/// meanwhile (see `run_send`); the time it takes is, for a Bluetooth pick, at most
/// `LAN_LOOKUP_TIMEOUT` (1.5 s) plus two `PROBE_TIMEOUT` (1.5 s each) per probed candidate; for a
/// LAN pick, at most two `PROBE_TIMEOUT` plus `BLE_LOOKUP_TIMEOUT` (3 s).
async fn choose_route<S: RouteSteps>(start: &SendStart, steps: &S) -> Result<EndpointInfo> {
    let picked = start.picked.clone();
    let endpoints = &start.endpoints;
    if picked.ble.is_none() {
        if steps.probe_twice(&lan_addr(&picked)).await {
            return Ok(picked);
        }
        // The list and the signals belong to the engine that found the device: after a restart
        // the new one may have found it again.
        if steps.engine_is_current() {
            endpoints.lock().unwrap().remove(&picked.id);
            steps.device_lost(&picked.id).await;
        }
        let mut ble = None;
        if start.wifi_upgrade {
            ble = same_device(endpoints, &picked, true);
            if ble.is_none() {
                tracing::info!("send: the LAN device is gone, looking for it over Bluetooth");
                tracing::debug!("{} is gone: looking for it over Bluetooth", picked.id);
                ble = look_up_bluetooth(steps, endpoints, &picked).await;
            }
        }
        let Some(ble) = ble else {
            if start.wifi_upgrade {
                tracing::info!("send refused: the LAN device is gone and has no Bluetooth entry");
                tracing::debug!("{} is gone and has no Bluetooth entry", picked.id);
            } else {
                tracing::info!("send refused: the LAN device is gone and Wi-Fi Direct is off");
                tracing::debug!(
                    "{} is gone (Wi-Fi Direct is off: no Bluetooth lookup)",
                    picked.id
                );
            }
            bail!("The device is not reachable");
        };
        tracing::info!("send goes over Bluetooth: the LAN device is gone");
        tracing::debug!("{} is gone, sending to {} (same name)", picked.id, ble.id);
        return Ok(ble);
    }
    let mut lan = same_device(endpoints, &picked, false);
    if lan.is_none() && start.wifi_upgrade && lan_entries(endpoints, &picked) == 0 {
        lan = look_up_lan(steps, endpoints, &picked).await;
    }
    let name = picked.name.as_deref().unwrap_or_default();
    match lan {
        Some(lan) if steps.probe_twice(&lan_addr(&lan)).await => {
            tracing::info!("send goes over the LAN: the device answers there");
            tracing::debug!(
                "{} answers on the LAN, sending to {} (same name)",
                picked.id,
                lan.id
            );
            return Ok(lan);
        }
        Some(_) => {
            tracing::info!("send stays on Bluetooth: LAN probe failed");
            tracing::debug!(
                "{} stays on Bluetooth: the LAN entry did not answer",
                picked.id
            );
        }
        None if lan_entries(endpoints, &picked) > 1 => {
            tracing::info!("send stays on Bluetooth: more than one LAN entry has that name");
            tracing::debug!("{} stays on Bluetooth: ambiguous LAN entries", picked.id);
        }
        None => {
            tracing::info!("send stays on Bluetooth: no LAN entry with that name");
            tracing::debug!(
                "{} stays on Bluetooth: no LAN entry named \"{name}\"",
                picked.id
            );
        }
    }
    Ok(picked)
}

/// Restarts the mDNS search (the list stays) and waits up to `LAN_LOOKUP_TIMEOUT` for the
/// phone picked over Bluetooth to show up on the LAN with the same name and device type. The
/// library notices a phone that joined the LAN only when the phone announces itself, and
/// NetworkManager reports nothing when it is the phone's network that changed. At most one
/// restart per `LAN_LOOKUP_GAP`; nothing happens while the discovery is not running. No
/// lock is held across the waits.
async fn look_up_lan<S: RouteSteps>(
    steps: &S,
    endpoints: &Endpoints,
    picked: &EndpointInfo,
) -> Option<EndpointInfo> {
    picked.name.as_ref()?;
    if !steps.restart_lan_search().await {
        return None;
    }
    tracing::info!("send: the device is not on the LAN list, restarting the search");
    tracing::debug!("{} is not on the LAN list", picked.id);
    let deadline = tokio::time::Instant::now() + LAN_LOOKUP_TIMEOUT;
    loop {
        tokio::time::sleep(LAN_LOOKUP_POLL).await;
        if let Some(lan) = same_device(endpoints, picked, false) {
            return Some(lan);
        }
        if tokio::time::Instant::now() >= deadline || lan_entries(endpoints, picked) > 1 {
            return None;
        }
    }
}

/// Starts a Bluetooth scan window and waits up to `BLE_LOOKUP_TIMEOUT` for the phone picked
/// over the LAN to be seen with the same name and device type. A scan does nothing while a window
/// runs or just after one; a running window delivers its sightings to the same list, so waiting
/// is still right. Stops early on a match, or when several phones match (no guessing). No lock
/// is held across the waits.
async fn look_up_bluetooth<S: RouteSteps>(
    steps: &S,
    endpoints: &Endpoints,
    picked: &EndpointInfo,
) -> Option<EndpointInfo> {
    picked.name.as_ref()?;
    steps.scan_bluetooth().await;
    let deadline = tokio::time::Instant::now() + BLE_LOOKUP_TIMEOUT;
    loop {
        tokio::time::sleep(BLE_LOOKUP_POLL).await;
        if let Some(ble) = same_device(endpoints, picked, true) {
            return Some(ble);
        }
        if tokio::time::Instant::now() >= deadline || ble_entries(endpoints, picked) > 1 {
            return None;
        }
    }
}

/// How many known Bluetooth endpoints have the name and the device type of `of`.
fn ble_entries(endpoints: &Endpoints, of: &EndpointInfo) -> usize {
    matching(endpoints.lock().unwrap().values(), of, true).count()
}

/// How many known LAN endpoints have the name and the device type of `of`.
fn lan_entries(endpoints: &Endpoints, of: &EndpointInfo) -> usize {
    matching(endpoints.lock().unwrap().values(), of, false).count()
}

/// The only known Bluetooth (`ble`) or LAN endpoint with the name and the device type of
/// `of`; `None` when there is none or when more than one matches (no guessing).
fn same_device(endpoints: &Endpoints, of: &EndpointInfo, ble: bool) -> Option<EndpointInfo> {
    unique_match(endpoints.lock().unwrap().values(), of, ble).cloned()
}

/// Longest device name in bytes: it goes into a one-byte length field of the mDNS record.
const MAX_DEVICE_NAME_BYTES: usize = 64;

/// `name` cut at a char boundary to at most `MAX_DEVICE_NAME_BYTES` bytes.
fn cap_device_name(name: &str) -> String {
    let mut end = name.len().min(MAX_DEVICE_NAME_BYTES);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].trim_end().to_owned()
}

/// Removes the devices found over the LAN (everything but `ble:` ids) and returns their ids.
fn take_lan_endpoints(endpoints: &mut HashMap<String, EndpointInfo>) -> Vec<String> {
    let lan: Vec<String> = endpoints
        .iter()
        .filter(|(_, endpoint)| endpoint.ble.is_none())
        .map(|(id, _)| id.clone())
        .collect();
    for id in &lan {
        endpoints.remove(id);
    }
    lan
}

/// The endpoints of `candidates` on the wanted route that have the name and the device type of
/// `of`; none when `of` is unnamed.
fn matching<'a, 'b>(
    candidates: impl Iterator<Item = &'a EndpointInfo>,
    of: &'b EndpointInfo,
    ble: bool,
) -> impl Iterator<Item = &'a EndpointInfo> {
    candidates.filter(move |e| {
        of.name.is_some()
            && e.ble.is_some() == ble
            && e.id != of.id
            && e.name == of.name
            && e.rtype == of.rtype
    })
}

/// The only endpoint of `candidates` on the wanted route (`ble` or LAN) that has the name and the
/// device type of `of`. An unnamed device never matches.
fn unique_match<'a>(
    candidates: impl Iterator<Item = &'a EndpointInfo>,
    of: &EndpointInfo,
    ble: bool,
) -> Option<&'a EndpointInfo> {
    let mut found = matching(candidates, of, ble);
    let first = found.next()?;
    found.next().is_none().then_some(first)
}

/// Probes every device found over the LAN at once and drops the ones that do not answer (a phone
/// whose Wi-Fi went off sends no mDNS goodbye). A device that fails is probed once more (a lost SYN
/// is not a gone phone), again all at once, before it is dropped. Takes about two `PROBE_TIMEOUT` at most.
pub async fn drop_unreachable_lan(endpoints: Endpoints, conn: Option<Connection>) {
    let mut suspects: Vec<EndpointInfo> = endpoints
        .lock()
        .unwrap()
        .values()
        .filter(|endpoint| endpoint.ble.is_none())
        .cloned()
        .collect();
    for _ in 0..2 {
        let alive = futures_util::future::join_all(
            suspects
                .iter()
                .map(|e| async move { probe(&lan_addr(e)).await }),
        )
        .await;
        let mut alive = alive.into_iter();
        suspects.retain(|_| !alive.next().unwrap_or(true));
    }
    for endpoint in &suspects {
        if endpoints.lock().unwrap().remove(&endpoint.id).is_some()
            && let Some(conn) = &conn
        {
            dbus::emit_device_lost(conn, &endpoint.id).await;
        }
    }
}

/// `ip:port` of a device found over the LAN.
fn lan_addr(endpoint: &EndpointInfo) -> String {
    format!(
        "{}:{}",
        endpoint.ip.as_deref().unwrap_or_default(),
        endpoint.port.as_deref().unwrap_or_default()
    )
}

/// How long a send waits for the device to accept a TCP connection before it is taken to be gone.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// How long a send to a Bluetooth phone waits for it to appear on the LAN after the mDNS search
/// was restarted, and how often the list is looked at meanwhile.
const LAN_LOOKUP_TIMEOUT: Duration = Duration::from_millis(1500);
const LAN_LOOKUP_POLL: Duration = Duration::from_millis(100);
/// How long a send to a vanished LAN phone waits for a Bluetooth scan to see it, and how often
/// the list is looked at meanwhile.
const BLE_LOOKUP_TIMEOUT: Duration = Duration::from_millis(3000);
const BLE_LOOKUP_POLL: Duration = Duration::from_millis(100);
/// Least time between two restarts of the search for that purpose, so repeated sends do not
/// thrash the discovery.
const LAN_LOOKUP_GAP: Duration = Duration::from_secs(5);

/// True when the search may be restarted again, `last` being the previous restart.
fn lookup_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|last| now.saturating_duration_since(last) >= LAN_LOOKUP_GAP)
}

/// Time between two restarts of the search for a phone seen only over Bluetooth, and how many
/// there are per sighting.
const LAN_RETRY_EVERY: Duration = Duration::from_secs(5);
const LAN_RETRIES: u32 = 4;

/// True when `ble` is a named Bluetooth sighting and none of `endpoints` is a LAN entry with its
/// name and device type.
pub fn needs_lan_retry<'a>(
    endpoints: impl Iterator<Item = &'a EndpointInfo>,
    ble: &EndpointInfo,
) -> bool {
    ble.present == Some(true)
        && ble.ble.is_some()
        && ble.name.is_some()
        && matching(endpoints, ble, false).next().is_none()
}

/// Body of the task started by `Core::watch_lan`. The core is locked only for the check and the
/// restart, never across the sleeps; the restart shares `last_lan_lookup` with the send.
async fn retry_lan(core: Weak<Mutex<Core>>, ble: EndpointInfo) {
    for _ in 0..LAN_RETRIES {
        tokio::time::sleep(LAN_RETRY_EVERY).await;
        let Some(core) = core.upgrade() else {
            return;
        };
        let mut core = core.lock().await;
        if !core.lan_retry_wanted(&ble) {
            return;
        }
        let now = Instant::now();
        if lookup_due(core.last_lan_lookup, now) && core.restart_discovery() {
            core.last_lan_lookup = Some(now);
            tracing::info!("a Bluetooth device has no LAN entry yet: restarting the search");
            tracing::debug!("{} has no LAN entry yet", ble.id);
        }
    }
}

/// True when `addr` accepts a TCP connection within `PROBE_TIMEOUT`.
async fn probe(addr: &str) -> bool {
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect(addr)).await,
        Ok(Ok(_))
    )
}

/// Follows NetworkManager: `Core::network_lost` when the last connection drops and
/// `Core::network_gained` when one comes back.
pub async fn watch_network(core: Arc<tokio::sync::Mutex<Core>>) {
    let (tx, mut events) = mpsc::unbounded_channel();
    tokio::spawn(crate::link::watch_network(tx));
    while let Some(event) = events.recv().await {
        match event {
            NetworkEvent::Lost => {
                let lost = core.lock().await.network_lost();
                if let Some((conn, ids)) = lost {
                    for id in &ids {
                        dbus::emit_device_lost(&conn, id).await;
                    }
                }
            }
            NetworkEvent::Gained => core.lock().await.network_gained(),
        }
    }
}

/// A Bluetooth send scans for at most 12 s, then dials (a GATT dial takes up to 22 s) and waits
/// for the phone's first frame. The library reports a failure itself; this only covers a hang.
const BLE_START_TIMEOUT: Duration = Duration::from_secs(40);

/// A LAN send connects within 8 s and finishes the handshake within 20 s (both enforced by the
/// library, which reports the failure); this only covers a hang. Below the shell's own 45 s.
const LAN_START_TIMEOUT: Duration = Duration::from_secs(40);

/// Waits for the first event of the transfer sent under `ids.0` (the id used with the library);
/// if none comes in time it is reported as failed under `ids.1` (the id the user picked).
async fn report_silent_send(
    conn: Connection,
    mut events: broadcast::Receiver<ChannelMessage>,
    bus: broadcast::Sender<ChannelMessage>,
    ids: (String, String),
    device: String,
    within: Duration,
    pending: PendingSend,
) {
    let (sent_as, shown_as) = ids;
    if await_first_event(&mut events, &bus, &sent_as, within, &pending).await {
        return;
    }
    tracing::info!("no answer to a send in time: reporting it as failed");
    tracing::debug!("no answer from {sent_as}");
    // Stop the library's transfer too, or it would go on and a retry would offer the files twice.
    // No effect while it is still scanning or dialling (no request exists yet to receive it).
    cancel_in_library(&bus, &sent_as);
    dbus::emit_failed_transfer(&conn, &shown_as, &device).await;
}

fn cancel_in_library(bus: &broadcast::Sender<ChannelMessage>, id: &str) {
    let _ = bus.send(ChannelMessage {
        id: id.to_owned(),
        msg: Message::Lib {
            action: TransferAction::TransferCancel,
        },
    });
}

/// Waits for the first event of the library for the transfer `id` (see `send_started`). The
/// library listens for a cancel only after it took the send off its queue, and its first event
/// comes after that: a Cancel that was marked on `pending` meanwhile never reached it, so it is
/// said again now. True when the transfer started (or the service stopped).
async fn await_first_event(
    events: &mut broadcast::Receiver<ChannelMessage>,
    bus: &broadcast::Sender<ChannelMessage>,
    id: &str,
    within: Duration,
    pending: &PendingSend,
) -> bool {
    if !send_started(events, id, within).await {
        return false;
    }
    if pending.cancelled() {
        cancel_in_library(bus, id);
    }
    true
}

/// True when the library reported the transfer `id` (a `Message::Client`) within `within`, or
/// when the service stopped (then there is nothing to report). The actions on the bus, a Cancel
/// among them, are not the library's word.
async fn send_started(
    events: &mut broadcast::Receiver<ChannelMessage>,
    id: &str,
    within: Duration,
) -> bool {
    let started = async {
        loop {
            match events.recv().await {
                Ok(ChannelMessage {
                    id: from,
                    msg: Message::Client(_),
                }) if from == id => return true,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return true,
            }
        }
    };
    tokio::time::timeout(within, started).await.is_ok()
}

/// A transfer that has not been heard of for this long is taken to be over (its final event was
/// lost), so it does not block the settings that restart the service for ever.
const STALE_AFTER_SECS: i64 = 600;

fn drop_stale(active: &mut HashMap<String, TransferEvent>, now: i64) {
    active.retain(|_, transfer| now - transfer.last_event < STALE_AFTER_SECS);
}

/// `Core::request_wifi_upgrade` refused: the firewall consent needs the Wi-Fi upgrade to be on.
#[derive(Debug, PartialEq, Eq)]
pub struct InsecureNeedsUpgrade;

impl std::fmt::Display for InsecureNeedsUpgrade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("allowing the open firewall zone needs the Wi-Fi upgrade to be on")
    }
}

impl std::error::Error for InsecureNeedsUpgrade {}

/// What `change_wifi_upgrade` found before it wrote, so the caller can emit the right signals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
    pub enabled_before: bool,
    pub insecure_before: bool,
}

/// Why `change_wifi_upgrade` wrote nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpgradeRefused {
    /// The consent to an open firewall zone needs the upgrade to be on.
    InsecureNeedsUpgrade,
    /// Turning the upgrade on, while the requirements report proves neither sending nor
    /// receiving can work on this computer.
    RequirementsBlocked,
}

impl std::fmt::Display for UpgradeRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InsecureNeedsUpgrade => std::fmt::Display::fmt(&InsecureNeedsUpgrade, f),
            Self::RequirementsBlocked => f.write_str(
                "neither sending nor receiving over Wi-Fi can work on this computer now",
            ),
        }
    }
}

impl std::error::Error for UpgradeRefused {}

impl From<InsecureNeedsUpgrade> for UpgradeRefused {
    fn from(_: InsecureNeedsUpgrade) -> Self {
        Self::InsecureNeedsUpgrade
    }
}

/// Orders the writers of the Wi-Fi upgrade: a gate that waits for the requirements report must
/// not let a later write overtake it. Not the `Core` lock, which the report must not hold.
static UPGRADE_WRITES: Mutex<()> = Mutex::const_new(());

/// The one gate on every write of the Wi-Fi upgrade setting (the method, the legacy property).
///
/// Turning the upgrade on (it was off) is judged by the requirements report, which is awaited
/// *without* the `Core` lock: both directions proven blocked is refused, anything else is saved
/// (the report only says what to expect). Turning it off is never refused; a write that leaves it
/// on only changes the consent, which is never granted without `allow_insecure`. The stored value
/// never follows NetworkManager or BlueZ coming and going: only this call writes it.
pub async fn change_wifi_upgrade<P: RequirementsProbe>(
    core: &Mutex<Core>,
    requirements: &Arc<RequirementsEngine<P>>,
    enabled: bool,
    allow_insecure: bool,
) -> Result<Change, UpgradeRefused> {
    if allow_insecure && !enabled {
        return Err(UpgradeRefused::InsecureNeedsUpgrade);
    }
    let _ordered = UPGRADE_WRITES.lock().await;
    let turning_on = enabled && !core.lock().await.settings().wifi_upgrade;
    if turning_on {
        let report = requirements.report(false).await;
        // A refusal rests on facts gathered after the request, not on a cache up to 30 s old or
        // a run that began earlier.
        if enable_verdict(&report.capabilities) == EnableVerdict::Reject {
            let fresh = requirements.report_from_new_run().await;
            if enable_verdict(&fresh.capabilities) == EnableVerdict::Reject {
                return Err(UpgradeRefused::RequirementsBlocked);
            }
        }
    }
    let mut core = core.lock().await;
    let change = Change {
        enabled_before: core.settings().wifi_upgrade,
        insecure_before: core.insecure_hotspot_allowed(),
    };
    core.request_wifi_upgrade(enabled, allow_insecure)?;
    Ok(change)
}

/// What `release_wifi` works on: the shared link. `main` takes it once at start
/// (`Core::wifi_release`) and keeps it, so the cleanup at exit never waits for the `Core` lock.
#[derive(Clone)]
#[must_use]
pub struct WifiRelease {
    nm: Arc<NmLink>,
}

/// Stops the daemon within the shell's 3 s wait after SIGTERM: the engine first (the wait for the
/// `Core` lock included) for at most `engine_bound`, whatever it does, then ALWAYS the Wi-Fi
/// cleanup, so a hotspot, an open firewall port or the replaced connection are restored even when
/// the engine hangs or something holds the `Core` lock.
pub async fn stop_daemon(
    stop_engine: impl Future<Output = ()>,
    engine_bound: Duration,
    release: impl Future<Output = ()>,
) {
    if tokio::time::timeout(engine_bound, stop_engine)
        .await
        .is_err()
    {
        tracing::warn!(
            "the service did not stop in {engine_bound:?}: going on with the Wi-Fi cleanup"
        );
    }
    release.await;
}

/// Bound of the Wi-Fi cleanup when the daemon stops. The shell waits 3 s for the daemon to end
/// after SIGTERM (`waitForFinished` in `corebridge.cpp`): this and `SHUTDOWN_ENGINE_BOUND`, which
/// goes first, stay within it (2 s + 0.8 s).
pub const WIFI_STOP_CLEANUP_BOUND: Duration = Duration::from_secs(2);
/// Bound of the sweep of leftover profiles at start, before the service takes its name.
pub const WIFI_START_SWEEP_BOUND: Duration = Duration::from_secs(5);

/// At daemon start and stop: closes the firewall port and restores the previous connection of a
/// hotspot that is up or half-made, and removes `kuickshare-*` profiles left by a crash (whatever
/// the settings say: the crashed run may have had the upgrade on), through the link the transfers
/// use (a new one would not know the port or the connection). Quiet when NetworkManager is absent;
/// stops waiting after `within`, so it cannot hang the daemon.
pub async fn release_wifi(release: WifiRelease, within: Duration) {
    let WifiRelease { nm } = release;
    if tokio::time::timeout(within, nm.shutdown()).await.is_err() {
        tracing::warn!(
            "Wi-Fi cleanup did not finish in {within:?}: a hotspot or an open firewall port may remain"
        );
    }
}

fn wifi_link(enabled: bool, nm: &Arc<NmLink>) -> Option<Arc<dyn WifiLink>> {
    enabled.then(|| nm.clone() as Arc<dyn WifiLink>)
}

/// Longest temporary visibility a caller can ask for.
const MAX_TEMP_VISIBLE_SECS: u32 = 24 * 60 * 60;

/// The expiry task re-checks the wall clock at least this often.
const TEMP_VISIBLE_STEP_SECS: i64 = 30;

struct TempVisible {
    id: u64,
    until: i64,
    task: JoinHandle<()>,
}

/// The effective visibility: temporary visibility overrides `Visible`, never `Suspended`.
fn effective_visible(visible: bool, temporary: bool, suspended: bool) -> bool {
    (visible || temporary) && !suspended
}

/// How long to stay visible for a request of `seconds`: none for 0, capped otherwise.
fn temp_window(seconds: u32) -> Option<Duration> {
    (seconds > 0).then(|| Duration::from_secs(seconds.min(MAX_TEMP_VISIBLE_SECS).into()))
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Waits until the wall clock reaches `until` (Unix seconds), in steps: a monotonic sleep does not
/// count the time the system spent suspended.
async fn expire_temp_visible(core: Weak<Mutex<Core>>, id: u64, until: i64) {
    loop {
        let left = until - unix_now();
        if left <= 0 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(left.min(TEMP_VISIBLE_STEP_SECS) as u64)).await;
    }
    let Some(core) = core.upgrade() else {
        return;
    };
    let conn = {
        let mut core = core.lock().await;
        if !core.expire_temp_visible(id) {
            return;
        }
        core.connection()
    };
    if let Some(conn) = conn {
        dbus::emit_visible_until_changed(&conn).await;
    }
}

fn visibility(visible: bool) -> Visibility {
    if visible {
        Visibility::Visible
    } else {
        Visibility::Invisible
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::Notify;

    use super::*;

    #[test]
    fn temporary_visibility_overrides_visible_but_not_suspended() {
        assert!(!effective_visible(false, false, false));
        assert!(effective_visible(false, true, false));
        assert!(effective_visible(true, false, false));
        assert!(!effective_visible(true, true, true));
    }

    fn test_core() -> Core {
        let (tx, rx) = mpsc::unbounded_channel();
        let trust = Trust::load(None, "test".into(), tx).unwrap();
        let history = History::load(None);
        Core::new(Settings::default(), None, history, trust, rx)
    }

    #[tokio::test]
    async fn consent_needs_the_upgrade_and_every_other_write_takes_it_back() {
        let mut core = test_core();
        assert!(!core.insecure_hotspot_allowed());
        assert_eq!(
            core.request_wifi_upgrade(false, true),
            Err(InsecureNeedsUpgrade)
        );
        assert!(!core.insecure_hotspot_allowed() && !core.settings().wifi_upgrade);

        core.request_wifi_upgrade(true, true).unwrap();
        assert!(core.insecure_hotspot_allowed() && core.settings().wifi_upgrade);
        assert_eq!(core.nm.hotspot_policy(), HotspotPolicy::AllowOpenZone);

        core.request_wifi_upgrade(true, false).unwrap();
        assert!(!core.insecure_hotspot_allowed() && core.settings().wifi_upgrade);

        core.request_wifi_upgrade(true, true).unwrap();
        core.request_wifi_upgrade(false, false).unwrap();
        assert!(!core.insecure_hotspot_allowed() && !core.settings().wifi_upgrade);
    }

    /// A probe whose answers are all the same: nobody there (`blocked`) or no system bus.
    struct Fixed {
        blocked: bool,
    }

    impl RequirementsProbe for Fixed {
        fn packaging(&self) -> crate::requirements::model::Packaging {
            crate::requirements::model::Packaging::Native
        }
        async fn sandbox(&self) -> crate::requirements::facts::SandboxFacts {
            Default::default()
        }
        async fn bluez(
            &self,
            _b: crate::requirements::Budget,
        ) -> crate::requirements::facts::BluezFacts {
            use crate::requirements::facts::{BluezFacts, ProbeError};
            if self.blocked {
                BluezFacts::default()
            } else {
                BluezFacts::failed(ProbeError::BusUnreachable)
            }
        }
        async fn network_manager(
            &self,
            _b: crate::requirements::Budget,
        ) -> crate::requirements::facts::NmFacts {
            use crate::requirements::facts::{NmFacts, ProbeError};
            if self.blocked {
                NmFacts::default()
            } else {
                NmFacts::failed(ProbeError::BusUnreachable)
            }
        }
        async fn firewall(
            &self,
            _b: crate::requirements::Budget,
        ) -> crate::requirements::facts::FirewallFacts {
            Default::default()
        }
        async fn dhcp(
            &self,
            _p: crate::requirements::model::Packaging,
        ) -> crate::requirements::facts::DhcpFacts {
            Default::default()
        }
    }

    #[tokio::test]
    async fn the_gate_refuses_only_a_proven_block_and_only_turning_on() {
        let core = Mutex::new(test_core());
        let blocked = RequirementsEngine::new(Fixed { blocked: true }, Default::default());
        let unknown = RequirementsEngine::new(Fixed { blocked: false }, Default::default());

        // Both directions proven blocked: nothing changes.
        assert_eq!(
            change_wifi_upgrade(&core, &blocked, true, false).await,
            Err(UpgradeRefused::RequirementsBlocked)
        );
        assert!(!core.lock().await.settings().wifi_upgrade);
        // The consent without the upgrade is refused before anything is judged.
        assert_eq!(
            change_wifi_upgrade(&core, &blocked, false, true).await,
            Err(UpgradeRefused::InsecureNeedsUpgrade)
        );
        // Unknown never blocks.
        let change = change_wifi_upgrade(&core, &unknown, true, true)
            .await
            .unwrap();
        assert!(!change.enabled_before && !change.insecure_before);
        assert!(core.lock().await.settings().wifi_upgrade);
        assert!(core.lock().await.insecure_hotspot_allowed());
        // Already on: not checked again, only the consent changes.
        let change = change_wifi_upgrade(&core, &blocked, true, false)
            .await
            .unwrap();
        assert!(change.enabled_before && change.insecure_before);
        assert!(!core.lock().await.insecure_hotspot_allowed());
        // Off is always allowed.
        change_wifi_upgrade(&core, &blocked, false, false)
            .await
            .unwrap();
        assert!(!core.lock().await.settings().wifi_upgrade);
    }

    #[tokio::test]
    async fn lib_actions_do_not_wait_for_the_core_lock() {
        let core = Arc::new(Mutex::new(test_core()));
        let actions = core.lock().await.lib_actions();
        assert!(actions.send("a", TransferAction::ConsentAccept).is_err()); // no engine yet
        let (tx, mut rx) = broadcast::channel(4);
        actions.set(Some(tx));
        actions
            .redirects
            .lock()
            .unwrap()
            .insert("picked".into(), "sent".into());
        let _held = core.lock().await; // a send holds the lock for seconds
        actions
            .send("picked", TransferAction::ConsentDecline)
            .unwrap();
        actions
            .send("other", TransferAction::TransferCancel)
            .unwrap();
        assert_eq!(rx.recv().await.unwrap().id, "sent");
        assert_eq!(rx.recv().await.unwrap().id, "other");
        actions.set(None);
        assert!(
            actions
                .send("other", TransferAction::TransferCancel)
                .is_err()
        );
    }

    /// Puts a transfer into `Core.active` the way the forwarder does.
    fn seed_active(core: &Core, id: &str, kind: &str, state: &str, device: &str) {
        core.active.lock().unwrap().insert(
            id.to_owned(),
            TransferEvent::for_test(id, kind, state, device),
        );
    }

    /// `allow_device` must refuse an outbound transfer and an inbound one that is already
    /// finished; it must say so by the transfer state, before looking at the ARP table (so the
    /// error does not mention the MAC address) and must not touch the allowed list.
    #[test]
    fn allow_device_refuses_outbound_and_finished() {
        let mut core = test_core();
        seed_active(&core, "10.0.0.5:4000", "outbound", "SendingFiles", "phone");
        seed_active(&core, "10.0.0.6:4000", "inbound", "Finished", "tablet");
        for id in ["10.0.0.5:4000", "10.0.0.6:4000"] {
            let err = core.allow_device(id).unwrap_err();
            assert_eq!(
                err.downcast_ref::<crate::consent::ConsentError>(),
                Some(&crate::consent::ConsentError::NotWaiting),
                "refused for the wrong reason: {err}"
            );
            assert!(!err.to_string().contains("MAC"));
            assert!(core.allowed_json() == "[]", "nothing may be trusted");
        }
    }

    #[tokio::test]
    async fn trusting_follows_the_consent_not_the_active_list() {
        let mut core = test_core();
        // Seen as incoming and active, but no consent waits for it: refused.
        seed_active(&core, "10.0.0.7:4000", "inbound", "ReceivingFiles", "phone");
        assert!(core.allow_device("10.0.0.7:4000").is_err());
        assert_eq!(core.allowed_json(), "[]");
        // Restarting forgets the consents.
        let consents = core.consents();
        consents.open_for_test("10.0.0.8:4000", "phone", None, Instant::now());
        core.consents.clear();
        assert_eq!(consents.token("10.0.0.8:4000"), None);
    }

    #[tokio::test]
    async fn the_reset_notice_stays_until_it_is_acknowledged() {
        let mut core = test_core();
        assert!(!core.trusted_devices_reset());
        assert!(!core.acknowledge_trusted_devices_reset());
        core.settings.trusted_devices_reset_notice = true;
        assert!(core.trusted_devices_reset());
        assert!(core.acknowledge_trusted_devices_reset());
        assert!(!core.trusted_devices_reset());
    }

    #[tokio::test]
    async fn a_stale_expiry_is_a_no_op_and_set_visible_cancels() {
        let mut core = test_core();
        core.make_visible_for(600);
        let old = core.temp_visible_gen;
        core.make_visible_for(600);
        assert!(!core.expire_temp_visible(old));
        assert!(core.visible_until() > 0);
        assert!(core.expire_temp_visible(old + 1));
        assert_eq!(core.visible_until(), 0);

        core.make_visible_for(600);
        core.set_visible(false);
        assert_eq!(core.visible_until(), 0);
        core.make_visible_for(600);
        core.make_visible_for(0);
        assert_eq!(core.visible_until(), 0);
    }

    #[test]
    fn temporary_visibility_window_is_capped_and_zero_cancels() {
        assert_eq!(temp_window(0), None);
        assert_eq!(temp_window(600), Some(Duration::from_secs(600)));
        assert_eq!(temp_window(u32::MAX), Some(Duration::from_secs(86_400)));
    }

    #[test]
    fn device_name_is_capped_at_a_char_boundary() {
        assert_eq!(cap_device_name("laptop"), "laptop");
        let long = "a".repeat(200);
        assert_eq!(cap_device_name(&long).len(), MAX_DEVICE_NAME_BYTES);
        let wide = "é".repeat(100); // 2 bytes each
        let capped = cap_device_name(&wide);
        assert!(capped.len() <= MAX_DEVICE_NAME_BYTES && capped.chars().all(|c| c == 'é'));
    }

    fn transfer(last_event: i64) -> TransferEvent {
        let mut transfer = TransferEvent::default();
        transfer.last_event = last_event;
        transfer
    }

    #[test]
    fn losing_the_network_drops_the_lan_devices_and_keeps_the_bluetooth_ones() {
        let lan = EndpointInfo {
            id: "10.0.0.5:4000".into(),
            ..EndpointInfo::default()
        };
        let ble = EndpointInfo {
            id: "ble:G483".into(),
            ble: Some(rqs_lib::BleTarget {
                endpoint_id: *b"G483",
            }),
            ..EndpointInfo::default()
        };
        let mut endpoints = HashMap::from([(lan.id.clone(), lan), (ble.id.clone(), ble)]);
        assert_eq!(take_lan_endpoints(&mut endpoints), ["10.0.0.5:4000"]);
        assert_eq!(endpoints.keys().collect::<Vec<_>>(), ["ble:G483"]);
    }

    #[tokio::test]
    async fn a_refresh_drops_only_the_lan_devices_that_do_not_answer() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_port = listener.local_addr().unwrap().port();
        let dead_port = {
            let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            closed.local_addr().unwrap().port()
        };
        let lan = |id: &str, port: u16| EndpointInfo {
            id: id.into(),
            ip: Some("127.0.0.1".into()),
            port: Some(port.to_string()),
            ..EndpointInfo::default()
        };
        let ble = named("ble:G483", "Pixel", true);
        let endpoints: Endpoints = Arc::new(StdMutex::new(HashMap::from([
            ("live".to_string(), lan("live", live_port)),
            ("dead".to_string(), lan("dead", dead_port)),
            (ble.id.clone(), ble),
        ])));
        drop_unreachable_lan(endpoints.clone(), None).await;
        let mut left: Vec<String> = endpoints.lock().unwrap().keys().cloned().collect();
        left.sort();
        assert_eq!(left, ["ble:G483", "live"]);
    }

    fn named(id: &str, name: &str, ble: bool) -> EndpointInfo {
        EndpointInfo {
            id: id.into(),
            name: Some(name.into()),
            ble: ble.then_some(BleTarget {
                endpoint_id: *b"G483",
            }),
            ..EndpointInfo::default()
        }
    }

    #[test]
    fn the_same_phone_is_found_on_the_other_route_only_when_it_is_unambiguous() {
        let lan = named("10.0.0.5:4000", "Pixel", false);
        let ble = named("ble:G483", "Pixel", true);
        let other = named("ble:ABCD", "Galaxy", true);
        let all = [lan.clone(), ble.clone(), other.clone()];
        assert_eq!(
            unique_match(all.iter(), &lan, true).map(|e| &e.id),
            Some(&ble.id)
        );
        assert_eq!(
            unique_match(all.iter(), &ble, false).map(|e| &e.id),
            Some(&lan.id)
        );

        let twin = named("10.0.0.6:4000", "Pixel", false);
        let all = [lan.clone(), ble.clone(), twin];
        assert!(unique_match(all.iter(), &ble, false).is_none());

        let unnamed = EndpointInfo::default();
        assert!(unique_match([ble.clone()].iter(), &unnamed, true).is_none());
    }

    #[test]
    fn a_bluetooth_sighting_without_a_lan_entry_needs_a_retry() {
        let mut ble = named("ble:G483", "Pixel", true);
        ble.present = Some(true);
        let lan = named("10.0.0.5:4000", "Pixel", false);
        let other = named("10.0.0.6:4000", "Galaxy", false);
        assert!(needs_lan_retry([ble.clone(), other.clone()].iter(), &ble));
        assert!(!needs_lan_retry([ble.clone(), lan.clone()].iter(), &ble));
        assert!(!needs_lan_retry(
            [lan].iter(),
            &named("10.0.0.5:4000", "Pixel", false)
        ));
        ble.present = Some(false);
        assert!(!needs_lan_retry([other].iter(), &ble));
    }

    #[test]
    fn the_search_restarts_at_most_once_per_gap() {
        let t0 = Instant::now();
        assert!(lookup_due(None, t0));
        assert!(!lookup_due(Some(t0), t0 + Duration::from_secs(4)));
        assert!(lookup_due(Some(t0), t0 + LAN_LOOKUP_GAP));
    }

    #[test]
    fn transfers_not_heard_of_for_a_long_time_stop_counting_as_active() {
        let now = 10_000;
        let mut active = HashMap::from([
            ("recent".to_owned(), transfer(now - 5)),
            (
                "quiet but alive".to_owned(),
                transfer(now - STALE_AFTER_SECS + 1),
            ),
            ("lost".to_owned(), transfer(now - STALE_AFTER_SECS)),
        ]);
        drop_stale(&mut active, now);
        let mut left: Vec<_> = active.keys().cloned().collect();
        left.sort();
        assert_eq!(left, ["quiet but alive", "recent"]);
    }

    // --- The lifecycle of a send, a restart and the exit -------------------------------------
    // Nothing here sleeps to "let something happen": a fake holds the step under test on a
    // `Notify` gate, and the test acts while it is held.

    const PICKED: &str = "10.0.0.5:4000";

    /// Puts an engine that was never run into `core`; what is queued for the library arrives on
    /// the returned receiver. This is the test seam for `Core::engine`: a real `RQS::run` would
    /// bind a port and start mDNS and Bluetooth.
    fn install_engine(core: &mut Core) -> mpsc::Receiver<SendInfo> {
        let (send_tx, queue) = mpsc::channel(10);
        let (discovery_tx, _) = broadcast::channel(4);
        let (stop, _) = watch::channel(false);
        core.engine = Some(Engine {
            rqs: RQS::new(Visibility::Visible, None, None, None),
            send_tx,
            forwarders: dbus::Forwarders {
                stop,
                drain: tokio::spawn(async {}),
                abort: Vec::new(),
            },
            discovery_tx,
            discovery_on: false,
            lan_retries: HashMap::new(),
        });
        queue
    }

    fn lan_phone() -> EndpointInfo {
        EndpointInfo {
            id: PICKED.into(),
            name: Some("Pixel".into()),
            ip: Some("10.0.0.5".into()),
            port: Some("4000".into()),
            ..EndpointInfo::default()
        }
    }

    fn hello() -> OutboundPayload {
        OutboundPayload::Text {
            text: "hello".into(),
            text_type: TextPayloadType::Text,
        }
    }

    /// A route step that stops in `probe` until the test lets it go, and answers `true` then.
    struct Gated<'a> {
        entered: &'a Notify,
        release: &'a Notify,
    }

    impl RouteSteps for Gated<'_> {
        async fn probe(&self, _addr: &str) -> bool {
            self.entered.notify_one();
            self.release.notified().await;
            true
        }
        async fn restart_lan_search(&self) -> bool {
            false
        }
        async fn scan_bluetooth(&self) {}
        async fn device_lost(&self, _id: &str) {}
        fn engine_is_current(&self) -> bool {
            true
        }
    }

    /// A route step that stops in the lookup of a Bluetooth phone on the LAN, for good.
    struct StuckInLookup<'a> {
        entered: &'a Notify,
    }

    impl RouteSteps for StuckInLookup<'_> {
        async fn probe(&self, _addr: &str) -> bool {
            false
        }
        async fn restart_lan_search(&self) -> bool {
            self.entered.notify_one();
            std::future::pending().await
        }
        async fn scan_bluetooth(&self) {}
        async fn device_lost(&self, _id: &str) {}
        fn engine_is_current(&self) -> bool {
            true
        }
    }

    /// A core with an engine and the phone in its list, and the queue of the library.
    fn core_with_phone() -> (Mutex<Core>, mpsc::Receiver<SendInfo>, LibActions) {
        let mut core = test_core();
        let queue = install_engine(&mut core);
        core.endpoints
            .lock()
            .unwrap()
            .insert(PICKED.into(), lan_phone());
        let actions = core.lib_actions();
        (Mutex::new(core), queue, actions)
    }

    #[tokio::test]
    async fn a_send_does_not_hold_the_core_lock_while_it_chooses_its_route() {
        let (core, mut queue, actions) = core_with_phone();
        let sends = actions.sends();
        let (entered, release) = (Notify::new(), Notify::new());
        let send = run_send(
            &core,
            &sends,
            |_| Gated {
                entered: &entered,
                release: &release,
            },
            PICKED,
            hello(),
        );
        let meanwhile = async {
            entered.notified().await;
            // The route is being chosen: the properties, Cancel and the rest are not blocked.
            drop(core.try_lock().expect("the Core lock is held by the send"));
            let _ = core.lock().await.device_name(); // a property getter completes
            assert_eq!(sends.len(), 1, "the send is registered while it waits");
            release.notify_one();
        };
        let (outcome, ()) = tokio::join!(send, meanwhile);
        assert!(matches!(outcome.unwrap(), SendOutcome::Queued));
        let queued = queue.try_recv().expect("the send reaches the library");
        assert_eq!((queued.id.as_str(), queued.addr.as_str()), (PICKED, PICKED));
        assert_eq!(sends.len(), 0, "nothing stays registered");
    }

    #[tokio::test]
    async fn a_cancel_during_the_route_phase_stops_the_send_with_one_cancelled() {
        let (core, mut queue, actions) = core_with_phone();
        let sends = actions.sends();
        let (entered, release) = (Notify::new(), Notify::new());
        let send = run_send(
            &core,
            &sends,
            |_| Gated {
                entered: &entered,
                release: &release,
            },
            PICKED,
            hello(),
        );
        let meanwhile = async {
            entered.notified().await;
            // The library is not listening yet and nothing is mapped: the mark is what counts,
            // and the Cancel call itself holds the one `Cancelled` to emit. The probe is not
            // released: the send does not wait for it.
            let due = actions
                .cancel(PICKED)
                .expect("a pending send can be cancelled");
            assert_eq!(due.len(), 1);
            assert_eq!(
                (due[0].id.as_str(), due[0].device.as_str()),
                (PICKED, "Pixel")
            );
        };
        let (outcome, ()) = tokio::join!(send, meanwhile);
        match outcome.unwrap() {
            SendOutcome::Cancelled(report) => {
                assert!(report.is_none(), "the same Cancelled is emitted twice");
            }
            SendOutcome::Queued => panic!("a cancelled send was queued"),
        }
        assert!(queue.try_recv().is_err(), "nothing goes to the library");
        assert!(core.lock().await.redirects.lock().unwrap().is_empty());
        assert_eq!(sends.len(), 0);
    }

    #[tokio::test]
    async fn a_cancel_during_the_lookup_ends_the_send_at_once() {
        let mut core = test_core();
        let mut queue = install_engine(&mut core);
        core.settings.wifi_upgrade = true;
        let ble = named("ble:G483", "Pixel", true);
        core.endpoints.lock().unwrap().insert(ble.id.clone(), ble);
        let actions = core.lib_actions();
        let core = Mutex::new(core);
        let sends = actions.sends();
        let entered = Notify::new();
        let send = run_send(
            &core,
            &sends,
            |_| StuckInLookup { entered: &entered },
            "ble:G483",
            hello(),
        );
        let meanwhile = async {
            entered.notified().await;
            // The lookup never finishes by itself: only the cancel can end the send.
            assert_eq!(actions.cancel("ble:G483").unwrap().len(), 1);
        };
        let (outcome, ()) = tokio::join!(send, meanwhile);
        assert!(matches!(outcome.unwrap(), SendOutcome::Cancelled(None)));
        assert!(queue.try_recv().is_err(), "nothing goes to the library");
        assert_eq!(sends.len(), 0);
    }

    #[tokio::test]
    async fn a_cancel_before_the_report_is_armed_is_reported_by_the_send_itself() {
        let (core, mut queue, actions) = core_with_phone();
        let sends = actions.sends();
        // Cancelled while the send still waits for the lock: nothing to emit yet.
        let held = core.lock().await;
        let entered = Notify::new();
        let send = run_send(
            &core,
            &sends,
            |_| StuckInLookup { entered: &entered },
            PICKED,
            hello(),
        );
        let meanwhile = async {
            actions.cancel(PICKED).unwrap();
            drop(held);
        };
        let (outcome, ()) = tokio::join!(send, meanwhile);
        assert!(matches!(outcome.unwrap(), SendOutcome::Cancelled(Some(_))));
        assert!(queue.try_recv().is_err());
    }

    /// A message the library sends about a transfer (what `send_started` waits for).
    fn client_message(id: &str) -> ChannelMessage {
        ChannelMessage {
            id: id.to_owned(),
            msg: Message::Client(rqs_lib::channel::MessageClient {
                kind: rqs_lib::channel::TransferKind::Outbound,
                state: None,
                metadata: None,
            }),
        }
    }

    #[tokio::test]
    async fn a_cancel_just_after_queuing_reaches_the_library_when_it_takes_the_send() {
        let actions = LibActions::default();
        let (bus, _keep) = broadcast::channel(8);
        actions.set(Some(bus.clone()));
        actions
            .redirects
            .lock()
            .unwrap()
            .insert("picked".into(), "sent".into());
        // The send was handed over (queue_send), and `report_silent_send` listens already.
        let pending = actions.sends().begin("picked");
        assert!(pending.hand_over());
        let mut watcher = bus.subscribe();
        // The cancel comes before the library took the send off its queue: nobody hears it.
        assert!(actions.cancel("picked").unwrap().is_empty());
        assert!(pending.cancelled());
        // The library takes the send (it subscribes), then reports it.
        let mut library = bus.subscribe();
        bus.send(client_message("sent")).unwrap();
        assert!(
            await_first_event(&mut watcher, &bus, "sent", Duration::from_secs(5), &pending).await
        );
        let first = library.recv().await.unwrap();
        assert!(matches!(first.msg, Message::Client(_)));
        let cancel = library.recv().await.unwrap();
        assert_eq!(cancel.id, "sent");
        assert!(matches!(
            cancel.msg,
            Message::Lib {
                action: TransferAction::TransferCancel
            }
        ));
    }

    #[tokio::test]
    async fn a_send_that_was_not_cancelled_says_nothing_more_to_the_library() {
        let actions = LibActions::default();
        let (bus, _keep) = broadcast::channel(8);
        actions.set(Some(bus.clone()));
        let pending = actions.sends().begin("picked");
        assert!(pending.hand_over());
        let mut watcher = bus.subscribe();
        let mut library = bus.subscribe();
        bus.send(client_message("picked")).unwrap();
        assert!(
            await_first_event(
                &mut watcher,
                &bus,
                "picked",
                Duration::from_secs(5),
                &pending
            )
            .await
        );
        assert!(library.recv().await.is_ok());
        assert!(library.try_recv().is_err());
    }

    #[tokio::test]
    async fn the_watchdog_stays_armed_after_a_cancel() {
        let actions = LibActions::default();
        let (bus, _keep) = broadcast::channel(8);
        actions.set(Some(bus.clone()));
        let pending = actions.sends().begin("picked");
        assert!(pending.hand_over());
        let mut watcher = bus.subscribe();
        // The cancel itself is on the bus under the library's id; it is not a first event.
        actions.cancel("picked").unwrap();
        assert!(!await_first_event(&mut watcher, &bus, "picked", SHORT, &pending).await);
    }

    #[test]
    fn a_pending_send_blocks_the_settings_that_restart_the_service() {
        let core = test_core();
        core.ensure_idle().unwrap();
        let pending = core.lib_actions().sends().begin("10.0.0.5:4000");
        let error = core.ensure_idle().unwrap_err().to_string();
        assert!(error.contains("Wait for the current transfer"), "{error}");
        // A copy kept for the watchdog keeps it registered too.
        let kept = pending.clone();
        drop(pending);
        assert!(core.ensure_idle().is_err());
        drop(kept);
        core.ensure_idle().unwrap();
    }

    #[tokio::test]
    async fn a_sighting_of_a_stopped_engine_starts_no_lan_retry() {
        let mut core = test_core();
        let _queue = install_engine(&mut core);
        core.engine.as_mut().unwrap().discovery_on = true;
        core.settings.wifi_upgrade = true;
        let mut ble = named("ble:G483", "Pixel", true);
        ble.present = Some(true);
        core.endpoints
            .lock()
            .unwrap()
            .insert(ble.id.clone(), ble.clone());
        let stale = core.consents.generation();
        core.consents.clear(); // another engine took over
        core.watch_lan(stale, ble.clone());
        assert!(core.engine.as_ref().unwrap().lan_retries.is_empty());
        let current = core.consents.generation();
        core.watch_lan(current, ble);
        assert_eq!(core.engine.as_ref().unwrap().lan_retries.len(), 1);
        core.engine.as_mut().unwrap().stop_lan_retries();
    }

    #[tokio::test]
    async fn a_cancel_after_the_queue_still_reaches_the_library() {
        let (_core, _queue, actions) = core_with_phone();
        let (tx, mut rx) = broadcast::channel(4);
        actions.set(Some(tx));
        actions
            .redirects
            .lock()
            .unwrap()
            .insert("picked".into(), "sent".into());
        // Not pending any more (it was queued): the cancel is broadcast, under the mapped id.
        assert!(actions.cancel("picked").unwrap().is_empty());
        let heard = rx.recv().await.unwrap();
        assert_eq!(heard.id, "sent");
        assert!(matches!(
            heard.msg,
            Message::Lib {
                action: TransferAction::TransferCancel
            }
        ));
        // Nothing pending and nobody to hear it: still an error, as before.
        actions.set(None);
        assert!(actions.cancel("other").is_err());
    }

    #[test]
    fn a_cancel_marks_only_the_sends_started_before_it() {
        let book = SendBook::default();
        let before = book.begin("a");
        let other = book.begin("b");
        assert!(book.cancel("a").0);
        let after = book.begin("a");
        assert!(before.cancelled() && !other.cancelled() && !after.cancelled());
        assert!(!book.cancel("unknown").0);
        drop((before, other, after));
        assert_eq!(book.len(), 0);
    }

    #[tokio::test]
    async fn a_send_fails_cleanly_when_the_service_restarted_under_it() {
        let (core, mut queue, actions) = core_with_phone();
        let sends = actions.sends();
        let (entered, release) = (Notify::new(), Notify::new());
        let send = run_send(
            &core,
            &sends,
            |_| Gated {
                entered: &entered,
                release: &release,
            },
            PICKED,
            hello(),
        );
        let meanwhile = async {
            entered.notified().await;
            core.lock().await.stop_engine(SHUTDOWN_STOP).await;
            release.notify_one();
        };
        let (outcome, ()) = tokio::join!(send, meanwhile);
        let error = outcome.unwrap_err().to_string();
        assert!(error.contains("not running"), "{error}");
        assert!(queue.try_recv().is_err());
        assert!(core.lock().await.redirects.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_send_says_so_when_another_engine_took_over() {
        let (core, mut queue, actions) = core_with_phone();
        let sends = actions.sends();
        let (entered, release) = (Notify::new(), Notify::new());
        let send = run_send(
            &core,
            &sends,
            |_| Gated {
                entered: &entered,
                release: &release,
            },
            PICKED,
            hello(),
        );
        let meanwhile = async {
            entered.notified().await;
            // A restart: the engine is there again, but it is another one.
            core.lock().await.consents.clear();
            release.notify_one();
        };
        let (outcome, ()) = tokio::join!(send, meanwhile);
        let error = outcome.unwrap_err().to_string();
        assert!(error.contains("restarted"), "{error}");
        assert!(queue.try_recv().is_err());
    }

    #[tokio::test]
    async fn stopping_the_engine_ends_every_unfinished_transfer_and_empties_the_list() {
        let mut core = test_core();
        let _queue = install_engine(&mut core);
        seed_active(&core, "10.0.0.5:4000", "outbound", "SendingFiles", "phone");
        seed_active(
            &core,
            "10.0.0.6:4000",
            "inbound",
            "WaitingForUserConsent",
            "tablet",
        );
        core.redirects
            .lock()
            .unwrap()
            .insert("picked".into(), "sent".into());
        let generation = core.consents.generation();
        let (tx, _rx) = broadcast::channel(4);
        core.lib_actions.set(Some(tx));

        let mut unfinished = core.stop_engine(RESTART_STOP).await;
        unfinished.sort_by(|a, b| a.summary().0.cmp(b.summary().0));
        let seen: Vec<_> = unfinished.iter().map(TransferEvent::summary).collect();
        assert_eq!(
            seen,
            [
                ("10.0.0.5:4000", "outbound", "Disconnected", 0),
                ("10.0.0.6:4000", "inbound", "Disconnected", 0),
            ]
        );
        // A stale entry no longer blocks the settings that restart the service.
        assert!(core.active.lock().unwrap().is_empty());
        core.ensure_idle().unwrap();
        assert!(core.engine.is_none() && core.redirects.lock().unwrap().is_empty());
        assert_ne!(core.consents.generation(), generation);
        assert!(
            core.lib_actions
                .send("x", TransferAction::TransferCancel)
                .is_err()
        );
    }

    /// Records what happened in what order.
    type Log = Arc<StdMutex<Vec<&'static str>>>;

    /// Sets a flag when it is dropped: the task that owned it is gone.
    struct Gone(Arc<AtomicBool>);

    impl Drop for Gone {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    const QUICK: StopBounds = StopBounds {
        service: Duration::from_millis(20),
        drain: Duration::from_millis(20),
    };

    #[tokio::test]
    async fn the_library_stops_before_the_forwarders_and_they_get_to_drain() {
        let log = Log::default();
        let (stop, mut stopped) = watch::channel(false);
        let drain_log = log.clone();
        let drain = tokio::spawn(async move {
            let _ = stopped.changed().await;
            drain_log.lock().unwrap().push("drained");
        });
        let service_log = log.clone();
        wind_down(
            async move { service_log.lock().unwrap().push("service stopped") },
            QUICK,
            dbus::Forwarders {
                stop,
                drain,
                abort: Vec::new(),
            },
        )
        .await;
        assert_eq!(*log.lock().unwrap(), ["service stopped", "drained"]);
    }

    #[tokio::test]
    async fn every_step_of_stopping_is_bounded_and_the_next_one_still_runs() {
        let (stop, _) = watch::channel(false);
        let drain_gone = Arc::new(AtomicBool::new(false));
        let other_gone = Arc::new(AtomicBool::new(false));
        let guard = Gone(drain_gone.clone());
        // Ignores the stop signal: abandoned after the bound.
        let drain = tokio::spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        let guard = Gone(other_gone.clone());
        let other = tokio::spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        // A regression must fail the test, not hang it.
        tokio::time::timeout(Duration::from_secs(10), async {
            wind_down(
                std::future::pending::<()>(), // a library that never lets go
                QUICK,
                dbus::Forwarders {
                    stop,
                    drain,
                    abort: vec![other],
                },
            )
            .await;
            while !(drain_gone.load(Ordering::SeqCst) && other_gone.load(Ordering::SeqCst)) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("stopping did not finish within its bounds");
    }

    #[tokio::test]
    async fn the_wifi_cleanup_runs_when_the_engine_stop_hangs() {
        let cleaned = AtomicBool::new(false);
        stop_daemon(
            std::future::pending::<()>(),
            Duration::from_millis(20),
            async { cleaned.store(true, Ordering::SeqCst) },
        )
        .await;
        assert!(cleaned.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn the_wifi_cleanup_runs_when_something_holds_the_core_lock() {
        let core = Arc::new(Mutex::new(test_core()));
        let _held = core.lock().await; // a slow restart, for instance
        let cleaned = AtomicBool::new(false);
        stop_daemon(
            async { core.lock().await.shutdown().await },
            Duration::from_millis(20),
            async { cleaned.store(true, Ordering::SeqCst) },
        )
        .await;
        assert!(cleaned.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn the_wifi_cleanup_also_runs_after_a_clean_engine_stop() {
        let mut core = test_core();
        let _queue = install_engine(&mut core);
        let core = Mutex::new(core);
        let cleaned = AtomicBool::new(false);
        stop_daemon(
            async { core.lock().await.shutdown().await },
            SHUTDOWN_ENGINE_BOUND,
            async { cleaned.store(true, Ordering::SeqCst) },
        )
        .await;
        assert!(cleaned.load(Ordering::SeqCst) && core.lock().await.engine.is_none());
    }

    #[test]
    fn the_exit_fits_in_the_shells_wait_for_the_daemon() {
        // `waitForFinished(3000)` in corebridge.cpp, after SIGTERM.
        assert!(SHUTDOWN_ENGINE_BOUND + WIFI_STOP_CLEANUP_BOUND < Duration::from_secs(3));
        assert!(
            SHUTDOWN_ENGINE_BOUND + WIFI_STOP_CLEANUP_BOUND <= Duration::from_millis(2800),
            "the engine and the Wi-Fi cleanup leave no margin in the shell's wait"
        );
        assert!(SHUTDOWN_STOP.service + SHUTDOWN_STOP.drain <= SHUTDOWN_ENGINE_BOUND);
    }

    #[test]
    fn the_library_winds_down_before_the_core_gives_up_on_it() {
        // The sessions of a stopping library get `STOP_WIND_DOWN` (an inbound one needs up to
        // 400 ms to close): the core's bounds on `RQS::stop` are above it, at exit and at a
        // restart, or the library's last events would be cut off.
        assert!(rqs_lib::manager::STOP_WIND_DOWN >= Duration::from_millis(400));
        assert!(rqs_lib::manager::STOP_WIND_DOWN < SHUTDOWN_STOP.service);
        assert!(SHUTDOWN_STOP.service < RESTART_STOP.service);
    }

    /// What the user's Cancel puts on the bus: an action, not a word of the library.
    fn lib_message(id: &str) -> ChannelMessage {
        ChannelMessage {
            id: id.to_owned(),
            msg: rqs_lib::channel::Message::Lib {
                action: rqs_lib::channel::TransferAction::TransferCancel,
            },
        }
    }

    const SHORT: Duration = Duration::from_millis(60);

    #[tokio::test]
    async fn a_send_has_started_when_its_own_first_event_arrives() {
        let (tx, mut rx) = broadcast::channel(8);
        tx.send(client_message("other")).unwrap();
        tx.send(client_message("ble:A")).unwrap();
        assert!(send_started(&mut rx, "ble:A", SHORT).await);
    }

    #[tokio::test]
    async fn events_of_other_transfers_do_not_count_as_a_start() {
        let (tx, mut rx) = broadcast::channel(8);
        tx.send(client_message("other")).unwrap();
        assert!(!send_started(&mut rx, "ble:A", SHORT).await);
    }

    #[tokio::test]
    async fn an_action_on_the_bus_is_not_the_start_of_a_send() {
        let (tx, mut rx) = broadcast::channel(8);
        tx.send(lib_message("ble:A")).unwrap();
        assert!(!send_started(&mut rx, "ble:A", SHORT).await);
    }

    #[tokio::test]
    async fn silence_is_a_send_that_never_started() {
        let (_tx, mut rx) = broadcast::channel::<ChannelMessage>(8);
        assert!(!send_started(&mut rx, "ble:A", SHORT).await);
    }

    #[tokio::test]
    async fn a_stopped_service_is_not_reported_as_a_failed_send() {
        let (tx, mut rx) = broadcast::channel::<ChannelMessage>(8);
        drop(tx);
        assert!(send_started(&mut rx, "ble:A", SHORT).await);
    }
}
