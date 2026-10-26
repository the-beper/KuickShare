//! Session D-Bus API consumed by the Qt shell.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex as StdMutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use rqs_lib::{
    EndpointInfo, TransferState,
    channel::{ChannelMessage, Message, MessageClient, TransferAction, TransferKind},
    hdl::info::TransferPayload,
};
use tokio::{
    sync::{Mutex, broadcast, mpsc},
    task::JoinHandle,
};
use zbus::{
    Connection, Variant, connection, fdo, interface,
    object_server::{InterfaceRef, SignalEmitter},
};

use crate::{
    allowed::{self, Allowed},
    app::{
        ActiveTransfers, Core, Endpoints, Redirects, SharedHistory, TrustedIps, needs_lan_retry,
    },
    history::{self, HistoryEntry},
    safety,
    trust::{self, Trust, TrustEvent},
};

pub const BUS_NAME: &str = "io.github.the_beper.KuickShare";
pub const OBJECT_PATH: &str = "/io/github/the_beper/KuickShare";

type SharedCore = Arc<Mutex<Core>>;

struct Service {
    core: SharedCore,
}

fn failed(e: anyhow::Error) -> fdo::Error {
    fdo::Error::Failed(format!("{e:#}"))
}

impl Service {
    async fn send_action(&self, id: &str, action: TransferAction) -> fdo::Result<()> {
        // Lib actions travel on the same broadcast channel the service listens to.
        let core = self.core.lock().await;
        core.send_lib_action(id, action).map_err(failed)
    }
}

#[interface(name = "io.github.the_beper.KuickShare1")]
impl Service {
    async fn accept(&self, id: &str) -> fdo::Result<()> {
        self.send_action(id, TransferAction::ConsentAccept).await
    }

    async fn decline(&self, id: &str) -> fdo::Result<()> {
        self.send_action(id, TransferAction::ConsentDecline).await
    }

    async fn cancel(&self, id: &str) -> fdo::Result<()> {
        self.send_action(id, TransferAction::TransferCancel).await
    }

    /// Starts looking for nearby devices; they are reported via `DeviceFound`/`DeviceLost`.
    async fn start_discovery(&self) -> fdo::Result<()> {
        self.core.lock().await.start_discovery().map_err(failed)
    }

    /// Whether firewalld runs on the system bus. Without it the Wi-Fi upgrade cannot open one port
    /// in the firewall and lets the phone reach every port while the hotspot is up.
    async fn firewall_managed(&self) -> bool {
        crate::link::firewall_managed().await
    }

    /// Looks for nearby phones over Bluetooth once more (a few seconds); needs the Wi-Fi upgrade
    /// and a Bluetooth adapter, otherwise it silently finds nothing. The devices found over the
    /// LAN that no longer answer are dropped (`DeviceLost`) after the scan starts, before it returns.
    async fn scan_bluetooth(&self) -> fdo::Result<()> {
        let (endpoints, conn) = {
            let core = self.core.lock().await;
            core.scan_bluetooth().map_err(failed)?;
            core.lan_handles()
        };
        crate::app::drop_unreachable_lan(endpoints, conn).await;
        Ok(())
    }

    async fn stop_discovery(&self) {
        self.core.lock().await.stop_discovery();
    }

    async fn send_files(&self, device_id: &str, paths: Vec<String>) -> fdo::Result<()> {
        let mut core = self.core.lock().await;
        core.send_files(device_id, paths).await.map_err(failed)
    }

    /// `kind` is "text" or "url".
    async fn send_text(&self, device_id: &str, text: String, kind: &str) -> fdo::Result<()> {
        let mut core = self.core.lock().await;
        core.send_text(device_id, text, kind == "url")
            .await
            .map_err(failed)
    }

    #[zbus(property)]
    async fn device_name(&self) -> String {
        self.core.lock().await.device_name()
    }

    #[zbus(property)]
    async fn set_device_name(&self, name: String) -> zbus::Result<()> {
        let mut core = self.core.lock().await;
        core.set_device_name(&name)
            .await
            .map_err(|e| failed(e).into())
    }

    /// Empty string means the library default.
    #[zbus(property)]
    async fn download_dir(&self) -> String {
        self.core
            .lock()
            .await
            .settings()
            .download_dir
            .as_ref()
            .map(|d| d.display().to_string())
            .unwrap_or_default()
    }

    #[zbus(property)]
    async fn set_download_dir(&self, dir: String) -> zbus::Result<()> {
        let mut core = self.core.lock().await;
        core.set_download_dir(&dir).map_err(|e| failed(e).into())
    }

    /// 0 means a random port at every start.
    #[zbus(property)]
    async fn static_port(&self) -> u32 {
        self.core.lock().await.settings().static_port.unwrap_or(0)
    }

    #[zbus(property)]
    async fn set_static_port(&self, port: u32) -> zbus::Result<()> {
        let mut core = self.core.lock().await;
        core.set_static_port(port)
            .await
            .map_err(|e| failed(e).into())
    }

    #[zbus(property)]
    async fn visible(&self) -> bool {
        self.core.lock().await.settings().visible
    }

    #[zbus(property)]
    async fn set_visible(&self, visible: bool) {
        self.core.lock().await.set_visible(visible);
    }

    /// Finished transfers, newest first, as a JSON array.
    async fn history(&self) -> String {
        self.core.lock().await.history_json()
    }

    async fn clear_history(&self) {
        self.core.lock().await.clear_history();
    }

    /// Size of the saved history, in bytes; the shell offers to clear it when it gets big.
    async fn history_size(&self) -> u64 {
        self.core.lock().await.history_size()
    }

