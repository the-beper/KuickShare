//! Starts the daemon on a private D-Bus and drives its API, the way the shell does.
//!
//! Needs `dbus-daemon` (it is in the build container). The bus has no service activation, so it
//! never touches the desktop's own D-Bus session.

use std::{
    fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};

use zbus::{Connection, OwnedVariant, Variant, connection::Builder};

mod fake_services;
use fake_services::{CAP_AP, Config, FakeSystem, Service};

const NAME: &str = "io.github.the_beper.KuickShare";
const PATH: &str = "/io/github/the_beper/KuickShare";
const IFACE: &str = "io.github.the_beper.KuickShare1";
const PROPS: &str = "org.freedesktop.DBus.Properties";

struct Daemon {
    dir: PathBuf,
    bus: Child,
    core: Child,
    conn: Connection,
}

impl Daemon {
    /// `history` is written to the history file before the daemon starts.
    async fn start(name: &str, history: Option<&str>) -> Self {
        Self::start_with(name, history, None).await
    }

    /// `settings` is written to the settings file before the daemon starts.
    async fn start_with(name: &str, history: Option<&str>, settings: Option<&str>) -> Self {
        Self::start_inner(name, history, settings, None).await
    }

    /// Starts the daemon with `system` (a fake system bus) as its system bus. Declare `system`
    /// before the daemon so that the daemon is dropped first.
    async fn start_system(name: &str, system: &FakeSystem) -> Self {
        Self::start_inner(name, None, None, Some(&system.address)).await
    }

