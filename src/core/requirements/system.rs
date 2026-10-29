//! The probe on the real system bus. Read-only by construction.
//!
//! It makes only these calls: `NameHasOwner` and `ListActivatableNames` (the bus), `Get`/`GetAll`
//! (properties), `GetManagedObjects` (BlueZ), `GetDevices` and each device's `DeviceType`,
//! `GetPermissions` (NetworkManager), `Adapter1.GetDiscoveryFilters`, firewalld's `getZones`, and
//! polkit's `CheckAuthorization` with flags 0 (no interaction; native installs only, with our own
//! bus name as the subject). It also reads `/.flatpak-info`, `/proc/net/protocols` and looks for
//! `dnsmasq` at the standard paths. Nothing here creates a connection, starts a scan, changes the
//! state of a radio, or prompts for credentials. Every call is bounded by the run's `Budget`.
//!
//! Proxies are built with `CacheProperties::No`: no property cache is started, so they neither
//! subscribe to signals nor read more than they are asked. Every call to a service other than the
//! bus daemon carries the `NoAutoStart` flag, so a service that is installed but stopped (polkit,
//! BlueZ, NetworkManager, firewalld) is never started by asking about it.

use std::{
    collections::HashMap,
    future::Future,
    os::unix::fs::PermissionsExt as _,
    path::Path,
    sync::{Mutex as StdMutex, PoisonError},
};

use tokio::time::{Instant, timeout};
use zbus::{
    Connection, OwnedVariant, Proxy, Variant, fdo,
    proxy::{Builder as ProxyBuilder, CacheProperties, MethodFlags},
};

use crate::link::select_wifi_device;

use super::{
    engine::{Budget, RequirementsProbe},
    facts::{
        AdvSlots, BluezFacts, BtAdapter, DhcpFacts, FirewallFacts, FlatpakContext, NameState,
        NmFacts, NmPermissions, Obs, Perm, PolkitVerdict, ProbeError, SandboxFacts,
        WifiDeviceFacts,
    },
    model::Packaging,
    rules::select_bt_adapter,
};

const BLUEZ: &str = "org.bluez";
const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const FIREWALLD: &str = "org.fedoraproject.FirewallD1";
const FIREWALLD_PATH: &str = "/org/fedoraproject/FirewallD1";
const POLKIT: &str = "org.freedesktop.PolicyKit1";
/// The action firewalld asks polkit about for runtime changes *(inferred from firewalld's policy
/// file, not checked on a running system)*.
const FIREWALL_ACTION: &str = "org.fedoraproject.FirewallD1.all";
/// The action firewalld's own polkit rule requires for `getZones` (read-only, but guarded, so
/// it may prompt): checked first, with flags 0, and the call is made only when it is allowed
/// *(inferred, as above)*.
const FIREWALL_INFO_ACTION: &str = "org.fedoraproject.FirewallD1.info";
const FLATPAK_INFO: &str = "/.flatpak-info";
/// Where NetworkManager looks for the `dnsmasq` it runs for connection sharing.
const DNSMASQ_PATHS: [&str; 6] = [
    "/usr/sbin/dnsmasq",
    "/usr/local/sbin/dnsmasq",
    "/sbin/dnsmasq",
    "/usr/bin/dnsmasq",
    "/usr/local/bin/dnsmasq",
    "/bin/dnsmasq",
];

/// How this process is installed, from the environment the shell passes on.
pub fn detect_packaging() -> Packaging {
    packaging_from(
        Path::new(FLATPAK_INFO).exists(),
        std::env::var_os("APPIMAGE").is_some() || std::env::var_os("APPDIR").is_some(),
    )
}

fn packaging_from(flatpak_info: bool, appimage_env: bool) -> Packaging {
    if flatpak_info {
        Packaging::Flatpak
    } else if appimage_env {
        Packaging::Appimage
    } else {
        Packaging::Native
    }
}

pub struct SystemRequirementsProbe {
    /// The system bus connection, kept between runs; dropped when the bus is gone.
    conn: StdMutex<Option<Connection>>,
    packaging: Packaging,
}

impl Default for SystemRequirementsProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemRequirementsProbe {
    pub fn new() -> Self {
        Self {
            conn: StdMutex::new(None),
            packaging: detect_packaging(),
        }
    }