    /// Deletes one history entry, identified by the `history_uid` of its `TransferUpdated` event
    /// or the `uid` in `History()`.
    async fn remove_history(&self, uid: u64) {
        self.core.lock().await.remove_history(uid);
    }

    /// Whether finished transfers are recorded in the history.
    #[zbus(property)]
    async fn history_enabled(&self) -> bool {
        self.core.lock().await.settings().history_enabled
    }

    #[zbus(property)]
    async fn set_history_enabled(&self, enabled: bool) {
        self.core.lock().await.set_history_enabled(enabled);
    }

    /// History entries older than this many days are deleted; 0 keeps them.
    #[zbus(property)]
    async fn history_keep_days(&self) -> u32 {
        self.core.lock().await.settings().history_keep_days
    }

    #[zbus(property)]
    async fn set_history_keep_days(&self, days: u32) {
        self.core.lock().await.set_history_keep_days(days);
    }

    /// While open, other KuickShare computers can start a pairing with this one.
    async fn set_pairing(&self, open: bool) {
        self.core.lock().await.trust().set_pairing(open);
    }

    /// Starts pairing with a nearby device; progress arrives as `PairingCode` and `PairingFinished`.
    async fn pair_with(&self, device_id: &str) -> fdo::Result<()> {
        self.core.lock().await.pair_with(device_id).map_err(failed)
    }

    /// The user's answer to a `PairingCode`: do the codes match on both screens?
    async fn confirm_pairing(&self, accept: bool) {
        self.core.lock().await.trust().confirm_pairing(accept);
    }

    async fn unpair(&self, fingerprint: &str) {
        self.core.lock().await.trust().unpair(fingerprint);
    }

    /// Paired computers as a JSON array of `{name, fingerprint, public_key, paired_at}`.
    async fn trusted_devices(&self) -> String {
        self.core.lock().await.trust().peers_json()
    }

    /// Trust the sender of this transfer (matched by name and MAC).
    async fn allow_device(&self, id: &str) -> fdo::Result<()> {
        self.core.lock().await.allow_device(id).map_err(failed)
    }

    async fn forget_allowed(&self, name: &str, mac: &str) {
        self.core.lock().await.forget_allowed(name, mac);
    }

    /// Devices accepted without asking, as a JSON array of `{name, mac}`.
    async fn allowed_devices(&self) -> String {
        self.core.lock().await.allowed_json()
    }

    /// Whether transfers from trusted devices are accepted without asking.
    #[zbus(property)]
    async fn trusted_devices_enabled(&self) -> bool {
        self.core.lock().await.settings().trusted_devices
    }

    #[zbus(property)]
    async fn set_trusted_devices_enabled(&self, enabled: bool) {
        self.core.lock().await.set_trusted_devices(enabled);
    }

    /// Whether transfers may move to a temporary Wi-Fi hotspot (needs NetworkManager).
    #[zbus(property)]
    async fn wifi_upgrade(&self) -> bool {
        self.core.lock().await.settings().wifi_upgrade
    }

    #[zbus(property)]
    async fn set_wifi_upgrade(&self, enabled: bool) {
        self.core.lock().await.set_wifi_upgrade(enabled);
    }

    /// Puts every setting back to its default (the trusted devices list stays). Fails while a
    /// transfer is in progress.
    async fn reset_settings(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        // The lock is released here: the getters behind `*_changed` take it again.
        let result = self.core.lock().await.reset_settings().await;
        result.map_err(failed)?;
        let _ = self.device_name_changed(&emitter).await;
        let _ = self.download_dir_changed(&emitter).await;
        let _ = self.static_port_changed(&emitter).await;
        let _ = self.visible_changed(&emitter).await;
        let _ = self.history_enabled_changed(&emitter).await;
        let _ = self.history_keep_days_changed(&emitter).await;
        let _ = self.trusted_devices_enabled_changed(&emitter).await;
        let _ = self.wifi_upgrade_changed(&emitter).await;
        let _ = self.tray_icon_changed(&emitter).await;
        let _ = self.dolphin_integration_changed(&emitter).await;
        let _ = self.hide_when_dnd_changed(&emitter).await;
        Ok(())
    }

    /// Whether the shell shows its tray icon.
    #[zbus(property)]
    async fn tray_icon(&self) -> bool {
        self.core.lock().await.settings().tray_icon
    }

    #[zbus(property)]
    async fn set_tray_icon(&self, show: bool) {
        self.core.lock().await.set_tray_icon(show);
    }

    /// Whether Dolphin's context menu has the "Send with KuickShare" entry.
    #[zbus(property)]
    async fn dolphin_integration(&self) -> bool {
        self.core.lock().await.settings().dolphin_integration
    }

    #[zbus(property)]
    async fn set_dolphin_integration(&self, enabled: bool) {
        self.core.lock().await.set_dolphin_integration(enabled);
    }

    /// Hide this computer while the desktop's Do Not Disturb mode is on.
    #[zbus(property)]
    async fn hide_when_dnd(&self) -> bool {
        self.core.lock().await.settings().hide_when_dnd
    }

    #[zbus(property)]
    async fn set_hide_when_dnd(&self, hide: bool) {
        self.core.lock().await.set_hide_when_dnd(hide);
    }

    /// Temporarily hides this computer without changing `Visible`; not persisted.
    #[zbus(property)]
    async fn suspended(&self) -> bool {
        self.core.lock().await.suspended()
    }

    #[zbus(property)]
    async fn set_suspended(&self, suspended: bool) {
        self.core.lock().await.set_suspended(suspended);
    }