    async fn start_inner(
        name: &str,
        history: Option<&str>,
        settings: Option<&str>,
        system: Option<&str>,
    ) -> Self {
        let dir = std::env::temp_dir().join(format!("kuickshare-it-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("config")).unwrap();
        fs::create_dir_all(dir.join("data/kuickshare")).unwrap();
        if let Some(settings) = settings {
            fs::create_dir_all(dir.join("config/kuickshare")).unwrap();
            fs::write(dir.join("config/kuickshare/settings.json"), settings).unwrap();
        }
        if let Some(history) = history {
            fs::write(dir.join("data/kuickshare/history.json"), history).unwrap();
        }

        let address = format!("unix:abstract=kuickshare-it-{}-{name}", std::process::id());
        let conf = dir.join("bus.conf");
        fs::write(
            &conf,
            format!(
                "<busconfig><type>session</type><listen>{address}</listen><auth>EXTERNAL</auth>\
                 <policy context=\"default\"><allow send_destination=\"*\" eavesdrop=\"true\"/>\
                 <allow eavesdrop=\"true\"/><allow own=\"*\"/></policy></busconfig>"
            ),
        )
        .unwrap();
        let bus = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", conf.display()))
            .arg("--nofork")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("dbus-daemon is needed to run these tests");

        let conn = connect(&address).await;
        let core = Command::new(env!("CARGO_BIN_EXE_kuickshared"))
            .env("DBUS_SESSION_BUS_ADDRESS", &address)
            // The system bus of the machine running the tests must not leak in (firewalld).
            .env(
                "DBUS_SYSTEM_BUS_ADDRESS",
                system.unwrap_or("unix:path=/nonexistent/kuickshare-it"),
            )
            .env("HOME", &dir)
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("XDG_DATA_HOME", dir.join("data"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("cannot start kuickshared");

        let daemon = Self {
            dir,
            bus,
            core,
            conn,
        };
        daemon.wait_for_name().await;
        daemon
    }

    async fn wait_for_name(&self) {
        for _ in 0..100 {
            let reply = self
                .conn
                .call_method(
                    Some("org.freedesktop.DBus"),
                    "/org/freedesktop/DBus",
                    Some("org.freedesktop.DBus"),
                    "NameHasOwner",
                    &(NAME,),
                )
                .await
                .unwrap();
            if reply.body().deserialize::<bool>().unwrap() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("kuickshared did not take {NAME}");
    }

    async fn get<T: TryFrom<OwnedVariant>>(&self, property: &str) -> T
    where
        T::Error: std::fmt::Debug,
    {
        let reply = self
            .conn
            .call_method(Some(NAME), PATH, Some(PROPS), "Get", &(IFACE, property))
            .await
            .unwrap();
        let value: OwnedVariant = reply.body().deserialize().unwrap();
        T::try_from(value).unwrap()
    }

    async fn set(&self, property: &str, value: Variant<'_>) {
        self.conn
            .call_method(
                Some(NAME),
                PATH,
                Some(PROPS),
                "Set",
                &(IFACE, property, value),
            )
            .await
            .unwrap();
    }

    /// The JSON text returned by a method that returns a string (`History`, `AllowedDevices`).
    async fn json(&self, method: &str) -> serde_json::Value {
        let reply = self
            .conn
            .call_method(Some(NAME), PATH, Some(IFACE), method, &())
            .await
            .unwrap();
        serde_json::from_str(&reply.body().deserialize::<String>().unwrap()).unwrap()
    }

    async fn call<B: serde::Serialize + zbus::DynamicType>(
        &self,
        method: &str,
        body: &B,
    ) -> zbus::Result<()> {
        self.conn
            .call_method(Some(NAME), PATH, Some(IFACE), method, body)
            .await
            .map(|_| ())
    }

    fn file(&self, relative: &str) -> String {
        fs::read_to_string(self.dir.join(relative)).unwrap_or_default()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.core.kill();
        let _ = self.core.wait();
        let _ = self.bus.kill();
        let _ = self.bus.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

async fn connect(address: &str) -> Connection {
    for _ in 0..100 {
        if let Ok(conn) = Builder::address(address).build().await {
            return conn;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("cannot connect to the private bus at {address}");
}

fn entry(uid: u64, time: i64, device: &str) -> String {
    format!(
        r#"{{"uid":{uid},"time":{time},"kind":"inbound","state":"Finished","device":"{device}","payload_kind":"Files","files":["a.txt"],"text":"","total_bytes":10,"trusted":false}}"#
    )
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[tokio::test]
async fn starts_with_the_default_settings() {
    let daemon = Daemon::start("defaults", None).await;

    assert!(daemon.get::<bool>("Visible").await);
    assert!(daemon.get::<bool>("HistoryEnabled").await);
    assert!(daemon.get::<bool>("TrayIcon").await);
    assert!(daemon.get::<bool>("DolphinIntegration").await);
    assert!(daemon.get::<bool>("TrustedDevicesEnabled").await);
    assert!(!daemon.get::<bool>("WifiUpgrade").await);
    assert_eq!(daemon.get::<u32>("HistoryKeepDays").await, 0);
    assert_eq!(daemon.json("History").await, serde_json::json!([]));
    assert_eq!(daemon.json("AllowedDevices").await, serde_json::json!([]));
}

#[tokio::test]
async fn history_settings_are_applied_and_saved() {
    let daemon = Daemon::start("settings", None).await;

    daemon.set("HistoryKeepDays", Variant::from(30u32)).await;
    daemon.set("HistoryEnabled", Variant::from(false)).await;

    assert_eq!(daemon.get::<u32>("HistoryKeepDays").await, 30);
    assert!(!daemon.get::<bool>("HistoryEnabled").await);
    let saved: serde_json::Value =
        serde_json::from_str(&daemon.file("config/kuickshare/settings.json")).unwrap();
    assert_eq!(saved["history_keep_days"], 30);
    assert_eq!(saved["history_enabled"], false);
}

#[tokio::test]
async fn integration_settings_are_applied_and_saved() {
    let daemon = Daemon::start("integration", None).await;

    daemon.set("TrayIcon", Variant::from(false)).await;
    daemon.set("DolphinIntegration", Variant::from(false)).await;
    daemon
        .set("TrustedDevicesEnabled", Variant::from(false))
        .await;

    assert!(!daemon.get::<bool>("TrayIcon").await);
    assert!(!daemon.get::<bool>("DolphinIntegration").await);
    assert!(!daemon.get::<bool>("TrustedDevicesEnabled").await);
    daemon.set("WifiUpgrade", Variant::from(true)).await;
    assert!(daemon.get::<bool>("WifiUpgrade").await);
    let saved: serde_json::Value =
        serde_json::from_str(&daemon.file("config/kuickshare/settings.json")).unwrap();
    assert_eq!(saved["tray_icon"], false);
    assert_eq!(saved["dolphin_integration"], false);
    assert_eq!(saved["trusted_devices"], false);
    assert_eq!(saved["wifi_upgrade"], true);
}

#[tokio::test]
async fn resetting_the_settings_restores_the_defaults() {
    let daemon = Daemon::start("reset", None).await;
    daemon.set("HistoryEnabled", Variant::from(false)).await;
    daemon.set("HistoryKeepDays", Variant::from(90u32)).await;
    daemon.set("TrayIcon", Variant::from(false)).await;
    daemon.set("Visible", Variant::from(false)).await;
    daemon.set("DeviceName", Variant::from("Renamed")).await;

    daemon.call("ResetSettings", &()).await.unwrap();

    assert!(daemon.get::<bool>("HistoryEnabled").await);
    assert_eq!(daemon.get::<u32>("HistoryKeepDays").await, 0);
    assert!(daemon.get::<bool>("TrayIcon").await);
    assert!(daemon.get::<bool>("Visible").await);
    assert_ne!(daemon.get::<String>("DeviceName").await, "Renamed");
    let saved: serde_json::Value =
        serde_json::from_str(&daemon.file("config/kuickshare/settings.json")).unwrap();
    assert_eq!(saved["history_enabled"], true);
    assert!(saved["device_name"].is_null());
}

#[tokio::test]
async fn history_entries_can_be_removed_cleared_and_expire() {
    let recent = now() - 60;
    let saved = format!(
        "[{},{},{}]",
        entry(10, recent, "new"),
        entry(11, recent - 100, "newer than a week"),
        entry(12, now() - 40 * 86_400, "old")
    );
    let daemon = Daemon::start("history", Some(&saved)).await;

    assert_eq!(daemon.json("History").await.as_array().unwrap().len(), 3);

    // Entries older than the retention period go away, also from the file.
    daemon.set("HistoryKeepDays", Variant::from(7u32)).await;
    let left = daemon.json("History").await;
    let devices: Vec<_> = left
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["device"].as_str().unwrap())
        .collect();
    assert_eq!(devices, ["new", "newer than a week"]);
    assert!(!daemon.file("data/kuickshare/history.json").contains("old"));

    let size: u64 = daemon
        .conn
        .call_method(Some(NAME), PATH, Some(IFACE), "HistorySize", &())
        .await
        .unwrap()
        .body()
        .deserialize()
        .unwrap();
    assert!(size > 0);

    daemon.call("RemoveHistory", &(10u64,)).await.unwrap();
    let left = daemon.json("History").await;
    assert_eq!(left.as_array().unwrap().len(), 1);
    assert_eq!(left[0]["uid"], 11);

    daemon.call("ClearHistory", &()).await.unwrap();
    assert_eq!(daemon.json("History").await, serde_json::json!([]));
}

#[tokio::test]
async fn entries_saved_without_an_id_get_one() {
    // The format before `uid` existed.
    let saved = r#"[{"time":1000,"kind":"inbound","state":"Finished","device":"old","payload_kind":"Text","files":[],"text":"hi","total_bytes":0}]"#;
    let daemon = Daemon::start("legacy", Some(saved)).await;

    let history = daemon.json("History").await;
    let uid = history[0]["uid"].as_u64().unwrap();
    assert!(uid > 0);
    daemon.call("RemoveHistory", &(uid,)).await.unwrap();
    assert_eq!(daemon.json("History").await, serde_json::json!([]));
}

#[tokio::test]
async fn trusting_needs_a_known_transfer() {
    let daemon = Daemon::start("allow", None).await;

    // No such transfer: nothing to learn the device name from.
    assert!(
        daemon
            .call("AllowDevice", &("10.0.0.9:4000",))
            .await
            .is_err()
    );
    daemon
        .call("ForgetAllowed", &("nobody", "aa:bb:cc:dd:ee:ff"))
        .await
        .unwrap();
    assert_eq!(daemon.json("AllowedDevices").await, serde_json::json!([]));
}

/// REPRO (defect 7, decision D3): answering a transfer nobody is waiting on is an
/// error, not a silent success.
#[tokio::test]
async fn accept_unknown_id_is_error() {
    let daemon = Daemon::start("accept-unknown", None).await;
    assert!(
        daemon.call("Accept", &("10.0.0.9:4000",)).await.is_err(),
        "Accept on an id with no pending consent must fail"
    );
}

#[tokio::test]
async fn decline_unknown_id_is_error() {
    let daemon = Daemon::start("decline-unknown", None).await;
    assert!(
        daemon.call("Decline", &("10.0.0.9:4000",)).await.is_err(),
        "Decline on an id with no pending consent must fail"
    );
}

/// A token alone does not make an answer valid: no transfer waits, whatever the token and the
/// answer. (The same with a transfer that waits is `wrong_token_refused` and
/// `answer_transfer_accepts_once` in `src/core/dbus.rs`: no phone can be made to send one here.)
#[tokio::test]
async fn wrong_token_refused() {
    let daemon = Daemon::start("wrong-token", None).await;
    for answer in ["accept", "accept_and_trust", "decline"] {
        for token in [0u64, 1, u64::MAX] {
            assert!(
                daemon
                    .call("AnswerTransfer", &("10.0.0.9:4000", token, answer))
                    .await
                    .is_err(),
                "{answer} with token {token} must fail when nothing waits"
            );
        }
    }
    // An answer the API does not know is refused as such.
    let e = daemon
        .call("AnswerTransfer", &("10.0.0.9:4000", 1u64, "maybe"))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("InvalidArgs"), "{e}");
}

#[tokio::test]
async fn answer_transfer_without_a_waiting_transfer_is_refused_every_time() {
    let daemon = Daemon::start("answer-none", None).await;
    // Nothing waits, so nothing is accepted, the first time or the second, and the error says why.
    for _ in 0..2 {
        let e = daemon
            .call("AnswerTransfer", &("10.0.0.9:4000", 7u64, "accept"))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("Error.NotWaiting"), "{e}");
    }
}

#[tokio::test]
async fn the_trusted_devices_notice_is_pending_only_after_an_upgrade() {
    let daemon = Daemon::start("reset-notice-none", None).await;
    assert!(!daemon.get::<bool>("TrustedDevicesReset").await);
    daemon
        .call("AcknowledgeTrustedDevicesReset", &())
        .await
        .unwrap();
}

#[tokio::test]
async fn old_trusted_devices_are_removed_once_and_the_notice_survives_a_restart() {
    let old = r#"{"allowed_devices":[{"name":"old phone","mac":"aa:bb:cc:dd:ee:01"}]}"#;
    let daemon = Daemon::start_with("reset-notice", None, Some(old)).await;
    assert_eq!(daemon.json("AllowedDevices").await, serde_json::json!([]));
    assert!(daemon.get::<bool>("TrustedDevicesReset").await);
    // The file has the notice too: a restart before the user saw it keeps it.
    let saved = daemon.file("config/kuickshare/settings.json");
    assert!(
        saved.contains("\"trusted_devices_reset_notice\": true"),
        "{saved}"
    );
    daemon
        .call("AcknowledgeTrustedDevicesReset", &())
        .await
        .unwrap();
    assert!(!daemon.get::<bool>("TrustedDevicesReset").await);
    let saved = daemon.file("config/kuickshare/settings.json");
    assert!(
        saved.contains("\"trusted_devices_reset_notice\": false"),
        "{saved}"
    );
    assert!(saved.contains("\"allowed_reset_v1\": true"), "{saved}");
}

#[tokio::test]
async fn joining_a_network_needs_credentials_from_a_finished_transfer() {
    let daemon = Daemon::start("join", None).await;
    let e = daemon
        .call("JoinNetwork", &("10.0.0.9:4000",))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("no longer available"), "{e}");
}

#[tokio::test]
async fn bluetooth_ids_are_refused_where_they_make_no_sense() {
    let daemon = Daemon::start("ble", None).await;

    let err = daemon.call("PairWith", &("ble:G483",)).await.unwrap_err();
    assert!(err.to_string().contains("Bluetooth"), "{err}");
    // Never seen by a scan: unknown, like any other id.
    let err = daemon
        .call("SendText", &("ble:G483", "hi", "text"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unknown device"), "{err}");
    // A scan window needs discovery to be on.
    let err = daemon.call("ScanBluetooth", &()).await.unwrap_err();
    assert!(
        err.to_string().contains("discovery is not running"),
        "{err}"
    );
}

#[tokio::test]
async fn a_bluetooth_scan_without_bluez_is_harmless() {
    let daemon = Daemon::start("scan", None).await;

    daemon.call("StartDiscovery", &()).await.unwrap();
    daemon.call("ScanBluetooth", &()).await.unwrap();
    daemon.call("StopDiscovery", &()).await.unwrap();
}

#[tokio::test]
async fn the_firewall_check_answers_even_without_a_system_bus() {
    let daemon = Daemon::start("firewall", None).await;

    // No system bus is reachable: "not managed" is the answer, not an error.
    let reply = daemon
        .conn
        .call_method(Some(NAME), PATH, Some(IFACE), "FirewallManaged", &())
        .await
        .unwrap();
    assert!(!reply.body().deserialize::<bool>().unwrap());
}

#[tokio::test]
async fn legacy_wifi_upgrade_write_never_grants_consent() {
    let daemon = Daemon::start("legacy-wifi", None).await;
    assert!(!daemon.get::<bool>("InsecureHotspotAllowed").await);

    // The legacy property cannot carry the consent: it never turns it on...
    daemon.set("WifiUpgrade", Variant::from(true)).await;
    assert!(daemon.get::<bool>("WifiUpgrade").await);
    assert!(!daemon.get::<bool>("InsecureHotspotAllowed").await);

    // ...and any write through it takes a given consent back.
    daemon
        .call("RequestWifiUpgrade", &(true, true))
        .await
        .unwrap();
    assert!(daemon.get::<bool>("InsecureHotspotAllowed").await);
    daemon.set("WifiUpgrade", Variant::from(true)).await;
    assert!(daemon.get::<bool>("WifiUpgrade").await);
    assert!(!daemon.get::<bool>("InsecureHotspotAllowed").await);
}

#[tokio::test]
async fn consent_requires_enable() {
    let daemon = Daemon::start("consent-needs-enable", None).await;

    let err = daemon
        .call("RequestWifiUpgrade", &(false, true))
        .await
        .expect_err("consent without the upgrade must be refused");
    assert!(err.to_string().contains("InvalidArgs"), "{err}");
    assert!(!daemon.get::<bool>("WifiUpgrade").await);
    assert!(!daemon.get::<bool>("InsecureHotspotAllowed").await);

    daemon
        .call("RequestWifiUpgrade", &(true, true))
        .await
        .unwrap();
    assert!(daemon.get::<bool>("WifiUpgrade").await);
    assert!(daemon.get::<bool>("InsecureHotspotAllowed").await);

    // Only the upgrade is saved: the consent never reaches a file.
    let saved: serde_json::Value =
        serde_json::from_str(&daemon.file("config/kuickshare/settings.json")).unwrap();
    assert_eq!(saved["wifi_upgrade"], true);
    assert!(
        !saved.to_string().contains("insecure"),
        "the consent must not be saved: {saved}"
    );
}

#[tokio::test]
async fn reset_clears_consent() {
    let daemon = Daemon::start("reset-consent", None).await;
    daemon
        .call("RequestWifiUpgrade", &(true, true))
        .await
        .unwrap();
    assert!(daemon.get::<bool>("InsecureHotspotAllowed").await);

    daemon.call("ResetSettings", &()).await.unwrap();

    assert!(!daemon.get::<bool>("InsecureHotspotAllowed").await);
    assert!(!daemon.get::<bool>("WifiUpgrade").await);
}

#[tokio::test]
async fn request_wifi_upgrade_disable_always_allowed() {
    let daemon = Daemon::start("disable-wifi", None).await;

    // Off while already off.
    daemon
        .call("RequestWifiUpgrade", &(false, false))
        .await
        .unwrap();
    assert!(!daemon.get::<bool>("WifiUpgrade").await);

    // Off takes the consent back.
    daemon
        .call("RequestWifiUpgrade", &(true, true))
        .await
        .unwrap();
    daemon
        .call("RequestWifiUpgrade", &(false, false))
        .await
        .unwrap();
    assert!(!daemon.get::<bool>("WifiUpgrade").await);
    assert!(!daemon.get::<bool>("InsecureHotspotAllowed").await);

    // On without consent leaves it off.
    daemon
        .call("RequestWifiUpgrade", &(true, false))
        .await
        .unwrap();
    assert!(daemon.get::<bool>("WifiUpgrade").await);
    assert!(!daemon.get::<bool>("InsecureHotspotAllowed").await);
}

// ---------------------------------------------------------------------------------------------
// Wi-Fi requirements (contract: dist/tmp/mandate/designs/p1a-requirements.md, 2(b)).
//
// The daemon runs against a second private dbus-daemon used as its system bus, with fake
// BlueZ / NetworkManager / firewalld / polkit services (`fake_services.rs`) that log every call.
// ---------------------------------------------------------------------------------------------

const REQUIREMENTS_BLOCKED: &str = "io.github.the_beper.KuickShare1.Error.RequirementsBlocked";
const ACCESS_DENIED: &str = "org.freedesktop.DBus.Error.AccessDenied";
const STATUSES: [&str; 5] = ["ready", "warning", "blocked", "unknown", "checking"];
const CAPABILITY_STATUSES: [&str; 4] = ["available", "degraded", "unavailable", "unknown"];
const CATEGORIES: [&str; 4] = ["critical", "warning", "security", "info"];
const CAPABILITIES: [&str; 3] = ["sending", "receiving", "join_network"];

/// The only calls the probes may make, as logged by the fakes. The name queries
/// (NameHasOwner, ListActivatableNames) go to the bus daemon, and property reads (Get, GetAll)
/// are zbus's own: neither is logged. A polkit check is allowed only with flags 0.
const ALLOWED_CALLS: [&str; 6] = [
    "bluez:GetManagedObjects",
    "bluez:GetDiscoveryFilters",
    "nm:GetDevices",
    "nm:GetPermissions",
    "fw:getDefaultZone",
    "fw:getZones",
];

fn call_allowed(call: &str) -> bool {
    ALLOWED_CALLS.contains(&call) || call.starts_with("polkit:CheckAuthorization flags=0 ")
}

/// The calls that must never happen in a check, whatever the allow-list says (for a clear message).
const FORBIDDEN_FRAGMENTS: [&str; 12] = [
    "AddAndActivateConnection",
    "ActivateConnection",
    "DeactivateConnection",
    "RequestScan",
    "StartDiscovery",
    "SetDiscoveryFilter",
    "RegisterAdvertisement",
    "RegisterApplication",
    "addPort",
    "Set Powered",
    "Set WirelessEnabled",
    "GetAccessPoints",
];

async fn requirements(daemon: &Daemon, force: bool) -> serde_json::Value {
    let reply = daemon
        .conn
        .call_method(Some(NAME), PATH, Some(IFACE), "WifiRequirements", &(force,))
        .await
        .expect("WifiRequirements must answer");
    let text: String = reply.body().deserialize().unwrap();
    assert!(text.len() <= 64 * 1024, "report is over 64 KiB");
    serde_json::from_str(&text).expect("the report is JSON")
}

fn row<'a>(report: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    report["requirements"]
        .as_array()
        .expect("requirements is an array")
        .iter()
        .find(|r| r["id"] == id)
        .unwrap_or_else(|| panic!("no row {id} in {report}"))
}

fn status<'a>(report: &'a serde_json::Value, id: &str) -> &'a str {
    row(report, id)["status"].as_str().unwrap()
}

fn reason<'a>(report: &'a serde_json::Value, id: &str) -> &'a str {
    row(report, id)["reason_code"].as_str().unwrap()
}

fn capability<'a>(report: &'a serde_json::Value, name: &str) -> &'a str {
    report["capabilities"][name].as_str().unwrap()
}

fn error_name(err: &zbus::Error) -> String {
    match err {
        zbus::Error::MethodError(name, _, _) => name.to_string(),
        // A refused property write comes back as a standard error.
        zbus::Error::FDO(e) => zbus::DBusError::name(e.as_ref()).to_string(),
        other => panic!("not a D-Bus method error: {other}"),
    }
}

async fn try_set(daemon: &Daemon, property: &str, value: Variant<'_>) -> zbus::Result<()> {
    daemon
        .conn
        .call_method(
            Some(NAME),
            PATH,
            Some(PROPS),
            "Set",
            &(IFACE, property, value),
        )
        .await
        .map(|_| ())
}

fn saved_wifi_upgrade(daemon: &Daemon) -> bool {
    let text = daemon.file("config/kuickshare/settings.json");
    if text.is_empty() {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(&text).unwrap()["wifi_upgrade"] == true
}

/// A system bus with no service at all: BlueZ and NetworkManager are proven absent, so sending
/// and receiving are both unavailable.
async fn both_blocked(name: &str) -> (FakeSystem, Daemon) {
    let system = FakeSystem::start(name, Config::empty()).await;
    let daemon = Daemon::start_system(name, &system).await;
    let report = requirements(&daemon, false).await;
    assert_eq!(capability(&report, "sending"), "unavailable", "{report}");
    assert_eq!(capability(&report, "receiving"), "unavailable", "{report}");
    (system, daemon)
}

#[tokio::test]
async fn method_exists_schema_v1() {
    let system = FakeSystem::start("req-schema", Config::healthy()).await;
    let daemon = Daemon::start_system("req-schema", &system).await;

    let xml = daemon
        .conn
        .call_method(
            Some(NAME),
            PATH,
            Some("org.freedesktop.DBus.Introspectable"),
            "Introspect",
            &(),
        )
        .await
        .unwrap()
        .body()
        .deserialize::<String>()
        .unwrap();
    assert!(xml.contains("name=\"WifiRequirements\""), "{xml}");
    assert!(xml.contains("name=\"WifiRequirementsChanged\""), "{xml}");

    let report = requirements(&daemon, false).await;
    assert_eq!(report["schema_version"], 1);
    assert!(report["generated_at_ms"].as_u64().unwrap() > 0);
    assert!(report["ttl_ms"].as_u64().unwrap() > 0);
    assert!(["flatpak", "appimage", "native"].contains(&report["packaging"].as_str().unwrap()));
    assert_eq!(report["core_version"], env!("CARGO_PKG_VERSION"));
    for name in CAPABILITIES {
        assert!(
            CAPABILITY_STATUSES.contains(&capability(&report, name)),
            "{name}: {report}"
        );
    }

    let rows = report["requirements"].as_array().unwrap();
    assert!(!rows.is_empty() && rows.len() <= 64, "{} rows", rows.len());
    let mut ids = std::collections::HashSet::new();
    for r in rows {
        let id = r["id"].as_str().unwrap();
        assert!(ids.insert(id), "duplicate id {id}");
        assert!(STATUSES.contains(&r["status"].as_str().unwrap()), "{r}");
        assert!(CATEGORIES.contains(&r["category"].as_str().unwrap()), "{r}");
        assert!(!r["reason_code"].as_str().unwrap().is_empty(), "{r}");
        for field in ["details", "verification"] {
            let text = r[field].as_str().unwrap();
            assert!(text.chars().count() <= 200, "{id}.{field} too long");
            assert!(
                !text.contains(['<', '>']) && !text.chars().any(char::is_control),
                "{id}.{field}: {text:?}"
            );
        }
        assert!(
            r["device"].is_null() || r["device"].as_str().unwrap().chars().count() <= 32,
            "{r}"
        );
        for a in r["affects"].as_array().unwrap() {
            assert!(CAPABILITIES.contains(&a.as_str().unwrap()), "{r}");
        }
    }

    // The healthy fakes: nothing the contract checks is blocked, so nothing is unavailable.
    for id in [
        "bluez.service",
        "bluez.adapter",
        "nm.service",
        "nm.device",
        "nm.managed",
        "nm.firmware",
    ] {
        assert_eq!(status(&report, id), "ready", "{id}: {report}");
        assert_eq!(reason(&report, id), "ok", "{id}");
    }
    for name in ["sending", "receiving"] {
        assert!(
            ["available", "degraded"].contains(&capability(&report, name)),
            "{name}: {report}"
        );
    }

    // `false` returns the cache while it is younger than the TTL.
    let again = requirements(&daemon, false).await;
    assert_eq!(again["generated_at_ms"], report["generated_at_ms"]);
    // `true` is accepted and has the same schema.
    assert_eq!(requirements(&daemon, true).await["schema_version"], 1);
}

#[tokio::test]
async fn no_system_bus_all_unknown() {
    // The default harness: the system bus path does not exist.
    let daemon = Daemon::start("req-no-bus", None).await;

    let report = requirements(&daemon, false).await;
    assert_eq!(report["schema_version"], 1);
    for name in CAPABILITIES {
        assert_eq!(capability(&report, name), "unknown", "{name}: {report}");
    }
    // An unreachable bus is not proof of anything: no BlueZ or NM row may be blocked.
    for r in report["requirements"].as_array().unwrap() {
        let id = r["id"].as_str().unwrap();
        if id.starts_with("bluez.") || id.starts_with("nm.") {
            assert_eq!(r["status"], "unknown", "{r}");
        }
    }
    assert_eq!(reason(&report, "bluez.service"), "bus_unreachable");
    assert_eq!(reason(&report, "nm.service"), "bus_unreachable");

    // Unknown never blocks the user: enabling is allowed.
    daemon
        .call("RequestWifiUpgrade", &(true, false))
        .await
        .unwrap();
    assert!(daemon.get::<bool>("WifiUpgrade").await);
}

#[tokio::test]
async fn partial_services() {
    // Only NetworkManager is there.
    let mut cfg = Config::healthy();
    cfg.bluez = None;
    cfg.firewalld = None;
    cfg.polkit = false;
    let system = FakeSystem::start("req-partial-nm", cfg).await;
    let daemon = Daemon::start_system("req-partial-nm", &system).await;
    let report = requirements(&daemon, false).await;
    assert_eq!(status(&report, "bluez.service"), "blocked", "{report}");
    assert_eq!(reason(&report, "bluez.service"), "not_running");
    assert_eq!(status(&report, "nm.service"), "ready");
    assert_eq!(capability(&report, "sending"), "unavailable");
    assert_eq!(capability(&report, "receiving"), "unavailable");
    // Joining a network does not use Bluetooth.
    assert!(
        ["available", "degraded"].contains(&capability(&report, "join_network")),
        "{report}"
    );
    // No firewalld is a warning for receiving, never a block.
    assert_eq!(status(&report, "firewall.integration"), "warning");
    assert_eq!(reason(&report, "firewall.integration"), "not_running");
    drop(daemon);
    drop(system);

    // Only BlueZ is there: NetworkManager is needed by all three.
    let mut cfg = Config::healthy();
    cfg.nm = None;
    let system = FakeSystem::start("req-partial-bluez", cfg).await;
    let daemon = Daemon::start_system("req-partial-bluez", &system).await;
    let report = requirements(&daemon, false).await;
    assert_eq!(status(&report, "bluez.service"), "ready");
    assert_eq!(status(&report, "nm.service"), "blocked", "{report}");
    assert_eq!(reason(&report, "nm.service"), "not_running");
    for name in CAPABILITIES {
        assert_eq!(capability(&report, name), "unavailable", "{name}: {report}");
    }
}

#[tokio::test]
async fn hanging_service_reply_under_7s() {
    let mut cfg = Config::healthy();
    cfg.hang = vec!["nm:GetDevices".into()];
    let system = FakeSystem::start("req-hang", cfg).await;
    let daemon = Daemon::start_system("req-hang", &system).await;

    let started = std::time::Instant::now();
    let report = tokio::time::timeout(Duration::from_secs(7), requirements(&daemon, false))
        .await
        .expect("the report must not wait for a service that never replies");
    assert!(started.elapsed() < Duration::from_secs(7));

    // The missing answer is Unknown (never Blocked), and says why.
    assert_eq!(status(&report, "nm.device"), "unknown", "{report}");
    assert_eq!(reason(&report, "nm.device"), "timeout");
    // The other groups are not delayed or spoiled by it.
    assert_eq!(status(&report, "bluez.service"), "ready");
    assert_eq!(capability(&report, "sending"), "unknown");
}

#[tokio::test]
async fn probes_only_call_allowlist() {
    let system = FakeSystem::start("req-allowlist", Config::healthy()).await;
    let daemon = Daemon::start_system("req-allowlist", &system).await;
    let before = system.snapshot();

    let mark = system.mark();
    requirements(&daemon, true).await;
    let calls = system.since(mark);

    // The probes did talk to the fakes...
    assert!(calls.iter().any(|c| c == "nm:GetDevices"), "{calls:?}");
    assert!(
        calls.iter().any(|c| c == "bluez:GetManagedObjects"),
        "{calls:?}"
    );
    // ...and only with the read-only calls of the contract.
    for call in &calls {
        for fragment in FORBIDDEN_FRAGMENTS {
            assert!(!call.contains(fragment), "forbidden call {call}");
        }
        assert!(
            call_allowed(call),
            "call outside the allow-list: {call} (all: {calls:?})"
        );
    }
    // No side effects.
    assert_eq!(system.snapshot(), before);
    assert!(!daemon.get::<bool>("WifiUpgrade").await);
    assert!(!daemon.get::<bool>("InsecureHotspotAllowed").await);
}

#[tokio::test]
async fn enable_rejected_both_blocked() {
    let (system, daemon) = both_blocked("req-reject").await;
    let before = system.snapshot();

    for body in [(true, false), (true, true)] {
        let err = daemon
            .call("RequestWifiUpgrade", &body)
            .await
            .expect_err("enabling must be refused when sending and receiving are both blocked");
        assert_eq!(error_name(&err), REQUIREMENTS_BLOCKED, "{body:?}");
    }
    // Nothing changed: not the setting, not the consent, not the file, not the fakes.
    assert!(!daemon.get::<bool>("WifiUpgrade").await);
    assert!(!daemon.get::<bool>("InsecureHotspotAllowed").await);
    assert!(!saved_wifi_upgrade(&daemon));
    assert_eq!(system.snapshot(), before);

    // Consent without enabling is still InvalidArgs, not the new error.
    let err = daemon
        .call("RequestWifiUpgrade", &(false, true))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("InvalidArgs"), "{err}");
}

#[tokio::test]
async fn partial_allowed() {
    // The Wi-Fi chip has no AP mode: only receiving is blocked, sending still works.
    let mut cfg = Config::healthy();
    cfg.wifi_mut().wireless_caps &= !CAP_AP;
    let system = FakeSystem::start("req-partial-allowed", cfg).await;
    let daemon = Daemon::start_system("req-partial-allowed", &system).await;
    let report = requirements(&daemon, false).await;
    assert_eq!(capability(&report, "receiving"), "unavailable", "{report}");
    assert_ne!(capability(&report, "sending"), "unavailable", "{report}");
    assert_eq!(status(&report, "wifi.ap"), "blocked");
    assert_eq!(reason(&report, "wifi.ap"), "no_ap_mode");

    daemon
        .call("RequestWifiUpgrade", &(true, false))
        .await
        .unwrap();
    assert!(daemon.get::<bool>("WifiUpgrade").await);
    assert!(saved_wifi_upgrade(&daemon));
    assert!(!daemon.get::<bool>("InsecureHotspotAllowed").await);
}

#[tokio::test]
async fn disable_always_allowed() {
    let mut system = FakeSystem::start("req-disable", Config::healthy()).await;
    let daemon = Daemon::start_system("req-disable", &system).await;
    daemon
        .call("RequestWifiUpgrade", &(true, true))
        .await
        .unwrap();
    assert!(daemon.get::<bool>("WifiUpgrade").await);
    assert!(daemon.get::<bool>("InsecureHotspotAllowed").await);

    // Both services go away: everything is blocked now.
    system.stop_service(Service::Bluez).await;
    system.stop_service(Service::NetworkManager).await;

    // Already on: only the consent changes, the gate is not consulted (no trap).
    daemon
        .call("RequestWifiUpgrade", &(true, false))
        .await
        .unwrap();
    assert!(daemon.get::<bool>("WifiUpgrade").await);
    assert!(!daemon.get::<bool>("InsecureHotspotAllowed").await);

    // Turning off works through the method and through the setter, and revokes the consent.
    daemon
        .call("RequestWifiUpgrade", &(true, true))
        .await
        .unwrap();
    daemon
        .call("RequestWifiUpgrade", &(false, false))
        .await
        .unwrap();
    assert!(!daemon.get::<bool>("WifiUpgrade").await);
    assert!(!daemon.get::<bool>("InsecureHotspotAllowed").await);
    assert!(!saved_wifi_upgrade(&daemon));
    // Off while off.
    try_set(&daemon, "WifiUpgrade", Variant::from(false))
        .await
        .unwrap();
    assert!(!daemon.get::<bool>("WifiUpgrade").await);
}

#[tokio::test]
async fn setter_cannot_bypass() {
    let (system, daemon) = both_blocked("req-setter").await;
    let before = system.snapshot();

    // The legacy property goes through the same gate as the method.
    let err = try_set(&daemon, "WifiUpgrade", Variant::from(true))
        .await
        .expect_err("the setter must not bypass the requirements gate");
    assert_eq!(error_name(&err), ACCESS_DENIED);
    assert!(!daemon.get::<bool>("WifiUpgrade").await);
    assert!(!daemon.get::<bool>("InsecureHotspotAllowed").await);
    assert!(!saved_wifi_upgrade(&daemon));
    assert_eq!(system.snapshot(), before);

    // Writing false is always fine.
    try_set(&daemon, "WifiUpgrade", Variant::from(false))
        .await
        .unwrap();
}

#[tokio::test]
async fn no_implicit_consent() {
    let system = FakeSystem::start("req-consent", Config::healthy()).await;
    let daemon = Daemon::start_system("req-consent", &system).await;
    let before = system.snapshot();
    assert!(!daemon.get::<bool>("InsecureHotspotAllowed").await);

    // A check, a refresh and the setter never grant the consent or touch the firewall.
    requirements(&daemon, true).await;
    assert!(!daemon.get::<bool>("InsecureHotspotAllowed").await);
    try_set(&daemon, "WifiUpgrade", Variant::from(true))
        .await
        .unwrap();
    assert!(daemon.get::<bool>("WifiUpgrade").await);
    assert!(!daemon.get::<bool>("InsecureHotspotAllowed").await);
    daemon
        .call("RequestWifiUpgrade", &(true, false))
        .await
        .unwrap();
    assert!(!daemon.get::<bool>("InsecureHotspotAllowed").await);
    requirements(&daemon, true).await;
    assert!(!daemon.get::<bool>("InsecureHotspotAllowed").await);

    let calls = system.log();
    assert!(calls.iter().all(|c| !c.contains("addPort")), "{calls:?}");
    assert!(
        calls
            .iter()
            .all(|c| !c.starts_with("polkit:") || c.contains("flags=0 ")),
        "{calls:?}"
    );
    assert_eq!(system.snapshot(), before);
}

#[tokio::test]
async fn changed_signal_on_name_owner() {
    use futures_util::StreamExt;

    let mut system = FakeSystem::start("req-signal", Config::healthy()).await;
    let daemon = Daemon::start_system("req-signal", &system).await;
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(IFACE)
        .member("WifiRequirementsChanged")
        .build()
        .unwrap();
    let mut signals = zbus::MessageStream::for_match_rule(rule, &daemon.conn, None)
        .await
        .unwrap();

    // The first report is the baseline.
    let first = requirements(&daemon, false).await;
    assert_eq!(status(&first, "bluez.service"), "ready");

    // Bounded wait on the signal (the engine debounces 1.5 s and runs at most every 5 s).
    async fn until(
        signals: &mut zbus::MessageStream,
        wanted: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let msg = signals
                    .next()
                    .await
                    .expect("stream ended")
                    .expect("bad message");
                let text: String = msg.body().deserialize().unwrap();
                let report: serde_json::Value = serde_json::from_str(&text).unwrap();
                if wanted(&report) {
                    return report;
                }
            }
        })
        .await
        .expect("no WifiRequirementsChanged with the expected content")
    }

    system.stop_service(Service::Bluez).await;
    let gone = until(&mut signals, |r| {
        row(r, "bluez.service")["status"] == "blocked"
    })
    .await;
    assert_eq!(reason(&gone, "bluez.service"), "not_running");
    assert_eq!(capability(&gone, "sending"), "unavailable");
    assert_eq!(capability(&gone, "receiving"), "unavailable");

    system.start_service(Service::Bluez).await;
    let back = until(&mut signals, |r| {
        row(r, "bluez.service")["status"] == "ready"
    })
    .await;
    assert_ne!(capability(&back, "sending"), "unavailable");
}
