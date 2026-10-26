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
    sync::{Mutex, broadcast, mpsc},
    task::JoinHandle,
};
use zbus::Connection;

use crate::{
    allowed::{self, Allowed, AllowedDevice},
    dbus::{self, TransferEvent},
    history::{self, History},
    link::{NetworkEvent, NmLink},
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

/// Where Accept and Decline send their action: the library's event channel of the running
/// engine (`None` while it is stopped). It is shared with the D-Bus service, so these calls never
/// wait for the `Core` lock, which a send or a restart can hold for seconds.
#[derive(Clone, Default)]
pub struct LibActions {
    sender: Arc<StdMutex<Option<broadcast::Sender<ChannelMessage>>>>,
    redirects: Redirects,
}

impl LibActions {
    fn set(&self, sender: Option<broadcast::Sender<ChannelMessage>>) {
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
}

struct Engine {
    rqs: RQS,
    send_tx: mpsc::Sender<SendInfo>,
    tasks: Vec<JoinHandle<()>>,
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

    async fn stop(mut self) {
        self.stop_lan_retries();
        for task in &self.tasks {
            task.abort();
        }
        self.rqs.stop().await;
    }
}

pub struct Core {
    settings: Settings,
    settings_path: Option<PathBuf>,
    engine: Option<Engine>,
    endpoints: Endpoints,
    redirects: Redirects,
    lib_actions: LibActions,
    active: ActiveTransfers,
    /// Wi-Fi networks received from phones, in memory only (see `wifi_credentials`).
    wifi_creds: wifi_credentials::Shared,
    /// The NetworkManager link, shared by the Wi-Fi upgrade and "Join this network".
    nm: Arc<NmLink>,
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
        Self {
            allowed: Allowed::new(StdMutex::new(settings.allowed_devices.clone())),
            trusted_enabled: Arc::new(AtomicBool::new(settings.trusted_devices)),
            settings,
            settings_path,
            engine: None,
            endpoints: Endpoints::default(),
            lib_actions: LibActions {
                sender: Arc::default(),
                redirects: redirects.clone(),
            },
            redirects,
            active: ActiveTransfers::default(),
            wifi_creds: wifi_credentials::Shared::default(),
            nm: Arc::new(NmLink::new()),
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

    /// Tells the core which mutex it lives in; needed for `watch_lan`.
    pub fn set_handle(&mut self, handle: Weak<Mutex<Core>>) {
        self.handle = handle;
    }

    pub async fn start(&mut self, conn: Connection) -> Result<()> {
        if let Some(events) = self.trust_events.take() {
            tokio::spawn(dbus::forward_trust_events(conn.clone(), events));
        }
        let any = SocketAddr::from(([0, 0, 0, 0], trust::DEFAULT_PORT));
        if let Err(e) = self.trust.listen(any).await {
            // Trusted devices are optional: sharing works without them.
            tracing::warn!("trusted devices unavailable: {e:#}");
        }
        self.conn = Some(conn);
        if self.settings.wifi_upgrade {
            remove_stale_wifi_profiles().await;
        }
        self.start_engine(false).await
    }

    pub async fn shutdown(&mut self) {
        self.lib_actions.set(None);
        if let Some(engine) = self.engine.take() {
            engine.stop().await;
        }
        self.redirects.lock().unwrap().clear();
        if self.settings.wifi_upgrade {
            remove_stale_wifi_profiles().await;
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
        if BleTarget::parse(endpoint_id).is_some() {
            bail!("Bluetooth devices cannot be paired");
        }
        let ip = self
            .endpoints
            .lock()
            .unwrap()
            .get(endpoint_id)
            .and_then(|e| e.ip.clone())
            .with_context(|| format!("unknown device {endpoint_id}"))?;
        Ok(SocketAddr::new(ip.parse()?, trust::DEFAULT_PORT))
    }

    /// Remembers the sender of a transfer in progress: from now on its transfers are accepted
    /// without asking. Needs the device name and its MAC address (same network only).
    pub fn allow_device(&mut self, transfer_id: &str) -> Result<()> {
        let name = self
            .active
            .lock()
            .unwrap()
            .get(transfer_id)
            .map(|t| t.device().to_owned())
            .filter(|n| !n.is_empty())
            .context("unknown transfer or device without a name")?;
        let mac = transfer_id
            .rsplit_once(':')
            .and_then(|(ip, _)| allowed::mac_for_ip(ip))
            .context("cannot read the MAC address of this device (only devices on the same network can be trusted)")?;
        let device = AllowedDevice { name, mac };
        let mut list = self.allowed.lock().unwrap();
        if !list.contains(&device) {
            list.push(device);
        }
        self.settings.allowed_devices = list.clone();
        drop(list);
        self.save();
        Ok(())
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
        self.trust.set_name(self.device_name());
        Ok(())
    }

    pub fn set_trusted_devices(&mut self, enabled: bool) {
        self.settings.trusted_devices = enabled;
        self.trusted_enabled.store(enabled, Ordering::Relaxed);
        self.save();
    }

    pub fn set_wifi_upgrade(&mut self, enabled: bool) {
        self.settings.wifi_upgrade = enabled;
        if let Some(engine) = &self.engine {
            engine.rqs.set_wifi_link(wifi_link(enabled, &self.nm));
        }
        self.save();
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
    /// scan looks for the phones that remain.
    pub async fn network_lost(&mut self) {
        if !self.settings.wifi_upgrade {
            return;
        }
        let gone = take_lan_endpoints(&mut self.endpoints.lock().unwrap());
        if let Some(conn) = &self.conn {
            for id in &gone {
                dbus::emit_device_lost(conn, id).await;
            }
        }
        if let Err(e) = self.scan_bluetooth() {
            tracing::debug!("no Bluetooth scan after the network was lost: {e}");
        }
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

    pub async fn send_files(&mut self, endpoint_id: &str, paths: Vec<String>) -> Result<()> {
        if paths.is_empty() {
            bail!("no files to send");
        }
        for path in &paths {
            fs::metadata(path).with_context(|| format!("cannot read {path}"))?;
        }
        self.send(endpoint_id, OutboundPayload::Files(paths)).await
    }

    pub async fn send_text(&mut self, endpoint_id: &str, text: String, url: bool) -> Result<()> {
        if text.trim().is_empty() {
            bail!("nothing to send");
        }
        let text_type = if url {
            TextPayloadType::Url
        } else {
            TextPayloadType::Text
        };
        self.send(endpoint_id, OutboundPayload::Text { text, text_type })
            .await
    }

    async fn send(&mut self, endpoint_id: &str, ob: OutboundPayload) -> Result<()> {
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
        let endpoint = self.choose_route(picked).await?;
        let engine = self.engine.as_ref().context("service is not running")?;
        let ble = endpoint.ble.is_some();
        if ble && !self.settings.wifi_upgrade {
            bail!("Sending over Bluetooth needs the Wi-Fi upgrade, which is off (see Settings)");
        }
        // A paired KuickShare computer accepts the transfer by itself if it is told in advance.
        let trusted = endpoint
            .ip
            .as_ref()
            .is_some_and(|ip| self.trusted_ips.lock().unwrap().contains(ip));
        if trusted && let Ok(addr) = self.control_addr(&endpoint.id) {
            let announced =
                tokio::time::timeout(Duration::from_secs(2), self.trust.announce(addr)).await;
            if !matches!(announced, Ok(Ok(()))) {
                tracing::info!("could not announce the transfer to the paired computer");
            }
        }
        let name = endpoint.name.clone().unwrap_or_default();
        let addr = if ble {
            String::new()
        } else {
            format!(
                "{}:{}",
                endpoint.ip.unwrap_or_default(),
                endpoint.port.unwrap_or_default()
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
        if let Err(e) = engine.send_tx.send(send_info).await {
            self.redirects.lock().unwrap().remove(endpoint_id);
            return Err(e).context("service stopped while sending");
        }
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
            ));
        }
        Ok(())
    }

    /// Decides which endpoint a send really goes to. A device found over the LAN is probed first:
    /// a phone whose Wi-Fi went off sends no mDNS goodbye, so its entry would stay for minutes.
    /// If it is gone the entry is dropped and the same phone is tried over Bluetooth; the other
    /// way round, a Bluetooth phone that answers on the LAN is reached there. When no LAN entry
    /// is known for a Bluetooth phone, the mDNS search is restarted once and given
    /// `LAN_LOOKUP_TIMEOUT` to find it (see `look_up_lan`). The other way round, when a LAN phone
    /// is gone and no Bluetooth entry is known, a Bluetooth scan window is started and given
    /// `BLE_LOOKUP_TIMEOUT` to see it (see `look_up_bluetooth`). The `Core` lock is held by the
    /// caller meanwhile: for a Bluetooth pick at most `LAN_LOOKUP_TIMEOUT` (1.5 s) plus
    /// two `PROBE_TIMEOUT` (1.5 s each) per probed candidate; for a LAN pick at most two
    /// `PROBE_TIMEOUT` plus `BLE_LOOKUP_TIMEOUT` (3 s).
    async fn choose_route(&mut self, picked: EndpointInfo) -> Result<EndpointInfo> {
        if picked.ble.is_none() {
            if probe_twice(&lan_addr(&picked)).await {
                return Ok(picked);
            }
            self.endpoints.lock().unwrap().remove(&picked.id);
            if let Some(conn) = &self.conn {
                dbus::emit_device_lost(conn, &picked.id).await;
            }
            let mut ble = None;
            if self.settings.wifi_upgrade {
                ble = self.same_device(&picked, true);
                if ble.is_none() {
                    tracing::info!("{} is gone: looking for it over Bluetooth", picked.id);
                    ble = self.look_up_bluetooth(&picked).await;
                }
            }
            let Some(ble) = ble else {
                if self.settings.wifi_upgrade {
                    tracing::info!("{} is gone and has no Bluetooth entry", picked.id);
                } else {
                    tracing::info!(
                        "{} is gone (Wi-Fi Direct is off: no Bluetooth lookup)",
                        picked.id
                    );
                }
                bail!("The device is not reachable");
            };
            tracing::info!("{} is gone, sending to {} (same name)", picked.id, ble.id);
            return Ok(ble);
        }
        let mut lan = self.same_device(&picked, false);
        if lan.is_none() && self.settings.wifi_upgrade && self.lan_entries(&picked) == 0 {
            lan = self.look_up_lan(&picked).await;
        }
        let name = picked.name.as_deref().unwrap_or_default();
        match lan {
            Some(lan) if probe_twice(&lan_addr(&lan)).await => {
                tracing::info!(
                    "{} answers on the LAN, sending to {} (same name)",
                    picked.id,
                    lan.id
                );
                return Ok(lan);
            }
            Some(_) => tracing::info!(
                "{} stays on Bluetooth: the LAN entry did not answer",
                picked.id
            ),
            None if self.lan_entries(&picked) > 1 => tracing::info!(
                "{} stays on Bluetooth: more than one LAN entry has that name",
                picked.id
            ),
            None => tracing::info!(
                "{} stays on Bluetooth: no LAN entry named \"{name}\"",
                picked.id
            ),
        }
        Ok(picked)
    }

    /// A phone was seen over Bluetooth only: restarts the mDNS search every `LAN_RETRY_EVERY`, at
    /// most `LAN_RETRIES` times, until its LAN entry is known (the phone answers the first
    /// queries late). One task per device name; it ends with the discovery or the engine.
    pub fn watch_lan(&mut self, ble: EndpointInfo) {
        let Some(name) = ble.name.clone() else {
            return;
        };
        if !self.settings.wifi_upgrade || !self.lan_retry_wanted(&ble) {
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

    /// Restarts the mDNS search (the list stays) and waits up to `LAN_LOOKUP_TIMEOUT` for the
    /// phone picked over Bluetooth to show up on the LAN with the same name and device type. The
    /// library notices a phone that joined the LAN only when the phone announces itself, and
    /// NetworkManager reports nothing when it is the phone's network that changed. At most one
    /// restart per `LAN_LOOKUP_GAP`; nothing happens while the discovery is not running. No
    /// std mutex guard is held across the waits.
    async fn look_up_lan(&mut self, picked: &EndpointInfo) -> Option<EndpointInfo> {
        picked.name.as_ref()?;
        let now = Instant::now();
        if !lookup_due(self.last_lan_lookup, now) || !self.restart_discovery() {
            return None;
        }
        self.last_lan_lookup = Some(now);
        tracing::info!(
            "{} is not on the LAN list: restarting the search",
            picked.id
        );
        let deadline = tokio::time::Instant::now() + LAN_LOOKUP_TIMEOUT;
        loop {
            tokio::time::sleep(LAN_LOOKUP_POLL).await;
            if let Some(lan) = self.same_device(picked, false) {
                return Some(lan);
            }
            if tokio::time::Instant::now() >= deadline || self.lan_entries(picked) > 1 {
                return None;
            }
        }
    }

    /// Starts a Bluetooth scan window and waits up to `BLE_LOOKUP_TIMEOUT` for the phone picked
    /// over the LAN to be seen with the same name and device type. `scan_bluetooth` does nothing
    /// while a window runs or just after one; a running window delivers its sightings to the same
    /// list, so waiting is still right. Stops early on a match, or when several phones match (no
    /// guessing). No std mutex guard is held across the waits.
    async fn look_up_bluetooth(&mut self, picked: &EndpointInfo) -> Option<EndpointInfo> {
        picked.name.as_ref()?;
        if let Err(e) = self.scan_bluetooth() {
            tracing::debug!("Bluetooth lookup: no scan started: {e:#}");
        }
        let deadline = tokio::time::Instant::now() + BLE_LOOKUP_TIMEOUT;
        loop {
            tokio::time::sleep(BLE_LOOKUP_POLL).await;
            if let Some(ble) = self.same_device(picked, true) {
                return Some(ble);
            }
            if tokio::time::Instant::now() >= deadline || self.ble_entries(picked) > 1 {
                return None;
            }
        }
    }

    /// How many known Bluetooth endpoints have the name and the device type of `of`.
    fn ble_entries(&self, of: &EndpointInfo) -> usize {
        let endpoints = self.endpoints.lock().unwrap();
        matching(endpoints.values(), of, true).count()
    }

    /// How many known LAN endpoints have the name and the device type of `of`.
    fn lan_entries(&self, of: &EndpointInfo) -> usize {
        let endpoints = self.endpoints.lock().unwrap();
        matching(endpoints.values(), of, false).count()
    }

    /// The only known Bluetooth (`ble`) or LAN endpoint with the name and the device type of
    /// `of`; `None` when there is none or when more than one matches (no guessing).
    fn same_device(&self, of: &EndpointInfo, ble: bool) -> Option<EndpointInfo> {
        let endpoints = self.endpoints.lock().unwrap();
        unique_match(endpoints.values(), of, ble).cloned()
    }

    fn is_idle(&self) -> bool {
        let mut active = self.active.lock().unwrap();
        drop_stale(&mut active, history::now());
        active.is_empty()
    }

    fn ensure_idle(&self) -> Result<()> {
        if self.is_idle() {
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
        // Accept and Decline fail at once from here: the restart ends the old transfers anyway.
        self.lib_actions.set(None);
        if let Some(engine) = self.engine.take() {
            engine.stop().await;
        }
        self.redirects.lock().unwrap().clear();
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
        let tasks = dbus::spawn_forwarders(
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
                lib_sender: rqs.message_sender.clone(),
                core: self.handle.clone(),
            },
            rqs.message_sender.subscribe(),
            discovery_tx.subscribe(),
        );
        self.lib_actions.set(Some(rqs.message_sender.clone()));
        self.engine = Some(Engine {
            rqs,
            send_tx,
            tasks,
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
            tracing::info!("{} has no LAN entry yet: restarting the search", ble.id);
        }
    }
}

/// `probe`, tried once more when it fails (a lost SYN is not a gone phone).
async fn probe_twice(addr: &str) -> bool {
    probe(addr).await || probe(addr).await
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
            NetworkEvent::Lost => core.lock().await.network_lost().await,
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
) {
    let (sent_as, shown_as) = ids;
    if send_started(&mut events, &sent_as, within).await {
        return;
    }
    tracing::info!("no answer from {sent_as}: reporting the send as failed");
    // Stop the library's transfer too, or it would go on and a retry would offer the files twice.
    // No effect while it is still scanning or dialling (no request exists yet to receive it).
    let _ = bus.send(ChannelMessage {
        id: sent_as,
        msg: Message::Lib {
            action: TransferAction::TransferCancel,
        },
    });
    dbus::emit_failed_transfer(&conn, &shown_as, &device).await;
}

/// True when the transfer `id` produced an event within `within`, or when the service stopped
/// (then there is nothing to report).
async fn send_started(
    events: &mut broadcast::Receiver<ChannelMessage>,
    id: &str,
    within: Duration,
) -> bool {
    let started = async {
        loop {
            match events.recv().await {
                Ok(message) if message.id == id => return true,
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

/// Removes `kuickshare-*` Wi-Fi profiles left by a crash or by the shutdown, only when the Wi-Fi
/// upgrade is on (it is the only user of NetworkManager). Quiet when NetworkManager is absent;
/// bounded so it cannot hang the daemon.
async fn remove_stale_wifi_profiles() {
    let link = NmLink::new();
    let sweep = link.cleanup_stale();
    if tokio::time::timeout(Duration::from_secs(5), sweep)
        .await
        .is_err()
    {
        tracing::debug!("Wi-Fi profile cleanup timed out");
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

    fn message(id: &str) -> ChannelMessage {
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
        tx.send(message("other")).unwrap();
        tx.send(message("ble:A")).unwrap();
        assert!(send_started(&mut rx, "ble:A", SHORT).await);
    }

    #[tokio::test]
    async fn events_of_other_transfers_do_not_count_as_a_start() {
        let (tx, mut rx) = broadcast::channel(8);
        tx.send(message("other")).unwrap();
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
