//! `WifiLink` on top of NetworkManager's D-Bus API.
//!
//! Hosting creates an AP-mode WPA2 profile whose `shared` IPv4 method makes NetworkManager run
//! the DHCP server (its own `dnsmasq`, which must be installed); joining
//! connects to the phone's network as a client. All NetworkManager access goes
//! through the small `NmApi` trait so tests can fake it. The Wi-Fi key only
//! travels over D-Bus (never in a command line).
//!
//! Flatpak: no `nmcli` is needed; the sandbox only needs
//! `--system-talk-name=org.freedesktop.NetworkManager`.

use std::{
    io,
    net::Ipv4Addr,
    sync::Mutex as StdMutex,
    time::{Duration, Instant},
};

use rand::{RngExt, distr::Alphanumeric};
use rqs_lib::upgrade::{BoxFuture, WifiCredentials, WifiLink};
use tokio::{
    sync::{Mutex, MutexGuard, mpsc::UnboundedSender},
    time::sleep,
};

const AP_PROFILE: &str = "kuickshare-ap";
const JOIN_PROFILE: &str = "kuickshare-join";
/// firewalld zone NetworkManager uses for connection sharing: it allows DHCP and DNS only, so the
/// upgrade port is opened in it for the transfer.
const SHARED_ZONE: &str = "nm-shared";
/// Used only with the user's consent for this session, when firewalld refuses to open the port:
/// every port is reachable by the phone that joined.
const OPEN_ZONE: &str = "trusted";
/// Used only with the user's consent for this session, when firewalld is not there: the zone is
/// left to NetworkManager (another firewall may still block the phone).
const DEFAULT_ZONE: &str = "";
/// The port opening expires by itself if the daemon dies before closing it.
const PORT_OPEN_SECS: i32 = 300;
/// Every profile we create starts with this; used to sweep leftovers.
const PROFILE_PREFIX: &str = "kuickshare-";

// A fixed subnet can clash with a local network that uses 192.168.49.0/24.
const GATEWAY: Ipv4Addr = Ipv4Addr::new(192, 168, 49, 1);

/// The phone accepts a join attempt for only ~4-5 s, so a few short attempts.
const JOIN_ATTEMPTS: u32 = 4;
const JOIN_BUDGET: Duration = Duration::from_secs(15);
const JOIN_RETRY_DELAY: Duration = Duration::from_secs(1);
/// Attempts before the SSID is also tried as hidden.
const HIDDEN_AFTER: u32 = 2;
/// Per attempt: scan results are polled this many times, `AP_POLL_DELAY` apart. The Galaxy's
/// Wi-Fi Direct group was missing from the old 2.5 s window in every run (it showed up about 4 s
/// after the offer, inferred from the retry), hence 4.5 s.
const AP_POLLS: u32 = 10;
const AP_POLL_DELAY: Duration = Duration::from_millis(500);
/// Polls for the IPv4 address after the connection is up.
const IP_POLLS: u32 = 5;
const ACTIVATE_AP_TIMEOUT: Duration = Duration::from_secs(10);
const ACTIVATE_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
/// A saved network is joined once, with the user waiting: no phone-side window to race.
/// Worst case of `join_saved`: this wait, plus the D-Bus calls before it (device, active
/// connection, add) and the teardown after a failure (deactivate, delete, re-activate the previous
/// connection), each normally a few milliseconds. It must stay well under the shell's 25 s D-Bus
/// timeout, hence 15 s.
const ACTIVATE_SAVED_TIMEOUT: Duration = Duration::from_secs(15);

const MIN_PSK: usize = 8;
const MAX_PSK: usize = 63;
const MAX_SSID: usize = 32;
/// 32 alphanumerics are ~190 bits, far out of reach of an offline attack on the handshake.
const PSK_LEN: usize = 32;

#[derive(Clone, Copy, Debug)]
pub struct Band {
    nm_name: &'static str,
    channel: u32,
    mhz: u32,
}

const BAND_5GHZ: Band = Band {
    nm_name: "a",
    channel: 36,
    mhz: 5180,
};
const BAND_2GHZ: Band = Band {
    nm_name: "bg",
    channel: 6,
    mhz: 2437,
};

// ---------------------------------------------------------------------------
// Connection settings (what is sent to NetworkManager)
// ---------------------------------------------------------------------------

/// Wi-Fi key that never shows up in `Debug` output.
#[derive(Clone)]
pub struct Secret(pub String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Kind {
    Ap(Band),
    Client {
        hidden: bool,
    },
    /// A network the user chose to join for good: saved, autoconnect, normal routing. `open`: no key.
    Saved {
        open: bool,
    },
}

/// Where NetworkManager keeps the profile: our own temporary ones never reach the disk.
fn persist_mode(kind: Kind) -> &'static str {
    match kind {
        Kind::Saved { .. } => "disk",
        Kind::Ap(_) | Kind::Client { .. } => "memory",
    }
}

#[derive(Clone, Debug)]
pub struct ConnSettings {
    id: String,
    iface: String,
    ssid: Vec<u8>,
    psk: Secret,
    kind: Kind,
    /// firewalld zone of an access point; empty leaves the choice to NetworkManager.
    zone: &'static str,
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

impl ConnSettings {
    /// Rejects an SSID or key WPA2 cannot carry (the error never quotes either).
    fn new(id: &str, iface: &str, ssid: &str, psk: &str, kind: Kind) -> io::Result<Self> {
        if ssid.is_empty() || ssid.len() > MAX_SSID {
            return Err(invalid("Wi-Fi SSID must be 1..=32 bytes"));
        }
        if !(MIN_PSK..=MAX_PSK).contains(&psk.len()) {
            return Err(invalid("Wi-Fi key must be 8..=63 bytes"));
        }
        Ok(Self {
            id: id.to_owned(),
            iface: iface.to_owned(),
            ssid: ssid.as_bytes().to_vec(),
            psk: Secret(psk.to_owned()),
            kind,
            zone: "",
        })
    }

    /// A saved profile named after the network; `psk` is `None` for an open one.
    fn saved(iface: &str, ssid: &str, psk: Option<&str>) -> io::Result<Self> {
        // The sweep of leftover temporary profiles would delete a saved one with such a name.
        if ssid.starts_with(PROFILE_PREFIX) {
            return Err(invalid(
                "Networks whose name starts with \"kuickshare-\" cannot be joined from here",
            ));
        }
        match psk {
            Some(psk) => Self::new(ssid, iface, ssid, psk, Kind::Saved { open: false }),
            None => {
                if ssid.is_empty() || ssid.len() > MAX_SSID {
                    return Err(invalid("Wi-Fi SSID must be 1..=32 bytes"));
                }
                Ok(Self {
                    id: ssid.to_owned(),
                    iface: iface.to_owned(),
                    ssid: ssid.as_bytes().to_vec(),
                    psk: Secret(String::new()),
                    kind: Kind::Saved { open: true },
                    zone: "",
                })
            }
        }
    }
}

mod wire {
    use std::collections::HashMap;

    use zbus::Variant;

    use super::{ConnSettings, GATEWAY, Kind};

    pub type Dict = HashMap<&'static str, HashMap<&'static str, Variant<'static>>>;

    fn section<const N: usize>(
        entries: [(&'static str, Variant<'static>); N],
    ) -> HashMap<&'static str, Variant<'static>> {
        entries.into_iter().collect()
    }

    fn strings(items: &[&str]) -> Variant<'static> {
        Variant::from(items.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
    }

    /// The `a{sa{sv}}` argument of `AddAndActivateConnection2`.
    pub fn dict(s: &ConnSettings) -> Dict {
        let mut conn = vec![
            ("id", Variant::from(s.id.clone())),
            ("type", Variant::from("802-11-wireless")),
            (
                "autoconnect",
                Variant::from(matches!(s.kind, Kind::Saved { .. })),
            ),
            ("interface-name", Variant::from(s.iface.clone())),
        ];
        let mut wireless = vec![("ssid", Variant::from(s.ssid.clone()))];
        let mut security = vec![
            ("key-mgmt", Variant::from("wpa-psk")),
            ("psk", Variant::from(s.psk.0.clone())),
        ];
        let (ipv4, ipv6);
        match s.kind {
            Kind::Ap(band) => {
                if !s.zone.is_empty() {
                    conn.push(("zone", Variant::from(s.zone)));
                }
                wireless.extend([
                    ("mode", Variant::from("ap")),
                    ("band", Variant::from(band.nm_name)),
                    ("channel", Variant::from(band.channel)),
                ]);
                security.extend([
                    ("proto", strings(&["rsn"])),
                    ("pairwise", strings(&["ccmp"])),
                    ("group", strings(&["ccmp"])),
                ]);
                let address = section([
                    ("address", Variant::from(GATEWAY.to_string())),
                    ("prefix", Variant::from(24u32)),
                ]);
                ipv4 = section([
                    ("method", Variant::from("shared")),
                    ("address-data", Variant::from(vec![address])),
                ]);
                ipv6 = section([("method", Variant::from("disabled"))]);
            }
            Kind::Client { hidden } => {
                wireless.extend([
                    ("mode", Variant::from("infrastructure")),
                    ("hidden", Variant::from(hidden)),
                ]);
                // The AP is not trusted: it must not provide the default route or DNS.
                ipv4 = section([
                    ("method", Variant::from("auto")),
                    ("never-default", Variant::from(true)),
                    ("ignore-auto-dns", Variant::from(true)),
                ]);
                ipv6 = section([("method", Variant::from("disabled"))]);
            }
            Kind::Saved { .. } => {
                // The user's own choice, like Plasma's "Connect": normal routing and DNS.
                wireless.push(("mode", Variant::from("infrastructure")));
                ipv4 = section([("method", Variant::from("auto"))]);
                ipv6 = section([("method", Variant::from("auto"))]);
            }
        }
        let mut dict = HashMap::from([
            ("connection", conn.into_iter().collect()),
            ("802-11-wireless", wireless.into_iter().collect()),
            ("802-11-wireless-security", security.into_iter().collect()),
            ("ipv4", ipv4),
            ("ipv6", ipv6),
        ]);
        // An open network has no security section at all.
        if matches!(s.kind, Kind::Saved { open: true }) {
            dict.remove("802-11-wireless-security");
        }
        dict
    }
}

// ---------------------------------------------------------------------------
// NetworkManager access
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WifiDevice {
    /// D-Bus object path.
    path: String,
    iface: String,
}

/// A profile added by `add_and_activate` and its active connection (object paths).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Created {
    settings: String,
    active: String,
}

/// The NetworkManager operations `NmLink` needs. Objects are D-Bus paths.
pub trait NmApi: Send + Sync {
    /// First Wi-Fi (not Wi-Fi P2P) device.
    fn wifi_device(&self) -> impl Future<Output = io::Result<WifiDevice>> + Send;
    /// Settings objects whose id starts with `kuickshare-`.
    fn stale_profiles(&self) -> impl Future<Output = io::Result<Vec<String>>> + Send;
    /// Settings object of the connection active on the device, if any.
    fn current_profile(
        &self,
        dev: &WifiDevice,
    ) -> impl Future<Output = io::Result<Option<String>>> + Send;
    /// Adds the profile and starts activating it, pinned to `ap` when given.
    fn add_and_activate(
        &self,
        dev: &WifiDevice,
        ap: Option<&str>,
        settings: &ConnSettings,
    ) -> impl Future<Output = io::Result<Created>> + Send;
    /// Waits until the active connection is ACTIVATED; errors if it fails or times out.
    fn wait_activated(
        &self,
        active: &str,
        timeout: Duration,
    ) -> impl Future<Output = io::Result<()>> + Send;
    fn request_scan(&self, dev: &WifiDevice) -> impl Future<Output = io::Result<()>> + Send;
    /// Access point currently visible with exactly this SSID.
    fn find_ap(
        &self,
        dev: &WifiDevice,
        ssid: &[u8],
    ) -> impl Future<Output = io::Result<Option<String>>> + Send;
    fn ipv4(&self, dev: &WifiDevice) -> impl Future<Output = io::Result<Option<Ipv4Addr>>> + Send;
    fn deactivate(&self, active: &str) -> impl Future<Output = io::Result<()>> + Send;
    fn delete(&self, settings: &str) -> impl Future<Output = io::Result<()>> + Send;
    /// Opens a TCP port in the `nm-shared` firewalld zone until `close_port` (or a timeout).
    fn open_port(&self, port: u16) -> impl Future<Output = io::Result<()>> + Send;
    fn close_port(&self, port: u16) -> impl Future<Output = io::Result<()>> + Send;
    /// Activates an existing profile on the device.
    fn activate(
        &self,
        dev: &WifiDevice,
        settings: &str,
    ) -> impl Future<Output = io::Result<()>> + Send;
}