    /// `info` keys: kind, state, device, pin, preview, payload_kind, files, saved_paths, text, trusted, always_accepted (strings), history_uid, total_bytes, ack_bytes (u64).
    #[zbus(signal)]
    async fn transfer_updated(
        emitter: &SignalEmitter<'_>,
        id: &str,
        info: HashMap<&str, Variant<'_>>,
    ) -> zbus::Result<()>;

    /// `info` keys: name, ip, port, device_type, trust, medium (strings; trust is "", "kuickshare" or "trusted"; medium is "lan" or "ble").
    #[zbus(signal)]
    async fn device_found(
        emitter: &SignalEmitter<'_>,
        id: &str,
        info: HashMap<&str, Variant<'_>>,
    ) -> zbus::Result<()>;

    /// Compare `code` with the one on the other computer.
    #[zbus(signal)]
    async fn pairing_code(emitter: &SignalEmitter<'_>, code: &str, peer: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn pairing_finished(
        emitter: &SignalEmitter<'_>,
        ok: bool,
        peer: &str,
        message: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn trusted_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn device_lost(emitter: &SignalEmitter<'_>, id: &str) -> zbus::Result<()>;
}

/// Flat, D-Bus friendly view of a transfer event.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TransferEvent {
    id: String,
    kind: &'static str,
    state: String,
    device: String,
    pin: String,
    preview: String,
    total_bytes: u64,
    ack_bytes: u64,
    payload_kind: String,
    files: Vec<String>,
    /// Where the received files were saved (not kept in the history).
    saved_paths: Vec<String>,
    text: String,
    /// Name of the paired computer whose transfer was accepted automatically (empty otherwise).
    trusted: String,
    /// Set (to the device name) when the sender is on the always-accepted list.
    always_accepted: String,
    /// `uid` of the history entry written when the transfer ended (0: none, or no history).
    history_uid: u64,
    /// Unix time in seconds of the latest event seen for this transfer.
    pub(crate) last_event: i64,
}

impl TransferEvent {
    pub fn device(&self) -> &str {
        &self.device
    }

    fn from_client(id: &str, client: &MessageClient) -> Self {
        let meta = client.metadata.as_ref();
        let (files, text) = match meta.and_then(|m| m.payload.as_ref()) {
            Some(TransferPayload::Files(files)) => (files.clone(), String::new()),
            Some(TransferPayload::Text(text) | TransferPayload::Url(text)) => {
                (Vec::new(), text.clone())
            }
            // Only the network name: the password does not belong in a history file.
            Some(TransferPayload::Wifi { ssid, .. }) => (Vec::new(), ssid.clone()),
            None => (Vec::new(), String::new()),
        };
        Self {
            id: id.to_owned(),
            kind: match client.kind {
                TransferKind::Inbound => "inbound",
                TransferKind::Outbound => "outbound",
            },
            state: client
                .state
                .as_ref()
                .map(|s| format!("{s:?}"))
                .unwrap_or_default(),
            device: meta
                .and_then(|m| m.source.as_ref())
                .map(|s| s.name.clone())
                .unwrap_or_default(),
            pin: meta.and_then(|m| m.pin_code.clone()).unwrap_or_default(),
            preview: meta
                .and_then(|m| m.payload_preview.clone())
                .unwrap_or_default(),
            total_bytes: meta.map_or(0, |m| m.total_bytes),
            ack_bytes: meta.map_or(0, |m| m.ack_bytes),
            payload_kind: meta.map_or_else(String::new, |m| format!("{:?}", m.payload_kind)),
            files,
            saved_paths: meta.map_or_else(Vec::new, |m| m.saved_paths.clone()),
            text,
            trusted: String::new(),
            always_accepted: String::new(),
            history_uid: 0,
            last_event: 0,
        }
    }

    /// Overlays a newer event: later events often carry less, so empty fields keep their old value.
    fn merge(&mut self, newer: &TransferEvent) {
        self.kind = newer.kind;
        self.state.clone_from(&newer.state);
        for (old, new) in [
            (&mut self.device, &newer.device),
            (&mut self.pin, &newer.pin),
            (&mut self.preview, &newer.preview),
            (&mut self.payload_kind, &newer.payload_kind),
            (&mut self.text, &newer.text),
            (&mut self.trusted, &newer.trusted),
            (&mut self.always_accepted, &newer.always_accepted),
        ] {
            if !new.is_empty() {
                old.clone_from(new);
            }
        }
        if !newer.files.is_empty() {
            self.files.clone_from(&newer.files);
        }
        if !newer.saved_paths.is_empty() {
            self.saved_paths.clone_from(&newer.saved_paths);
        }
        self.history_uid = self.history_uid.max(newer.history_uid);
        self.last_event = self.last_event.max(newer.last_event);
        self.total_bytes = self.total_bytes.max(newer.total_bytes);
        self.ack_bytes = self.ack_bytes.max(newer.ack_bytes);
    }

    fn into_history_entry(mut self, time: i64) -> HistoryEntry {
        // A sender picks the names and the text: a huge one must not bloat the history file.
        self.files.truncate(MAX_HISTORY_FILES);
        if let Some((i, _)) = self.text.char_indices().nth(MAX_HISTORY_TEXT) {
            self.text.truncate(i);
        }
        HistoryEntry {
            uid: 0, // assigned when it is added
            time,
            kind: self.kind.to_owned(),
            state: self.state,
            device: self.device,
            payload_kind: self.payload_kind,
            files: self.files,
            text: self.text,
            total_bytes: self.total_bytes,
            trusted: !self.trusted.is_empty(),
        }
    }

