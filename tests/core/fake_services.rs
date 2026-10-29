//! Fake system-bus services for the Wi-Fi requirements tests: BlueZ, NetworkManager, firewalld
//! and polkit, served by zbus on a private dbus-daemon that stands in for the system bus.
//!
//! Every method call that reaches a fake is logged as `<service>:<member>` (see [`Service`]), so
//! a test can assert that the daemon only used the read-only calls of the contract. Methods that
//! would change the machine (AddAndActivateConnection*, ActivateConnection, RequestScan,
//! StartDiscovery, addPort, Set on Powered/WirelessEnabled, ...) exist on the fakes only to be
//! logged: they answer with an error and change nothing. Property reads go through zbus's own
//! `org.freedesktop.DBus.Properties` and are not logged (they are allowed anyway); property
//! writes have a logging setter.
//!
//! A method listed in [`Config::hang`] (for example `"nm:GetDevices"`) never replies, to test the
//! timeouts. Each service has its own connection, so a hanging method stalls that service only.

#![allow(dead_code)]

use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use zbus::{
    Connection, ObjectPath, OwnedObjectPath, OwnedVariant, Variant, connection::Builder, fdo,
    interface,
};

pub const BLUEZ_NAME: &str = "org.bluez";
pub const NM_NAME: &str = "org.freedesktop.NetworkManager";
pub const FIREWALLD_NAME: &str = "org.fedoraproject.FirewallD1";
pub const POLKIT_NAME: &str = "org.freedesktop.PolicyKit1";

/// The fake services, in the order the log prefixes name them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Service {
    Bluez,
    NetworkManager,
    Firewalld,
    Polkit,
}