mod proxies {
    use std::collections::HashMap;

    use zbus::{ObjectPath, OwnedObjectPath, OwnedVariant, Variant, proxy};

    #[proxy(
        interface = "org.fedoraproject.FirewallD1.zone",
        default_service = "org.fedoraproject.FirewallD1",
        default_path = "/org/fedoraproject/FirewallD1"
    )]
    pub trait FirewallZone {
        #[zbus(name = "addPort")]
        fn add_port(
            &self,
            zone: &str,
            port: &str,
            protocol: &str,
            timeout: i32,
        ) -> zbus::Result<String>;
        #[zbus(name = "removePort")]
        fn remove_port(&self, zone: &str, port: &str, protocol: &str) -> zbus::Result<String>;
    }

    #[proxy(
        interface = "org.freedesktop.NetworkManager",
        default_service = "org.freedesktop.NetworkManager",
        default_path = "/org/freedesktop/NetworkManager"
    )]
    pub trait Manager {
        fn get_devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

        fn add_and_activate_connection2(
            &self,
            connection: HashMap<&str, HashMap<&str, Variant<'_>>>,
            device: &ObjectPath<'_>,
            specific_object: &ObjectPath<'_>,
            options: HashMap<&str, Variant<'_>>,
        ) -> zbus::Result<(
            OwnedObjectPath,
            OwnedObjectPath,
            HashMap<String, OwnedVariant>,
        )>;

        fn activate_connection(
            &self,
            connection: &ObjectPath<'_>,
            device: &ObjectPath<'_>,
            specific_object: &ObjectPath<'_>,
        ) -> zbus::Result<OwnedObjectPath>;

        fn deactivate_connection(&self, active_connection: &ObjectPath<'_>) -> zbus::Result<()>;
    }

    /// Own module: the generated signal types would clash with the ones of another proxy here.
    pub mod network_state {
        use zbus::proxy;

        #[proxy(
            interface = "org.freedesktop.NetworkManager",
            default_service = "org.freedesktop.NetworkManager",
            default_path = "/org/freedesktop/NetworkManager"
        )]
        pub trait NetworkState {
            /// Overall connectivity (`NMState`) changed.
            #[zbus(signal)]
            fn state_changed(&self, state: u32) -> zbus::Result<()>;
        }
    }

    #[proxy(
        interface = "org.freedesktop.NetworkManager.Settings",
        default_service = "org.freedesktop.NetworkManager",
        default_path = "/org/freedesktop/NetworkManager/Settings"
    )]
    pub trait Settings {
        fn list_connections(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    }

    #[proxy(
        interface = "org.freedesktop.NetworkManager.Settings.Connection",
        default_service = "org.freedesktop.NetworkManager"
    )]
    pub trait Profile {
        fn get_settings(&self) -> zbus::Result<HashMap<String, HashMap<String, OwnedVariant>>>;
        fn delete(&self) -> zbus::Result<()>;
    }

    #[proxy(
        interface = "org.freedesktop.NetworkManager.Connection.Active",
        default_service = "org.freedesktop.NetworkManager"
    )]
    pub trait ActiveConn {
        /// Named apart from the `StateChanged` signal's stream.
        #[zbus(property, name = "State")]
        fn current_state(&self) -> zbus::Result<u32>;

        #[zbus(signal)]
        fn state_changed(&self, state: u32, reason: u32) -> zbus::Result<()>;
    }

    #[proxy(
        interface = "org.freedesktop.NetworkManager.Device",
        default_service = "org.freedesktop.NetworkManager"
    )]
    pub trait Device {
        #[zbus(property)]
        fn device_type(&self) -> zbus::Result<u32>;
        #[zbus(property)]
        fn interface(&self) -> zbus::Result<String>;
        #[zbus(property)]
        fn active_connection(&self) -> zbus::Result<OwnedObjectPath>;
        #[zbus(property)]
        fn ip4_config(&self) -> zbus::Result<OwnedObjectPath>;
    }

    #[proxy(
        interface = "org.freedesktop.NetworkManager.Device.Wireless",
        default_service = "org.freedesktop.NetworkManager"
    )]
    pub trait Wireless {
        fn request_scan(&self, options: HashMap<&str, Variant<'_>>) -> zbus::Result<()>;
        #[zbus(property)]
        fn access_points(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    }

    #[proxy(
        interface = "org.freedesktop.NetworkManager.AccessPoint",
        default_service = "org.freedesktop.NetworkManager"
    )]
    pub trait AccessPoint {
        #[zbus(property)]
        fn ssid(&self) -> zbus::Result<Vec<u8>>;
    }

    #[proxy(
        interface = "org.freedesktop.NetworkManager.IP4Config",
        default_service = "org.freedesktop.NetworkManager"
    )]
    pub trait Ip4Config {
        #[zbus(property)]
        fn address_data(&self) -> zbus::Result<Vec<HashMap<String, OwnedVariant>>>;
    }
}

use proxies::*;
use zbus::{Connection, ObjectPath, proxy::CacheProperties};

/// NM_DEVICE_TYPE_WIFI
const DEVICE_TYPE_WIFI: u32 = 2;
/// NM_ACTIVE_CONNECTION_STATE_ACTIVATED / _DEACTIVATED
const STATE_ACTIVATED: u32 = 2;
const STATE_DEACTIVATED: u32 = 4;
const NO_OBJECT: &str = "/";

fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("NetworkManager: {e}"))
}

/// What a failed firewalld call says about the firewall.
#[derive(Debug, PartialEq, Eq)]
enum FirewallFault {
    /// firewalld is not there: nobody owns its name, or the system bus cannot be reached.
    Missing,
    /// firewalld (or the bus policy, or polkit) refused the change.
    Denied,
    /// Any other error; handled like a refusal, and logged.
    Other,
}

fn classify_firewall_error(e: &zbus::Error) -> FirewallFault {
    use zbus::fdo::Error as Fdo;
    match e {
        zbus::Error::FDO(f) => match **f {
            Fdo::ServiceUnknown(_)
            | Fdo::NameHasNoOwner(_)
            | Fdo::Disconnected(_)
            | Fdo::NoServer(_)
            | Fdo::SpawnExecFailed(_)
            | Fdo::SpawnForkFailed(_)
            | Fdo::SpawnChildExited(_)
            | Fdo::SpawnChildSignaled(_)
            | Fdo::SpawnFailed(_)
            | Fdo::SpawnFailedToSetup(_)
            | Fdo::SpawnConfigInvalid(_) => FirewallFault::Missing,
            Fdo::AccessDenied(_) | Fdo::AuthFailed(_) => FirewallFault::Denied,
            _ => FirewallFault::Other,
        },
        zbus::Error::InputOutput(_) | zbus::Error::Address(_) => FirewallFault::Missing,
        // firewalld's own refusal (polkit): its error carries NOT_AUTHORIZED, or asks for an
        // interactive authorization that nobody can give here.
        zbus::Error::MethodError(name, detail, _)
            if name.as_str().ends_with("InteractiveAuthorizationRequired")
                || detail
                    .as_deref()
                    .is_some_and(|d| d.contains("NOT_AUTHORIZED")) =>
        {
            FirewallFault::Denied
        }
        _ => FirewallFault::Other,
    }
}

/// An error of the firewalld call. `NotFound` means firewalld is not there; anything else is a
/// refusal or a failure of firewalld itself (both stop an unconsented hotspot).
fn firewall_err(e: zbus::Error) -> io::Error {
    let fault = classify_firewall_error(&e);
    if fault == FirewallFault::Other {
        tracing::warn!("unexpected error from firewalld, taken as a refusal: {e:?}");
    }
    if fault == FirewallFault::Missing {
        io::Error::new(io::ErrorKind::NotFound, format!("firewalld: {e}"))
    } else {
        io::Error::other(format!("firewalld: {e}"))
    }
}

fn path(p: &str) -> zbus::Result<ObjectPath<'_>> {
    ObjectPath::try_from(p)
}

/// Real implementation on the system bus.
pub struct DbusNm {
    conn: tokio::sync::OnceCell<Connection>,
}

macro_rules! proxy_at {
    ($proxy:ident, $c:expr, $path:expr) => {
        $proxy::builder($c)
            .path($path)
            .cache_properties(CacheProperties::No)
            .build()
            .await?
    };
}

impl DbusNm {
    fn new() -> Self {
        Self {
            conn: tokio::sync::OnceCell::new(),
        }
    }

    async fn conn(&self) -> zbus::Result<&Connection> {
        self.conn.get_or_try_init(Connection::system).await
    }

    async fn wifi_device_z(&self) -> zbus::Result<Option<WifiDevice>> {
        let c = self.conn().await?;
        for p in ManagerProxy::new(c).await?.get_devices().await? {
            let dev = proxy_at!(DeviceProxy, c, p.as_str());
            if dev.device_type().await? == DEVICE_TYPE_WIFI {
                return Ok(Some(WifiDevice {
                    path: p.to_string(),
                    iface: dev.interface().await?,
                }));
            }
        }
        Ok(None)
    }