    fn info(&self) -> HashMap<&'static str, Variant<'_>> {
        HashMap::from([
            ("kind", Variant::from(self.kind)),
            ("state", Variant::from(self.state.as_str())),
            ("device", Variant::from(self.device.as_str())),
            ("pin", Variant::from(self.pin.as_str())),
            ("preview", Variant::from(self.preview.as_str())),
            ("total_bytes", Variant::from(self.total_bytes)),
            ("ack_bytes", Variant::from(self.ack_bytes)),
            ("payload_kind", Variant::from(self.payload_kind.as_str())),
            // One path or name per line.
            ("files", Variant::from(self.files.join("\n"))),
            ("saved_paths", Variant::from(self.saved_paths.join("\n"))),
            ("text", Variant::from(self.text.as_str())),
            ("trusted", Variant::from(self.trusted.as_str())),
            ("history_uid", Variant::from(self.history_uid)),
            (
                "always_accepted",
                Variant::from(self.always_accepted.as_str()),
            ),
        ])
    }
}

fn is_final(state: &TransferState) -> bool {
    matches!(
        state,
        TransferState::Disconnected
            | TransferState::Rejected
            | TransferState::Cancelled
            | TransferState::Finished
    )
}

pub async fn serve(core: SharedCore) -> Result<Connection> {
    connection::Builder::session()
        .name(BUS_NAME)
        .serve_at(OBJECT_PATH, Service { core })
        .build()
        .await
        .with_context(|| format!("failed to own {BUS_NAME} on the session bus"))
}

async fn interface_ref(connection: &Connection) -> zbus::Result<InterfaceRef<Service>> {
    connection
        .object_server()
        .interface::<_, Service>(OBJECT_PATH)
        .await
}

#[derive(Clone)]
pub struct Forwarding {
    pub connection: Connection,
    pub endpoints: Endpoints,
    pub redirects: Redirects,
    pub active: ActiveTransfers,
    pub history: SharedHistory,
    pub trust: Arc<Trust>,
    pub trusted_ips: TrustedIps,
    pub allowed: Allowed,
    pub trusted_enabled: Arc<AtomicBool>,
    pub lib_sender: broadcast::Sender<ChannelMessage>,
    pub core: Weak<Mutex<Core>>,
}

/// Re-emits library events (transfers, discovered devices) as D-Bus signals.
pub fn spawn_forwarders(
    shared: Forwarding,
    transfers: broadcast::Receiver<ChannelMessage>,
    devices: broadcast::Receiver<EndpointInfo>,
) -> Vec<JoinHandle<()>> {
    vec![
        tokio::spawn(forward_transfers(shared.clone(), transfers)),
        tokio::spawn(forward_devices(
            shared.connection,
            devices,
            shared.endpoints,
            shared.trust,
            shared.trusted_ips,
            shared.core,
        )),
    ]
}

async fn forward_transfers(shared: Forwarding, mut events: broadcast::Receiver<ChannelMessage>) {
    let Forwarding {
        connection,
        endpoints,
        redirects,
        active,
        history,
        trust,
        allowed,
        trusted_enabled,
        lib_sender,
        ..
    } = shared;
    // Transfers waiting for an answer: each is declined if nobody answers in time.
    let mut consent_timers: HashMap<String, JoinHandle<()>> = HashMap::new();
    loop {
        let mut message = match events.recv().await {
            Ok(message) => message,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("dropped {n} transfer events");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };
        // A send that was redirected to another route is reported under the id the user picked.
        if let Some(picked) = redirects
            .lock()
            .unwrap()
            .iter()
            .find_map(|(picked, sent_as)| (*sent_as == message.id).then(|| picked.clone()))
        {
            message.id = picked;
        }
        let Message::Client(client) = &message.msg else {
            continue;
        };
        let decline = |id: &str| {
            let _ = lib_sender.send(ChannelMessage {
                id: id.to_owned(),
                msg: Message::Lib {
                    action: TransferAction::ConsentDecline,
                },
            });
        };

        // The sender chooses the file names and the library trusts them: refuse a transfer that tries
        // to write outside the download folder before anything can accept it.
        // Checked on every event that carries the names, not only the one asking for consent.
        if matches!(client.kind, TransferKind::Inbound)
            && !client.state.as_ref().is_some_and(is_final)
            && let Some(name) = incoming_file_names(client)
                .iter()
                .find(|n| safety::unsafe_file_name(n))
        {
            tracing::warn!(
                "declined a transfer from {}: unsafe file name {name:?}",
                message.id
            );
            decline(&message.id);
            continue;
        }
        if matches!(client.kind, TransferKind::Inbound)
            && client.state == Some(TransferState::WaitingForUserConsent)
        {
            consent_timers.entry(message.id.clone()).or_insert_with(|| {
                let id = message.id.clone();
                let sender = lib_sender.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(CONSENT_TIMEOUT).await;
                    tracing::info!("declined {id}: no answer within the time limit");
                    let _ = sender.send(ChannelMessage {
                        id,
                        msg: Message::Lib {
                            action: TransferAction::ConsentDecline,
                        },
                    });
                })
            });
        } else if let Some(timer) = consent_timers.remove(&message.id) {
            timer.abort();
        }

        let mut event = TransferEvent::from_client(&message.id, client);
        event.last_event = history::now();
        if event.device.is_empty()
            && let Some(name) = endpoints
                .lock()
                .unwrap()
                .get(&message.id)
                .and_then(|e| e.name.clone())
        {
            event.device = name;
        }