impl Service {
    fn bus_name(self) -> &'static str {
        match self {
            Service::Bluez => BLUEZ_NAME,
            Service::NetworkManager => NM_NAME,
            Service::Firewalld => FIREWALLD_NAME,
            Service::Polkit => POLKIT_NAME,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AdapterCfg {
    pub name: String,
    pub powered: bool,
    /// `on`, `off`, `off-blocked`, ...
    pub power_state: String,
    /// `central` and/or `peripheral`.
    pub roles: Vec<String>,
    /// Serves `org.bluez.LEAdvertisingManager1`.
    pub advertising_manager: bool,
    /// Serves `org.bluez.GattManager1`.
    pub gatt_manager: bool,
}

#[derive(Clone, Debug)]
pub struct BluezCfg {
    pub adapters: Vec<AdapterCfg>,
}

#[derive(Clone, Debug)]
pub struct DeviceCfg {
    /// 2 is Wi-Fi.
    pub device_type: u32,
    pub interface: String,
    pub managed: bool,
    /// NM_DEVICE_STATE_*: 20 unavailable, 30 disconnected.
    pub state: u32,
    pub firmware_missing: bool,
    pub plugin_missing: bool,
    /// NM_WIFI_DEVICE_CAP_* bit set; 0 means "not reported".
    pub wireless_caps: u32,
}

#[derive(Clone, Debug)]
pub struct NmCfg {
    pub networking_enabled: bool,
    pub wireless_enabled: bool,
    pub wireless_hardware_enabled: bool,
    pub version: String,
    /// GetPermissions: permission key -> `yes`, `auth` or `no`.
    pub permissions: Vec<(String, String)>,
    pub devices: Vec<DeviceCfg>,
}

#[derive(Clone, Debug)]
pub struct FirewalldCfg {
    pub default_zone: String,
    pub zones: Vec<String>,
}

/// What is on the fake system bus. `None` means the service is absent (its name is not owned).
#[derive(Clone, Debug, Default)]
pub struct Config {
    pub bluez: Option<BluezCfg>,
    pub nm: Option<NmCfg>,
    pub firewalld: Option<FirewalldCfg>,
    /// polkit's Authority, answering every CheckAuthorization with "authorized".
    pub polkit: bool,
    /// Log keys (`nm:GetDevices`) of the methods that never reply.
    pub hang: Vec<String>,
}

/// Wi-Fi capability bits (NM_WIFI_DEVICE_CAP_*).
pub const CAP_CIPHER_CCMP: u32 = 0x8;
pub const CAP_RSN: u32 = 0x20;
pub const CAP_AP: u32 = 0x40;
pub const CAP_FREQ_5GHZ: u32 = 0x200;

pub const PERM_NETWORK_CONTROL: &str = "org.freedesktop.NetworkManager.network-control";
pub const PERM_SETTINGS_MODIFY: &str = "org.freedesktop.NetworkManager.settings.modify.system";
pub const PERM_WIFI_SHARE_PROTECTED: &str = "org.freedesktop.NetworkManager.wifi.share.protected";

impl Config {
    /// A machine where everything the contract checks is in order.
    pub fn healthy() -> Self {
        Self {
            bluez: Some(BluezCfg {
                adapters: vec![AdapterCfg {
                    name: "hci0".into(),
                    powered: true,
                    power_state: "on".into(),
                    roles: vec!["central".into(), "peripheral".into()],
                    advertising_manager: true,
                    gatt_manager: true,
                }],
            }),
            nm: Some(NmCfg {
                networking_enabled: true,
                wireless_enabled: true,
                wireless_hardware_enabled: true,
                version: "1.46.0".into(),
                permissions: vec![
                    (PERM_NETWORK_CONTROL.into(), "yes".into()),
                    (PERM_SETTINGS_MODIFY.into(), "yes".into()),
                    (PERM_WIFI_SHARE_PROTECTED.into(), "yes".into()),
                ],
                devices: vec![DeviceCfg {
                    device_type: 2,
                    interface: "wlan0".into(),
                    managed: true,
                    state: 30,
                    firmware_missing: false,
                    plugin_missing: false,
                    wireless_caps: CAP_CIPHER_CCMP | CAP_RSN | CAP_AP | CAP_FREQ_5GHZ,
                }],
            }),
            firewalld: Some(FirewalldCfg {
                default_zone: "public".into(),
                zones: vec!["public".into(), "nm-shared".into()],
            }),
            polkit: true,
            hang: Vec::new(),
        }
    }

    /// A system bus with no service on it at all.
    pub fn empty() -> Self {
        Self::default()
    }

    /// The first Wi-Fi device of the NM fake.
    pub fn wifi_mut(&mut self) -> &mut DeviceCfg {
        self.nm
            .as_mut()
            .expect("no NM fake")
            .devices
            .iter_mut()
            .find(|d| d.device_type == 2)
            .expect("no Wi-Fi device")
    }
}

struct Shared {
    log: Mutex<Vec<String>>,
    cfg: Mutex<Config>,
}

impl Shared {
    fn bluez<R>(&self, f: impl FnOnce(&BluezCfg) -> R) -> R {
        f(self
            .cfg
            .lock()
            .unwrap()
            .bluez
            .as_ref()
            .expect("BlueZ fake is not configured"))
    }

    fn bluez_mut<R>(&self, f: impl FnOnce(&mut BluezCfg) -> R) -> R {
        f(self
            .cfg
            .lock()
            .unwrap()
            .bluez
            .as_mut()
            .expect("BlueZ fake is not configured"))
    }

    fn nm<R>(&self, f: impl FnOnce(&NmCfg) -> R) -> R {
        f(self
            .cfg
            .lock()
            .unwrap()
            .nm
            .as_ref()
            .expect("NM fake is not configured"))
    }

    fn nm_mut<R>(&self, f: impl FnOnce(&mut NmCfg) -> R) -> R {
        f(self
            .cfg
            .lock()
            .unwrap()
            .nm
            .as_mut()
            .expect("NM fake is not configured"))
    }

    fn firewalld<R>(&self, f: impl FnOnce(&FirewalldCfg) -> R) -> R {
        f(self
            .cfg
            .lock()
            .unwrap()
            .firewalld
            .as_ref()
            .expect("firewalld fake is not configured"))
    }

    /// Records the call; never returns when the test asked this method to hang.
    async fn enter(&self, key: &str) {
        self.log.lock().unwrap().push(key.to_string());
        let hang = self.cfg.lock().unwrap().hang.iter().any(|h| h == key);
        if hang {
            std::future::pending::<()>().await;
        }
    }

    fn note(&self, key: String) {
        self.log.lock().unwrap().push(key);
    }
}

fn refused(what: &str) -> fdo::Error {
    fdo::Error::Failed(format!(
        "fake service: {what} is not allowed in a read-only check"
    ))
}

fn path(p: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(p).expect("valid object path")
}

fn ov<T: Into<Variant<'static>>>(v: T) -> OwnedVariant {
    OwnedVariant::try_from(v.into()).expect("value without file descriptors")
}

/// An array of strings as a variant.
fn ova(v: Vec<String>) -> OwnedVariant {
    OwnedVariant::try_from(Variant::Array(zbus::Array::from(v))).expect("array of strings")
}

// ---------------------------------------------------------------- BlueZ

struct BluezObjects(Arc<Shared>);

type Props = HashMap<String, OwnedVariant>;

fn adapter_path(name: &str) -> String {
    format!("/org/bluez/{name}")
}

fn adapter_props(a: &AdapterCfg) -> Props {
    HashMap::from([
        ("Address".to_string(), ov("00:00:00:00:00:01".to_string())),
        ("Name".to_string(), ov("fake".to_string())),
        ("Alias".to_string(), ov("fake".to_string())),
        ("Powered".to_string(), ov(a.powered)),
        ("PowerState".to_string(), ov(a.power_state.clone())),
        ("Roles".to_string(), ova(a.roles.clone())),
        ("Discovering".to_string(), ov(false)),
    ])
}

#[interface(name = "org.freedesktop.DBus.ObjectManager")]
impl BluezObjects {
    async fn get_managed_objects(&self) -> HashMap<OwnedObjectPath, HashMap<String, Props>> {
        self.0.enter("bluez:GetManagedObjects").await;
        self.0.bluez(|b| {
            b.adapters
                .iter()
                .map(|a| {
                    let mut ifaces =
                        HashMap::from([("org.bluez.Adapter1".to_string(), adapter_props(a))]);
                    if a.advertising_manager {
                        ifaces.insert(
                            "org.bluez.LEAdvertisingManager1".to_string(),
                            HashMap::from([
                                ("ActiveInstances".to_string(), ov(0u8)),
                                ("SupportedInstances".to_string(), ov(5u8)),
                                ("SupportedIncludes".to_string(), ova(Vec::new())),
                            ]),
                        );
                    }
                    if a.gatt_manager {
                        ifaces.insert("org.bluez.GattManager1".to_string(), HashMap::new());
                    }
                    (path(&adapter_path(&a.name)), ifaces)
                })
                .collect()
        })
    }
}

struct Adapter {
    sh: Arc<Shared>,
    name: String,
}

impl Adapter {
    fn with<R>(&self, f: impl FnOnce(&AdapterCfg) -> R) -> R {
        self.sh.bluez(|b| {
            f(b.adapters
                .iter()
                .find(|a| a.name == self.name)
                .expect("adapter"))
        })
    }
}

#[interface(name = "org.bluez.Adapter1")]
impl Adapter {
    #[zbus(property)]
    fn address(&self) -> String {
        "00:00:00:00:00:01".into()
    }