    async fn stale_profiles_z(&self) -> zbus::Result<Vec<String>> {
        let c = self.conn().await?;
        let mut stale = Vec::new();
        for p in SettingsProxy::new(c).await?.list_connections().await? {
            let profile = proxy_at!(ProfileProxy, c, p.as_str());
            let mut settings = match profile.get_settings().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!("skipping unreadable profile {p}: {e}");
                    continue;
                }
            };
            let id = settings
                .remove("connection")
                .and_then(|mut s| s.remove("id"))
                .and_then(|v| String::try_from(v).ok());
            if id.is_some_and(|id| id.starts_with(PROFILE_PREFIX)) {
                stale.push(p.to_string());
            }
        }
        Ok(stale)
    }

    async fn current_profile_z(&self, dev: &WifiDevice) -> zbus::Result<Option<String>> {
        let c = self.conn().await?;
        let active = proxy_at!(DeviceProxy, c, dev.path.as_str())
            .active_connection()
            .await?;
        if active.as_str() == NO_OBJECT {
            return Ok(None);
        }
        let profile = proxy_at!(ActiveConnProxy, c, active.as_str())
            .inner()
            .get_property::<zbus::OwnedObjectPath>("Connection")
            .await?;
        Ok(Some(profile.to_string()))
    }

    async fn add_and_activate_z(
        &self,
        dev: &WifiDevice,
        ap: Option<&str>,
        settings: &ConnSettings,
    ) -> zbus::Result<Created> {
        let c = self.conn().await?;
        let dict = wire::dict(settings)
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().collect()))
            .collect();
        // "memory": our temporary profile (and its key) is never written to disk.
        let options = [("persist", persist_mode(settings.kind).into())]
            .into_iter()
            .collect();
        let (settings, active, _) = ManagerProxy::new(c)
            .await?
            .add_and_activate_connection2(
                dict,
                &path(&dev.path)?,
                &path(ap.unwrap_or(NO_OBJECT))?,
                options,
            )
            .await?;
        Ok(Created {
            settings: settings.to_string(),
            active: active.to_string(),
        })
    }

    async fn wait_activated_z(&self, active: &str, limit: Duration) -> zbus::Result<()> {
        use futures_util::StreamExt;

        let c = self.conn().await?;
        let conn = proxy_at!(ActiveConnProxy, c, active);
        // Subscribe first, then read the current state: no transition is missed.
        let mut changes = conn.receive_state_changed().await?;
        let mut state = conn.current_state().await?;
        // The reason NetworkManager gave for the latest change (none for the state read above).
        let mut reason = None;
        let wait = async {
            loop {
                match state {
                    STATE_ACTIVATED => return Ok(()),
                    STATE_DEACTIVATED => {
                        // The raw `NMActiveConnectionStateReason`, logged as it is: no guess at
                        // the cause (such as a missing dnsmasq) is made from it.
                        return Err(zbus::Error::Failure(match reason {
                            Some(r) => format!(
                                "connection was deactivated (NetworkManager reason code {r})"
                            ),
                            None => "connection was deactivated".into(),
                        }));
                    }
                    _ => {}
                }
                let Some(signal) = changes.next().await else {
                    return Err(zbus::Error::Failure("signal stream ended".into()));
                };
                let args = signal.args()?;
                state = args.state;
                reason = Some(args.reason);
            }
        };
        tokio::time::timeout(limit, wait)
            .await
            .unwrap_or_else(|_| Err(zbus::Error::Failure("activation timed out".into())))
    }

    async fn find_ap_z(&self, dev: &WifiDevice, ssid: &[u8]) -> zbus::Result<Option<String>> {
        let c = self.conn().await?;
        let wireless = proxy_at!(WirelessProxy, c, dev.path.as_str());
        for p in wireless.access_points().await? {
            let ap = proxy_at!(AccessPointProxy, c, p.as_str());
            // An AP can vanish between the listing and this read.
            if ap.ssid().await.is_ok_and(|s| s == ssid) {
                return Ok(Some(p.to_string()));
            }
        }
        Ok(None)
    }

    async fn ipv4_z(&self, dev: &WifiDevice) -> zbus::Result<Option<Ipv4Addr>> {
        let c = self.conn().await?;
        let cfg = proxy_at!(DeviceProxy, c, dev.path.as_str())
            .ip4_config()
            .await?;
        if cfg.as_str() == NO_OBJECT {
            return Ok(None);
        }
        for mut entry in proxy_at!(Ip4ConfigProxy, c, cfg.as_str())
            .address_data()
            .await?
        {
            let ip = entry
                .remove("address")
                .and_then(|v| String::try_from(v).ok())
                .and_then(|s| s.parse().ok());
            if ip.is_some() {
                return Ok(ip);
            }
        }
        Ok(None)
    }
}

impl NmApi for DbusNm {
    async fn wifi_device(&self) -> io::Result<WifiDevice> {
        self.wifi_device_z()
            .await
            .map_err(io_err)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no Wi-Fi device found"))
    }

    async fn stale_profiles(&self) -> io::Result<Vec<String>> {
        self.stale_profiles_z().await.map_err(io_err)
    }

    async fn current_profile(&self, dev: &WifiDevice) -> io::Result<Option<String>> {
        self.current_profile_z(dev).await.map_err(io_err)
    }

    async fn add_and_activate(
        &self,
        dev: &WifiDevice,
        ap: Option<&str>,
        settings: &ConnSettings,
    ) -> io::Result<Created> {
        self.add_and_activate_z(dev, ap, settings)
            .await
            .map_err(io_err)
    }

    async fn wait_activated(&self, active: &str, timeout: Duration) -> io::Result<()> {
        self.wait_activated_z(active, timeout).await.map_err(io_err)
    }

    async fn request_scan(&self, dev: &WifiDevice) -> io::Result<()> {
        async {
            let c = self.conn().await?;
            proxy_at!(WirelessProxy, c, dev.path.as_str())
                .request_scan(Default::default())
                .await
        }
        .await
        .map_err(io_err)
    }

    async fn find_ap(&self, dev: &WifiDevice, ssid: &[u8]) -> io::Result<Option<String>> {
        self.find_ap_z(dev, ssid).await.map_err(io_err)
    }

    async fn ipv4(&self, dev: &WifiDevice) -> io::Result<Option<Ipv4Addr>> {
        self.ipv4_z(dev).await.map_err(io_err)
    }

    async fn deactivate(&self, active: &str) -> io::Result<()> {
        async {
            let c = self.conn().await?;
            ManagerProxy::new(c)
                .await?
                .deactivate_connection(&path(active)?)
                .await
        }
        .await
        .map_err(io_err)
    }

    async fn delete(&self, settings: &str) -> io::Result<()> {
        async {
            let c = self.conn().await?;
            proxy_at!(ProfileProxy, c, settings).delete().await
        }
        .await
        .map_err(io_err)
    }

    async fn open_port(&self, port: u16) -> io::Result<()> {
        async {
            let c = self.conn().await?;
            FirewallZoneProxy::new(c)
                .await?
                .add_port(SHARED_ZONE, &port.to_string(), "tcp", PORT_OPEN_SECS)
                .await
                .map(drop)
        }
        .await
        .map_err(firewall_err)
    }

    async fn close_port(&self, port: u16) -> io::Result<()> {
        async {
            let c = self.conn().await?;
            FirewallZoneProxy::new(c)
                .await?
                .remove_port(SHARED_ZONE, &port.to_string(), "tcp")
                .await
                .map(drop)
        }
        .await
        .map_err(|e| io::Error::other(format!("firewalld: {e}")))
    }

    async fn activate(&self, dev: &WifiDevice, settings: &str) -> io::Result<()> {
        async {
            let c = self.conn().await?;
            ManagerProxy::new(c)
                .await?
                .activate_connection(&path(settings)?, &path(&dev.path)?, &path(NO_OBJECT)?)
                .await
                .map(drop)
        }
        .await
        .map_err(io_err)
    }
}

// ---------------------------------------------------------------------------
// The link
// ---------------------------------------------------------------------------

struct Timing {
    retry_delay: Duration,
    poll_delay: Duration,
    budget: Duration,
}

/// What `host` or `join` set up. Empty (default) means the link is free.
#[derive(Default)]
struct State {
    busy: bool,
    dev: Option<WifiDevice>,
    /// Profile added by us and not yet removed.
    created: Option<Created>,
    /// Profile the device had active before we took it over.
    previous: Option<String>,
    /// Set before the first profile is sent to NetworkManager.
    touched: bool,
    /// Port opened in the firewall zone and not yet closed.
    open_port: Option<u16>,
    /// The hotspot runs in an open (or default) zone, on the user's consent for this session.
    open_zone: bool,
}

/// What the hotspot may do to the firewall when the one port it needs cannot be opened.
/// Kept in memory only (never written to a file); the user's consent lasts until the daemon stops.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HotspotPolicy {
    /// Open the upgrade port in the sharing zone; if that fails, do not host at all.
    #[default]
    TargetedPortOnly,
    /// As above, but if the port cannot be opened host anyway in an open zone (firewalld refused)
    /// or in the default zone (firewalld absent).
    AllowOpenZone,
}

/// Why the firewall stopped the hotspot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockReason {
    /// firewalld is running but refused to open the port (no permission, or an error).
    FirewallDenied,
    /// firewalld is not running (or not reachable).
    FirewallMissing,
}

impl BlockReason {
    /// The stable code carried by the `WifiUpgradeBlocked` D-Bus signal.
    pub fn code(self) -> &'static str {
        match self {
            Self::FirewallDenied => "firewall_denied",
            Self::FirewallMissing => "firewall_missing",
        }
    }
}

/// Something the link tells the rest of the daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkNotice {
    /// The hotspot was not started because of the firewall; the transfer stays on its first channel.
    WifiUpgradeBlocked(BlockReason),
}

pub struct NmLink<A = DbusNm> {
    api: A,
    timing: Timing,
    state: Mutex<State>,
    /// Plain mutex: read and written without I/O, never held across an await.
    policy: StdMutex<HotspotPolicy>,
    notices: Option<UnboundedSender<LinkNotice>>,
}

impl NmLink {
    pub fn new() -> Self {
        Self::with_parts(
            DbusNm::new(),
            Timing {
                retry_delay: JOIN_RETRY_DELAY,
                poll_delay: AP_POLL_DELAY,
                budget: JOIN_BUDGET,
            },
        )
    }

    /// Sends the notices to `notices` (see `LinkNotice`).
    pub fn with_notices(mut self, notices: UnboundedSender<LinkNotice>) -> Self {
        self.notices = Some(notices);
        self
    }
}

/// `NM_STATE_DISCONNECTED` and below: no network at all (Wi-Fi off and nothing else connected).
const NM_STATE_DISCONNECTED: u32 = 20;
/// `NM_STATE_CONNECTED_LOCAL` and above: a network is up.
const NM_STATE_CONNECTED: u32 = 50;

#[derive(Debug, PartialEq, Eq)]
pub enum NetworkEvent {
    /// The last connection is gone.
    Lost,
    /// A network came up after there was none.
    Gained,
}

/// What the change from `previous` to `now` means. The first state seen has no `previous`: a
/// connected one is the normal case at start and says nothing.
fn network_event(previous: Option<u32>, now: u32) -> Option<NetworkEvent> {
    if now <= NM_STATE_DISCONNECTED && previous.is_none_or(|p| p > NM_STATE_DISCONNECTED) {
        Some(NetworkEvent::Lost)
    } else if now >= NM_STATE_CONNECTED && previous.is_some_and(|p| p < NM_STATE_CONNECTED) {
        Some(NetworkEvent::Gained)
    } else {
        None
    }
}

/// Reports every loss and gain of the network that NetworkManager announces, until the receiver
/// is dropped. Returns at once, quietly, when NetworkManager is not there.
pub async fn watch_network(events: tokio::sync::mpsc::UnboundedSender<NetworkEvent>) {
    use futures_util::StreamExt;
    let watch = async {
        let connection = Connection::system().await?;
        let manager = proxies::network_state::NetworkStateProxy::new(&connection).await?;
        let mut changes = manager.receive_state_changed().await?;
        let mut previous = None;
        while let Some(change) = changes.next().await {
            let state = change.args()?.state;
            if let Some(event) = network_event(previous, state)
                && events.send(event).is_err()
            {
                break;
            }
            previous = Some(state);
        }
        zbus::Result::Ok(())
    };
    if let Err(e) = watch.await {
        tracing::debug!("not watching the network: {e}");
    }
}

/// Whether firewalld is running: only then the upgrade port can be opened on its own. Without it
/// the hotspot is refused unless the user allowed it for this session (`HotspotPolicy`).
pub async fn firewall_managed() -> bool {
    async {
        let connection = Connection::system().await?;
        let name = zbus::names::BusName::try_from("org.fedoraproject.FirewallD1")?;
        zbus::fdo::DBusProxy::new(&connection)
            .await?
            .name_has_owner(name)
            .await
    }
    .await
    .unwrap_or(false)
}

fn busy() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "a Wi-Fi upgrade is already in progress",
    )
}

