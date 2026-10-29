//! Session D-Bus API consumed by the Qt shell.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc, Mutex as StdMutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use rqs_lib::{
    EndpointInfo, TransferState,
    channel::{ChannelMessage, Message, MessageClient, TransferKind},
    hdl::info::TransferPayload,
};
use tokio::{
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, broadcast, mpsc, watch},
    task::JoinHandle,
};
use zbus::{
    Connection, Variant, connection, fdo, interface,
    object_server::{InterfaceRef, SignalEmitter},
};

use crate::{
    allowed::{self, Allowed},
    app::{
        ActiveTransfers, Core, Endpoints, LibActions, Redirects, SharedHistory, TrustedIps,
        UpgradeRefused, change_wifi_upgrade, needs_lan_retry,
    },
    consent::{Answer, By, ConsentError, Consents, Opened},
    history::{self, HistoryEntry},
    link::LinkNotice,
    requirements::{RequirementsReport, SystemEngine},
    safety,
    trust::{self, Trust, TrustEvent},
    wifi_credentials,
};

pub const BUS_NAME: &str = "io.github.the_beper.KuickShare";
pub const OBJECT_PATH: &str = "/io/github/the_beper/KuickShare";

type SharedCore = Arc<Mutex<Core>>;

struct Service {
    core: SharedCore,
    /// Every answer to an incoming transfer goes through this, not through the `Core` lock.
    consents: Consents,
    /// Cancel goes through this too: it must not wait for the `Core` lock either.
    lib_actions: LibActions,
    /// The Wi-Fi requirements engine. The report and the gate on the Wi-Fi upgrade go through
    /// this, never through the `Core` lock.
    requirements: Arc<SystemEngine>,
}

fn failed(e: anyhow::Error) -> fdo::Error {
    fdo::Error::Failed(format!("{e:#}"))
}

/// Why an answer to a transfer was not taken, as D-Bus error names (`<interface>.Error.<Name>`).
/// `NotWaiting`, `Expired` and `StaleToken` mean nothing waits for that answer any more; the
/// others (`DeviceChanged`, `NoMac`, `NoName`) mean the sender cannot be trusted but the transfer
/// still waits, so the user can accept it without trusting.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "io.github.the_beper.KuickShare1.Error")]
enum AnswerError {
    #[zbus(error)]
    ZBus(zbus::Error),
    NotWaiting(String),
    Expired(String),
    StaleToken(String),
    DeviceChanged(String),
    NoMac(String),
    NoName(String),
    NotRunning(String),
    InvalidArgs(String),
}

impl From<ConsentError> for AnswerError {
    fn from(e: ConsentError) -> Self {
        let text = e.to_string();
        match e {
            ConsentError::NotWaiting => Self::NotWaiting(text),
            ConsentError::Expired => Self::Expired(text),
            ConsentError::StaleToken => Self::StaleToken(text),
            ConsentError::DeviceChanged => Self::DeviceChanged(text),
            ConsentError::NoMac => Self::NoMac(text),
            ConsentError::NoName => Self::NoName(text),
            ConsentError::NotRunning => Self::NotRunning(text),
        }
    }
}

/// `RequestWifiUpgrade` refused because the requirements report proves that neither sending nor
/// receiving can work on this computer (`<interface>.Error.RequirementsBlocked`).
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "io.github.the_beper.KuickShare1.Error")]
enum RequirementsError {
    RequirementsBlocked(String),
}

/// The errors of `RequestWifiUpgrade`: `InvalidArgs` of the bus, or `RequirementsBlocked`.
#[derive(Debug)]
enum UpgradeError {
    Bus(fdo::Error),
    Requirements(RequirementsError),
}

impl zbus::DBusError for UpgradeError {
    fn create_reply(
        &self,
        msg: &zbus::message::Header<'_>,
    ) -> zbus::Result<zbus::message::Message> {
        match self {
            Self::Bus(e) => zbus::DBusError::create_reply(e, msg),
            Self::Requirements(e) => zbus::DBusError::create_reply(e, msg),
        }
    }

    fn name(&self) -> zbus::names::ErrorName<'_> {
        match self {
            Self::Bus(e) => zbus::DBusError::name(e),
            Self::Requirements(e) => zbus::DBusError::name(e),
        }
    }

    fn description(&self) -> Option<&str> {
        match self {
            Self::Bus(e) => zbus::DBusError::description(e),
            Self::Requirements(e) => zbus::DBusError::description(e),
        }
    }
}

impl From<UpgradeRefused> for UpgradeError {
    fn from(e: UpgradeRefused) -> Self {
        match e {
            UpgradeRefused::InsecureNeedsUpgrade => {
                Self::Bus(fdo::Error::InvalidArgs(e.to_string()))
            }
            UpgradeRefused::RequirementsBlocked => {
                Self::Requirements(RequirementsError::RequirementsBlocked(e.to_string()))
            }
        }
    }
}

impl Service {
    /// The one way a user answer reaches the library. `token` is `None` for the legacy methods.
    async fn answer(
        &self,
        id: &str,
        token: Option<u64>,
        answer: Answer,
    ) -> Result<(), AnswerError> {
        let resolved = self
            .consents
            .resolve(id, token, answer, By::User, Instant::now())?;
        // The accept is on its way; saving the device may wait for the core lock, which a send
        // can hold for seconds.
        if let Some(mac) = resolved.mac {
            let mut core = self.core.lock().await;
            core.remember_device(resolved.device, mac);
        }
        Ok(())
    }
}

#[interface(name = "io.github.the_beper.KuickShare1")]
impl Service {
    /// Accepts a transfer that waits for an answer. Fails when none does for `id` (unknown,
    /// already answered, expired). It cannot tell a reused id from the original: prefer
    /// `AnswerTransfer`.
    async fn accept(&self, id: &str) -> Result<(), AnswerError> {
        self.answer(id, None, Answer::Accept).await
    }

    /// Declines a transfer that waits for an answer; fails when none does for `id`.
    async fn decline(&self, id: &str) -> Result<(), AnswerError> {
        self.answer(id, None, Answer::Decline).await
    }

    /// Answers the transfer `id` whose `TransferUpdated` carried the key `consent`. `answer` is
    /// "accept", "accept_and_trust" (accept and remember the sender by name and by the MAC
    /// address seen when the transfer arrived; fails, leaving the transfer waiting, when that
    /// cannot be verified) or "decline". Fails with the error `NotWaiting` when no transfer waits
    /// for `id`, `StaleToken` when `consent` is not the one of the transfer that waits (the id
    /// was reused), `Expired` when the time to answer is over; `accept_and_trust` fails with
    /// `NoMac`, `NoName` or `DeviceChanged` and leaves the transfer waiting (all are under
    /// `io.github.the_beper.KuickShare1.Error`).
    async fn answer_transfer(
        &self,
        id: &str,
        consent: u64,
        answer: &str,
    ) -> Result<(), AnswerError> {
        let answer = Answer::parse(answer)
            .ok_or_else(|| AnswerError::InvalidArgs(format!("unknown answer {answer:?}")))?;
        self.answer(id, Some(consent), answer).await
    }

    /// Cancels a transfer, also one that is still "Starting" (choosing its route): that send is
    /// marked and does not go out, and ends with a final `Cancelled` event, emitted before this
    /// call returns (so it precedes the events of a later send to the same device). Never waits
    /// for the `Core` lock.
    async fn cancel(&self, id: &str) -> fdo::Result<()> {
        for report in self.lib_actions.cancel(id).map_err(failed)? {
            report.emit().await;
        }
        Ok(())
    }