    async fn connect(&self, b: &Budget) -> Result<Connection, ProbeError> {
        let cached = self
            .conn
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(c) = cached {
            return Ok(c);
        }
        let c = match timeout(limit(b), Connection::system()).await {
            Err(_) => return Err(ProbeError::Timeout),
            Ok(Err(e)) => {
                tracing::debug!("requirements: no system bus: {e}");
                return Err(ProbeError::BusUnreachable);
            }
            Ok(Ok(c)) => c,
        };
        *self.conn.lock().unwrap_or_else(PoisonError::into_inner) = Some(c.clone());
        Ok(c)
    }

    /// A bus that stopped answering is reconnected at the next run.
    fn forget_if_gone<T>(&self, o: &Obs<T>) {
        if matches!(o, Obs::Failed(ProbeError::BusUnreachable)) {
            *self.conn.lock().unwrap_or_else(PoisonError::into_inner) = None;
        }
    }
}

impl RequirementsProbe for SystemRequirementsProbe {
    fn packaging(&self) -> Packaging {
        self.packaging
    }

    async fn sandbox(&self) -> SandboxFacts {
        if self.packaging != Packaging::Flatpak {
            return SandboxFacts::default();
        }
        let context =
            match tokio::task::spawn_blocking(|| std::fs::read_to_string(FLATPAK_INFO)).await {
                Ok(Ok(text)) => Obs::Got(parse_flatpak_info(&text)),
                _ => Obs::Failed(ProbeError::Other),
            };
        SandboxFacts { context }
    }

    async fn bluez(&self, b: Budget) -> BluezFacts {
        let conn = match self.connect(&b).await {
            Ok(c) => c,
            Err(e) => return BluezFacts::failed(e),
        };
        let l2cap_kernel = l2cap_loaded().await;
        let name = name_state(&conn, BLUEZ, &b).await;
        self.forget_if_gone(&name);
        let mut adapters = if matches!(name, Obs::Got(NameState::Owned)) {
            read_adapters(&conn, &b).await
        } else {
            carry(&name)
        };
        let mut name = name;
        if gone(&adapters) {
            // It went away after its name was seen: it is not running now.
            name = Obs::Missing;
            adapters = Obs::Missing;
        }
        BluezFacts {
            name,
            adapters,
            l2cap_kernel,
        }
    }

    async fn network_manager(&self, b: Budget) -> NmFacts {
        let conn = match self.connect(&b).await {
            Ok(c) => c,
            Err(e) => return NmFacts::failed(e),
        };
        let name = name_state(&conn, NM, &b).await;
        self.forget_if_gone(&name);
        let mut facts = NmFacts::default();
        if !matches!(name, Obs::Got(NameState::Owned)) {
            facts.name = name;
            return facts;
        }
        facts.name = name;

        match call(&b, get_all(&conn, NM, NM_PATH, NM)).await {
            Obs::Got(mut m) => {
                facts.version = take(&mut m, "Version");
                facts.networking_enabled = take(&mut m, "NetworkingEnabled");
                facts.wireless_enabled = take(&mut m, "WirelessEnabled");
                facts.wireless_hardware_enabled = take(&mut m, "WirelessHardwareEnabled");
            }
            Obs::Missing => {}
            Obs::Failed(e) => {
                facts.version = Obs::Failed(e);
                facts.networking_enabled = Obs::Failed(e);
                facts.wireless_enabled = Obs::Failed(e);
                facts.wireless_hardware_enabled = Obs::Failed(e);
            }
        }

        // The runtime's selector (`link::select_wifi_device`) over every device's type.
        let candidates = call(&b, nm_devices(&conn)).await;
        match &candidates {
            Obs::Got(devices) => {
                let types: Vec<u32> = devices.iter().map(|(_, t)| *t).collect();
                facts.wifi = match select_wifi_device(&types).and_then(|i| devices.get(i)) {
                    Some((path, _)) => read_wifi_device(&conn, &b, path).await,
                    None => Obs::Missing,
                };
                facts.device_types = Obs::Got(types);
            }
            other => {
                facts.device_types = carry(other);
                facts.wifi = carry(other);
            }
        }

        facts.permissions = read_permissions(&conn, &b).await;
        // The service went away after its name was seen: it is not running now.
        if gone(&facts.version)
            || gone(&facts.networking_enabled)
            || gone(&facts.device_types)
            || gone(&facts.wifi)
            || gone(&facts.permissions)
        {
            return NmFacts::default();
        }
        facts
    }

