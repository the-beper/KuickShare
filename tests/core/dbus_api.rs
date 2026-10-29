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
        let dir = std::env::temp_dir().join(format!("kuickshare-it-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("config")).unwrap();
        fs::create_dir_all(dir.join("data/kuickshare")).unwrap();
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
                "unix:path=/nonexistent/kuickshare-it",
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

    async fn call<B: serde::Serialize + zbus::zvariant::DynamicType>(
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