    /// Joins the Wi-Fi network received in a finished transfer, as a saved connection that
    /// reconnects by itself. The password stays in the core (never on the bus). Fails for an
    /// unknown or expired transfer, a WEP or unknown security type, while a transfer or a Wi-Fi
    /// upgrade is running, or when NetworkManager cannot connect.
    async fn join_network(&self, transfer_id: &str) -> fdo::Result<()> {
        let (nm, store) = self.core.lock().await.wifi_join_parts().map_err(failed)?;
        let wifi = store
            .lock()
            .unwrap()
            .get(transfer_id, Instant::now())
            .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        // Its own task: if the caller goes away (the call is dropped) the join still runs to the
        // end, so the link is freed and a failed profile removed.
        let (ssid, psk) = (wifi.ssid.clone(), wifi.psk().map(str::to_owned));
        tokio::spawn(async move { nm.join_saved(&ssid, psk.as_deref()).await })
            .await
            .map_err(|_| fdo::Error::Failed("Could not join the network: interrupted".into()))?
            .map_err(|e| fdo::Error::Failed(format!("Could not join the network: {e}")))?;
        store.lock().unwrap().remove(transfer_id);
        tracing::info!("joined a Wi-Fi network received from a phone");
        Ok(())
    }

    /// Starts looking for nearby devices; they are reported via `DeviceFound`/`DeviceLost`.
    async fn start_discovery(&self) -> fdo::Result<()> {
        self.core.lock().await.start_discovery().map_err(failed)
    }

    /// Whether firewalld runs on the system bus. Without it the Wi-Fi upgrade cannot open one port
    /// in the firewall: receiving over the computer's hotspot is refused, unless the user allowed
    /// it for this session (`RequestWifiUpgrade`).
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

    /// The `Core` lock is held only briefly at the start and at the end of a send, not while it
    /// chooses its route (see `app::run_send`).
    async fn send_files(&self, device_id: &str, paths: Vec<String>) -> fdo::Result<()> {
        let sends = self.lib_actions.sends();
        crate::app::send_files(&self.core, &sends, device_id, paths)
            .await
            .map_err(failed)
    }

    /// `kind` is "text" or "url".
    async fn send_text(&self, device_id: &str, text: String, kind: &str) -> fdo::Result<()> {
        let sends = self.lib_actions.sends();
        crate::app::send_text(&self.core, &sends, device_id, text, kind == "url")
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
        let conn = {
            let mut core = self.core.lock().await;
            core.set_visible(visible);
            core.connection()
        };
        // Setting `Visible` cancels a temporary visibility. Emitted from a task: the getter needs
        // the interface lock this setter still holds.
        if let Some(conn) = conn {
            tokio::spawn(async move { emit_visible_until_changed(&conn).await });
        }
    }

    /// Unix seconds when the temporary visibility from `MakeVisibleFor` ends; 0 when none is active.
    #[zbus(property)]
    async fn visible_until(&self) -> i64 {
        self.core.lock().await.visible_until()
    }