    async fn firewall(&self, b: Budget) -> FirewallFacts {
        let conn = match self.connect(&b).await {
            Ok(c) => c,
            Err(e) => return FirewallFacts::failed(e),
        };
        let name = name_state(&conn, FIREWALLD, &b).await;
        self.forget_if_gone(&name);
        if !matches!(name, Obs::Got(NameState::Owned)) {
            return FirewallFacts {
                name,
                ..FirewallFacts::default()
            };
        }
        // polkit is asked first and only when it runs: `getZones` is guarded by firewalld's
        // `info` action and could prompt, so it is called only when that is allowed. Where polkit
        // cannot be asked (a Flatpak sandbox, no polkit) the zones stay unread.
        let (mut zones, polkit) = if self.packaging == Packaging::Flatpak {
            (Obs::Missing, Obs::Missing)
        } else {
            match name_state(&conn, POLKIT, &b).await {
                Obs::Got(NameState::Owned) => {
                    let info = check_authorization(&conn, &b, FIREWALL_INFO_ACTION).await;
                    let zones = if matches!(info, Obs::Got(PolkitVerdict::Authorized)) {
                        read_zones(&conn, &b).await
                    } else {
                        Obs::Missing
                    };
                    (zones, check_authorization(&conn, &b, FIREWALL_ACTION).await)
                }
                other => (Obs::Missing, carry(&other)),
            }
        };
        let mut name = name;
        if gone(&zones) {
            name = Obs::Missing;
            zones = Obs::Missing;
        }
        FirewallFacts {
            name,
            zones,
            polkit,
        }
    }

    async fn dhcp(&self, p: Packaging) -> DhcpFacts {
        if p == Packaging::Flatpak {
            // The host's files cannot be seen from the sandbox.
            return DhcpFacts::default();
        }
        let dnsmasq = match tokio::task::spawn_blocking(|| dnsmasq_present(&DNSMASQ_PATHS)).await {
            Ok(found) => Obs::Got(found),
            Err(_) => Obs::Failed(ProbeError::Other),
        };
        DhcpFacts { dnsmasq }
    }
}

// ---------------------------------------------------------------------------
// D-Bus helpers
// ---------------------------------------------------------------------------

/// What is left of the call's time: the per-call limit, and never past the run's deadline.
fn limit(b: &Budget) -> std::time::Duration {
    b.per_call
        .min(b.deadline.saturating_duration_since(Instant::now()))
}

/// What a failed call says about the thing that was asked.
fn classify(e: &zbus::Error) -> ProbeError {
    use zbus::fdo::Error as Fdo;
    match e {
        zbus::Error::InputOutput(_)
        | zbus::Error::Address(_)
        | zbus::Error::Handshake(_)
        | zbus::Error::Connection(..) => ProbeError::BusUnreachable,
        zbus::Error::FDO(f) => match **f {
            Fdo::ServiceUnknown(_) | Fdo::NameHasNoOwner(_) => ProbeError::NoOwner,
            Fdo::AccessDenied(_) | Fdo::AuthFailed(_) => ProbeError::AccessDenied,
            Fdo::NoReply(_) | Fdo::Timeout(_) | Fdo::TimedOut(_) => ProbeError::Timeout,
            Fdo::Disconnected(_) | Fdo::NoServer(_) | Fdo::IOError(_) | Fdo::BadAddress(_) => {
                ProbeError::BusUnreachable
            }
            _ => ProbeError::Other,
        },
        zbus::Error::MethodError(name, _, _)
            if name.as_str().ends_with("AccessDenied")
                || name.as_str().ends_with("NotAuthorized") =>
        {
            ProbeError::AccessDenied
        }
        zbus::Error::IncorrectType
        | zbus::Error::InvalidReply
        | zbus::Error::SignatureMismatch(..)
        | zbus::Error::OutOfBounds
        | zbus::Error::Utf8(_) => ProbeError::Malformed,
        _ => ProbeError::Other,
    }
}

/// The error says the thing asked about does not exist (as opposed to the call failing).
fn absent(e: &zbus::Error) -> bool {
    use zbus::fdo::Error as Fdo;
    matches!(
        e,
        zbus::Error::FDO(f) if matches!(
            **f,
            Fdo::UnknownProperty(_)
                | Fdo::UnknownInterface(_)
                | Fdo::UnknownObject(_)
                | Fdo::UnknownMethod(_)
        )
    )
}