impl<A: NmApi> NmLink<A> {
    fn with_parts(api: A, timing: Timing) -> Self {
        Self {
            api,
            timing,
            state: Mutex::new(State::default()),
            policy: StdMutex::new(HotspotPolicy::default()),
            notices: None,
        }
    }

    /// Sets what the next hotspot may do about the firewall. No I/O: safe under any lock.
    pub fn set_hotspot_policy(&self, policy: HotspotPolicy) {
        *self
            .policy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = policy;
    }

    pub fn hotspot_policy(&self) -> HotspotPolicy {
        *self
            .policy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn notify(&self, notice: LinkNotice) {
        if let Some(tx) = &self.notices {
            // The receiver is gone only while the daemon stops.
            let _ = tx.send(notice);
        }
    }

    /// The zone for the hotspot when the port could not be opened (`error`), or the refusal.
    /// Nothing has been added to NetworkManager at this point.
    fn zone_without_port(&self, port: u16, error: &io::Error) -> io::Result<&'static str> {
        // Read now, not when the hotspot started: a consent taken back meanwhile counts.
        let policy = self.hotspot_policy();
        let reason = if error.kind() == io::ErrorKind::NotFound {
            BlockReason::FirewallMissing
        } else {
            BlockReason::FirewallDenied
        };
        match (policy, reason) {
            (HotspotPolicy::AllowOpenZone, BlockReason::FirewallDenied) => {
                tracing::warn!(
                    "could not open port {port} in the firewall ({error}); using the trusted zone as allowed for this session"
                );
                Ok(OPEN_ZONE)
            }
            (HotspotPolicy::AllowOpenZone, BlockReason::FirewallMissing) => {
                tracing::warn!(
                    "could not open port {port} in the firewall ({error}); leaving the zone to NetworkManager as allowed for this session"
                );
                Ok(DEFAULT_ZONE)
            }
            (HotspotPolicy::TargetedPortOnly, _) => {
                tracing::warn!(
                    "could not open port {port} in the firewall ({error}); not starting the hotspot"
                );
                self.notify(LinkNotice::WifiUpgradeBlocked(reason));
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("the firewall did not allow the upgrade port ({error})"),
                ))
            }
        }
    }

    /// Claims the link; refuses when another upgrade holds it.
    fn begin(&self) -> io::Result<MutexGuard<'_, State>> {
        let mut st = self.state.try_lock().map_err(|_| busy())?;
        if st.busy {
            return Err(busy());
        }
        st.busy = true;
        Ok(st)
    }

    /// Removes leftover `kuickshare-*` profiles (crashed run). Failures are only logged.
    async fn sweep(&self) {
        match self.api.stale_profiles().await {
            Ok(stale) => {
                for p in stale {
                    if let Err(e) = self.api.delete(&p).await {
                        tracing::warn!("could not delete stale profile {p}: {e}");
                    }
                }
            }
            // Expected when NetworkManager is absent.
            Err(e) => tracing::debug!("could not list stale profiles: {e}"),
        }
    }

    /// For daemon start and shutdown, on the link the transfers used: closes the firewall port,
    /// removes the profile and restores the previous connection if a hotspot is up or half-made,
    /// and sweeps leftover `kuickshare-*` profiles (also those of a crashed run, whatever the
    /// settings say now). Waits for the link if an upgrade holds it.
    pub async fn shutdown(&self) {
        let mut st = self.state.lock().await;
        self.teardown(&mut st).await;
        self.sweep().await;
    }

    /// The consent for an open zone was taken back: a hotspot that runs in one is taken down (the
    /// transfer goes on over its first channel). Waits for the link, so a hotspot that is still
    /// starting is judged when it is done.
    pub async fn drop_open_zone_hotspot(&self) {
        let mut st = self.state.lock().await;
        if st.open_zone && self.hotspot_policy() == HotspotPolicy::TargetedPortOnly {
            tracing::info!("the consent was taken back: stopping the hotspot");
            self.teardown(&mut st).await;
        }
    }

    async fn remember_previous(&self, st: &mut State, dev: &WifiDevice) {
        st.previous = match self.api.current_profile(dev).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("could not read the active connection: {e}");
                None
            }
        };
    }

    /// Removes the profile in `st.created`, if any. Failures are only logged.
    /// Cancel safe: the record goes only when the profile is gone, so a later call finishes it.
    async fn drop_created(&self, st: &mut State) {
        if let Some(c) = st.created.clone() {
            if let Err(e) = self.api.deactivate(&c.active).await {
                tracing::debug!("deactivate: {e}");
            }
            if let Err(e) = self.api.delete(&c.settings).await {
                tracing::warn!("could not delete profile {}: {e}", c.settings);
            }
            st.created = None;
        }
    }

    async fn bring_up(
        &self,
        st: &mut State,
        dev: &WifiDevice,
        ssid: &str,
        psk: &str,
        band: Band,
        zone: &'static str,
    ) -> io::Result<()> {
        let mut settings = ConnSettings::new(AP_PROFILE, &dev.iface, ssid, psk, Kind::Ap(band))?;
        settings.zone = zone;
        st.touched = true;
        let created = self.api.add_and_activate(dev, None, &settings).await?;
        st.created = Some(created.clone());
        self.api
            .wait_activated(&created.active, ACTIVATE_AP_TIMEOUT)
            .await
    }

    async fn host_inner(&self, st: &mut State, port: u16) -> io::Result<WifiCredentials> {
        let dev = self.api.wifi_device().await?;
        st.dev = Some(dev.clone());

        // The phone only needs the upgrade port: open it in the sharing zone. This comes before
        // anything else is touched, so a refusal leaves the profiles and the connection alone. With
        // the user's consent for this session an open zone is used instead; never without it.
        let zone = match self.api.open_port(port).await {
            Ok(()) => {
                st.open_port = Some(port);
                SHARED_ZONE
            }
            Err(e) => {
                let zone = self.zone_without_port(port, &e)?;
                st.open_zone = true;
                zone
            }
        };
        self.sweep().await;
        self.remember_previous(st, &dev).await;
        let (ssid, password) = (random_ssid(), random_password());
        let band = match self
            .bring_up(st, &dev, &ssid, &password, BAND_5GHZ, zone)
            .await
        {
            Ok(()) => BAND_5GHZ,
            Err(e) => {
                tracing::warn!("5 GHz access point failed ({e}); trying 2.4 GHz");
                self.drop_created(st).await;
                self.bring_up(st, &dev, &ssid, &password, BAND_2GHZ, zone)
                    .await
                    .inspect_err(|e| {
                        tracing::warn!("the hotspot could not start on 2.4 GHz either: {e}");
                    })?;
                BAND_2GHZ
            }
        };
        Ok(WifiCredentials {
            ssid,
            password,
            port,
            frequency_mhz: band.mhz,
            gateway: GATEWAY,
        })
    }

    /// Looks for the AP in the scan results for a few polls.
    async fn poll_ap(&self, dev: &WifiDevice, ssid: &[u8]) -> io::Result<Option<String>> {
        for poll in 0..AP_POLLS {
            if poll > 0 {
                sleep(self.timing.poll_delay).await;
            }
            if let Some(ap) = self.api.find_ap(dev, ssid).await? {
                return Ok(Some(ap));
            }
        }
        Ok(None)
    }

    async fn join_once(
        &self,
        st: &mut State,
        dev: &WifiDevice,
        creds: &WifiCredentials,
        may_be_hidden: bool,
    ) -> io::Result<()> {
        // Fails when a scan is already running; harmless.
        let _ = self.api.request_scan(dev).await;
        let ap = self.poll_ap(dev, creds.ssid.as_bytes()).await?;
        let hidden = ap.is_none() && may_be_hidden;
        if ap.is_none() && !hidden {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the access point was not found",
            ));
        }
        let settings = client_settings(&dev.iface, creds, hidden)?;
        st.touched = true;
        let created = self
            .api
            .add_and_activate(dev, ap.as_deref(), &settings)
            .await?;
        st.created = Some(created.clone());
        self.api
            .wait_activated(&created.active, ACTIVATE_JOIN_TIMEOUT)
            .await
    }

    async fn join_inner(&self, st: &mut State, creds: &WifiCredentials) -> io::Result<Ipv4Addr> {
        let dev = self.api.wifi_device().await?;
        // Validates the SSID and key before anything is touched.
        client_settings(&dev.iface, creds, false)?;
        st.dev = Some(dev.clone());
        self.sweep().await;
        self.remember_previous(st, &dev).await;

        let start = Instant::now();
        let mut last_err = None;
        for attempt in 0..JOIN_ATTEMPTS {
            if attempt > 0 {
                sleep(self.timing.retry_delay).await;
            }
            match self
                .join_once(st, &dev, creds, attempt >= HIDDEN_AFTER)
                .await
            {
                Ok(()) => {
                    last_err = None;
                    break;
                }
                Err(e) => {
                    tracing::debug!("join attempt {} failed: {e}", attempt + 1);
                    self.drop_created(st).await;
                    last_err = Some(e);
                }
            }
            if start.elapsed() >= self.timing.budget {
                break;
            }
        }
        if let Some(e) = last_err {
            return Err(e);
        }

        for poll in 0..IP_POLLS {
            if poll > 0 {
                sleep(self.timing.poll_delay).await;
            }
            if let Some(ip) = self.api.ipv4(&dev).await? {
                return Ok(ip);
            }
        }
        Err(io::Error::other("no IPv4 address on the Wi-Fi interface"))
    }

    async fn join_saved_inner(
        &self,
        st: &mut State,
        ssid: &str,
        psk: Option<&str>,
    ) -> io::Result<()> {
        let dev = self.api.wifi_device().await?;
        let settings = ConnSettings::saved(&dev.iface, ssid, psk)?;
        st.dev = Some(dev.clone());
        self.remember_previous(st, &dev).await;
        st.touched = true;
        let created = self.api.add_and_activate(&dev, None, &settings).await?;
        st.created = Some(created.clone());
        self.api
            .wait_activated(&created.active, ACTIVATE_SAVED_TIMEOUT)
            .await
    }

    /// Joins a network the user chose, as a saved profile that connects by itself next time
    /// (unlike `join`, whose profile is temporary). `psk` is `None` for an open network.
    /// Refuses while an upgrade holds the link. On failure the profile is removed and the previous
    /// connection restored; on success the profile stays. The error never contains the key.
    pub async fn join_saved(&self, ssid: &str, psk: Option<&str>) -> io::Result<()> {
        let mut st = self.begin()?;
        match self.join_saved_inner(&mut st, ssid, psk).await {
            Ok(()) => {
                // Not ours to clean up any more: keep the profile and the connection.
                *st = State::default();
                Ok(())
            }
            Err(e) => {
                self.teardown(&mut st).await;
                Err(e)
            }
        }
    }

    /// Undoes whatever `st` records and resets it. Failures are only logged. Cancel safe: each
    /// record is cleared only after its undo is done, and `busy` last, so when the future is
    /// dropped half-way a later call undoes the rest.
    async fn teardown(&self, st: &mut State) {
        if let Some(port) = st.open_port {
            if let Err(e) = self.api.close_port(port).await {
                tracing::warn!("could not close port {port} in the firewall: {e}");
            }
            st.open_port = None;
        }
        self.drop_created(st).await;
        if let (Some(prev), Some(dev)) = (st.previous.clone(), st.dev.clone()) {
            if (st.touched || self.current_differs(&dev, &prev).await)
                && let Err(e) = self.api.activate(&dev, &prev).await
            {
                tracing::warn!("could not restore the previous connection: {e}");
            }
            st.previous = None;
        }
        *st = State::default();
    }

    /// Whether the device's active connection is not `previous` (unknown counts as no).
    async fn current_differs(&self, dev: &WifiDevice, previous: &str) -> bool {
        match self.api.current_profile(dev).await {
            Ok(current) => current.as_deref() != Some(previous),
            Err(e) => {
                tracing::debug!("could not read the active connection: {e}");
                false
            }
        }
    }
}