    /// Makes this computer visible for `seconds` even if `Visible` is false (capped at 24 h;
    /// 0 cancels). Not persisted.
    async fn make_visible_for(
        &self,
        seconds: u32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        self.core.lock().await.make_visible_for(seconds);
        let _ = self.visible_until_changed(&emitter).await;
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

    /// Trust the sender of this transfer (matched by name and MAC address, which another device on
    /// the same network can copy). Works only while the transfer waits for an answer, or within
    /// 30 s after the user accepted it; prefer `AnswerTransfer` with "accept_and_trust".
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

    /// True until the shell acknowledges that the trusted devices of an older version were removed
    /// (they were recognised by name and network address only, which can be copied). Saved, so a
    /// restart before the user saw the notice keeps it.
    #[zbus(property)]
    async fn trusted_devices_reset(&self) -> bool {
        self.core.lock().await.trusted_devices_reset()
    }

    /// The user saw the notice of `TrustedDevicesReset`.
    async fn acknowledge_trusted_devices_reset(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        let changed = self.core.lock().await.acknowledge_trusted_devices_reset();
        if changed {
            let _ = self.trusted_devices_reset_changed(&emitter).await;
        }
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

    /// Legacy write: it cannot carry the firewall consent, so it never grants it and takes back
    /// any given (see `RequestWifiUpgrade`). It goes through the same gate: turning the upgrade
    /// on while the requirements report proves that neither sending nor receiving can work fails
    /// with `org.freedesktop.DBus.Error.AccessDenied`, and nothing changes.
    #[zbus(property)]
    async fn set_wifi_upgrade(
        &self,
        enabled: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let change = change_wifi_upgrade(&self.core, &self.requirements, enabled, false)
            .await
            .map_err(|e| match e {
                UpgradeRefused::RequirementsBlocked => fdo::Error::AccessDenied(e.to_string()),
                UpgradeRefused::InsecureNeedsUpgrade => fdo::Error::InvalidArgs(e.to_string()),
            })?;
        if change.insecure_before {
            let _ = self.insecure_hotspot_allowed_changed(&emitter).await;
        }
        Ok(())
    }

    /// The single way to change the Wi-Fi upgrade together with the firewall consent.
    /// `enabled` is saved. `allow_insecure_hotspot` lets the computer's hotspot use an open
    /// firewall zone when the firewall refuses to open its one port, for this session only: it is
    /// kept in memory, and any other write to the upgrade (`WifiUpgrade`, `RequestWifiUpgrade`
    /// without it, `ResetSettings`) takes it back. `(false, true)` fails with `InvalidArgs`.
    /// Turning the upgrade on (it was off) is judged by `WifiRequirements`: when neither sending
    /// nor receiving can work on this computer it fails with `RequirementsBlocked` (under
    /// `io.github.the_beper.KuickShare1.Error`) and nothing changes; when only one direction
    /// works, or something could not be checked, it is saved. Turning it off always works.
    async fn request_wifi_upgrade(
        &self,
        enabled: bool,
        allow_insecure_hotspot: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), UpgradeError> {
        // The `Core` lock is released before the signals: the getters behind them take it again.
        let change = change_wifi_upgrade(
            &self.core,
            &self.requirements,
            enabled,
            allow_insecure_hotspot,
        )
        .await?;
        if change.enabled_before != enabled {
            let _ = self.wifi_upgrade_changed(&emitter).await;
        }
        if change.insecure_before != allow_insecure_hotspot {
            let _ = self.insecure_hotspot_allowed_changed(&emitter).await;
        }
        Ok(())
    }

    /// What the Wi-Fi upgrade needs from this computer, as JSON (schema version 1; see
    /// docs/dbus-api.md). `false` returns the cached report if it is younger than 30 s, otherwise
    /// checks again; `true` checks again unless a check ended less than 2 s ago. It only reads:
    /// it never starts a hotspot, scan or prompt, and never changes Bluetooth, Wi-Fi or the
    /// firewall. What could not be checked is Unknown in the report, not an error (at most 6 s).
    async fn wifi_requirements(&self, force_refresh: bool) -> fdo::Result<String> {
        let report = self.requirements.report(force_refresh).await;
        report.to_json().map_err(failed)
    }

    /// Whether the user allowed the computer's hotspot to use an open firewall zone for this
    /// session. Never saved: it is false after every start, and `ResetSettings` and any write to
    /// the Wi-Fi upgrade without consent make it false.
    #[zbus(property)]
    async fn insecure_hotspot_allowed(&self) -> bool {
        self.core.lock().await.insecure_hotspot_allowed()
    }

    /// Puts every setting back to its default (the trusted devices list stays). Fails while a
    /// transfer is in progress.
    async fn reset_settings(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        // The lock is released here: the getters behind `*_changed` take it again.
        let (result, until_changed) = {
            let mut core = self.core.lock().await;
            let before = core.visible_until();
            let result = core.reset_settings().await;
            (result, before != core.visible_until())
        };
        result.map_err(failed)?;
        let _ = self.device_name_changed(&emitter).await;
        let _ = self.download_dir_changed(&emitter).await;
        let _ = self.static_port_changed(&emitter).await;
        let _ = self.visible_changed(&emitter).await;
        if until_changed {
            let _ = self.visible_until_changed(&emitter).await;
        }
        let _ = self.history_enabled_changed(&emitter).await;
        let _ = self.history_keep_days_changed(&emitter).await;
        let _ = self.trusted_devices_enabled_changed(&emitter).await;
        let _ = self.wifi_upgrade_changed(&emitter).await;
        let _ = self.insecure_hotspot_allowed_changed(&emitter).await;
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

    /// `info` keys: kind, state, device, pin, preview, payload_kind, files, saved_paths, text, trusted, always_accepted (strings), history_uid, total_bytes, ack_bytes, consent (u64), reason (s). `reason` is a stable code when the library refused an incoming offer (bad_name, too_many_files, bad_size, duplicate_id, not_enough_space, destination; more may come), empty otherwise. `consent` is the token `AnswerTransfer` needs while the state is WaitingForUserConsent; 0 otherwise, or when the transfer was accepted automatically.
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

    /// The firewall did not allow the computer's hotspot, so the transfer stays on its first
    /// channel (Bluetooth: very slow). `reason`: "firewall_denied" (firewalld refused to open the
    /// port) or "firewall_missing" (firewalld is not running). More codes may be added.
    #[zbus(signal)]
    async fn wifi_upgrade_blocked(emitter: &SignalEmitter<'_>, reason: &str) -> zbus::Result<()>;

    /// A check of `WifiRequirements` ended with a different result (the time of the check does
    /// not count): a service appeared or vanished, the Bluetooth or Wi-Fi state changed, or the
    /// hotspot or the firewall showed how they work. `report_json` is as `WifiRequirements`
    /// returns it. Nothing is sent before a first report was asked for.
    #[zbus(signal)]
    async fn wifi_requirements_changed(
        emitter: &SignalEmitter<'_>,
        report_json: &str,
    ) -> zbus::Result<()>;

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
    /// Token of the consent that waits for an answer (see `consent`); 0 when none does.
    consent: u64,
    /// Why the library refused an incoming offer (`Refusal::key`), empty otherwise.
    reason: String,
    /// Unix time in seconds of the latest event seen for this transfer.
    pub(crate) last_event: i64,
}

impl TransferEvent {
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
            consent: 0,
            reason: meta
                .and_then(|m| m.refusal.as_ref())
                .map(|r| r.key().to_owned())
                .unwrap_or_default(),
            last_event: 0,
        }
    }

    /// The final event of a transfer whose engine stopped before it ended: nothing waits for an
    /// answer any more, so the consent token goes too.
    pub(crate) fn disconnected(mut self, now: i64) -> Self {
        self.state = format!("{:?}", TransferState::Disconnected);
        self.consent = 0;
        self.last_event = now;
        self
    }

    /// A transfer for the tests of other modules.
    #[cfg(test)]
    pub(crate) fn for_test(id: &str, kind: &str, state: &str, device: &str) -> Self {
        Self {
            id: id.to_owned(),
            kind: if kind == "inbound" {
                "inbound"
            } else {
                "outbound"
            },
            state: state.to_owned(),
            device: device.to_owned(),
            last_event: history::now(),
            ..Self::default()
        }
    }

    /// What a test of another module looks at: id, kind, state and consent token.
    #[cfg(test)]
    pub(crate) fn summary(&self) -> (&str, &str, &str, u64) {
        (&self.id, self.kind, &self.state, self.consent)
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
            (&mut self.reason, &newer.reason),
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
        self.consent = newer.consent;
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
            ("consent", Variant::from(self.consent)),
            ("reason", Variant::from(self.reason.as_str())),
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

pub async fn serve(
    core: SharedCore,
    consents: Consents,
    requirements: Arc<SystemEngine>,
) -> Result<Connection> {
    let reports = requirements.subscribe();
    let lib_actions = core.lock().await.lib_actions();
    let connection = connection::Builder::session()
        .name(BUS_NAME)
        .serve_at(
            OBJECT_PATH,
            Service {
                core,
                consents,
                lib_actions,
                requirements,
            },
        )
        .build()
        .await
        .with_context(|| format!("failed to own {BUS_NAME} on the session bus"))?;
    tokio::spawn(forward_requirements(connection.clone(), reports));
    Ok(connection)
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
    pub wifi_creds: wifi_credentials::Shared,
    pub history: SharedHistory,
    pub trust: Arc<Trust>,
    pub trusted_ips: TrustedIps,
    pub allowed: Allowed,
    pub trusted_enabled: Arc<AtomicBool>,
    pub consents: Consents,
    /// `Consents::generation` of the engine these forwarders belong to.
    pub generation: u64,
    pub core: Weak<Mutex<Core>>,
}

/// The tasks of one engine's forwarders. The transfer forwarder is stopped by letting it empty
/// its queue (so the last events of a stopping engine are still reported); the others are
/// aborted.
pub struct Forwarders {
    /// Set to true: `forward_transfers` sends what is queued and ends.
    pub stop: watch::Sender<bool>,
    pub drain: JoinHandle<()>,
    pub abort: Vec<JoinHandle<()>>,
}

/// Re-emits library events (transfers, discovered devices) as D-Bus signals.
pub fn spawn_forwarders(
    shared: Forwarding,
    transfers: broadcast::Receiver<ChannelMessage>,
    devices: broadcast::Receiver<EndpointInfo>,
) -> Forwarders {
    let (stop, stopped) = watch::channel(false);
    Forwarders {
        stop,
        drain: tokio::spawn(forward_transfers(shared.clone(), transfers, stopped)),
        abort: vec![
            tokio::spawn(expire_consents(shared.consents.clone())),
            tokio::spawn(forward_devices(
                DeviceForwarding {
                    connection: shared.connection,
                    endpoints: shared.endpoints,
                    trust: shared.trust,
                    trusted_ips: shared.trusted_ips,
                    consents: shared.consents,
                    generation: shared.generation,
                    core: shared.core,
                },
                devices,
            )),
        ],
    }
}

/// What `consent_step` needs, apart from the transfer.
struct ConsentCtx<'a> {
    consents: &'a Consents,
    /// The `Consents::generation` of the engine the event comes from.
    generation: u64,
    trust: &'a Trust,
    allowed: &'a Allowed,
    trusted_enabled: bool,
}

/// The consent part of one transfer event, before it is reported. Returns false when the event
/// must not be reported: it comes from an engine that was stopped, or the transfer was declined
/// (unsafe file name, too many waiting).
///
/// For an incoming transfer that waits for the user it opens the consent, accepts it at once when
/// a paired computer announced it or the sender is on the allowed list (same name and the MAC
/// address read when the consent opened), and sets the token on `event`. Any later state closes
/// the consent, a final one forgets the transfer.
fn consent_step(
    ctx: &ConsentCtx<'_>,
    id: &str,
    client: &MessageClient,
    event: &mut TransferEvent,
    now: Instant,
) -> bool {
    let consents = ctx.consents;
    if ctx.generation != consents.generation() {
        return false;
    }
    if !matches!(client.kind, TransferKind::Inbound) {
        return true;
    }
    let is_final_state = client.state.as_ref().is_some_and(is_final);

    // The sender chooses the file names and the library trusts them: refuse a transfer that tries
    // to write outside the download folder before anything can accept it.
    // Checked on every event that carries the names, not only the one asking for consent.
    if !is_final_state
        && let Some(name) = incoming_file_names(client)
            .iter()
            .find(|n| safety::unsafe_file_name(n))
    {
        tracing::info!("declined a transfer: unsafe file name");
        tracing::debug!("declined {id}: unsafe file name {name:?}");
        consents.force_decline(id);
        return false;
    }

    if client.state != Some(TransferState::WaitingForUserConsent) {
        if is_final_state {
            consents.finish(id);
        } else {
            // It moved on: nobody answers it any more.
            consents.close(id);
        }
        return true;
    }

    // From here on only `consents` can answer this transfer, whoever does.
    let ip = ip_of_id(id);
    let token = match consents.open(ctx.generation, id, &event.device, &event.pin, ip, now) {
        Opened::Token(token) => token,
        Opened::Stale => return false,
        Opened::Full => {
            if consents.note_refusal(now) {
                tracing::warn!("declined transfers: too many are waiting for an answer");
            }
            consents.force_decline(id);
            return false;
        }
    };

    // A paired computer announced this transfer, or a phone the user trusts sends it (same name
    // and same MAC address as when the consent opened, only while the option is on): accept it
    // without asking.
    let auto = ip.and_then(|ip| {
        ctx.trust
            .take_intent(&ip.to_string())
            .map(|peer| (peer, By::PairedComputer))
            .or_else(|| {
                allowed::trusted_sender(
                    &ctx.allowed.lock().unwrap(),
                    ctx.trusted_enabled,
                    &event.device,
                    || consents.opened_mac(id),
                )
                .then(|| (event.device.clone(), By::TrustedDevice))
            })
    });
    if let Some((name, by)) = auto
        && consents
            .resolve(id, Some(token), Answer::Accept, by, now)
            .is_ok()
    {
        if by == By::TrustedDevice {
            event.always_accepted.clone_from(&name);
        }
        event.trusted = name;
    }
    // Zero when it was accepted just now: there is nothing left to answer.
    event.consent = consents.token(id).unwrap_or(0);
    true
}

/// The next transfer event of the engine. Once `stopped` fires the events already queued are
/// still returned, then `None`; `None` also when the channel closed.
async fn next_transfer(
    events: &mut broadcast::Receiver<ChannelMessage>,
    stopped: &mut watch::Receiver<bool>,
    draining: &mut bool,
) -> Option<ChannelMessage> {
    loop {
        if *draining {
            match events.try_recv() {
                Ok(message) => return Some(message),
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    tracing::warn!("dropped {n} transfer events");
                }
                Err(_) => return None,
            }
            continue;
        }
        tokio::select! {
            biased;
            received = events.recv() => match received {
                Ok(message) => return Some(message),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("dropped {n} transfer events");
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            },
            // Also when the sender is gone.
            _ = stopped.changed() => *draining = true,
        }
    }
}

async fn forward_transfers(
    shared: Forwarding,
    mut events: broadcast::Receiver<ChannelMessage>,
    mut stopped: watch::Receiver<bool>,
) {
    let Forwarding {
        connection,
        endpoints,
        redirects,
        active,
        wifi_creds,
        history,
        trust,
        allowed,
        trusted_enabled,
        consents,
        generation,
        ..
    } = shared;
    let mut draining = false;
    loop {
        let Some(mut message) = next_transfer(&mut events, &mut stopped, &mut draining).await
        else {
            return;
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
        let inbound = matches!(client.kind, TransferKind::Inbound);
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
        let ctx = ConsentCtx {
            consents: &consents,
            generation,
            trust: &trust,
            allowed: &allowed,
            trusted_enabled: trusted_enabled.load(Ordering::Relaxed),
        };
        if !consent_step(&ctx, &message.id, client, &mut event, Instant::now()) {
            continue;
        }

        // Received Wi-Fi credentials are kept in memory for "Join this network" until used,
        // declined, cancelled or expired. The password is not part of `event`.
        if let Some(state) = &client.state {
            let payload = client.metadata.as_ref().and_then(|m| m.payload.as_ref());
            let stored = wifi_creds.lock().unwrap().on_event(
                &message.id,
                state,
                inbound,
                payload,
                Instant::now(),
            );
            if stored {
                let store = wifi_creds.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(wifi_credentials::TTL + Duration::from_secs(1)).await;
                    store.lock().unwrap().prune(Instant::now());
                });
            }
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
        // No device name, PIN, text or token here.
        tracing::debug!(kind = event.kind, state = %event.state, "transfer event");

        let emitted = async {
            let iface = interface_ref(&connection).await?;
            Service::transfer_updated(iface.signal_emitter(), &event.id, event.info()).await
        };
        if let Err(e) = emitted.await {
            tracing::warn!("failed to emit TransferUpdated: {e}");
        }
    }
}

/// Addresses remembered as probed before the oldest are forgotten.
const MAX_PROBED: usize = 256;
/// How often the same address is probed again when it keeps announcing itself.
const PROBE_EVERY: Duration = Duration::from_secs(20);
/// An address not probed for this long is forgotten when the list is full.
const PROBE_FORGET_AFTER: Duration = Duration::from_secs(600);
/// KuickShare probes running at once: a stream of adverts cannot start more connections.
const MAX_PROBES_AT_ONCE: usize = 8;

/// When each address was last probed for KuickShare, at most `MAX_PROBED` of them.
#[derive(Default)]
struct ProbeLog(HashMap<String, Instant>);

impl ProbeLog {
    /// True when `ip` was not probed within `PROBE_EVERY` before `now`.
    fn due(&self, ip: &str, now: Instant) -> bool {
        self.0
            .get(ip)
            .is_none_or(|at| now.saturating_duration_since(*at) > PROBE_EVERY)
    }

    /// Notes a probe of `ip` at `now`. When the list is full, the addresses not seen for
    /// `PROBE_FORGET_AFTER` go, and if that is not enough the oldest one.
    fn record(&mut self, ip: String, now: Instant) {
        if self.0.len() >= MAX_PROBED && !self.0.contains_key(&ip) {
            self.0
                .retain(|_, at| now.saturating_duration_since(*at) < PROBE_FORGET_AFTER);
            while self.0.len() >= MAX_PROBED {
                let Some(oldest) = self
                    .0
                    .iter()
                    .min_by_key(|(_, at)| **at)
                    .map(|(k, _)| k.clone())
                else {
                    break;
                };
                self.0.remove(&oldest);
            }
        }
        self.0.insert(ip, now);
    }

    fn forget(&mut self, ip: &str) {
        self.0.remove(ip);
    }
}

/// Bounds the probes that run at once.
#[derive(Clone)]
struct ProbeLimit(Arc<Semaphore>);

impl ProbeLimit {
    fn new(at_once: usize) -> Self {
        Self(Arc::new(Semaphore::new(at_once)))
    }

    /// A slot for one probe, held until it ends; `None` when all are taken (the address is
    /// probed at its next announcement).
    fn try_start(&self) -> Option<OwnedSemaphorePermit> {
        self.0.clone().try_acquire_owned().ok()
    }
}

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

/// How often the consents whose time is over are declined.
const CONSENT_TICK: Duration = Duration::from_secs(5);

/// The only place that declines a transfer nobody answered within `consent::CONSENT_TIMEOUT`.
async fn expire_consents(consents: Consents) {
    let mut tick = tokio::time::interval(CONSENT_TICK);
    loop {
        tick.tick().await;
        let expired = consents.expire(Instant::now());
        if !expired.is_empty() {
            tracing::info!(
                "declined {} transfer(s): no answer within the time limit",
                expired.len()
            );
            tracing::debug!("declined for want of an answer: {expired:?}");
        }
    }
}

/// The address in the id of an incoming transfer (`ip:port`, or `[ip]:port` for IPv6).
fn ip_of_id(id: &str) -> Option<IpAddr> {
    let (ip, _) = id.rsplit_once(':')?;
    ip.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

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

/// What `forward_devices` works with.
struct DeviceForwarding {
    connection: Connection,
    endpoints: Endpoints,
    trust: Arc<Trust>,
    trusted_ips: TrustedIps,
    consents: Consents,
    /// `Consents::generation` of the engine these forwarders belong to: what a delayed task of
    /// theirs does is dropped once that engine was replaced.
    generation: u64,
    core: Weak<Mutex<Core>>,
}

async fn forward_devices(shared: DeviceForwarding, mut events: broadcast::Receiver<EndpointInfo>) {
    let DeviceForwarding {
        connection,
        endpoints,
        trust,
        trusted_ips,
        consents,
        generation: engine,
        core,
    } = shared;
    let presence = Arc::new(StdMutex::new(Presence::default()));
    let mut probed = ProbeLog::default();
    let limit = ProbeLimit::new(MAX_PROBES_AT_ONCE);
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
                        // All slots taken: the next round asks again.
                        let Some(slot) = limit.try_start() else { break };
                        tokio::spawn(probe_device(
                            connection.clone(),
                            endpoint,
                            trust.clone(),
                            trusted_ips.clone(),
                            endpoints.clone(),
                            cache.clone(),
                            slot,
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
                probed.forget(ip);
            }

            // A phone seen only over Bluetooth: ask the core to keep searching the LAN. Spawned,
            // because a send may hold the core while it waits for this very loop to list the phone.
            if needs_lan_retry(endpoints.lock().unwrap().values(), &endpoint)
                && let Some(core) = core.upgrade()
            {
                let ble = endpoint.clone();
                tokio::spawn(async move { core.lock().await.watch_lan(engine, ble) });
            }

            // Is it a computer running KuickShare? Android rotates its announcements often: probe
            // each address only now and then, and only a few at a time.
            if let Some(ip) = endpoint.ip.clone()
                && is_local_address(&ip)
                && probed.due(&ip, Instant::now())
                && let Some(slot) = limit.try_start()
            {
                probed.record(ip, Instant::now());
                tokio::spawn(probe_device(
                    connection.clone(),
                    endpoint,
                    trust.clone(),
                    trusted_ips.clone(),
                    endpoints.clone(),
                    cache.clone(),
                    slot,
                ));
            }
        } else {
            let generation = presence.lock().unwrap().current(&endpoint.id);
            tokio::spawn(expire_device(
                connection.clone(),
                endpoint.id,
                generation,
                (consents.clone(), engine),
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
    _slot: OwnedSemaphorePermit,
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

/// Re-emits what the Wi-Fi link reports as D-Bus signals, and hands its evidence (a hotspot that
/// came up or not, what the firewall did) to the requirements engine.
pub async fn forward_link_notices(
    connection: Connection,
    mut notices: mpsc::UnboundedReceiver<LinkNotice>,
    requirements: Option<Arc<SystemEngine>>,
) {
    while let Some(notice) = notices.recv().await {
        let reason = match notice {
            LinkNotice::Evidence(evidence) => {
                if let Some(requirements) = &requirements {
                    requirements.record(evidence);
                }
                continue;
            }
            LinkNotice::WifiUpgradeBlocked(reason) => reason,
        };
        let emitted = async {
            let iface = interface_ref(&connection).await?;
            Service::wifi_upgrade_blocked(iface.signal_emitter(), reason.code()).await
        };
        if let Err(e) = emitted.await {
            tracing::warn!("failed to emit WifiUpgradeBlocked: {e}");
        }
    }
}

/// Emits `WifiRequirementsChanged` when a new report says something other than the one before.
/// The first report is only remembered: whoever asked for it has it.
async fn forward_requirements(
    connection: Connection,
    mut reports: watch::Receiver<Option<Arc<RequirementsReport>>>,
) {
    let mut last = reports.borrow_and_update().clone();
    while reports.changed().await.is_ok() {
        let Some(report) = reports.borrow_and_update().clone() else {
            continue;
        };
        let changed = last.as_ref().is_some_and(|l| !l.same_content(&report));
        last = Some(Arc::clone(&report));
        if !changed {
            continue;
        }
        let json = match report.to_json() {
            Ok(json) => json,
            Err(e) => {
                tracing::warn!("not emitting WifiRequirementsChanged: {e:#}");
                continue;
            }
        };
        let emitted = async {
            let iface = interface_ref(&connection).await?;
            Service::wifi_requirements_changed(iface.signal_emitter(), &json).await
        };
        if let Err(e) = emitted.await {
            tracing::warn!("failed to emit WifiRequirementsChanged: {e}");
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
    emit_transfer(connection, &failed_transfer_event(id, device)).await;
}

/// Reports a send the user cancelled before it went out as `Cancelled`, without a history entry.
pub async fn emit_cancelled_transfer(connection: &Connection, id: &str, device: &str) {
    let event = TransferEvent {
        state: format!("{:?}", TransferState::Cancelled),
        ..failed_transfer_event(id, device)
    };
    emit_transfer(connection, &event).await;
}

/// Emits `TransferUpdated` for `event`.
pub async fn emit_transfer(connection: &Connection, event: &TransferEvent) {
    let emitted = async {
        let iface = interface_ref(connection).await?;
        Service::transfer_updated(iface.signal_emitter(), &event.id, event.info()).await
    };
    if let Err(e) = emitted.await {
        tracing::warn!("failed to emit TransferUpdated: {e}");
    }
}

/// Removes a device that stayed silent for `LOST_GRACE`. `engine` is the `Consents::generation`
/// of the engine that saw it go: when the service restarted meanwhile, the device may have been
/// found again by the new engine (the list is shared), so nothing is removed.
async fn expire_device(
    connection: Connection,
    id: String,
    generation: u64,
    engine: (Consents, u64),
    presence: Arc<StdMutex<Presence>>,
    endpoints: Endpoints,
) {
    tokio::time::sleep(LOST_GRACE).await;
    if !presence.lock().unwrap().expire(&id, generation) {
        return;
    }
    if !engine_is_current(&engine.0, engine.1) {
        return;
    }
    endpoints.lock().unwrap().remove(&id);
    emit_device_lost(&connection, &id).await;
}

/// True while the engine of `generation` is the one that runs.
fn engine_is_current(consents: &Consents, generation: u64) -> bool {
    consents.generation() == generation
}

pub async fn emit_visible_until_changed(connection: &Connection) {
    let emitted = async {
        let iface = interface_ref(connection).await?;
        let service = iface.get().await;
        service.visible_until_changed(iface.signal_emitter()).await
    };
    if let Err(e) = emitted.await {
        tracing::warn!("failed to emit VisibleUntil: {e}");
    }
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
        channel::TransferAction,
        hdl::info::{TransferMetadata, TransferPayloadKind},
    };

    use super::*;
    use rqs_lib::safety::Refusal;

    use crate::{allowed::AllowedDevice, app::LibActions, history::History, settings::Settings};

    /// A service on a core without an engine, and what it sends to the library.
    fn test_service() -> (Service, broadcast::Receiver<ChannelMessage>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let trust = Trust::load(None, "test".into(), tx).unwrap();
        let core = Core::new(Settings::default(), None, History::load(None), trust, rx);
        let (lib_tx, lib_rx) = broadcast::channel(16);
        core.lib_actions().set(Some(lib_tx));
        let consents = core.consents();
        let lib_actions = core.lib_actions();
        let core = Arc::new(Mutex::new(core));
        let requirements = crate::requirements::RequirementsEngine::new(
            crate::requirements::SystemRequirementsProbe::new(),
            crate::requirements::EngineConfig::default(),
        );
        (
            Service {
                core,
                consents,
                lib_actions,
                requirements,
            },
            lib_rx,
        )
    }

    fn accepted(message: &ChannelMessage) -> Option<bool> {
        match &message.msg {
            Message::Lib {
                action: TransferAction::ConsentAccept,
            } => Some(true),
            Message::Lib {
                action: TransferAction::ConsentDecline,
            } => Some(false),
            _ => None,
        }
    }

    const WAITING: &str = "10.0.0.5:4000";
    const MAC: &str = "aa:bb:cc:dd:ee:01";

    /// What `consent_step` works with, around a library channel the test can read.
    struct Harness {
        consents: Consents,
        trust: Arc<Trust>,
        allowed: Allowed,
        sent: broadcast::Receiver<ChannelMessage>,
        _trust_events: mpsc::UnboundedReceiver<TrustEvent>,
        trusted_enabled: bool,
    }

    impl Harness {
        fn new() -> Self {
            let actions = LibActions::default();
            let (tx, sent) = broadcast::channel(64);
            actions.set(Some(tx));
            let (trust_tx, _trust_events) = mpsc::unbounded_channel();
            Self {
                consents: Consents::with_mac_reader(actions, |_| Some(MAC.to_owned())),
                trust: Trust::load(None, "me".into(), trust_tx).unwrap(),
                allowed: Allowed::default(),
                sent,
                _trust_events,
                trusted_enabled: true,
            }
        }

        fn step(&self, id: &str, client: &MessageClient, event: &mut TransferEvent) -> bool {
            self.step_in(self.consents.generation(), id, client, event)
        }

        fn step_in(
            &self,
            generation: u64,
            id: &str,
            client: &MessageClient,
            event: &mut TransferEvent,
        ) -> bool {
            let ctx = ConsentCtx {
                consents: &self.consents,
                generation,
                trust: &self.trust,
                allowed: &self.allowed,
                trusted_enabled: self.trusted_enabled,
            };
            consent_step(&ctx, id, client, event, Instant::now())
        }

        /// The actions that reached the library: (id, accepted).
        fn sent(&mut self) -> Vec<(String, bool)> {
            let mut out = Vec::new();
            while let Ok(message) = self.sent.try_recv() {
                if let Some(accepted) = accepted(&message) {
                    out.push((message.id, accepted));
                }
            }
            out
        }
    }

    fn client(kind: TransferKind, state: TransferState, files: &[&str]) -> MessageClient {
        MessageClient {
            kind,
            state: Some(state),
            metadata: Some(TransferMetadata {
                id: "t".into(),
                source: None,
                pin_code: None,
                payload_kind: TransferPayloadKind::Files,
                payload_preview: None,
                payload: Some(TransferPayload::Files(
                    files.iter().map(|f| (*f).to_owned()).collect(),
                )),
                total_bytes: 0,
                ack_bytes: 0,
                saved_paths: Vec::new(),
                refusal: None,
            }),
        }
    }

    fn waiting() -> MessageClient {
        client(
            TransferKind::Inbound,
            TransferState::WaitingForUserConsent,
            &["a.txt"],
        )
    }

    fn event(id: &str, state: &str, pin: &str) -> TransferEvent {
        let mut event = TransferEvent::for_test(id, "inbound", state, "Pixel");
        event.pin = pin.to_owned();
        event
    }

    #[test]
    fn a_waiting_transfer_gets_a_token_on_its_event() {
        let h = Harness::new();
        let mut e = event(WAITING, "WaitingForUserConsent", "1111");
        assert!(h.step(WAITING, &waiting(), &mut e));
        assert_ne!(e.consent, 0);
        assert_eq!(h.consents.token(WAITING), Some(e.consent));
        assert_eq!(e.info()["consent"], Variant::from(e.consent));
        // The same event again: the same consent.
        let mut again = event(WAITING, "WaitingForUserConsent", "1111");
        assert!(h.step(WAITING, &waiting(), &mut again));
        assert_eq!(again.consent, e.consent);
    }

    #[test]
    fn a_paired_computers_intent_accepts_once_and_gives_no_token() {
        let mut h = Harness::new();
        h.trust
            .plant_intent_for_test("10.0.0.5".parse().unwrap(), "alice");
        let mut e = event(WAITING, "WaitingForUserConsent", "1111");
        assert!(h.step(WAITING, &waiting(), &mut e));
        assert_eq!((e.trusted.as_str(), e.consent), ("alice", 0));
        assert_eq!(h.sent(), [(WAITING.to_owned(), true)]);
        // The intent was used up: the next transfer of that address asks.
        let mut next = event("10.0.0.5:4001", "WaitingForUserConsent", "2222");
        assert!(h.step("10.0.0.5:4001", &waiting(), &mut next));
        assert!(next.trusted.is_empty() && next.consent != 0);
        assert!(h.sent().is_empty());
    }

    #[test]
    fn the_allowed_list_accepts_only_the_same_name_and_address_while_on() {
        let mut h = Harness::new();
        let device = |name: &str, mac: &str| AllowedDevice {
            name: name.into(),
            mac: mac.into(),
        };
        *h.allowed.lock().unwrap() =
            vec![device("Other", MAC), device("Pixel", "aa:bb:cc:dd:ee:02")];
        let mut e = event(WAITING, "WaitingForUserConsent", "1111");
        assert!(h.step(WAITING, &waiting(), &mut e));
        assert!(e.trusted.is_empty(), "name and address must both match");
        assert!(h.sent().is_empty());

        h.allowed.lock().unwrap().push(device("Pixel", MAC));
        let mut e = event("10.0.0.5:4001", "WaitingForUserConsent", "2222");
        assert!(h.step("10.0.0.5:4001", &waiting(), &mut e));
        assert_eq!(e.always_accepted, "Pixel");
        assert_eq!(e.consent, 0);
        assert_eq!(h.sent(), [("10.0.0.5:4001".to_owned(), true)]);

        h.trusted_enabled = false;
        let mut e = event("10.0.0.5:4002", "WaitingForUserConsent", "3333");
        assert!(h.step("10.0.0.5:4002", &waiting(), &mut e));
        assert!(e.trusted.is_empty() && e.consent != 0);
    }

    #[test]
    fn later_events_close_the_consent_and_a_final_one_forgets_the_transfer() {
        let h = Harness::new();
        let mut e = event(WAITING, "WaitingForUserConsent", "1111");
        h.step(WAITING, &waiting(), &mut e);
        // An outgoing event under the same id is not this transfer.
        let outgoing = client(TransferKind::Outbound, TransferState::SendingFiles, &[]);
        let mut o = TransferEvent::for_test(WAITING, "outbound", "SendingFiles", "x");
        assert!(h.step(WAITING, &outgoing, &mut o));
        assert!(h.consents.token(WAITING).is_some());

        let receiving = client(TransferKind::Inbound, TransferState::ReceivingFiles, &["a"]);
        let mut e = event(WAITING, "ReceivingFiles", "1111");
        assert!(h.step(WAITING, &receiving, &mut e));
        assert_eq!((e.consent, h.consents.token(WAITING)), (0, None));

        // Accepted by the user, it can still be trusted until it ends.
        let mut e = event("10.0.0.5:4001", "WaitingForUserConsent", "2222");
        h.step("10.0.0.5:4001", &waiting(), &mut e);
        h.consents
            .resolve(
                "10.0.0.5:4001",
                Some(e.consent),
                Answer::Accept,
                By::User,
                Instant::now(),
            )
            .unwrap();
        let finished = client(TransferKind::Inbound, TransferState::Finished, &["a"]);
        let mut e = event("10.0.0.5:4001", "Finished", "2222");
        assert!(h.step("10.0.0.5:4001", &finished, &mut e));
        assert!(
            h.consents
                .allow_trust("10.0.0.5:4001", Instant::now())
                .is_err()
        );
    }

    #[test]
    fn a_name_that_escapes_the_download_folder_is_declined_and_not_reported() {
        let mut h = Harness::new();
        let evil = client(
            TransferKind::Inbound,
            TransferState::WaitingForUserConsent,
            &["../evil"],
        );
        let mut e = event(WAITING, "WaitingForUserConsent", "1111");
        assert!(!h.step(WAITING, &evil, &mut e));
        assert_eq!(h.sent(), [(WAITING.to_owned(), false)]);
        assert_eq!(h.consents.token(WAITING), None);
    }

    #[test]
    fn one_address_and_the_network_in_all_are_capped_and_bluetooth_keeps_room() {
        let mut h = Harness::new();
        for n in 0..4 {
            let id = format!("10.0.0.5:{n}");
            let mut e = event(&id, "WaitingForUserConsent", "1111");
            assert!(h.step(&id, &waiting(), &mut e), "{id}");
        }
        let mut e = event("10.0.0.5:9", "WaitingForUserConsent", "1111");
        assert!(!h.step("10.0.0.5:9", &waiting(), &mut e));
        assert_eq!(h.sent(), [("10.0.0.5:9".to_owned(), false)]);
        // Many other addresses fill the network share; Bluetooth is still taken.
        for n in 0..40 {
            let id = format!("10.1.{n}.1:1");
            let mut e = event(&id, "WaitingForUserConsent", "1111");
            h.step(&id, &waiting(), &mut e);
        }
        let mut e = event("ble-1", "WaitingForUserConsent", "1111");
        assert!(h.step("ble-1", &waiting(), &mut e));
        assert_ne!(e.consent, 0);
    }

    #[test]
    fn a_new_session_under_a_reused_id_replaces_the_old_consent() {
        let h = Harness::new();
        let mut old = event("ble-1", "WaitingForUserConsent", "1111");
        h.step("ble-1", &waiting(), &mut old);
        let mut new = event("ble-1", "WaitingForUserConsent", "2222");
        h.step("ble-1", &waiting(), &mut new);
        assert_ne!(old.consent, new.consent);
        assert_eq!(h.consents.token("ble-1"), Some(new.consent));
    }

    #[test]
    fn events_of_a_stopped_engine_change_nothing() {
        let mut h = Harness::new();
        let old = h.consents.generation();
        h.consents.clear();
        let mut e = event(WAITING, "WaitingForUserConsent", "1111");
        assert!(!h.step_in(old, WAITING, &waiting(), &mut e));
        assert_eq!(h.consents.token(WAITING), None);
        assert!(h.sent().is_empty());
    }

    #[test]
    fn a_refusal_of_the_library_is_reported_with_its_code() {
        let mut c = client(TransferKind::Inbound, TransferState::Rejected, &[]);
        c.metadata.as_mut().unwrap().refusal = Some(Refusal::TooManyFiles);
        let e = TransferEvent::from_client("x", &c);
        assert_eq!(e.reason, "too_many_files");
        assert_eq!(e.info()["reason"], Variant::from("too_many_files"));
        // Later events carry no refusal: the code stays.
        let mut merged = e.clone();
        merged.merge(&TransferEvent::default());
        assert_eq!(merged.reason, "too_many_files");
        assert_eq!(TransferEvent::default().info()["reason"], Variant::from(""));
    }

    #[tokio::test]
    async fn answers_are_refused_with_distinct_errors() {
        let (service, _sent) = test_service();
        let token = service
            .consents
            .open_for_test(WAITING, "Pixel", None, Instant::now())
            .unwrap();
        assert!(matches!(
            service.answer_transfer("10.9.9.9:1", 1, "accept").await,
            Err(AnswerError::NotWaiting(_))
        ));
        assert!(matches!(
            service
                .answer_transfer(WAITING, token.wrapping_add(1), "accept")
                .await,
            Err(AnswerError::StaleToken(_))
        ));
        // No address: the sender cannot be trusted, and the transfer still waits.
        assert!(matches!(
            service
                .answer_transfer(WAITING, token, "accept_and_trust")
                .await,
            Err(AnswerError::NoMac(_))
        ));
        assert_eq!(service.consents.token(WAITING), Some(token));
        assert!(matches!(
            service.answer_transfer(WAITING, token, "maybe").await,
            Err(AnswerError::InvalidArgs(_))
        ));
    }

    #[tokio::test]
    async fn answer_transfer_accepts_once() {
        let (service, mut sent) = test_service();
        let token = service
            .consents
            .open_for_test(WAITING, "Pixel", None, Instant::now())
            .unwrap();
        service
            .answer_transfer(WAITING, token, "accept")
            .await
            .unwrap();
        assert!(
            service
                .answer_transfer(WAITING, token, "accept")
                .await
                .is_err()
        );
        assert!(service.accept(WAITING).await.is_err());
        assert!(service.decline(WAITING).await.is_err());
        assert_eq!(accepted(&sent.try_recv().unwrap()), Some(true));
        assert!(sent.try_recv().is_err(), "one answer, one action");
    }

    #[tokio::test]
    async fn wrong_token_refused() {
        let (service, mut sent) = test_service();
        let token = service
            .consents
            .open_for_test(WAITING, "Pixel", None, Instant::now())
            .unwrap();
        for answer in ["accept", "accept_and_trust", "decline"] {
            let wrong = token.wrapping_add(1);
            assert!(
                service
                    .answer_transfer(WAITING, wrong, answer)
                    .await
                    .is_err()
            );
            assert!(service.answer_transfer(WAITING, 0, answer).await.is_err());
        }
        assert!(
            service
                .answer_transfer(WAITING, token, "maybe")
                .await
                .is_err()
        );
        assert!(sent.try_recv().is_err(), "nothing may reach the library");
        // The transfer still waits for its own prompt.
        service
            .answer_transfer(WAITING, token, "decline")
            .await
            .unwrap();
        assert_eq!(accepted(&sent.try_recv().unwrap()), Some(false));
    }

    #[tokio::test]
    async fn legacy_answers_work_only_while_a_transfer_waits() {
        let (service, mut sent) = test_service();
        assert!(service.accept("10.0.0.9:1").await.is_err());
        assert!(service.decline("10.0.0.9:1").await.is_err());
        service
            .consents
            .open_for_test(WAITING, "Pixel", None, Instant::now());
        service.decline(WAITING).await.unwrap();
        assert_eq!(accepted(&sent.try_recv().unwrap()), Some(false));
        assert!(sent.try_recv().is_err());
    }

    #[tokio::test]
    async fn trusting_needs_a_verified_address_and_leaves_the_transfer_waiting() {
        let (service, mut sent) = test_service();
        // No address, so no MAC to bind the device to.
        let token = service
            .consents
            .open_for_test(WAITING, "Pixel", None, Instant::now())
            .unwrap();
        assert!(
            service
                .answer_transfer(WAITING, token, "accept_and_trust")
                .await
                .is_err()
        );
        assert!(sent.try_recv().is_err());
        assert_eq!(service.core.lock().await.allowed_json(), "[]");
        service
            .answer_transfer(WAITING, token, "accept")
            .await
            .unwrap();
        assert_eq!(service.core.lock().await.allowed_json(), "[]");
    }

    #[test]
    fn a_consent_token_travels_with_the_event() {
        let mut event = TransferEvent::for_test(WAITING, "inbound", "WaitingForUserConsent", "x");
        event.consent = 42;
        assert_eq!(event.info()["consent"], Variant::from(42u64));
        assert_eq!(
            TransferEvent::default().info()["consent"],
            Variant::from(0u64)
        );
    }

    #[test]
    fn the_address_comes_from_the_transfer_id() {
        assert_eq!(ip_of_id("10.0.0.5:4000"), "10.0.0.5".parse().ok());
        assert_eq!(ip_of_id("[fe80::1]:4000"), "fe80::1".parse().ok());
        assert_eq!(ip_of_id("ble:G483"), None);
        assert_eq!(ip_of_id("nonsense"), None);
    }

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
                refusal: None,
            }),
        };
        let event = TransferEvent::from_client("t2", &client);
        assert_eq!(event.kind, "outbound");
        assert_eq!(event.pin, "1234");
        assert_eq!((event.total_bytes, event.ack_bytes), (100, 40));
        let info = event.info();
        assert_eq!(info.len(), 16);
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

    #[test]
    fn the_probe_log_is_capped_and_forgets_the_oldest_first() {
        let t0 = Instant::now();
        let mut log = ProbeLog::default();
        for i in 0..MAX_PROBED + 44 {
            let at = t0 + Duration::from_millis(i as u64);
            log.record(format!("10.0.{}.{}", i / 250, i % 250), at);
        }
        assert_eq!(log.0.len(), MAX_PROBED);
        let last = format!(
            "10.0.{}.{}",
            (MAX_PROBED + 43) / 250,
            (MAX_PROBED + 43) % 250
        );
        assert!(log.0.contains_key(&last), "the newest stays");
        assert!(!log.0.contains_key("10.0.0.0"), "the oldest went");
        // An address already in the list never evicts another one.
        let before: Vec<_> = log.0.keys().cloned().collect();
        log.record(last.clone(), t0 + Duration::from_secs(1));
        assert_eq!(log.0.len(), before.len());
    }

    #[test]
    fn the_probe_log_forgets_old_entries_before_evicting_young_ones() {
        let t0 = Instant::now();
        let mut log = ProbeLog::default();
        for i in 0..MAX_PROBED {
            log.record(format!("a{i}"), t0);
        }
        let later = t0 + PROBE_FORGET_AFTER + Duration::from_secs(1);
        log.record("new".into(), later);
        assert_eq!(log.0.len(), 1);
        let again = later + PROBE_EVERY + Duration::from_secs(1);
        assert!(!log.due("new", later) && log.due("new", again));
        assert!(log.due("unknown", later));
        log.forget("new");
        assert!(log.due("new", later));
    }

    #[test]
    fn only_a_few_probes_run_at_once() {
        let limit = ProbeLimit::new(MAX_PROBES_AT_ONCE);
        let held: Vec<_> = (0..MAX_PROBES_AT_ONCE)
            .map(|_| limit.try_start().expect("a slot is free"))
            .collect();
        assert!(limit.try_start().is_none());
        drop(held);
        assert!(limit.try_start().is_some());
    }

    #[tokio::test]
    async fn a_stopping_forwarder_reports_what_is_still_queued_and_then_ends() {
        let (tx, mut events) = broadcast::channel(8);
        let (stop, mut stopped) = watch::channel(false);
        let mut draining = false;
        for id in ["a", "b"] {
            tx.send(ChannelMessage {
                id: id.into(),
                msg: Message::Lib {
                    action: TransferAction::TransferCancel,
                },
            })
            .unwrap();
        }
        // Stopped with two events waiting: both are still handed over, in order.
        stop.send(true).unwrap();
        let mut seen = Vec::new();
        while let Some(message) = next_transfer(&mut events, &mut stopped, &mut draining).await {
            seen.push(message.id);
        }
        assert_eq!(seen, ["a", "b"]);
        assert!(draining);
        // Nothing queued, not stopped: it waits (here: until the sender goes away).
        let (stop, mut stopped) = watch::channel(false);
        let (tx, mut events) = broadcast::channel::<ChannelMessage>(2);
        let mut draining = false;
        drop(stop);
        let ended = next_transfer(&mut events, &mut stopped, &mut draining).await;
        assert!(ended.is_none());
        drop(tx);
    }

    #[tokio::test]
    async fn a_delayed_task_of_an_old_engine_leaves_the_list_alone() {
        let (service, _) = test_service();
        let old = service.consents.generation();
        assert!(engine_is_current(&service.consents, old));
        service.consents.clear(); // the service restarted
        assert!(!engine_is_current(&service.consents, old));
    }

    #[test]
    fn an_unfinished_transfer_becomes_a_final_disconnection_without_a_token() {
        let mut waiting = TransferEvent::for_test(
            "10.0.0.6:4000",
            "inbound",
            "WaitingForUserConsent",
            "tablet",
        );
        waiting.consent = 77;
        let ended = waiting.disconnected(123);
        let state = (ended.state.as_str(), ended.consent, ended.last_event);
        assert_eq!(state, ("Disconnected", 0, 123));
        assert_eq!((ended.kind, ended.device.as_str()), ("inbound", "tablet"));
    }
}