    #[zbus(property)]
    fn name(&self) -> String {
        "fake".into()
    }

    #[zbus(property)]
    fn alias(&self) -> String {
        "fake".into()
    }

    #[zbus(property)]
    fn powered(&self) -> bool {
        self.with(|a| a.powered)
    }

    #[zbus(property)]
    fn set_powered(&mut self, value: bool) {
        self.sh.note("bluez:Set Powered".to_string());
        let name = self.name.clone();
        self.sh.bluez_mut(|b| {
            if let Some(a) = b.adapters.iter_mut().find(|a| a.name == name) {
                a.powered = value;
            }
        });
    }

    #[zbus(property)]
    fn power_state(&self) -> String {
        self.with(|a| a.power_state.clone())
    }

    #[zbus(property)]
    fn roles(&self) -> Vec<String> {
        self.with(|a| a.roles.clone())
    }

    #[zbus(property)]
    fn discovering(&self) -> bool {
        false
    }

    async fn get_discovery_filters(&self) -> Vec<String> {
        self.sh.enter("bluez:GetDiscoveryFilters").await;
        vec!["Transport".into(), "DuplicateData".into()]
    }

    async fn start_discovery(&self) -> fdo::Result<()> {
        self.sh.enter("bluez:StartDiscovery").await;
        Err(refused("StartDiscovery"))
    }

    async fn set_discovery_filter(
        &self,
        _filter: HashMap<String, OwnedVariant>,
    ) -> fdo::Result<()> {
        self.sh.enter("bluez:SetDiscoveryFilter").await;
        Err(refused("SetDiscoveryFilter"))
    }
}

struct LeAdvertisingManager(Arc<Shared>);

#[interface(name = "org.bluez.LEAdvertisingManager1")]
impl LeAdvertisingManager {
    #[zbus(property)]
    fn active_instances(&self) -> u8 {
        0
    }