impl<A: NmApi> WifiLink for NmLink<A> {
    fn host(&self, port: u16) -> BoxFuture<'_, io::Result<WifiCredentials>> {
        Box::pin(async move {
            let mut st = self.begin()?;
            let result = self.host_inner(&mut st, port).await;
            if result.is_err() {
                self.teardown(&mut st).await;
            }
            result
        })
    }

    fn join<'a>(&'a self, creds: &'a WifiCredentials) -> BoxFuture<'a, io::Result<Ipv4Addr>> {
        Box::pin(async move {
            let mut st = self.begin()?;
            let result = self.join_inner(&mut st, creds).await;
            if result.is_err() {
                self.teardown(&mut st).await;
            }
            result
        })
    }

    fn release(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.shutdown().await;
        })
    }
}

fn client_settings(iface: &str, creds: &WifiCredentials, hidden: bool) -> io::Result<ConnSettings> {
    ConnSettings::new(
        JOIN_PROFILE,
        iface,
        &creds.ssid,
        &creds.password,
        Kind::Client { hidden },
    )
}

/// Looks like a Wi-Fi Direct group name: `DIRECT-xy-KuickShare`.
fn random_ssid() -> String {
    format!("DIRECT-{}-KuickShare", random_alnum(2))
}

fn random_password() -> String {
    random_alnum(PSK_LEN)
}

