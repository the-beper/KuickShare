//! What the probes saw: plain, constructible observations, no judgement.
//!
//! Every field is an `Obs`: `Got(value)`, `Missing` (the thing is not there: a property or an
//! interface that does not exist, a name nobody owns) or `Failed(error)` (the question got no
//! answer: timeout, no bus, a denied call, a malformed reply). `rules::evaluate` turns facts into
//! rows; a `Failed` or an unexpected `Missing` can only make a row Unknown, never Blocked.
//!
//! The `Default` of every struct has every field `Missing`: tests start from a full set of `Got`
//! values and change what they need.

use crate::link::BlockReason;

use super::model::Packaging;

/// Why a question got no answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeError {
    /// The call (or the whole group) ran out of time.
    Timeout,
    /// The system bus cannot be reached.
    BusUnreachable,
    /// The service vanished: nobody owns its name.
    NoOwner,
    /// The bus, the sandbox or polkit refused the call.
    AccessDenied,
    /// The reply did not have the expected type.
    Malformed,
    Other,
}

/// One observation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Obs<T> {
    Got(T),
    #[default]
    Missing,
    Failed(ProbeError),
}

impl<T> Obs<T> {
    pub fn got(&self) -> Option<&T> {
        match self {
            Self::Got(v) => Some(v),
            _ => None,
        }
    }
}

/// Whether a bus name has an owner. `Missing` means neither owned nor activatable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameState {
    /// `NameHasOwner` is true.
    Owned,
    /// Not owned, but in `ListActivatableNames`: the bus would start it.
    Activatable,
}

// ---------------------------------------------------------------------------
// Sandbox
// ---------------------------------------------------------------------------

/// The effective permissions of a Flatpak sandbox, from `/.flatpak-info` (overrides included).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FlatpakContext {
    /// `[System Bus Policy]` names with `talk` or `own`.
    pub system_talk_names: Vec<String>,
    /// `[Context] features`, e.g. `bluetooth`.
    pub features: Vec<String>,
    /// `[Context] shared`, e.g. `network`.
    pub shared: Vec<String>,
    /// `[Context] filesystems`, e.g. `host`, `xdg-download`.
    pub filesystems: Vec<String>,
    /// `[Context] sockets`, e.g. `system-bus` (the whole system bus, no per-name policy).
    pub sockets: Vec<String>,
}

impl FlatpakContext {
    /// Whether the sandbox may talk to `name` on the system bus: the whole bus, a `talk` or `own`
    /// entry for the name, or a wildcard entry (`org.foo.*`) that covers it.
    pub fn can_talk(&self, name: &str) -> bool {
        self.sockets.iter().any(|s| s == "system-bus")
            || self.system_talk_names.iter().any(|n| {
                n == name
                    || n.strip_suffix(".*").is_some_and(|prefix| {
                        name.strip_prefix(prefix)
                            .is_some_and(|r| r.starts_with('.'))
                    })
            })
    }
}

/// `Missing` for a native or AppImage install (no sandbox); `Failed` when `/.flatpak-info` exists
/// but cannot be read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SandboxFacts {
    pub context: Obs<FlatpakContext>,
}

// ---------------------------------------------------------------------------
// BlueZ
// ---------------------------------------------------------------------------

/// `LEAdvertisingManager1` of an adapter.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AdvSlots {
    /// `ActiveInstances`.
    pub active: Option<u8>,
    /// `SupportedInstances`.
    pub supported: Option<u8>,
}

/// One `org.bluez.Adapter1` object from `GetManagedObjects`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BtAdapter {
    /// Last segment of the object path, e.g. `hci0`.
    pub name: String,
    /// `Adapter1.Powered`.
    pub powered: Obs<bool>,
    /// `Adapter1.PowerState`: `on`, `off`, `off-enabling`, `on-disabling`, `off-blocked`.
    pub power_state: Obs<String>,
    /// `Adapter1.Roles`: `central`, `peripheral`, `central-peripheral`. `Missing` when BlueZ does
    /// not report it.
    pub roles: Obs<Vec<String>>,
    /// `LEAdvertisingManager1` properties; `Missing` when the adapter has no such interface.
    pub advertising: Obs<AdvSlots>,
    /// Whether the adapter has `GattManager1`.
    pub gatt_manager: bool,
    /// `Adapter1.GetDiscoveryFilters`; read for the selected adapter only. `Missing` when the
    /// method does not exist.
    pub discovery_filters: Obs<Vec<String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BluezFacts {
    /// `NameHasOwner` and `ListActivatableNames` for `org.bluez`.
    pub name: Obs<NameState>,
    /// Every adapter of `GetManagedObjects` (`/org/bluez/hciN`), sorted by name; the rules judge
    /// the one `rules::select_bt_adapter` picks.
    pub adapters: Obs<Vec<BtAdapter>>,
    /// Whether `/proc/net/protocols` lists `L2CAP`.
    pub l2cap_kernel: Obs<bool>,
}