    #[zbus(property)]
    fn supported_instances(&self) -> u8 {
        5
    }

    #[zbus(property)]
    fn supported_includes(&self) -> Vec<String> {
        Vec::new()
    }

    async fn register_advertisement(
        &self,
        _advertisement: ObjectPath<'_>,
        _options: HashMap<String, OwnedVariant>,
    ) -> fdo::Result<()> {
        self.0.enter("bluez:RegisterAdvertisement").await;
        Err(refused("RegisterAdvertisement"))
    }
}

struct GattManager(Arc<Shared>);

#[interface(name = "org.bluez.GattManager1")]
impl GattManager {
    async fn register_application(
        &self,
        _application: ObjectPath<'_>,
        _options: HashMap<String, OwnedVariant>,
    ) -> fdo::Result<()> {
        self.0.enter("bluez:RegisterApplication").await;
        Err(refused("RegisterApplication"))
    }
}

// ---------------------------------------------------------------- NetworkManager

const NM_PATH: &str = "/org/freedesktop/NetworkManager";

fn nm_device_path(i: usize) -> String {
    format!("/org/freedesktop/NetworkManager/Devices/{i}")
}

struct NmManager(Arc<Shared>);

#[interface(name = "org.freedesktop.NetworkManager")]
impl NmManager {
    #[zbus(property)]
    fn networking_enabled(&self) -> bool {
        self.0.nm(|n| n.networking_enabled)
    }

    #[zbus(property)]
    fn wireless_enabled(&self) -> bool {
        self.0.nm(|n| n.wireless_enabled)
    }

    #[zbus(property)]
    fn set_wireless_enabled(&mut self, value: bool) {
        self.0.note("nm:Set WirelessEnabled".to_string());
        self.0.nm_mut(|n| n.wireless_enabled = value);
    }

    #[zbus(property)]
    fn wireless_hardware_enabled(&self) -> bool {
        self.0.nm(|n| n.wireless_hardware_enabled)
    }

    #[zbus(property)]
    fn version(&self) -> String {
        self.0.nm(|n| n.version.clone())
    }

    async fn get_devices(&self) -> Vec<OwnedObjectPath> {
        self.0.enter("nm:GetDevices").await;
        let count = self.0.nm(|n| n.devices.len());
        (0..count).map(|i| path(&nm_device_path(i))).collect()
    }

    async fn get_permissions(&self) -> HashMap<String, String> {
        self.0.enter("nm:GetPermissions").await;
        self.0.nm(|n| n.permissions.iter().cloned().collect())
    }

    async fn activate_connection(
        &self,
        _connection: ObjectPath<'_>,
        _device: ObjectPath<'_>,
        _specific_object: ObjectPath<'_>,
    ) -> fdo::Result<OwnedObjectPath> {
        self.0.enter("nm:ActivateConnection").await;
        Err(refused("ActivateConnection"))
    }

    async fn add_and_activate_connection(
        &self,
        _connection: HashMap<String, HashMap<String, OwnedVariant>>,
        _device: ObjectPath<'_>,
        _specific_object: ObjectPath<'_>,
    ) -> fdo::Result<(OwnedObjectPath, OwnedObjectPath)> {
        self.0.enter("nm:AddAndActivateConnection").await;
        Err(refused("AddAndActivateConnection"))
    }

    #[zbus(name = "AddAndActivateConnection2")]
    async fn add_and_activate_connection2(
        &self,
        _connection: HashMap<String, HashMap<String, OwnedVariant>>,
        _device: ObjectPath<'_>,
        _specific_object: ObjectPath<'_>,
        _options: HashMap<String, OwnedVariant>,
    ) -> fdo::Result<(
        OwnedObjectPath,
        OwnedObjectPath,
        HashMap<String, OwnedVariant>,
    )> {
        self.0.enter("nm:AddAndActivateConnection2").await;
        Err(refused("AddAndActivateConnection2"))
    }

    async fn deactivate_connection(&self, _active: ObjectPath<'_>) -> fdo::Result<()> {
        self.0.enter("nm:DeactivateConnection").await;
        Err(refused("DeactivateConnection"))
    }
}

struct NmDevice {
    sh: Arc<Shared>,
    idx: usize,
}

impl NmDevice {
    fn with<R>(&self, f: impl FnOnce(&DeviceCfg) -> R) -> R {
        self.sh.nm(|n| f(&n.devices[self.idx]))
    }
}

#[interface(name = "org.freedesktop.NetworkManager.Device")]
impl NmDevice {
    #[zbus(property)]
    fn device_type(&self) -> u32 {
        self.with(|d| d.device_type)
    }