/// Runs one D-Bus call within the budget.
async fn call<T>(b: &Budget, fut: impl Future<Output = zbus::Result<T>>) -> Obs<T> {
    match timeout(limit(b), fut).await {
        Err(_) => Obs::Failed(ProbeError::Timeout),
        Ok(Ok(v)) => Obs::Got(v),
        Ok(Err(e)) if absent(&e) => Obs::Missing,
        Ok(Err(e)) => Obs::Failed(classify(&e)),
    }
}

/// A failure of an earlier step goes on to the later ones; anything else is just not there.
fn carry<T, U>(o: &Obs<T>) -> Obs<U> {
    match o {
        Obs::Failed(e) => Obs::Failed(*e),
        Obs::Got(_) | Obs::Missing => Obs::Missing,
    }
}

/// A method call with `NoAutoStart`: a service that is installed but not running is not started
/// by asking about it (it answers `ServiceUnknown`, which `classify` reads as `NoOwner`).
async fn nas<R, B>(p: &Proxy<'_>, method: &str, body: &B) -> zbus::Result<R>
where
    B: serde::ser::Serialize + zbus::DynamicType,
    R: for<'d> zbus::wire::DynamicDeserialize<'d>,
{
    p.call_with_flags::<_, _, R>(method, MethodFlags::NO_AUTO_START, body)
        .await?
        .ok_or(zbus::Error::InvalidReply)
}

fn gone<T>(o: &Obs<T>) -> bool {
    matches!(o, Obs::Failed(ProbeError::NoOwner))
}

/// A proxy that does not cache and so does not subscribe to anything.
async fn raw<'a>(
    c: &Connection,
    dest: &'a str,
    path: &'a str,
    iface: &'a str,
) -> zbus::Result<Proxy<'a>> {
    ProxyBuilder::<Proxy>::new(c)
        .destination(dest)
        .path(path)
        .interface(iface)
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

/// `org.freedesktop.DBus.Properties.GetAll`.
async fn get_all(
    c: &Connection,
    dest: &str,
    path: &str,
    iface: &str,
) -> zbus::Result<HashMap<String, OwnedVariant>> {
    nas(
        &raw(c, dest, path, "org.freedesktop.DBus.Properties").await?,
        "GetAll",
        &(iface,),
    )
    .await
}

/// A property of a `GetAll` reply, converted. `Missing` when the reply has no such key.
fn take<T: TryFrom<OwnedVariant>>(m: &mut HashMap<String, OwnedVariant>, key: &str) -> Obs<T> {
    match m.remove(key) {
        None => Obs::Missing,
        Some(v) => T::try_from(v).map_or(Obs::Failed(ProbeError::Malformed), Obs::Got),
    }
}