fn random_alnum(len: usize) -> String {
    rand::rng()
        .sample_iter(Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::{BTreeSet, HashMap},
        sync::{Arc, Mutex as StdMutex},
    };
    use zbus::Variant;

    type Fail = Box<dyn Fn(&str, usize) -> Option<io::Error> + Send + Sync>;

    /// Records every call (settings by their redacted `Debug`) and keeps the
    /// set of profiles that exist. `fail(op, n)` fails the n-th (1-based) call of `op`.
    struct Fake {
        log: Arc<StdMutex<Vec<String>>>,
        counts: StdMutex<HashMap<&'static str, usize>>,
        live: StdMutex<BTreeSet<String>>,
        added: StdMutex<Vec<(Option<String>, ConnSettings)>>,
        fail: Fail,
        stale: Vec<String>,
        previous: Option<String>,
        /// `find_ap` finds the AP from this call on (1-based); 0 = never.
        ap_from: usize,
        ip: Option<Ipv4Addr>,
        /// This operation never returns (the caller drops the future to simulate a timeout).
        hang: StdMutex<Option<&'static str>>,
    }

    impl Fake {
        fn step(&self, op: &'static str, detail: &str) -> io::Result<()> {
            self.log.lock().unwrap().push(format!("{op} {detail}"));
            let n = {
                let mut counts = self.counts.lock().unwrap();
                let n = counts.entry(op).or_default();
                *n += 1;
                *n
            };
            (self.fail)(op, n).map_or(Ok(()), Err)
        }

        /// Never returns when `hang` names this operation.
        async fn maybe_hang(&self, op: &'static str) {
            let hang = *self.hang.lock().unwrap() == Some(op);
            if hang {
                std::future::pending::<()>().await;
            }
        }
    }

    impl NmApi for Fake {
        async fn wifi_device(&self) -> io::Result<WifiDevice> {
            self.step("wifi_device", "")?;
            Ok(WifiDevice {
                path: "/dev/1".into(),
                iface: "wlan0".into(),
            })
        }
        async fn stale_profiles(&self) -> io::Result<Vec<String>> {
            self.step("stale_profiles", "")?;
            Ok(self.stale.clone())
        }
        async fn current_profile(&self, _: &WifiDevice) -> io::Result<Option<String>> {
            self.step("current_profile", "")?;
            Ok(self.previous.clone())
        }
        async fn add_and_activate(
            &self,
            _: &WifiDevice,
            ap: Option<&str>,
            s: &ConnSettings,
        ) -> io::Result<Created> {
            self.step("add_and_activate", &format!("{ap:?} {s:?}"))?;
            let n = self.added.lock().unwrap().len() + 1;
            self.added
                .lock()
                .unwrap()
                .push((ap.map(str::to_owned), s.clone()));
            let created = Created {
                settings: format!("/set/{n}"),
                active: format!("/act/{n}"),
            };
            self.live.lock().unwrap().insert(created.settings.clone());
            Ok(created)
        }
        async fn wait_activated(&self, active: &str, _: Duration) -> io::Result<()> {
            self.step("wait_activated", active)?;
            self.maybe_hang("wait_activated").await;
            Ok(())
        }
        async fn request_scan(&self, _: &WifiDevice) -> io::Result<()> {
            self.step("request_scan", "")
        }
        async fn find_ap(&self, _: &WifiDevice, _: &[u8]) -> io::Result<Option<String>> {
            self.step("find_ap", "")?;
            let n = self.counts.lock().unwrap()["find_ap"];
            Ok((self.ap_from != 0 && n >= self.ap_from).then(|| "/ap/1".to_owned()))
        }
        async fn ipv4(&self, _: &WifiDevice) -> io::Result<Option<Ipv4Addr>> {
            self.step("ipv4", "")?;
            Ok(self.ip)
        }
        async fn deactivate(&self, active: &str) -> io::Result<()> {
            self.step("deactivate", active)?;
            self.maybe_hang("deactivate").await;
            Ok(())
        }
        async fn delete(&self, settings: &str) -> io::Result<()> {
            self.step("delete", settings)?;
            self.maybe_hang("delete").await;
            self.live.lock().unwrap().remove(settings);
            Ok(())
        }
        async fn open_port(&self, port: u16) -> io::Result<()> {
            self.step("open_port", &port.to_string())
        }
        async fn close_port(&self, port: u16) -> io::Result<()> {
            self.step("close_port", &port.to_string())?;
            self.maybe_hang("close_port").await;
            Ok(())
        }
        async fn activate(&self, _: &WifiDevice, settings: &str) -> io::Result<()> {
            self.step("activate", settings)
        }
    }

    struct Builder {
        fail: Fail,
        stale: Vec<String>,
        previous: Option<String>,
        ap_from: usize,
        budget: Duration,
        hang: Option<&'static str>,
    }

    fn fake() -> Builder {
        Builder {
            fail: Box::new(|_, _| None),
            stale: vec![],
            previous: Some("/prev/home".into()),
            ap_from: 1,
            budget: Duration::from_secs(60),
            hang: None,
        }
    }

    impl Builder {
        fn fail(mut self, f: impl Fn(&str, usize) -> bool + Send + Sync + 'static) -> Self {
            self.fail = Box::new(move |op, n| f(op, n).then(|| io::Error::other("boom")));
            self
        }

        fn build(self) -> (NmLink<Fake>, Arc<StdMutex<Vec<String>>>) {
            let log = Arc::new(StdMutex::default());
            let api = Fake {
                log: log.clone(),
                counts: StdMutex::default(),
                live: StdMutex::default(),
                added: StdMutex::default(),
                fail: self.fail,
                stale: self.stale,
                previous: self.previous,
                ap_from: self.ap_from,
                ip: Some(Ipv4Addr::new(192, 168, 49, 7)),
                hang: StdMutex::new(self.hang),
            };
            let timing = Timing {
                retry_delay: Duration::ZERO,
                poll_delay: Duration::ZERO,
                budget: self.budget,
            };
            (NmLink::with_parts(api, timing), log)
        }
    }

    impl NmLink<Fake> {
        fn with_test_notices(mut self, tx: UnboundedSender<LinkNotice>) -> Self {
            self.notices = Some(tx);
            self
        }
    }

    fn calls(log: &Arc<StdMutex<Vec<String>>>) -> Vec<String> {
        log.lock().unwrap().clone()
    }

    /// Calls recorded so far, not counting the read-only stale-profile listing.
    fn quiet_len(log: &Arc<StdMutex<Vec<String>>>) -> usize {
        calls(log)
            .iter()
            .filter(|c| !c.starts_with("stale_profiles"))
            .count()
    }

    fn count(log: &Arc<StdMutex<Vec<String>>>, prefix: &str) -> usize {
        calls(log).iter().filter(|c| c.starts_with(prefix)).count()
    }

    fn position(log: &Arc<StdMutex<Vec<String>>>, prefix: &str) -> usize {
        calls(log)
            .iter()
            .position(|c| c.starts_with(prefix))
            .unwrap_or_else(|| panic!("no call {prefix}"))
    }

    fn live(link: &NmLink<Fake>) -> usize {
        link.api.live.lock().unwrap().len()
    }

    fn creds() -> WifiCredentials {
        WifiCredentials {
            ssid: "DIRECT-ab-Pixel; $(x) `y`".into(),
            password: "pa ss'w\"d-from-the-phone".into(),
            port: 1234,
            frequency_mhz: 5180,
            gateway: GATEWAY,
        }
    }

    async fn settled() {
        sleep(Duration::from_millis(20)).await;
    }

    /// No secret in the log, in the error or in the settings' `Debug`.
    fn assert_no_secret(link: &NmLink<Fake>, log: &Arc<StdMutex<Vec<String>>>, secret: &str) {
        assert!(!calls(log).iter().any(|c| c.contains(secret)));
        for (_, s) in link.api.added.lock().unwrap().iter() {
            assert!(!format!("{s:?}").contains(secret));
        }
    }

    #[test]
    fn random_credentials_have_the_expected_shape() {
        for _ in 0..50 {
            let ssid = random_ssid();
            assert!(ssid.len() <= MAX_SSID);
            assert!(ssid.starts_with("DIRECT-") && ssid.ends_with("-KuickShare"));
            assert!(ssid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
            let pw = random_password();
            assert!(pw.len() >= 24 && (MIN_PSK..=MAX_PSK).contains(&pw.len()));
            assert!(pw.bytes().all(|b| b.is_ascii_alphanumeric()));
        }
        assert_ne!(random_password(), random_password());
    }

    #[test]
    fn invalid_ssid_or_key_is_rejected_without_quoting_them() {
        let ok = |ssid: &str, psk: &str| {
            ConnSettings::new("x", "wlan0", ssid, psk, Kind::Client { hidden: false })
        };
        assert!(ok(&"s".repeat(32), &"k".repeat(8)).is_ok());
        assert!(ok(&"s".repeat(32), &"k".repeat(63)).is_ok());
        for (ssid, psk) in [
            ("s".repeat(33), "k".repeat(8)),
            (String::new(), "k".repeat(8)),
            ("s".into(), "k".repeat(7)),
            ("s".into(), "secretsecretsecret".repeat(4)),
        ] {
            let e = ok(&ssid, &psk).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
            assert!(!e.to_string().contains(&psk));
        }
    }

    #[test]
    fn debug_hides_the_key() {
        let s = ConnSettings::new(
            AP_PROFILE,
            "wlan0",
            "ssid",
            "supersecretkey",
            Kind::Ap(BAND_5GHZ),
        )
        .unwrap();
        assert!(!format!("{s:?}").contains("supersecretkey"));
        assert!(format!("{s:?}").contains("redacted"));
    }

    #[test]
    fn ap_settings_dictionary() {
        let s = ConnSettings::new(
            AP_PROFILE,
            "wlan0",
            "DIRECT-ab-KuickShare",
            "Passw0rdPassw0rd",
            Kind::Ap(BAND_5GHZ),
        )
        .unwrap();
        let d = wire::dict(&s);
        let get = |sec: &str, key: &str| d[sec][key].clone();
        assert_eq!(get("connection", "id"), Variant::from("kuickshare-ap"));
        assert_eq!(get("connection", "type"), Variant::from("802-11-wireless"));
        assert_eq!(get("connection", "autoconnect"), Variant::from(false));
        assert!(!d["connection"].contains_key("zone"));
        assert_eq!(get("connection", "interface-name"), Variant::from("wlan0"));
        assert_eq!(
            get("802-11-wireless", "ssid"),
            Variant::from(b"DIRECT-ab-KuickShare".to_vec())
        );
        assert_eq!(get("802-11-wireless", "mode"), Variant::from("ap"));
        assert_eq!(get("802-11-wireless", "band"), Variant::from("a"));
        assert_eq!(get("802-11-wireless", "channel"), Variant::from(36u32));
        let sec = "802-11-wireless-security";
        assert_eq!(get(sec, "key-mgmt"), Variant::from("wpa-psk"));
        assert_eq!(get(sec, "psk"), Variant::from("Passw0rdPassw0rd"));
        assert_eq!(get(sec, "proto"), Variant::from(vec!["rsn".to_owned()]));
        assert_eq!(get(sec, "pairwise"), Variant::from(vec!["ccmp".to_owned()]));
        assert_eq!(get(sec, "group"), Variant::from(vec!["ccmp".to_owned()]));
        assert_eq!(get("ipv4", "method"), Variant::from("shared"));
        assert!(d["ipv4"].contains_key("address-data"));
        assert_eq!(get("ipv6", "method"), Variant::from("disabled"));
    }

    #[test]
    fn client_settings_dictionary() {
        let c = creds();
        let hidden = |h| wire::dict(&client_settings("wlan0", &c, h).unwrap());
        let d = hidden(false);
        assert_eq!(d["connection"]["id"], Variant::from("kuickshare-join"));
        assert_eq!(d["connection"]["autoconnect"], Variant::from(false));
        assert!(!d["connection"].contains_key("zone"));
        assert_eq!(
            d["802-11-wireless"]["ssid"],
            Variant::from(c.ssid.as_bytes().to_vec())
        );
        assert_eq!(d["802-11-wireless"]["hidden"], Variant::from(false));
        assert_eq!(
            d["802-11-wireless-security"]["psk"],
            Variant::from(c.password.clone())
        );
        assert_eq!(d["ipv4"]["method"], Variant::from("auto"));
        assert_eq!(d["ipv4"]["never-default"], Variant::from(true));
        assert_eq!(d["ipv4"]["ignore-auto-dns"], Variant::from(true));
        assert_eq!(d["ipv6"]["method"], Variant::from("disabled"));
        assert_eq!(
            hidden(true)["802-11-wireless"]["hidden"],
            Variant::from(true)
        );
    }

    #[test]
    fn network_events_are_the_loss_of_the_last_connection_and_the_return_of_one() {
        use NetworkEvent::{Gained, Lost};
        assert_eq!(network_event(Some(70), 20), Some(Lost));
        assert_eq!(network_event(None, 20), Some(Lost));
        assert_eq!(network_event(Some(40), 10), Some(Lost));
        assert_eq!(network_event(Some(20), 70), Some(Gained));
        assert_eq!(network_event(Some(40), 50), Some(Gained));
        // Nothing new: still down, still up, on the way, or the first state seen being "up".
        assert_eq!(network_event(Some(20), 20), None);
        assert_eq!(network_event(Some(20), 40), None);
        assert_eq!(network_event(Some(70), 50), None);
        assert_eq!(network_event(None, 70), None);
    }

    #[tokio::test]
    async fn host_creates_a_shared_ap_with_a_fresh_password() {
        let (link, log) = fake().build();
        let c = link.host(4000).await.unwrap();
        assert_eq!((c.port, c.frequency_mhz, c.gateway), (4000, 5180, GATEWAY));
        assert!(c.password.len() >= 24);
        assert!(position(&log, "stale_profiles") < position(&log, "add_and_activate"));
        let added = link.api.added.lock().unwrap().clone();
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].0, None);
        assert_eq!(added[0].1.psk.0, c.password);
        assert_eq!(added[0].1.ssid, c.ssid.as_bytes());
        assert!(matches!(added[0].1.kind, Kind::Ap(b) if b.mhz == 5180 && b.channel == 36));
        assert_no_secret(&link, &log, &c.password);
    }

    #[tokio::test]
    async fn host_opens_only_its_port_in_the_sharing_zone_and_release_closes_it() {
        let (link, log) = fake().build();
        link.host(4000).await.unwrap();
        assert!(position(&log, "open_port 4000") < position(&log, "add_and_activate"));
        assert_eq!(link.api.added.lock().unwrap()[0].1.zone, SHARED_ZONE);
        assert_eq!(count(&log, "close_port"), 0);
        link.release().await;
        assert_eq!(count(&log, "close_port 4000"), 1);
        link.release().await;
        assert_eq!(count(&log, "close_port"), 1);
    }

    /// The firewall refused (or is not there): `host` must fail without touching
    /// the AP profile, the previous connection or the zone.
    async fn assert_host_refused(link: &NmLink<Fake>, log: &Arc<StdMutex<Vec<String>>>) {
        let err = link.host(4000).await.expect_err("host must refuse");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert!(
            link.api.added.lock().unwrap().is_empty(),
            "no AP profile may be added, in any zone"
        );
        assert!(
            link.api
                .added
                .lock()
                .unwrap()
                .iter()
                .all(|(_, s)| s.zone != OPEN_ZONE)
        );
        assert_eq!(count(log, "add_and_activate"), 0);
        assert_eq!(count(log, "activate"), 0);
        assert_eq!(count(log, "deactivate"), 0);
        assert_eq!(live(link), 0);
    }

    #[tokio::test]
    async fn host_refuses_when_firewall_denies_and_touches_nothing() {
        let (link, log) = fake().fail(|op, _| op == "open_port").build();
        assert_host_refused(&link, &log).await;
    }

    #[tokio::test]
    async fn firewalld_absent_refuses_without_consent() {
        let mut b = fake();
        b.fail = Box::new(|op, _| {
            (op == "open_port")
                .then(|| io::Error::new(io::ErrorKind::NotFound, "firewalld is not running"))
        });
        let (link, log) = b.build();
        assert_host_refused(&link, &log).await;
    }

    #[tokio::test]
    async fn join_never_opens_a_port() {
        let (link, log) = fake().build();
        link.join(&creds()).await.unwrap();
        link.release().await;
        assert_eq!(count(&log, "open_port"), 0);
        assert_eq!(count(&log, "close_port"), 0);
        assert!(
            link.api
                .added
                .lock()
                .unwrap()
                .iter()
                .all(|(_, s)| s.zone != OPEN_ZONE)
        );
    }

    #[tokio::test]
    async fn the_refusal_is_reported_with_a_stable_reason() {
        let denied = || fake().fail(|op, _| op == "open_port");
        let absent = || {
            let mut b = fake();
            b.fail = Box::new(|op, _| {
                (op == "open_port").then(|| io::Error::new(io::ErrorKind::NotFound, "gone"))
            });
            b
        };
        for (b, code) in [
            (denied(), "firewall_denied"),
            (absent(), "firewall_missing"),
        ] {
            let (link, _) = b.build();
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let link = link.with_test_notices(tx);
            assert!(link.host(4000).await.is_err());
            let Ok(LinkNotice::WifiUpgradeBlocked(reason)) = rx.try_recv() else {
                panic!("no notice for {code}");
            };
            assert_eq!(reason.code(), code);
            assert!(rx.try_recv().is_err(), "one notice per refusal");
        }
    }

    #[tokio::test]
    async fn consent_allows_open_zone_when_denied() {
        let (link, log) = fake().fail(|op, _| op == "open_port").build();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let link = link.with_test_notices(tx);
        link.set_hotspot_policy(HotspotPolicy::AllowOpenZone);
        link.host(4000).await.unwrap();
        assert_eq!(link.api.added.lock().unwrap()[0].1.zone, OPEN_ZONE);
        assert!(rx.try_recv().is_err(), "nothing was blocked");
        link.release().await;
        // The port was never opened, so there is none to close.
        assert_eq!(count(&log, "close_port"), 0);
    }

    #[tokio::test]
    async fn no_firewalld_with_consent_leaves_zone_empty() {
        let mut b = fake();
        b.fail = Box::new(|op, _| {
            (op == "open_port")
                .then(|| io::Error::new(io::ErrorKind::NotFound, "firewalld is not running"))
        });
        let (link, _) = b.build();
        link.set_hotspot_policy(HotspotPolicy::AllowOpenZone);
        link.host(4000).await.unwrap();
        let added = link.api.added.lock().unwrap().clone();
        assert_eq!(added[0].1.zone, "");
        assert!(added.iter().all(|(_, s)| s.zone != OPEN_ZONE));
    }

    #[tokio::test]
    async fn consent_does_not_change_the_targeted_port_when_it_can_be_opened() {
        let (link, _) = fake().build();
        link.set_hotspot_policy(HotspotPolicy::AllowOpenZone);
        link.host(4000).await.unwrap();
        assert_eq!(link.api.added.lock().unwrap()[0].1.zone, SHARED_ZONE);
    }

    #[tokio::test]
    async fn consent_taken_back_refuses_the_next_hotspot() {
        // The consent counts when a hotspot starts: once taken back, the next one is refused.
        let (link, _) = fake().fail(|op, _| op == "open_port").build();
        assert_eq!(link.hotspot_policy(), HotspotPolicy::TargetedPortOnly);
        link.set_hotspot_policy(HotspotPolicy::AllowOpenZone);
        link.host(4000).await.unwrap();
        link.release().await;
        link.set_hotspot_policy(HotspotPolicy::TargetedPortOnly);
        let e = link.host(4000).await.unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn dropped_host_is_cleaned_by_release() {
        let mut b = fake();
        b.hang = Some("wait_activated");
        let (link, log) = b.build();
        // The host future is dropped half-way (the upgrade timed out or was cancelled).
        let timed_out = tokio::time::timeout(Duration::from_millis(50), link.host(4000)).await;
        assert!(timed_out.is_err(), "the host call was meant to hang");
        assert_eq!(count(&log, "open_port 4000"), 1);
        assert_eq!(live(&link), 1);
        assert_eq!(count(&log, "close_port"), 0);
        // The link still looks busy until it is released.
        assert_eq!(
            link.host(5).await.unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );

        link.release().await;
        assert_eq!(count(&log, "close_port 4000"), 1);
        assert_eq!(live(&link), 0);
        assert_eq!(count(&log, "activate /prev/home"), 1);
        // Free again, and a second release does nothing.
        let n = quiet_len(&log);
        link.release().await;
        assert_eq!(quiet_len(&log), n);
    }

    #[tokio::test]
    async fn shutdown_closes_port_and_restores_previous() {
        let (link, log) = fake().build();
        link.host(4000).await.unwrap();
        assert_eq!(count(&log, "close_port"), 0);
        // The shutdown goes through the very link the transfer used, not through a new one.
        link.shutdown().await;
        assert_eq!(count(&log, "close_port 4000"), 1);
        assert_eq!(count(&log, "deactivate /act/1"), 1);
        assert_eq!(count(&log, "delete /set/1"), 1);
        assert_eq!(count(&log, "activate /prev/home"), 1);
        assert_eq!(live(&link), 0);
        // Idempotent.
        let n = quiet_len(&log);
        link.shutdown().await;
        assert_eq!(quiet_len(&log), n);
    }

    #[tokio::test]
    async fn shutdown_sweeps_leftover_profiles_even_when_nothing_is_up() {
        let mut b = fake();
        b.stale = vec!["/leaked/1".into()];
        let (link, log) = b.build();
        link.shutdown().await;
        assert_eq!(count(&log, "delete /leaked/1"), 1);
        assert_eq!(count(&log, "close_port"), 0);
        assert_eq!(count(&log, "activate"), 0);
    }

    #[tokio::test]
    async fn dropping_shutdown_while_host_holds_the_link_loses_nothing() {
        let mut b = fake();
        b.hang = Some("wait_activated");
        let (link, log) = b.build();
        let link = Arc::new(link);
        let host = tokio::spawn({
            let link = link.clone();
            async move { link.host(4000).await }
        });
        settled().await;
        // The shutdown waits for the link and gives up (the daemon's bound runs out).
        let waited = tokio::time::timeout(Duration::from_millis(50), link.shutdown()).await;
        assert!(waited.is_err());
        assert_eq!(count(&log, "close_port"), 0);
        // Then the host goes too (its future is dropped).
        host.abort();
        let _ = host.await;
        assert_eq!(live(&link), 1);

        link.release().await;
        assert_eq!(count(&log, "close_port 4000"), 1);
        assert_eq!(live(&link), 0);
        assert_eq!(count(&log, "activate /prev/home"), 1);
    }

    #[tokio::test]
    async fn dropping_the_teardown_half_way_lets_release_finish_it() {
        let (link, log) = fake().build();
        link.host(4000).await.unwrap();
        // The profile cannot be deleted in time: the port is closed and the profile is stopped.
        *link.api.hang.lock().unwrap() = Some("delete");
        let waited = tokio::time::timeout(Duration::from_millis(50), link.shutdown()).await;
        assert!(waited.is_err());
        assert_eq!(count(&log, "close_port 4000"), 1);
        assert_eq!(count(&log, "deactivate /act/1"), 1);
        assert_eq!(count(&log, "activate /prev/home"), 0);
        assert_eq!(live(&link), 1);

        *link.api.hang.lock().unwrap() = None;
        link.release().await;
        // The port is not closed twice; the rest is done.
        assert_eq!(count(&log, "close_port"), 1);
        assert_eq!(count(&log, "delete /set/1"), 2);
        assert_eq!(live(&link), 0);
        assert_eq!(count(&log, "activate /prev/home"), 1);
    }

    #[tokio::test]
    async fn dropping_the_teardown_while_closing_the_port_keeps_the_port_for_release() {
        let (link, log) = fake().build();
        link.host(4000).await.unwrap();
        *link.api.hang.lock().unwrap() = Some("close_port");
        let waited = tokio::time::timeout(Duration::from_millis(50), link.shutdown()).await;
        assert!(waited.is_err());
        *link.api.hang.lock().unwrap() = None;
        link.release().await;
        assert_eq!(count(&log, "close_port 4000"), 2);
        assert_eq!(live(&link), 0);
        assert_eq!(count(&log, "activate /prev/home"), 1);
    }

    #[tokio::test]
    async fn taking_back_the_consent_stops_a_hotspot_in_an_open_zone() {
        let (link, log) = fake().fail(|op, _| op == "open_port").build();
        link.set_hotspot_policy(HotspotPolicy::AllowOpenZone);
        link.host(4000).await.unwrap();
        assert_eq!(live(&link), 1);

        // Still allowed: nothing happens.
        link.drop_open_zone_hotspot().await;
        assert_eq!(live(&link), 1);

        link.set_hotspot_policy(HotspotPolicy::TargetedPortOnly);
        link.drop_open_zone_hotspot().await;
        assert_eq!(live(&link), 0);
        assert_eq!(count(&log, "deactivate /act/1"), 1);
        assert_eq!(count(&log, "activate /prev/home"), 1);
        // The link is free for the next upgrade, which is now refused.
        let e = link.host(4000).await.unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn taking_back_the_consent_leaves_a_targeted_hotspot_alone() {
        let (link, _) = fake().build();
        link.set_hotspot_policy(HotspotPolicy::AllowOpenZone);
        link.host(4000).await.unwrap();
        link.set_hotspot_policy(HotspotPolicy::TargetedPortOnly);
        link.drop_open_zone_hotspot().await;
        assert_eq!(live(&link), 1);
    }

    #[tokio::test]
    async fn a_consent_taken_back_while_the_port_is_refused_is_honoured() {
        // The policy is read at the zone choice, after the calls that precede it.
        let (link, _) = fake().fail(|op, _| op == "open_port").build();
        link.set_hotspot_policy(HotspotPolicy::AllowOpenZone);
        link.set_hotspot_policy(HotspotPolicy::TargetedPortOnly);
        let e = link.host(4000).await.unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(live(&link), 0);
    }

    #[test]
    fn firewall_errors_are_told_apart() {
        use zbus::fdo::Error as Fdo;
        let fdo = |e: Fdo| zbus::Error::FDO(Box::new(e));
        let missing = [
            fdo(Fdo::ServiceUnknown("x".into())),
            fdo(Fdo::NameHasNoOwner("x".into())),
            fdo(Fdo::Disconnected("x".into())),
            fdo(Fdo::NoServer("x".into())),
            fdo(Fdo::SpawnExecFailed("x".into())),
            fdo(Fdo::SpawnFailed("x".into())),
            zbus::Error::InputOutput(Arc::new(io::Error::other("no socket"))),
            zbus::Error::Address("bad".into()),
        ];
        for e in missing {
            assert_eq!(classify_firewall_error(&e), FirewallFault::Missing, "{e:?}");
            assert_eq!(firewall_err(e).kind(), io::ErrorKind::NotFound);
        }
        let denied = [
            fdo(Fdo::AccessDenied("x".into())),
            fdo(Fdo::AuthFailed("x".into())),
        ];
        for e in denied {
            assert_eq!(classify_firewall_error(&e), FirewallFault::Denied, "{e:?}");
            assert_ne!(firewall_err(e).kind(), io::ErrorKind::NotFound);
        }
        // Anything else is a refusal too, never "missing".
        for e in [
            fdo(Fdo::Failed("x".into())),
            zbus::Error::Failure("x".into()),
        ] {
            assert_eq!(classify_firewall_error(&e), FirewallFault::Other, "{e:?}");
            assert_ne!(firewall_err(e).kind(), io::ErrorKind::NotFound);
        }
    }

    #[test]
    fn a_firewalld_refusal_by_polkit_is_a_denial() {
        let call = zbus::Message::method_call("/org/fedoraproject/FirewallD1", "addPort")
            .build(&())
            .unwrap();
        let name = |n: &str| zbus::names::OwnedErrorName::try_from(n).unwrap();
        let by_message = zbus::Error::MethodError(
            name("org.fedoraproject.FirewallD1.Exception"),
            Some("NOT_AUTHORIZED: addPort()".into()),
            call.clone(),
        );
        assert_eq!(classify_firewall_error(&by_message), FirewallFault::Denied);
        let by_name = zbus::Error::MethodError(
            name("org.freedesktop.PolicyKit1.Error.InteractiveAuthorizationRequired"),
            None,
            call.clone(),
        );
        assert_eq!(classify_firewall_error(&by_name), FirewallFault::Denied);
        let other = zbus::Error::MethodError(
            name("org.fedoraproject.FirewallD1.Exception"),
            Some("INVALID_ZONE: nm-shared".into()),
            call,
        );
        assert_eq!(classify_firewall_error(&other), FirewallFault::Other);
    }

    #[tokio::test]
    async fn release_removes_everything_restores_previous_and_is_idempotent() {
        let (link, log) = fake().build();
        link.host(1).await.unwrap();
        link.release().await;
        settled().await;
        assert_eq!(live(&link), 0);
        assert_eq!(count(&log, "deactivate /act/1"), 1);
        assert_eq!(count(&log, "delete /set/1"), 1);
        assert_eq!(count(&log, "activate /prev/home"), 1);
        assert!(position(&log, "delete /set/1") < position(&log, "activate /prev/home"));
        let n = quiet_len(&log);
        link.release().await;
        assert_eq!(quiet_len(&log), n);
        // The link is free again.
        link.host(1).await.unwrap();
    }

    #[tokio::test]
    async fn host_falls_back_to_2ghz_and_drops_the_5ghz_profile() {
        let (link, log) = fake()
            .fail(|op, n| op == "wait_activated" && n == 1)
            .build();
        let c = link.host(1).await.unwrap();
        assert_eq!(c.frequency_mhz, 2437);
        let added = link.api.added.lock().unwrap().clone();
        assert_eq!(added.len(), 2);
        assert!(matches!(added[0].1.kind, Kind::Ap(b) if b.nm_name == "a"));
        assert!(matches!(added[1].1.kind, Kind::Ap(b) if b.nm_name == "bg" && b.channel == 6));
        assert_eq!(count(&log, "delete /set/1"), 1);
        assert_eq!(live(&link), 1);
    }

    #[tokio::test]
    async fn add_failure_also_falls_back() {
        let (link, _) = fake()
            .fail(|op, n| op == "add_and_activate" && n == 1)
            .build();
        assert_eq!(link.host(1).await.unwrap().frequency_mhz, 2437);
    }

    #[tokio::test]
    async fn host_cleans_up_after_every_failure_point() {
        for (op, from) in [
            ("wifi_device", 1),
            ("add_and_activate", 1),
            ("wait_activated", 1),
        ] {
            let (link, log) = fake().fail(move |o, n| o == op && n >= from).build();
            let e = link.host(1).await.unwrap_err();
            assert_eq!(e.to_string(), "boom", "{op}");
            settled().await;
            assert_eq!(live(&link), 0, "{op}");
            assert_eq!(count(&log, "close_port"), count(&log, "open_port"), "{op}");
            let wifi_touched = count(&log, "add_and_activate") > 0;
            assert_eq!(count(&log, "activate /prev/home"), wifi_touched as usize);
            let n = quiet_len(&log);
            link.release().await;
            assert_eq!(quiet_len(&log), n, "{op}");
        }
    }

    #[tokio::test]
    async fn only_the_second_attempt_failing_cleans_up_too() {
        let (link, _) = fake()
            .fail(|op, n| {
                (op == "wait_activated" && n >= 1) || (op == "add_and_activate" && n == 2)
            })
            .build();
        assert!(link.host(1).await.is_err());
        assert_eq!(live(&link), 0);
    }

    #[tokio::test]
    async fn lookups_that_fail_do_not_stop_hosting() {
        let (link, _) = fake()
            .fail(|op, _| op == "stale_profiles" || op == "current_profile")
            .build();
        link.host(1).await.unwrap();
        link.release().await;
        assert_eq!(live(&link), 0);
    }

    #[tokio::test]
    async fn a_second_upgrade_is_refused_and_does_not_tear_down_the_first() {
        let (link, log) = fake().build();
        link.host(1).await.unwrap();
        let n = quiet_len(&log);
        let e = link.host(2).await.unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
        let e = link.join(&creds()).await.unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(quiet_len(&log), n);
        assert_eq!(live(&link), 1);
        link.release().await;
        assert_eq!(live(&link), 0);
    }

    #[tokio::test]
    async fn stale_profiles_are_swept_before_hosting_and_joining() {
        let stale = || vec!["/old/1".to_owned(), "/old/2".to_owned()];
        let mut b = fake();
        b.stale = stale();
        let (link, log) = b.build();
        link.host(1).await.unwrap();
        assert_eq!(count(&log, "delete /old/"), 2);
        assert!(position(&log, "delete /old/1") < position(&log, "add_and_activate"));
        link.release().await;

        let mut b = fake();
        b.stale = stale();
        let (link, log) = b.build();
        link.join(&creds()).await.unwrap();
        assert_eq!(count(&log, "delete /old/"), 2);
        assert!(position(&log, "delete /old/2") < position(&log, "add_and_activate"));
    }

    #[tokio::test]
    async fn cleanup_stale_sweeps_and_tears_down() {
        let mut b = fake();
        b.stale = vec!["/old/1".into()];
        let (link, log) = b.build();
        link.host(1).await.unwrap();
        link.shutdown().await;
        assert_eq!(count(&log, "delete /old/1"), 2); // sweep at host + at cleanup
        assert_eq!(live(&link), 0);
        link.host(1).await.unwrap(); // free again
    }

    #[tokio::test]
    async fn join_pins_the_ap_returns_ip_and_restores_previous() {
        let (link, log) = fake().build();
        let c = creds();
        assert_eq!(link.join(&c).await.unwrap(), Ipv4Addr::new(192, 168, 49, 7));
        let added = link.api.added.lock().unwrap().clone();
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].0.as_deref(), Some("/ap/1"));
        assert_eq!(added[0].1.psk.0, c.password);
        assert!(matches!(added[0].1.kind, Kind::Client { hidden: false }));
        assert_no_secret(&link, &log, &c.password);

        link.release().await;
        assert_eq!(live(&link), 0);
        assert_eq!(count(&log, "activate /prev/home"), 1);
        let n = quiet_len(&log);
        link.release().await;
        assert_eq!(quiet_len(&log), n);
    }

    #[tokio::test]
    async fn join_waits_for_the_ap_to_show_up() {
        let mut b = fake();
        b.ap_from = 4;
        let (link, log) = b.build();
        link.join(&creds()).await.unwrap();
        assert_eq!(count(&log, "find_ap"), 4);
        assert_eq!(count(&log, "request_scan"), 1);
        assert_eq!(count(&log, "add_and_activate"), 1);
    }

    #[tokio::test]
    async fn join_retries_and_cleans_each_failed_attempt() {
        let (link, log) = fake()
            .fail(|op, n| op == "wait_activated" && n <= 2)
            .build();
        link.join(&creds()).await.unwrap();
        assert_eq!(count(&log, "add_and_activate"), 3);
        assert_eq!(count(&log, "delete"), 2);
        assert_eq!(live(&link), 1);
        // The third attempt is the first allowed to be hidden, but the AP is visible.
        let added = link.api.added.lock().unwrap().clone();
        assert!(
            added
                .iter()
                .all(|(ap, s)| ap.is_some() && matches!(s.kind, Kind::Client { hidden: false }))
        );
        link.release().await;
        assert_eq!(live(&link), 0);
    }

    #[tokio::test]
    async fn join_tries_hidden_when_the_ap_is_never_seen_then_gives_up() {
        let mut b = fake().fail(|op, _| op == "wait_activated");
        b.ap_from = 0;
        let (link, log) = b.build();
        let c = creds();
        let e = link.join(&c).await.unwrap_err();
        assert_eq!(e.to_string(), "boom");
        let added = link.api.added.lock().unwrap().clone();
        assert_eq!(added.len(), (JOIN_ATTEMPTS - HIDDEN_AFTER) as usize);
        assert!(
            added
                .iter()
                .all(|(ap, s)| ap.is_none() && matches!(s.kind, Kind::Client { hidden: true }))
        );
        assert_eq!(count(&log, "request_scan"), JOIN_ATTEMPTS as usize);
        assert_eq!(live(&link), 0);
        assert_eq!(count(&log, "activate /prev/home"), 1);
        assert!(!e.to_string().contains(&c.password));
        assert_no_secret(&link, &log, &c.password);
    }

    #[tokio::test]
    async fn join_never_seen_is_not_found_before_the_hidden_attempts() {
        let mut b = fake();
        b.ap_from = 0;
        b.budget = Duration::ZERO;
        let (link, log) = b.build();
        let e = link.join(&creds()).await.unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert_eq!(count(&log, "request_scan"), 1); // budget spent after one attempt
        assert_eq!(count(&log, "add_and_activate"), 0);
        assert_eq!(count(&log, "activate /prev/home"), 0); // nothing was touched
    }

    #[tokio::test]
    async fn join_cleans_up_after_every_failure_point() {
        for op in [
            "wifi_device",
            "find_ap",
            "add_and_activate",
            "wait_activated",
            "ipv4",
        ] {
            let (link, log) = fake().fail(move |o, _| o == op).build();
            assert!(link.join(&creds()).await.is_err(), "{op}");
            assert_eq!(live(&link), 0, "{op}");
            let n = quiet_len(&log);
            link.release().await;
            assert_eq!(quiet_len(&log), n, "{op}");
            // Free again.
            assert!(link.api.live.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn join_without_ip_fails_and_cleans_up() {
        let (mut link, _) = fake().build();
        link.api.ip = None;
        assert!(link.join(&creds()).await.is_err());
        assert_eq!(live(&link), 0);
    }

    #[tokio::test]
    async fn join_rejects_bad_credentials_before_touching_anything() {
        let (link, log) = fake().build();
        let mut c = creds();
        c.ssid = "s".repeat(33);
        let e = link.join(&c).await.unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        for touching in ["delete", "add_and_activate", "activate", "deactivate"] {
            assert_eq!(count(&log, touching), 0, "{touching}");
        }
        let mut c = creds();
        c.password = "short".into();
        assert_eq!(
            link.join(&c).await.unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[tokio::test]
    async fn untouched_wifi_is_not_reactivated_unless_the_connection_changed() {
        let mut b = fake();
        b.ap_from = 0;
        b.budget = Duration::ZERO;
        let (link, log) = b.build();
        assert!(link.join(&creds()).await.is_err());
        assert_eq!(count(&log, "activate /prev/home"), 0);

        // The device's active connection is no longer the one remembered.
        let (mut link, log) = fake().build();
        link.api.previous = Some("/prev/home".into());
        let mut st = link.state.lock().await;
        st.dev = Some(link.api.wifi_device().await.unwrap());
        st.previous = Some("/prev/other".into());
        link.teardown(&mut st).await;
        drop(st);
        assert_eq!(count(&log, "activate /prev/other"), 1);
    }

    #[tokio::test]
    async fn release_sweeps_a_profile_whose_creation_was_interrupted() {
        let mut b = fake();
        b.stale = vec!["/leaked/1".into()];
        let (link, log) = b.build();
        link.release().await;
        assert_eq!(count(&log, "delete /leaked/1"), 1);
    }

    #[tokio::test]
    async fn an_unreadable_profile_list_is_not_fatal_for_release() {
        let (link, _) = fake().fail(|op, _| op == "stale_profiles").build();
        link.release().await;
    }

    #[tokio::test]
    async fn no_previous_connection_means_nothing_to_restore() {
        let mut b = fake();
        b.previous = None;
        let (link, log) = b.build();
        link.host(1).await.unwrap();
        link.release().await;
        assert_eq!(count(&log, "activate"), 0);
    }

    #[test]
    fn saved_settings_autoconnect_with_normal_routing_and_are_kept_on_disk() {
        let s = ConnSettings::saved("wlan0", "Home", Some("Passw0rdPassw0rd")).unwrap();
        let d = wire::dict(&s);
        assert_eq!(d["connection"]["id"], Variant::from("Home"));
        assert_eq!(d["connection"]["autoconnect"], Variant::from(true));
        assert_eq!(
            d["802-11-wireless"]["mode"],
            Variant::from("infrastructure")
        );
        let sec = "802-11-wireless-security";
        assert_eq!(d[sec]["key-mgmt"], Variant::from("wpa-psk"));
        assert_eq!(d[sec]["psk"], Variant::from("Passw0rdPassw0rd"));
        assert_eq!(d["ipv4"]["method"], Variant::from("auto"));
        assert!(!d["ipv4"].contains_key("never-default"));
        assert!(!d["ipv4"].contains_key("ignore-auto-dns"));
        assert_eq!(persist_mode(s.kind), "disk");
        assert_eq!(persist_mode(Kind::Client { hidden: false }), "memory");
        assert_eq!(persist_mode(Kind::Ap(BAND_2GHZ)), "memory");
        assert!(!format!("{s:?}").contains("Passw0rdPassw0rd"));
    }

    #[test]
    fn saved_open_network_has_no_security_section() {
        let s = ConnSettings::saved("wlan0", "Cafe", None).unwrap();
        let d = wire::dict(&s);
        assert!(!d.contains_key("802-11-wireless-security"));
        assert_eq!(d["connection"]["autoconnect"], Variant::from(true));
        assert!(ConnSettings::saved("wlan0", "", None).is_err());
        for psk in [None, Some("Passw0rdPassw0rd")] {
            let e = ConnSettings::saved("wlan0", "kuickshare-ap", psk).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
            assert!(!e.to_string().contains("Passw0rd"));
        }
        // A 64-hex PSK does not fit the 8..=63 passphrase rule and is refused.
        assert!(ConnSettings::saved("wlan0", "Home", Some(&"a".repeat(64))).is_err());
        assert!(ConnSettings::saved("wlan0", "Home", Some("short")).is_err());
    }

    #[tokio::test]
    async fn join_saved_keeps_the_profile_and_frees_the_link() {
        let (link, log) = fake().build();
        link.join_saved("Home", Some("Passw0rdPassw0rd"))
            .await
            .unwrap();
        assert_eq!(count(&log, "add_and_activate"), 1);
        assert_eq!(count(&log, "delete"), 0);
        assert_eq!(count(&log, "deactivate"), 0);
        assert_eq!(count(&log, "activate /prev"), 0);
        assert_eq!(count(&log, "request_scan"), 0);
        assert_eq!(live(&link), 1);
        assert_no_secret(&link, &log, "Passw0rdPassw0rd");
        // The link is free again.
        link.join_saved("Cafe", None).await.unwrap();
        assert_eq!(live(&link), 2);
    }

    #[tokio::test]
    async fn join_saved_failure_removes_the_profile_restores_the_previous_one_and_hides_the_key() {
        let (link, log) = fake().fail(|op, _| op == "wait_activated").build();
        let e = link
            .join_saved("Home", Some("Passw0rdPassw0rd"))
            .await
            .unwrap_err();
        assert!(!e.to_string().contains("Passw0rdPassw0rd"));
        assert_eq!(live(&link), 0);
        assert_eq!(count(&log, "activate /prev/home"), 1);
        assert_no_secret(&link, &log, "Passw0rdPassw0rd");
        // The link is free again.
        assert!(link.begin().is_ok());
    }

    #[tokio::test]
    async fn join_saved_is_refused_while_an_upgrade_holds_the_link() {
        let (link, log) = fake().build();
        let st = link.begin().unwrap();
        let e = link.join_saved("Home", Some("Passw0rdPassw0rd")).await;
        assert_eq!(e.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        drop(st);
        assert_eq!(count(&log, "add_and_activate"), 0);
    }

    #[tokio::test]
    async fn join_saved_rejects_a_bad_key_before_touching_anything() {
        let (link, log) = fake().build();
        let e = link.join_saved("Home", Some("short")).await.unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(count(&log, "add_and_activate"), 0);
        assert_eq!(count(&log, "activate /prev"), 0);
    }

    /// Run by hand: `cargo test -p kuickshare-core real_networkmanager -- --ignored`. The Wi-Fi
    /// connection drops while the hotspot is up and is restored by `release`.
    #[tokio::test]
    #[ignore = "needs NetworkManager and a Wi-Fi card that can run an access point"]
    async fn real_networkmanager_hosts_and_releases() {
        let link = NmLink::new();
        let creds = link.host(40000).await.expect("hotspot");
        assert!(!creds.ssid.is_empty() && creds.password.len() >= 8);
        link.release().await;
    }
}