    #[zbus(property)]
    fn interface(&self) -> String {
        self.with(|d| d.interface.clone())
    }

    #[zbus(property)]
    fn managed(&self) -> bool {
        self.with(|d| d.managed)
    }

    #[zbus(property)]
    fn state(&self) -> u32 {
        self.with(|d| d.state)
    }

    #[zbus(property)]
    fn firmware_missing(&self) -> bool {
        self.with(|d| d.firmware_missing)
    }

    #[zbus(property)]
    fn nm_plugin_missing(&self) -> bool {
        self.with(|d| d.plugin_missing)
    }
}

struct NmWireless {
    sh: Arc<Shared>,
    idx: usize,
}

#[interface(name = "org.freedesktop.NetworkManager.Device.Wireless")]
impl NmWireless {
    #[zbus(property)]
    fn wireless_capabilities(&self) -> u32 {
        self.sh.nm(|n| n.devices[self.idx].wireless_caps)
    }

    async fn request_scan(&self, _options: HashMap<String, OwnedVariant>) -> fdo::Result<()> {
        self.sh.enter("nm:RequestScan").await;
        Err(refused("RequestScan"))
    }

    async fn get_access_points(&self) -> Vec<OwnedObjectPath> {
        self.sh.enter("nm:GetAccessPoints").await;
        Vec::new()
    }
}

// ---------------------------------------------------------------- firewalld, polkit

struct FirewalldRoot(Arc<Shared>);

#[interface(name = "org.fedoraproject.FirewallD1")]
impl FirewalldRoot {
    #[zbus(name = "getDefaultZone")]
    async fn get_default_zone(&self) -> String {
        self.0.enter("fw:getDefaultZone").await;
        self.0.firewalld(|f| f.default_zone.clone())
    }
}

struct FirewalldZone(Arc<Shared>);

#[interface(name = "org.fedoraproject.FirewallD1.zone")]
impl FirewalldZone {
    #[zbus(name = "getZones")]
    async fn get_zones(&self) -> Vec<String> {
        self.0.enter("fw:getZones").await;
        self.0.firewalld(|f| f.zones.clone())
    }