/// Whether the bus has an owner for `name`, or would start one.
async fn name_state(c: &Connection, name: &str, b: &Budget) -> Obs<NameState> {
    let bus = match raw(
        c,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await
    {
        Ok(p) => p,
        Err(e) => return Obs::Failed(classify(&e)),
    };
    match call(b, bus.call::<_, _, bool>("NameHasOwner", &(name,))).await {
        Obs::Got(true) => Obs::Got(NameState::Owned),
        Obs::Got(false) => {
            match call(
                b,
                bus.call::<_, _, Vec<String>>("ListActivatableNames", &()),
            )
            .await
            {
                Obs::Got(list) if list.iter().any(|n| n == name) => {
                    Obs::Got(NameState::Activatable)
                }
                Obs::Got(_) | Obs::Missing => Obs::Missing,
                Obs::Failed(e) => Obs::Failed(e),
            }
        }
        Obs::Missing => Obs::Missing,
        Obs::Failed(e) => Obs::Failed(e),
    }
}

// ---------------------------------------------------------------------------
// BlueZ
// ---------------------------------------------------------------------------

/// firewalld's zone names; call only after polkit allowed the `info` action.
async fn read_zones(c: &Connection, b: &Budget) -> Obs<Vec<String>> {
    match raw(
        c,
        FIREWALLD,
        FIREWALLD_PATH,
        "org.fedoraproject.FirewallD1.zone",
    )
    .await
    {
        Ok(p) => call(b, nas::<Vec<String>, _>(&p, "getZones", &())).await,
        Err(e) => Obs::Failed(classify(&e)),
    }
}

/// `GetDevices` and each device's `DeviceType`, in order (strict: the runtime's own loop is
/// `link::first_wifi`). Every call carries `NoAutoStart`.
async fn nm_devices(c: &Connection) -> zbus::Result<Vec<(String, u32)>> {
    let manager = raw(c, NM, NM_PATH, NM).await?;
    let paths: Vec<zbus::OwnedObjectPath> = nas(&manager, "GetDevices", &()).await?;
    let mut devices = Vec::with_capacity(paths.len());
    for p in paths {
        let props = raw(c, NM, p.as_str(), "org.freedesktop.DBus.Properties").await?;
        let kind: OwnedVariant = nas(
            &props,
            "Get",
            &("org.freedesktop.NetworkManager.Device", "DeviceType"),
        )
        .await?;
        let kind = u32::try_from(kind).map_err(|_| zbus::Error::IncorrectType)?;
        devices.push((p.to_string(), kind));
    }
    Ok(devices)
}

async fn read_adapters(c: &Connection, b: &Budget) -> Obs<Vec<BtAdapter>> {
    let manager = match raw(c, BLUEZ, "/", "org.freedesktop.DBus.ObjectManager").await {
        Ok(p) => p,
        Err(e) => return Obs::Failed(classify(&e)),
    };
    let objects = match call(
        b,
        nas::<fdo::ManagedObjects, _>(&manager, "GetManagedObjects", &()),
    )
    .await
    {
        Obs::Got(o) => o,
        other => return carry(&other),
    };
    let flat = objects.into_iter().map(|(path, interfaces)| {
        let interfaces: HashMap<String, HashMap<String, OwnedVariant>> = interfaces
            .into_iter()
            .map(|(name, props)| (name.to_string(), props))
            .collect();
        (path.to_string(), interfaces)
    });
    let mut adapters = parse_adapters(flat);

    // Only the adapter the runtime would use is asked for its discovery filters.
    let names: Vec<String> = adapters.iter().map(|a| a.name.clone()).collect();
    if let Some(i) = select_bt_adapter(&names) {
        let path = format!("/org/bluez/{}", names[i]);
        adapters[i].discovery_filters = match raw(c, BLUEZ, &path, "org.bluez.Adapter1").await {
            Ok(p) => call(b, nas::<Vec<String>, _>(&p, "GetDiscoveryFilters", &())).await,
            Err(e) => Obs::Failed(classify(&e)),
        };
    }
    Obs::Got(adapters)
}

type Props = HashMap<String, OwnedVariant>;

/// The `org.bluez.Adapter1` objects (`/org/bluez/hciN`), sorted by name.
fn parse_adapters(
    objects: impl IntoIterator<Item = (String, HashMap<String, Props>)>,
) -> Vec<BtAdapter> {
    let mut adapters = Vec::new();
    for (path, mut interfaces) in objects {
        let Some(name) = path.strip_prefix("/org/bluez/") else {
            continue;
        };
        if name.is_empty() || name.contains('/') {
            continue;
        }
        let Some(mut props) = interfaces.remove("org.bluez.Adapter1") else {
            continue;
        };
        let advertising = match interfaces.remove("org.bluez.LEAdvertisingManager1") {
            Some(mut p) => Obs::Got(AdvSlots {
                active: take::<u8>(&mut p, "ActiveInstances").got().copied(),
                supported: take::<u8>(&mut p, "SupportedInstances").got().copied(),
            }),
            None => Obs::Missing,
        };
        adapters.push(BtAdapter {
            name: name.to_owned(),
            powered: take(&mut props, "Powered"),
            power_state: take(&mut props, "PowerState"),
            roles: take(&mut props, "Roles"),
            advertising,
            gatt_manager: interfaces.contains_key("org.bluez.GattManager1"),
            discovery_filters: Obs::Missing,
        });
    }
    adapters.sort_by(|a, b| a.name.cmp(&b.name));
    adapters
}

/// Whether the kernel has registered the L2CAP protocol (the `bluetooth` module is loaded).
async fn l2cap_loaded() -> Obs<bool> {
    match tokio::task::spawn_blocking(|| std::fs::read_to_string("/proc/net/protocols")).await {
        Ok(Ok(text)) => Obs::Got(has_l2cap(&text)),
        _ => Obs::Failed(ProbeError::Other),
    }
}

fn has_l2cap(protocols: &str) -> bool {
    protocols
        .lines()
        .any(|l| l.split_whitespace().next() == Some("L2CAP"))
}

// ---------------------------------------------------------------------------
// NetworkManager
// ---------------------------------------------------------------------------

async fn read_wifi_device(c: &Connection, b: &Budget, path: &str) -> Obs<WifiDeviceFacts> {
    let mut facts = WifiDeviceFacts::default();
    match call(
        b,
        get_all(c, NM, path, "org.freedesktop.NetworkManager.Device"),
    )
    .await
    {
        Obs::Got(mut m) => {
            facts.interface = take(&mut m, "Interface");
            facts.managed = take(&mut m, "Managed");
            facts.firmware_missing = take(&mut m, "FirmwareMissing");
            facts.plugin_missing = take(&mut m, "NmPluginMissing");
            facts.state = take(&mut m, "State");
            // `StateReason` is a struct (state, reason).
            facts.state_reason = match take::<(u32, u32)>(&mut m, "StateReason") {
                Obs::Got((_, reason)) => Obs::Got(reason),
                other => carry(&other),
            };
        }
        other => return carry(&other),
    }
    facts.capabilities = match call(
        b,
        get_all(
            c,
            NM,
            path,
            "org.freedesktop.NetworkManager.Device.Wireless",
        ),
    )
    .await
    {
        Obs::Got(mut m) => take(&mut m, "WirelessCapabilities"),
        other => carry(&other),
    };
    Obs::Got(facts)
}

/// `GetPermissions`: what NetworkManager would answer to us, without asking anyone.
async fn read_permissions(c: &Connection, b: &Budget) -> Obs<NmPermissions> {
    let manager = match raw(c, NM, NM_PATH, NM).await {
        Ok(p) => p,
        Err(e) => return Obs::Failed(classify(&e)),
    };
    match call(
        b,
        nas::<HashMap<String, String>, _>(&manager, "GetPermissions", &()),
    )
    .await
    {
        Obs::Got(m) => Obs::Got(NmPermissions {
            network_control: perm_of(&m, "org.freedesktop.NetworkManager.network-control"),
            settings_modify_system: perm_of(
                &m,
                "org.freedesktop.NetworkManager.settings.modify.system",
            ),
            wifi_share_protected: perm_of(
                &m,
                "org.freedesktop.NetworkManager.wifi.share.protected",
            ),
        }),
        other => carry(&other),
    }
}

fn perm_of(m: &HashMap<String, String>, key: &str) -> Obs<Perm> {
    match m.get(key).map(String::as_str) {
        Some("yes") => Obs::Got(Perm::Yes),
        Some("auth") => Obs::Got(Perm::Auth),
        Some("no") => Obs::Got(Perm::No),
        Some(_) => Obs::Failed(ProbeError::Malformed),
        None => Obs::Missing,
    }
}

// ---------------------------------------------------------------------------
// Firewall, sandbox, files
// ---------------------------------------------------------------------------

/// polkit's answer for `action`, flags 0: it never asks the user. The caller has seen polkit's
/// name owned (`NoAutoStart` covers the race after that).
async fn check_authorization(c: &Connection, b: &Budget, action: &str) -> Obs<PolkitVerdict> {
    let Some(me) = c.unique_name() else {
        return Obs::Failed(ProbeError::Other);
    };
    let authority = match raw(
        c,
        POLKIT,
        "/org/freedesktop/PolicyKit1/Authority",
        "org.freedesktop.PolicyKit1.Authority",
    )
    .await
    {
        Ok(p) => p,
        Err(e) => return Obs::Failed(classify(&e)),
    };
    let subject: (&str, HashMap<&str, Variant<'_>>) = (
        "system-bus-name",
        HashMap::from([("name", Variant::from(me.as_str().to_owned()))]),
    );
    let details: HashMap<&str, &str> = HashMap::new();
    let flags = 0u32;
    match call(
        b,
        nas::<(bool, bool, HashMap<String, String>), _>(
            &authority,
            "CheckAuthorization",
            &(subject, action, details, flags, ""),
        ),
    )
    .await
    {
        Obs::Got((true, _, _)) => Obs::Got(PolkitVerdict::Authorized),
        Obs::Got((false, true, _)) => Obs::Got(PolkitVerdict::Challenge),
        Obs::Got((false, false, _)) => Obs::Got(PolkitVerdict::Denied),
        other => carry(&other),
    }
}

/// The effective permissions of a Flatpak sandbox: `[Context]` and `[System Bus Policy]` of
/// `/.flatpak-info`. A negated entry (`!bluetooth`) is not a grant.
fn parse_flatpak_info(text: &str) -> FlatpakContext {
    fn list(value: &str) -> impl Iterator<Item = String> + '_ {
        value
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.starts_with('!'))
            .map(|s| s.split(':').next().unwrap_or(s).to_owned())
    }
    let mut ctx = FlatpakContext::default();
    let mut section = "";
    for line in text.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match (section, key) {
            ("Context", "features") => ctx.features.extend(list(value)),
            ("Context", "shared") => ctx.shared.extend(list(value)),
            ("Context", "filesystems") => ctx.filesystems.extend(list(value)),
            ("Context", "sockets") => ctx.sockets.extend(list(value)),
            ("System Bus Policy", name) if matches!(value, "talk" | "own") => {
                ctx.system_talk_names.push(name.to_owned());
            }
            _ => {}
        }
    }
    ctx
}