// ---------------------------------------------------------------------------
// NetworkManager
// ---------------------------------------------------------------------------

/// A PolicyKit-style answer of `NetworkManager.GetPermissions`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Perm {
    Yes,
    /// `auth`: needs an authentication (not a refusal).
    Auth,
    No,
}

/// `GetPermissions` entries; `Missing` when the key is not in the reply.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NmPermissions {
    /// `org.freedesktop.NetworkManager.network-control`.
    pub network_control: Obs<Perm>,
    /// `org.freedesktop.NetworkManager.settings.modify.system`.
    pub settings_modify_system: Obs<Perm>,
    /// `org.freedesktop.NetworkManager.wifi.share.protected`.
    pub wifi_share_protected: Obs<Perm>,
}

/// The Wi-Fi device the runtime would use (`link::select_wifi_device`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WifiDeviceFacts {
    /// `Device.Interface`.
    pub interface: Obs<String>,
    /// `Device.Managed`.
    pub managed: Obs<bool>,
    /// `Device.FirmwareMissing`.
    pub firmware_missing: Obs<bool>,
    /// `Device.NmPluginMissing`; `Missing` on a NetworkManager that has no such property.
    pub plugin_missing: Obs<bool>,
    /// `Device.State` (`NMDeviceState`; 20 is unavailable).
    pub state: Obs<u32>,
    /// The reason of `Device.StateReason` (`NMDeviceStateReason`).
    pub state_reason: Obs<u32>,
    /// `Device.Wireless.WirelessCapabilities` (`NMDeviceWifiCapabilities` bits).
    pub capabilities: Obs<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NmFacts {
    /// `NameHasOwner` and `ListActivatableNames` for `org.freedesktop.NetworkManager`.
    pub name: Obs<NameState>,
    /// `NetworkManager.Version`.
    pub version: Obs<String>,
    /// `NetworkManager.NetworkingEnabled`.
    pub networking_enabled: Obs<bool>,
    /// `NetworkManager.WirelessEnabled`.
    pub wireless_enabled: Obs<bool>,
    /// `NetworkManager.WirelessHardwareEnabled`.
    pub wireless_hardware_enabled: Obs<bool>,
    /// `GetDevices` and each `Device.DeviceType`, in `GetDevices` order (what
    /// the probe reads them, one `DeviceType` each).
    pub device_types: Obs<Vec<u32>>,
    /// The device `link::select_wifi_device` picks. `Missing` when there is none.
    pub wifi: Obs<WifiDeviceFacts>,
    /// `NetworkManager.GetPermissions`.
    pub permissions: Obs<NmPermissions>,
}

// ---------------------------------------------------------------------------
// Firewall and hotspot addressing
// ---------------------------------------------------------------------------

/// `PolicyKit1.Authority.CheckAuthorization` with flags 0 (no interaction).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolkitVerdict {
    Authorized,
    /// `is_challenge`: authentication would be asked.
    Challenge,
    Denied,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FirewallFacts {
    /// `NameHasOwner` and `ListActivatableNames` for `org.fedoraproject.FirewallD1`.
    pub name: Obs<NameState>,
    /// `org.fedoraproject.FirewallD1.zone.getZones`.
    pub zones: Obs<Vec<String>>,
    /// The check of firewalld's action for runtime changes. `Missing` in a Flatpak sandbox (no
    /// way to ask polkit from there).
    pub polkit: Obs<PolkitVerdict>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DhcpFacts {
    /// Whether a `dnsmasq` executable is at one of the standard paths. `Missing` in a Flatpak
    /// sandbox (the host's files cannot be seen).
    pub dnsmasq: Obs<bool>,
}

/// Everything one run saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Facts {
    pub packaging: Packaging,
    pub sandbox: SandboxFacts,
    pub bluez: BluezFacts,
    pub nm: NmFacts,
    pub firewall: FirewallFacts,
    pub dhcp: DhcpFacts,
}