    #[zbus(name = "addPort")]
    async fn add_port(
        &self,
        _zone: String,
        _port: String,
        _protocol: String,
        _timeout: i32,
    ) -> fdo::Result<String> {
        self.0.enter("fw:addPort").await;
        Err(refused("addPort"))
    }
}

struct PolkitAuthority(Arc<Shared>);

#[interface(name = "org.freedesktop.PolicyKit1.Authority")]
impl PolkitAuthority {
    async fn check_authorization(
        &self,
        _subject: (String, HashMap<String, OwnedVariant>),
        action_id: String,
        _details: HashMap<String, String>,
        flags: u32,
        _cancellation_id: String,
    ) -> (bool, bool, HashMap<String, String>) {
        // The flags are in the key so that a test sees an interactive check (flags != 0).
        self.0
            .enter(&format!(
                "polkit:CheckAuthorization flags={flags} action={action_id}"
            ))
            .await;
        (true, false, HashMap::new())
    }
}

// ---------------------------------------------------------------- the system bus

/// A private dbus-daemon used as `DBUS_SYSTEM_BUS_ADDRESS`, with the fake services on it.
/// Dropping it kills the bus: declare it before the `Daemon` that uses it, so that the daemon is
/// dropped first.
pub struct FakeSystem {
    pub address: String,
    dir: PathBuf,
    bus: Child,
    shared: Arc<Shared>,
    conns: HashMap<Service, Connection>,
}

impl FakeSystem {
    /// Starts the bus and every service present in `cfg`.
    pub async fn start(name: &str, cfg: Config) -> Self {
        let dir =
            std::env::temp_dir().join(format!("kuickshare-it-{}-{name}-sys", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let address = format!(
            "unix:abstract=kuickshare-it-{}-{name}-sys",
            std::process::id()
        );
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

        let mut system = Self {
            address,
            dir,
            bus,
            shared: Arc::new(Shared {
                log: Mutex::new(Vec::new()),
                cfg: Mutex::new(cfg),
            }),
            conns: HashMap::new(),
        };
        for service in [
            Service::Bluez,
            Service::NetworkManager,
            Service::Firewalld,
            Service::Polkit,
        ] {
            if system.configured(service) {
                system.start_service(service).await;
            }
        }
        system
    }

    fn configured(&self, service: Service) -> bool {
        let cfg = self.shared.cfg.lock().unwrap();
        match service {
            Service::Bluez => cfg.bluez.is_some(),
            Service::NetworkManager => cfg.nm.is_some(),
            Service::Firewalld => cfg.firewalld.is_some(),
            Service::Polkit => cfg.polkit,
        }
    }

    /// Puts the service on the bus (after `stop_service`, or when the test enabled it in the
    /// config with [`FakeSystem::configure`]).
    pub async fn start_service(&mut self, service: Service) {
        assert!(self.configured(service), "{service:?} has no config");
        // dbus-daemon needs a moment to listen: retry like the daemon harness does.
        for _ in 0..100 {
            match builder_for(&self.shared, &self.address, service)
                .build()
                .await
            {
                Ok(conn) => {
                    self.conns.insert(service, conn);
                    return;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        panic!("cannot start the fake {service:?} on {}", self.address);
    }

    /// Takes the service off the bus (its name loses its owner).
    pub async fn stop_service(&mut self, service: Service) {
        if let Some(conn) = self.conns.remove(&service) {
            conn.graceful_shutdown().await;
        }
    }

    /// Changes what the fakes answer from now on (and what `start_service` serves).
    pub fn configure(&self, f: impl FnOnce(&mut Config)) {
        f(&mut self.shared.cfg.lock().unwrap());
    }

    /// Everything the fakes were asked, in order, as `<service>:<member>`.
    pub fn log(&self) -> Vec<String> {
        self.shared.log.lock().unwrap().clone()
    }

    /// A position in the log, to read what was asked after it with [`FakeSystem::since`].
    pub fn mark(&self) -> usize {
        self.shared.log.lock().unwrap().len()
    }

    pub fn since(&self, mark: usize) -> Vec<String> {
        self.shared.log.lock().unwrap()[mark..].to_vec()
    }

    /// The state of all the fakes as text: equal before and after means nothing was changed.
    pub fn snapshot(&self) -> String {
        format!("{:?}", self.shared.cfg.lock().unwrap())
    }
}

impl Drop for FakeSystem {
    fn drop(&mut self) {
        let _ = self.bus.kill();
        let _ = self.bus.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn builder_for(sh: &Arc<Shared>, address: &str, service: Service) -> Builder<'static> {
    let builder = Builder::address(address).name(service.bus_name());
    match service {
        Service::Bluez => {
            let adapters: Vec<AdapterCfg> = sh.bluez(|b| b.adapters.clone());
            let mut b = builder.serve_at("/", BluezObjects(sh.clone()));
            for a in adapters {
                let p = adapter_path(&a.name);
                b = b.serve_at(
                    p.clone(),
                    Adapter {
                        sh: sh.clone(),
                        name: a.name.clone(),
                    },
                );
                if a.advertising_manager {
                    b = b.serve_at(p.clone(), LeAdvertisingManager(sh.clone()));
                }
                if a.gatt_manager {
                    b = b.serve_at(p.clone(), GattManager(sh.clone()));
                }
            }
            b
        }
        Service::NetworkManager => {
            let devices: Vec<u32> = sh.nm(|n| n.devices.iter().map(|d| d.device_type).collect());
            let mut b = builder.serve_at(NM_PATH, NmManager(sh.clone()));
            for (idx, kind) in devices.into_iter().enumerate() {
                let p = nm_device_path(idx);
                b = b.serve_at(
                    p.clone(),
                    NmDevice {
                        sh: sh.clone(),
                        idx,
                    },
                );
                if kind == 2 {
                    b = b.serve_at(
                        p,
                        NmWireless {
                            sh: sh.clone(),
                            idx,
                        },
                    );
                }
            }
            b
        }
        Service::Firewalld => builder
            .serve_at("/org/fedoraproject/FirewallD1", FirewalldRoot(sh.clone()))
            .serve_at("/org/fedoraproject/FirewallD1", FirewalldZone(sh.clone())),
        Service::Polkit => builder.serve_at(
            "/org/freedesktop/PolicyKit1/Authority",
            PolkitAuthority(sh.clone()),
        ),
    }
}