fn dnsmasq_present(paths: &[&str]) -> bool {
    paths.iter().any(|p| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaging_follows_flatpak_info_then_the_appimage_environment() {
        assert_eq!(packaging_from(true, true), Packaging::Flatpak);
        assert_eq!(packaging_from(false, true), Packaging::Appimage);
        assert_eq!(packaging_from(false, false), Packaging::Native);
    }

    #[test]
    fn flatpak_info_is_read_as_the_effective_context() {
        let ctx = parse_flatpak_info(
            "[Application]\nname=io.github.the_beper.KuickShare\n\n[Context]\nshared=network;ipc;\n\
             features=bluetooth;!devel;\nfilesystems=xdg-download;host:ro;\n\n\
             [System Bus Policy]\norg.bluez=talk\norg.freedesktop.Foo=see\n",
        );
        assert_eq!(ctx.shared, ["network", "ipc"]);
        assert_eq!(ctx.features, ["bluetooth"]);
        assert_eq!(ctx.filesystems, ["xdg-download", "host"]);
        assert_eq!(ctx.system_talk_names, ["org.bluez"]);
        assert!(ctx.can_talk("org.bluez") && !ctx.can_talk("org.freedesktop.Foo"));
    }

    #[test]
    fn the_whole_system_bus_and_wildcards_allow_talking() {
        let ctx = parse_flatpak_info("[Context]\nsockets=wayland;system-bus;\n");
        assert!(ctx.can_talk("org.bluez"));
        let ctx = parse_flatpak_info("[System Bus Policy]\norg.freedesktop.*=talk\n");
        assert!(ctx.can_talk("org.freedesktop.NetworkManager"));
        assert!(!ctx.can_talk("org.freedesktop"));
        assert!(!ctx.can_talk("org.bluez"));
    }

    #[test]
    fn l2cap_is_found_by_its_protocol_name() {
        assert!(has_l2cap(
            "protocol  size\nL2CAP  1176 0 -1 NI\nRFCOMM 1000\n"
        ));
        assert!(!has_l2cap("protocol  size\nRFCOMM 1000\n"));
    }

    #[test]
    fn a_missing_file_is_not_an_executable() {
        assert!(!dnsmasq_present(&["/nonexistent/dnsmasq"]));
    }

    #[test]
    fn errors_are_classified_without_guessing() {
        let missing = zbus::Error::FDO(Box::new(fdo::Error::ServiceUnknown("x".into())));
        assert_eq!(classify(&missing), ProbeError::NoOwner);
        let denied = zbus::Error::FDO(Box::new(fdo::Error::AccessDenied("x".into())));
        assert_eq!(classify(&denied), ProbeError::AccessDenied);
        let no_property = zbus::Error::FDO(Box::new(fdo::Error::UnknownProperty("x".into())));
        assert!(absent(&no_property) && !absent(&denied));
        assert_eq!(classify(&zbus::Error::IncorrectType), ProbeError::Malformed);
    }
}