        // A paired computer announced this transfer: accept it without asking.
        if matches!(client.kind, TransferKind::Inbound)
            && event.state == "WaitingForUserConsent"
            && let Some((ip, _)) = message.id.rsplit_once(':')
            && let Some(by) = trust.take_intent(ip).map(|peer| (peer, false)).or_else(|| {
                // A phone the user trusts: same name and same MAC (only while the option is on).
                allowed::trusted_sender(
                    &allowed.lock().unwrap(),
                    trusted_enabled.load(Ordering::Relaxed),
                    &event.device,
                    || allowed::mac_for_ip(ip),
                )
                .then(|| (event.device.clone(), true))
            })
        {
            let (name, always) = by;
            if always {
                event.always_accepted.clone_from(&name);
            }
            event.trusted = name;
            let _ = lib_sender.send(ChannelMessage {
                id: message.id.clone(),
                msg: Message::Lib {
                    action: TransferAction::ConsentAccept,
                },
            });
        }

        // Track what is in flight, and record a transfer in the history when it ends.
        if let Some(state) = &client.state {
            let mut active = active.lock().unwrap();
            if is_final(state) {
                redirects.lock().unwrap().remove(&message.id);
                // Only the first final state counts (Finished may be followed by Disconnected).
                if let Some(mut snapshot) = active.remove(&message.id) {
                    snapshot.merge(&event);
                    event.history_uid = history
                        .lock()
                        .unwrap()
                        .add(snapshot.into_history_entry(history::now()));
                }
            } else {
                active
                    .entry(message.id.clone())
                    .and_modify(|snapshot| snapshot.merge(&event))
                    .or_insert_with(|| event.clone());
            }
        }
        tracing::debug!(?event, "transfer event");

        let emitted = async {
            let iface = interface_ref(&connection).await?;
            Service::transfer_updated(iface.signal_emitter(), &event.id, event.info()).await
        };
        if let Err(e) = emitted.await {
            tracing::warn!("failed to emit TransferUpdated: {e}");
        }
    }
}

/// Addresses remembered as probed before the old ones are forgotten.
const MAX_PROBED: usize = 256;

/// True for addresses on the local network. An mDNS advert is not trusted to name any address:
/// someone could make this computer connect to an outside host (or scan one) by advertising it.
fn is_local_address(ip: &str) -> bool {
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => ip.is_private() || ip.is_link_local() || ip.is_loopback(),
        Ok(std::net::IpAddr::V6(ip)) => {
            let first = ip.segments()[0];
            ip.is_loopback() || first & 0xfe00 == 0xfc00 || first & 0xffc0 == 0xfe80
        }
        Err(_) => false,
    }
}

/// Caps on what one history entry keeps.
const MAX_HISTORY_FILES: usize = 200;
const MAX_HISTORY_TEXT: usize = 10_000;

const CONSENT_TIMEOUT: Duration = Duration::from_secs(300);

/// The names of the files a sender wants to put in the download folder (empty for text and the like).
fn incoming_file_names(client: &MessageClient) -> &[String] {
    match client.metadata.as_ref().and_then(|m| m.payload.as_ref()) {
        Some(TransferPayload::Files(files)) => files,
        _ => &[],
    }
}

/// How long a vanished device stays listed in case it re-announces itself. Android rotates its
/// mDNS instance name every few seconds, which looks like "lost" followed by "found".
const LOST_GRACE: Duration = Duration::from_secs(4);

/// The LAN entries a newly seen LAN device replaces: same address, name and type under another
/// id. A restarted app takes a new port and mDNS name, and the old announcement lingers.
fn superseded<'a>(
    known: impl Iterator<Item = &'a EndpointInfo>,
    new: &EndpointInfo,
) -> Vec<String> {
    if new.ble.is_some() || new.ip.is_none() || new.name.is_none() {
        return Vec::new();
    }
    known
        .filter(|e| {
            e.ble.is_none()
                && e.id != new.id
                && e.ip == new.ip
                && e.name == new.name
                && e.rtype == new.rtype
        })
        .map(|e| e.id.clone())
        .collect()
}

/// Tracks the latest sighting of each device so a delayed removal can tell if it is stale.
#[derive(Default)]
struct Presence(HashMap<String, u64>);

impl Presence {
    /// Records a sighting and returns its generation.
    fn seen(&mut self, id: &str) -> u64 {
        let generation = self.0.entry(id.to_owned()).or_default();
        *generation += 1;
        *generation
    }

    fn current(&self, id: &str) -> u64 {
        self.0.get(id).copied().unwrap_or_default()
    }

    /// True if nothing was seen for `id` since `generation`; the device is then forgotten.
    fn expire(&mut self, id: &str, generation: u64) -> bool {
        let stale = self.current(id) == generation;
        if stale {
            self.0.remove(id);
        }
        stale
    }
}

/// `trust` is "", "kuickshare" (runs KuickShare) or "trusted" (paired with this computer); `pairing` is
/// whether that computer has its pairing window open.
fn device_info(
    endpoint: &EndpointInfo,
    trust: &str,
    pairing: bool,
) -> HashMap<&'static str, Variant<'static>> {
    let device_type = endpoint
        .rtype
        .as_ref()
        .map(|t| format!("{t:?}"))
        .unwrap_or_default();
    HashMap::from([
        (
            "name",
            Variant::from(endpoint.name.clone().unwrap_or_default()),
        ),
        ("ip", Variant::from(endpoint.ip.clone().unwrap_or_default())),
        (
            "port",
            Variant::from(endpoint.port.clone().unwrap_or_default()),
        ),
        ("device_type", Variant::from(device_type)),
        ("trust", Variant::from(trust.to_owned())),
        ("pairing", Variant::from(pairing)),
        (
            "medium",
            Variant::from(endpoint.ble.map_or("lan", |_| "ble")),
        ),
    ])
}