impl Facts {
    /// Facts of a run where every question failed with `e`.
    pub fn failed(packaging: Packaging, e: ProbeError) -> Self {
        Self {
            packaging,
            sandbox: SandboxFacts::default(),
            bluez: BluezFacts::failed(e),
            nm: NmFacts::failed(e),
            firewall: FirewallFacts::failed(e),
            dhcp: DhcpFacts::failed(e),
        }
    }
}

impl BluezFacts {
    pub fn failed(e: ProbeError) -> Self {
        Self {
            name: Obs::Failed(e),
            adapters: Obs::Failed(e),
            l2cap_kernel: Obs::Failed(e),
        }
    }
}

impl NmFacts {
    pub fn failed(e: ProbeError) -> Self {
        Self {
            name: Obs::Failed(e),
            version: Obs::Failed(e),
            networking_enabled: Obs::Failed(e),
            wireless_enabled: Obs::Failed(e),
            wireless_hardware_enabled: Obs::Failed(e),
            device_types: Obs::Failed(e),
            wifi: Obs::Failed(e),
            permissions: Obs::Failed(e),
        }
    }
}

impl FirewallFacts {
    pub fn failed(e: ProbeError) -> Self {
        Self {
            name: Obs::Failed(e),
            zones: Obs::Failed(e),
            polkit: Obs::Failed(e),
        }
    }
}

impl DhcpFacts {
    pub fn failed(e: ProbeError) -> Self {
        Self {
            dnsmasq: Obs::Failed(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Session evidence
// ---------------------------------------------------------------------------

/// Something the runtime learned by acting; it reaches the engine through `LinkNotice::Evidence`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeEvidence {
    /// A hotspot came up.
    HotspotStarted,
    /// A hotspot did not come up.
    HotspotFailed,
    /// firewalld opened the upgrade port.
    FirewallPortOpened,
    /// The firewall did not allow the upgrade port.
    FirewallRefused(BlockReason),
    /// The user gave (true) or took back (false) the consent to an open firewall zone.
    FirewallConsent(bool),
}

/// What the latest firewall attempt of this session showed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FirewallEvidence {
    #[default]
    None,
    PortOpened,
    Refused(BlockReason),
}

/// The evidence of this session, folded from `RuntimeEvidence`. It changes only the rows it
/// concerns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionEvidence {
    /// A hotspot came up at least once this session.
    pub hotspot_started: bool,
    /// The latest hotspot attempt failed.
    pub hotspot_failed: bool,
    /// The latest firewall attempt.
    pub firewall: FirewallEvidence,
    /// The user's consent to an open firewall zone for this session.
    pub firewall_consent: bool,
}

impl SessionEvidence {
    pub fn apply(&mut self, e: RuntimeEvidence) {
        match e {
            RuntimeEvidence::HotspotStarted => {
                self.hotspot_started = true;
                self.hotspot_failed = false;
            }
            RuntimeEvidence::HotspotFailed => self.hotspot_failed = true,
            RuntimeEvidence::FirewallPortOpened => self.firewall = FirewallEvidence::PortOpened,
            RuntimeEvidence::FirewallRefused(reason) => {
                self.firewall = FirewallEvidence::Refused(reason);
            }
            RuntimeEvidence::FirewallConsent(allowed) => self.firewall_consent = allowed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_folds_in_order() {
        let mut ev = SessionEvidence::default();
        ev.apply(RuntimeEvidence::HotspotFailed);
        assert!(ev.hotspot_failed && !ev.hotspot_started);
        ev.apply(RuntimeEvidence::HotspotStarted);
        assert!(ev.hotspot_started && !ev.hotspot_failed);
        ev.apply(RuntimeEvidence::HotspotFailed);
        assert!(ev.hotspot_started && ev.hotspot_failed, "started stays");
        ev.apply(RuntimeEvidence::FirewallRefused(
            BlockReason::FirewallDenied,
        ));
        assert_eq!(
            ev.firewall,
            FirewallEvidence::Refused(BlockReason::FirewallDenied)
        );
        ev.apply(RuntimeEvidence::FirewallPortOpened);
        assert_eq!(ev.firewall, FirewallEvidence::PortOpened);
        ev.apply(RuntimeEvidence::FirewallConsent(true));
        assert!(ev.firewall_consent);
    }
}