async fn forward_devices(
    connection: Connection,
    mut events: broadcast::Receiver<EndpointInfo>,
    endpoints: Endpoints,
    trust: Arc<Trust>,
    trusted_ips: TrustedIps,
    core: Weak<Mutex<Core>>,
) {
    let presence = Arc::new(StdMutex::new(Presence::default()));
    let mut probed: HashMap<String, Instant> = HashMap::new();
    let cache = ProbeCache::default();
    let mut reprobe = tokio::time::interval(PAIRING_REPROBE);
    loop {
        let endpoint = tokio::select! {
            received = events.recv() => match received {
                Ok(endpoint) => endpoint,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            // The other computer's window can open or close at any time and mDNS says nothing about it.
            _ = reprobe.tick() => {
                if trust.pairing_open() {
                    let known: Vec<EndpointInfo> = {
                        let cache = cache.lock().unwrap();
                        let endpoints = endpoints.lock().unwrap();
                        endpoints
                            .values()
                            .filter(|e| e.ip.as_ref().is_some_and(|ip| cache.contains_key(ip)))
                            .cloned()
                            .collect()
                    };
                    for endpoint in known {
                        tokio::spawn(probe_device(
                            connection.clone(),
                            endpoint,
                            trust.clone(),
                            trusted_ips.clone(),
                            endpoints.clone(),
                            cache.clone(),
                        ));
                    }
                }
                continue;
            }
        };

        // A vanished device is an endpoint with only its id set (mDNS) or `present` false (Bluetooth).
        if endpoint.present == Some(true) {
            presence.lock().unwrap().seen(&endpoint.id);
            let replaced = {
                let mut endpoints = endpoints.lock().unwrap();
                let replaced = superseded(endpoints.values(), &endpoint);
                for id in &replaced {
                    endpoints.remove(id);
                }
                endpoints.insert(endpoint.id.clone(), endpoint.clone());
                replaced
            };
            let (known_trust, known_pairing) = match endpoint
                .ip
                .as_ref()
                .and_then(|ip| cache.lock().unwrap().get(ip).copied())
            {
                Some((true, _)) => ("trusted", false),
                Some((false, pairing)) => ("kuickshare", pairing),
                None => ("", false),
            };
            let emitted = async {
                let iface = interface_ref(&connection).await?;
                Service::device_found(
                    iface.signal_emitter(),
                    &endpoint.id,
                    device_info(&endpoint, known_trust, known_pairing),
                )
                .await
            };
            if let Err(e) = emitted.await {
                tracing::warn!("failed to emit DeviceFound: {e}");
            }
            // After DeviceFound, so the list swaps the entry instead of flickering. The new entry
            // has not been checked as a KuickShare computer yet: probe its address again.
            for id in &replaced {
                emit_device_lost(&connection, id).await;
            }
            if !replaced.is_empty()
                && let Some(ip) = &endpoint.ip
            {
                probed.remove(ip);
            }

            // A phone seen only over Bluetooth: ask the core to keep searching the LAN. Spawned,
            // because a send may hold the core while it waits for this very loop to list the phone.
            if needs_lan_retry(endpoints.lock().unwrap().values(), &endpoint)
                && let Some(core) = core.upgrade()
            {
                let ble = endpoint.clone();
                tokio::spawn(async move { core.lock().await.watch_lan(ble) });
            }

            // Is it a computer running KuickShare? Android rotates its announcements often: probe
            // each address only now and then.
            if let Some(ip) = endpoint.ip.clone()
                && is_local_address(&ip)
                && probed
                    .get(&ip)
                    .is_none_or(|at| at.elapsed() > Duration::from_secs(20))
            {
                // Forget the addresses not seen for a while, so a stream of adverts cannot grow this.
                if probed.len() >= MAX_PROBED {
                    probed.retain(|_, at| at.elapsed() < Duration::from_secs(600));
                }
                probed.insert(ip, Instant::now());
                tokio::spawn(probe_device(
                    connection.clone(),
                    endpoint,
                    trust.clone(),
                    trusted_ips.clone(),
                    endpoints.clone(),
                    cache.clone(),
                ));
            }
        } else {
            let generation = presence.lock().unwrap().current(&endpoint.id);
            tokio::spawn(expire_device(
                connection.clone(),
                endpoint.id,
                generation,
                presence.clone(),
                endpoints.clone(),
            ));
        }
    }
}

/// The last probe answer per address: re-announcing a device (mDNS adverts) must not reset its card.
type ProbeCache = Arc<StdMutex<HashMap<String, (bool, bool)>>>;

/// How often the computers already seen as running KuickShare are asked again while the pairing window is open.
const PAIRING_REPROBE: Duration = Duration::from_secs(4);

/// Checks whether the device runs KuickShare and updates the card with the answer.
async fn probe_device(
    connection: Connection,
    endpoint: EndpointInfo,
    trust: Arc<Trust>,
    trusted_ips: TrustedIps,
    endpoints: Endpoints,
    cache: ProbeCache,
) {
    let Some(ip) = endpoint.ip.clone() else {
        return;
    };
    let Ok(ip_addr) = ip.parse() else {
        return;
    };
    let Ok(result) = trust
        .probe(SocketAddr::new(ip_addr, trust::DEFAULT_PORT))
        .await
    else {
        // Not a KuickShare computer (or it is not reachable any more): drop what was known about it.
        trusted_ips.lock().unwrap().remove(&ip);
        let was_known = cache.lock().unwrap().remove(&ip).is_some();
        if was_known && endpoints.lock().unwrap().contains_key(&endpoint.id) {
            let emitted = async {
                let iface = interface_ref(&connection).await?;
                Service::device_found(
                    iface.signal_emitter(),
                    &endpoint.id,
                    device_info(&endpoint, "", false),
                )
                .await
            };
            if let Err(e) = emitted.await {
                tracing::warn!("failed to emit DeviceFound: {e}");
            }
        }
        return;
    };
    cache
        .lock()
        .unwrap()
        .insert(ip.clone(), (result.trusted, result.pairing));
    if result.trusted {
        trusted_ips.lock().unwrap().insert(ip);
    } else {
        trusted_ips.lock().unwrap().remove(&ip);
    }
    if !endpoints.lock().unwrap().contains_key(&endpoint.id) {
        return;
    }
    let label = if result.trusted {
        "trusted"
    } else {
        "kuickshare"
    };
    let emitted = async {
        let iface = interface_ref(&connection).await?;
        Service::device_found(
            iface.signal_emitter(),
            &endpoint.id,
            device_info(&endpoint, label, result.pairing),
        )
        .await
    };
    if let Err(e) = emitted.await {
        tracing::warn!("failed to emit DeviceFound: {e}");
    }
}

/// Re-emits pairing events as D-Bus signals.
pub async fn forward_trust_events(
    connection: Connection,
    mut events: mpsc::UnboundedReceiver<TrustEvent>,
) {
    while let Some(event) = events.recv().await {
        let emitted = async {
            let iface = interface_ref(&connection).await?;
            let emitter = iface.signal_emitter();
            match &event {
                TrustEvent::PairingCode { code, peer } => {
                    Service::pairing_code(emitter, code, peer).await
                }
                TrustEvent::PairingFinished { ok, peer, message } => {
                    Service::pairing_finished(emitter, *ok, peer, message).await
                }
                TrustEvent::PeersChanged => Service::trusted_changed(emitter).await,
            }
        };
        if let Err(e) = emitted.await {
            tracing::warn!("failed to emit a pairing signal: {e}");
        }
    }
}

fn failed_transfer_event(id: &str, device: &str) -> TransferEvent {
    TransferEvent {
        id: id.to_owned(),
        kind: "outbound",
        state: format!("{:?}", TransferState::Disconnected),
        device: device.to_owned(),
        last_event: history::now(),
        ..TransferEvent::default()
    }
}

/// Reports a transfer that never started as failed (`Disconnected`), without a history entry.
pub async fn emit_failed_transfer(connection: &Connection, id: &str, device: &str) {
    let event = failed_transfer_event(id, device);
    let emitted = async {
        let iface = interface_ref(connection).await?;
        Service::transfer_updated(iface.signal_emitter(), &event.id, event.info()).await
    };
    if let Err(e) = emitted.await {
        tracing::warn!("failed to emit TransferUpdated: {e}");
    }
}

async fn expire_device(
    connection: Connection,
    id: String,
    generation: u64,
    presence: Arc<StdMutex<Presence>>,
    endpoints: Endpoints,
) {
    tokio::time::sleep(LOST_GRACE).await;
    if !presence.lock().unwrap().expire(&id, generation) {
        return;
    }
    endpoints.lock().unwrap().remove(&id);
    emit_device_lost(&connection, &id).await;
}

pub async fn emit_device_lost(connection: &Connection, id: &str) {
    let emitted = async {
        let iface = interface_ref(connection).await?;
        Service::device_lost(iface.signal_emitter(), id).await
    };
    if let Err(e) = emitted.await {
        tracing::warn!("failed to emit DeviceLost: {e}");
    }
}

#[cfg(test)]
mod tests {
    use rqs_lib::{
        TransferState,
        hdl::info::{TransferMetadata, TransferPayloadKind},
    };

    use super::*;

    #[test]
    fn a_new_announcement_replaces_the_old_ones_of_the_same_device() {
        let lan = |id: &str, ip: &str, name: &str| EndpointInfo {
            id: id.into(),
            ip: Some(ip.into()),
            name: Some(name.into()),
            ..EndpointInfo::default()
        };
        let old = lan("10.0.0.5:4000", "10.0.0.5", "PC");
        let other_host = lan("10.0.0.6:4000", "10.0.0.6", "PC");
        let other_name = lan("10.0.0.5:4002", "10.0.0.5", "Phone");
        let new = lan("10.0.0.5:4001", "10.0.0.5", "PC");
        let known = [old, other_host, other_name, new.clone()];
        assert_eq!(superseded(known.iter(), &new), ["10.0.0.5:4000"]);
        let unnamed = EndpointInfo { name: None, ..new };
        assert!(superseded(known.iter(), &unnamed).is_empty());
    }

    fn ble_endpoint(present: bool) -> EndpointInfo {
        EndpointInfo {
            fullname: "ble:G483._ble".into(),
            id: "ble:G483".into(),
            name: Some("Pixel".into()),
            present: Some(present),
            ble: Some(rqs_lib::BleTarget {
                endpoint_id: *b"G483",
            }),
            ..EndpointInfo::default()
        }
    }

    fn text(info: &HashMap<&'static str, Variant<'static>>, key: &str) -> String {
        String::try_from(info[key].clone()).unwrap()
    }

    #[test]
    fn a_send_that_never_started_is_an_outbound_disconnection_without_data() {
        let event = failed_transfer_event("ble:G483", "Pixel");
        assert_eq!(event.id, "ble:G483");
        assert_eq!(event.kind, "outbound");
        assert_eq!(event.state, "Disconnected");
        assert_eq!(event.device, "Pixel");
        assert_eq!((event.total_bytes, event.ack_bytes), (0, 0));
    }

    #[test]
    fn ble_device_has_the_ble_medium_and_no_address() {
        let info = device_info(&ble_endpoint(true), "", false);
        assert_eq!(text(&info, "medium"), "ble");
        assert_eq!(text(&info, "name"), "Pixel");
        assert_eq!(
            (text(&info, "ip"), text(&info, "port")),
            (String::new(), String::new())
        );
    }

    #[test]
    fn lan_device_has_the_lan_medium() {
        let lan = EndpointInfo {
            id: "10.0.0.2:4000".into(),
            ip: Some("10.0.0.2".into()),
            port: Some("4000".into()),
            present: Some(true),
            ..EndpointInfo::default()
        };
        let info = device_info(&lan, "kuickshare", true);
        assert_eq!(text(&info, "medium"), "lan");
        assert_eq!(info["pairing"], Variant::from(true));
        assert_eq!(text(&info, "ip"), "10.0.0.2");
    }

    #[test]
    fn event_without_metadata_has_empty_fields() {
        let client = MessageClient {
            kind: TransferKind::Inbound,
            state: Some(TransferState::WaitingForUserConsent),
            metadata: None,
        };
        let event = TransferEvent::from_client("t1", &client);
        assert_eq!(event.kind, "inbound");
        assert_eq!(event.state, "WaitingForUserConsent");
        assert_eq!((event.total_bytes, event.device.as_str()), (0, ""));
    }

    #[test]
    fn event_carries_pin_and_progress() {
        let client = MessageClient {
            kind: TransferKind::Outbound,
            state: None,
            metadata: Some(TransferMetadata {
                id: "t2".into(),
                source: None,
                pin_code: Some("1234".into()),
                payload_kind: TransferPayloadKind::Files,
                payload_preview: None,
                payload: None,
                total_bytes: 100,
                ack_bytes: 40,
                saved_paths: Vec::new(),
            }),
        };
        let event = TransferEvent::from_client("t2", &client);
        assert_eq!(event.kind, "outbound");
        assert_eq!(event.pin, "1234");
        assert_eq!((event.total_bytes, event.ack_bytes), (100, 40));
        let info = event.info();
        assert_eq!(info.len(), 14);
        assert_eq!(info["ack_bytes"], Variant::from(40u64));
    }

    #[test]
    fn reappearing_device_is_not_expired() {
        let mut presence = Presence::default();
        presence.seen("a");
        let snapshot = presence.current("a");
        presence.seen("a"); // announced again before the grace period ended
        assert!(!presence.expire("a", snapshot));
        assert!(presence.expire("a", presence.current("a")));
        assert_eq!(presence.current("a"), 0);
    }

    #[test]
    fn merging_keeps_details_that_later_events_lack() {
        let mut snapshot = TransferEvent {
            kind: "inbound",
            state: "ReceivingFiles".into(),
            device: "Pixel".into(),
            files: vec!["a.mp4".into()],
            total_bytes: 100,
            ack_bytes: 40,
            ..Default::default()
        };
        // The final event of an interrupted transfer carries no metadata at all.
        let last = TransferEvent {
            kind: "inbound",
            state: "Disconnected".into(),
            ..Default::default()
        };
        snapshot.merge(&last);

        let entry = snapshot.into_history_entry(7);
        assert_eq!(entry.state, "Disconnected");
        assert_eq!(
            (entry.device.as_str(), entry.total_bytes, entry.time),
            ("Pixel", 100, 7)
        );
        assert_eq!(entry.files, ["a.mp4"]);
    }

    #[test]
    fn only_addresses_on_the_local_network_are_probed() {
        for ip in [
            "192.168.1.20",
            "10.0.0.5",
            "172.16.4.1",
            "169.254.3.3",
            "127.0.0.1",
            "fd12::1",
            "fe80::1",
            "::1",
        ] {
            assert!(is_local_address(ip), "{ip}");
        }
        for ip in [
            "8.8.8.8",
            "93.184.216.34",
            "172.32.0.1",
            "2001:db8::1",
            "not an address",
            "",
        ] {
            assert!(!is_local_address(ip), "{ip}");
        }
    }

    #[test]
    fn history_remembers_that_a_transfer_was_trusted() {
        let mut snapshot = TransferEvent {
            state: "Finished".into(),
            ..Default::default()
        };
        assert!(!snapshot.clone().into_history_entry(1).trusted);
        // The first event of an auto-accepted transfer carries the name; later ones do not.
        snapshot.merge(&TransferEvent {
            trusted: "Pixel".into(),
            ..Default::default()
        });
        assert!(snapshot.into_history_entry(1).trusted);
    }

    #[test]
    fn saved_paths_survive_later_events() {
        let mut snapshot = TransferEvent {
            saved_paths: vec!["/d/a (1).txt".into()],
            ..Default::default()
        };
        snapshot.merge(&TransferEvent::default());
        assert_eq!(
            snapshot.info()["saved_paths"],
            Variant::from("/d/a (1).txt")
        );
    }

    #[test]
    fn only_terminal_states_are_final() {
        assert!(is_final(&TransferState::Finished));
        assert!(is_final(&TransferState::Cancelled));
        assert!(!is_final(&TransferState::SendingFiles));
        assert!(!is_final(&TransferState::WaitingForUserConsent));
    }
}
