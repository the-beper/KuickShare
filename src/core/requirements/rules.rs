//! The judgement: facts and session evidence in, requirement rows and capabilities out.
//!
//! Pure functions, no I/O, no clock. The rules the rows follow:
//! - a missing service blocks only the capabilities that need it;
//! - what cannot be observed is Unknown, never Blocked (a failed or missing observation only
//!   gives the generic reasons `timeout`, `bus_unreachable`, `malformed_reply`, `query_failed`,
//!   or `skipped` for a row whose dependency is not Ready);
//! - NetworkManager's `auth` is a Warning, not a refusal;
//! - a service that is present is not an end-to-end success: nothing here says a hotspot works
//!   until the runtime started one (`SessionEvidence`).

use crate::link::{BlockReason, select_wifi_device};

use super::{
    facts::{
        BluezFacts, BtAdapter, Facts, FirewallEvidence, FlatpakContext, NameState, NmFacts, Obs,
        Perm, PolkitVerdict, ProbeError, SessionEvidence, WifiDeviceFacts,
    },
    model::{
        Capabilities, CapabilityStatus, Category, JOIN_NETWORK, MAX_DETAILS, MAX_DEVICE,
        MAX_VERIFICATION, Packaging, RECEIVING, Requirement, SENDING, Status,
    },
};

/// `NM_WIFI_DEVICE_CAP_CIPHER_CCMP`
const CAP_CCMP: u32 = 0x8;
/// `NM_WIFI_DEVICE_CAP_RSN`
const CAP_RSN: u32 = 0x20;
/// `NM_WIFI_DEVICE_CAP_AP`
const CAP_AP: u32 = 0x40;
/// `NM_WIFI_DEVICE_CAP_FREQ_VALID`: the two band bits below are meaningful.
const CAP_FREQ_VALID: u32 = 0x100;
/// `NM_WIFI_DEVICE_CAP_FREQ_2GHZ`
const CAP_FREQ_2GHZ: u32 = 0x200;
/// `NM_WIFI_DEVICE_CAP_FREQ_5GHZ`
const CAP_FREQ_5GHZ: u32 = 0x400;

/// `NM_DEVICE_STATE_UNAVAILABLE`
const NM_STATE_UNAVAILABLE: u32 = 20;
/// `NM_DEVICE_STATE_REASON_SHARED_START_FAILED` and `_SHARED_FAILED` *(inferred from the NM
/// headers, not checked against a running system)*.
const NM_REASON_SHARED_START_FAILED: u32 = 18;
const NM_REASON_SHARED_FAILED: u32 = 19;

/// Unknown reasons every row can have.
const GENERIC_REASONS: [&str; 5] = [
    "timeout",
    "bus_unreachable",
    "malformed_reply",
    "query_failed",
    "skipped",
];

// ---------------------------------------------------------------------------
// Static description of every row
// ---------------------------------------------------------------------------

struct Meta {
    id: &'static str,
    category: Category,
    affects: &'static [&'static str],
    verification: &'static str,
}

const S: &[&str] = &[SENDING];
const R: &[&str] = &[RECEIVING];
const SR: &[&str] = &[SENDING, RECEIVING];
const SJ: &[&str] = &[SENDING, JOIN_NETWORK];
const SRJ: &[&str] = &[SENDING, RECEIVING, JOIN_NETWORK];
const NONE: &[&str] = &[];

const FALLBACK: Meta = Meta {
    id: "",
    category: Category::Info,
    affects: NONE,
    verification: "",
};

#[rustfmt::skip]
static META: &[Meta] = &[
    Meta { id: "bluez.service", category: Category::Critical, affects: SR, verification: "NameHasOwner and ListActivatableNames for org.bluez" },
    Meta { id: "bluez.adapter", category: Category::Critical, affects: SR, verification: "GetManagedObjects of org.bluez, adapter picked like the runtime does" },
    Meta { id: "bluez.powered", category: Category::Critical, affects: SR, verification: "Adapter1 Powered and PowerState" },
    Meta { id: "bluez.le", category: Category::Critical, affects: SR, verification: "Adapter1 Roles" },
    Meta { id: "bluez.advertising", category: Category::Critical, affects: R, verification: "LEAdvertisingManager1 on the adapter" },
    Meta { id: "bluez.scanning", category: Category::Critical, affects: S, verification: "Adapter1 Roles and GetDiscoveryFilters" },
    Meta { id: "bluez.transport", category: Category::Critical, affects: R, verification: "GattManager1 on the adapter" },
    Meta { id: "bluez.l2cap", category: Category::Warning, affects: S, verification: "Flatpak features and /proc/net/protocols" },
    Meta { id: "nm.service", category: Category::Critical, affects: SRJ, verification: "NameHasOwner and Version of NetworkManager" },
    Meta { id: "nm.networking", category: Category::Critical, affects: SRJ, verification: "NetworkManager NetworkingEnabled" },
    Meta { id: "nm.wifi_enabled", category: Category::Critical, affects: SRJ, verification: "NetworkManager WirelessEnabled" },
    Meta { id: "nm.hardware_enabled", category: Category::Critical, affects: SRJ, verification: "NetworkManager WirelessHardwareEnabled" },
    Meta { id: "nm.device", category: Category::Critical, affects: SRJ, verification: "GetDevices and DeviceType, device picked like the runtime does" },
    Meta { id: "nm.managed", category: Category::Critical, affects: SRJ, verification: "Device Managed" },
    Meta { id: "nm.firmware", category: Category::Critical, affects: SRJ, verification: "Device FirmwareMissing" },
    Meta { id: "nm.driver", category: Category::Critical, affects: SRJ, verification: "Device NmPluginMissing, State and StateReason" },
    Meta { id: "nm.permissions", category: Category::Critical, affects: SRJ, verification: "GetPermissions without interaction" },
    Meta { id: "wifi.client", category: Category::Critical, affects: SJ, verification: "The NetworkManager rows above and the sandbox network permission" },
    Meta { id: "wifi.ap", category: Category::Critical, affects: R, verification: "Device WirelessCapabilities and the wifi.share.protected permission" },
    Meta { id: "wifi.dhcp", category: Category::Critical, affects: R, verification: "dnsmasq at the standard paths (not looked for in a Flatpak), or a hotspot that started this session" },
    Meta { id: "firewall.integration", category: Category::Security, affects: R, verification: "firewalld name, zone list and polkit CheckAuthorization without interaction" },
    Meta { id: "sandbox.permissions", category: Category::Security, affects: NONE, verification: "/.flatpak-info" },
];

fn meta(id: &str) -> &'static Meta {
    META.iter().find(|m| m.id == id).unwrap_or(&FALLBACK)
}

// ---------------------------------------------------------------------------
// Text limits
// ---------------------------------------------------------------------------

/// Invisible format characters that can hide or reorder text (zero-width, bidi marks and
/// overrides, word joiners, the byte order mark); the shell strips the same ones.
fn is_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

/// Strips control and format characters and `<>`, trims, and cuts to `max_chars` characters.
pub fn sanitize(s: &str, max_chars: usize) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| !c.is_control() && !is_format_char(*c) && *c != '<' && *c != '>')
        .take(max_chars)
        .collect();
    cleaned.trim().to_owned()
}

/// An interface or adapter name for the `device` field. Names such as `wlx<MAC>` carry a MAC
/// address: every run of twelve or more hexadecimal digits is replaced by an ellipsis.
fn sanitize_device(name: &str) -> String {
    fn flush(run: &mut String, out: &mut String) {
        if run.len() >= 12 {
            out.push('\u{2026}');
        } else {
            out.push_str(run);
        }
        run.clear();
    }
    let mut out = String::new();
    let mut run = String::new();
    for c in name.chars() {
        if c.is_ascii_hexdigit() {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    sanitize(&out, MAX_DEVICE)
}

fn req(
    id: &'static str,
    status: Status,
    reason: &str,
    details: &str,
    device: Option<&str>,
) -> Requirement {
    let m = meta(id);
    Requirement {
        id: id.to_owned(),
        status,
        category: m.category,
        reason_code: reason.to_owned(),
        details: sanitize(details, MAX_DETAILS),
        device: device.map(sanitize_device).filter(|d| !d.is_empty()),
        verification: sanitize(m.verification, MAX_VERIFICATION),
        affects: m.affects.iter().map(|a| (*a).to_owned()).collect(),
    }
}

fn ready(id: &'static str, device: Option<&str>) -> Requirement {
    req(id, Status::Ready, "ok", "", device)
}

fn unknown_row(id: &'static str, reason: &str) -> Requirement {
    req(id, Status::Unknown, reason, "", None)
}

fn generic(e: &ProbeError) -> &'static str {
    match e {
        ProbeError::Timeout => "timeout",
        ProbeError::BusUnreachable => "bus_unreachable",
        ProbeError::Malformed => "malformed_reply",
        ProbeError::NoOwner | ProbeError::AccessDenied | ProbeError::Other => "query_failed",
    }
}

/// The generic reason of an observation that is not a usable `Got`.
fn obs_reason<T>(o: &Obs<T>) -> &'static str {
    match o {
        Obs::Failed(e) => generic(e),
        Obs::Missing | Obs::Got(_) => "query_failed",
    }
}

/// An Unknown row when one of `deps` is not Ready; the reason is the dependency's own when that
/// is a generic one (so a timeout stays a timeout), otherwise `skipped`.
fn gate(id: &'static str, deps: &[&Requirement]) -> Option<Requirement> {
    let dep = deps.iter().find(|d| d.status != Status::Ready)?;
    let reason =
        if dep.status == Status::Unknown && GENERIC_REASONS.contains(&dep.reason_code.as_str()) {
            dep.reason_code.as_str()
        } else {
            "skipped"
        };
    Some(unknown_row(id, reason))
}

// ---------------------------------------------------------------------------
// Sandbox helpers
// ---------------------------------------------------------------------------

/// The Flatpak permissions, when the daemon runs in one and they could be read.
fn flatpak(f: &Facts) -> Option<&FlatpakContext> {
    if f.packaging == Packaging::Flatpak {
        f.sandbox.context.got()
    } else {
        None
    }
}

fn lacks_talk(f: &Facts, name: &str) -> bool {
    flatpak(f).is_some_and(|c| !c.can_talk(name))
}

fn lacks_feature(f: &Facts, feature: &str) -> bool {
    flatpak(f).is_some_and(|c| !c.features.iter().any(|n| n == feature))
}

fn lacks_network(f: &Facts) -> bool {
    flatpak(f).is_some_and(|c| !c.shared.iter().any(|n| n == "network"))
}

/// A service row; for NetworkManager `version` must have been read too.
fn service_row(
    f: &Facts,
    id: &'static str,
    bus_name: &str,
    name: &Obs<NameState>,
    version: Option<&Obs<String>>,
) -> Requirement {
    match name {
        Obs::Failed(e) => unknown_row(id, generic(e)),
        Obs::Got(NameState::Owned) => match version {
            None => ready(id, None),
            Some(Obs::Got(v)) => req(
                id,
                Status::Ready,
                "ok",
                &format!("version {}", version_text(v)),
                None,
            ),
            Some(other) => unknown_row(id, obs_reason(other)),
        },
        Obs::Got(NameState::Activatable) | Obs::Missing => {
            if lacks_talk(f, bus_name) {
                req(id, Status::Blocked, "sandbox_denied", bus_name, None)
            } else if matches!(name, Obs::Got(NameState::Activatable)) {
                req(
                    id,
                    Status::Warning,
                    "not_running_activatable",
                    bus_name,
                    None,
                )
            } else {
                req(id, Status::Blocked, "not_running", bus_name, None)
            }
        }
    }
}

/// Keeps only what a version number is made of.
fn version_text(v: &str) -> String {
    v.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
        .take(32)
        .collect()
}

// ---------------------------------------------------------------------------
// BlueZ
// ---------------------------------------------------------------------------

/// The adapter the runtime would use. It copies the rule of `bluer`'s `default_adapter`
/// (bluer 0.17: `hci0` if present, else the first name in sorted order); the runtime lives in
/// `third_party/rqs_lib`, so the two have to be kept the same by hand.
pub fn select_bt_adapter(names: &[String]) -> Option<usize> {
    if let Some(i) = names.iter().position(|n| n == "hci0") {
        return Some(i);
    }
    names
        .iter()
        .enumerate()
        .min_by(|a, b| a.1.cmp(b.1))
        .map(|(i, _)| i)
}

fn role_words(roles: &[String]) -> String {
    ["central", "peripheral", "central-peripheral"]
        .into_iter()
        .filter(|w| roles.iter().any(|r| r == w))
        .collect::<Vec<_>>()
        .join(" ")
}

fn bluez_rows(f: &Facts) -> Vec<Requirement> {
    let b: &BluezFacts = &f.bluez;
    let service = service_row(f, "bluez.service", "org.bluez", &b.name, None);

    let selected: Option<&BtAdapter> = match &b.adapters {
        Obs::Got(list) => {
            let names: Vec<String> = list.iter().map(|a| a.name.clone()).collect();
            select_bt_adapter(&names).and_then(|i| list.get(i))
        }
        _ => None,
    };
    let adapter =
        gate("bluez.adapter", &[&service]).unwrap_or_else(|| match (&b.adapters, selected) {
            (Obs::Got(_), Some(a)) => ready("bluez.adapter", Some(&a.name)),
            (Obs::Got(_), None) => req("bluez.adapter", Status::Blocked, "no_adapter", "", None),
            (other, _) => unknown_row("bluez.adapter", obs_reason(other)),
        });

    // Judged on the selected adapter, once the service and the adapter are Ready.
    let judged = |id: &'static str, judge: &dyn Fn(&BtAdapter) -> Requirement| match gate(
        id,
        &[&service, &adapter],
    ) {
        Some(skipped) => skipped,
        None => selected.map_or_else(|| unknown_row(id, "query_failed"), judge),
    };

    let powered = judged("bluez.powered", &|a| {
        let dev = Some(a.name.as_str());
        match (&a.power_state, &a.powered) {
            (Obs::Got(s), _) if s == "off-blocked" => {
                req("bluez.powered", Status::Blocked, "rfkill_blocked", "", dev)
            }
            (_, Obs::Got(false)) => req("bluez.powered", Status::Warning, "powered_off", "", dev),
            (_, Obs::Got(true)) => ready("bluez.powered", dev),
            (_, other) => unknown_row("bluez.powered", obs_reason(other)),
        }
    });

    let le = judged("bluez.le", &|a| {
        let dev = Some(a.name.as_str());
        match &a.roles {
            Obs::Missing => unknown_row("bluez.le", "roles_unreported"),
            Obs::Failed(e) => unknown_row("bluez.le", generic(e)),
            Obs::Got(r)
                if r.iter()
                    .any(|x| x.contains("central") || x.contains("peripheral")) =>
            {
                req("bluez.le", Status::Ready, "ok", &role_words(r), dev)
            }
            Obs::Got(_) => req("bluez.le", Status::Blocked, "no_le_roles", "", dev),
        }
    });

    let advertising = judged("bluez.advertising", &|a| {
        let dev = Some(a.name.as_str());
        let id = "bluez.advertising";
        match &a.advertising {
            Obs::Failed(e) => unknown_row(id, generic(e)),
            // An adapter that is off may not have registered its managers yet: not a verdict.
            Obs::Missing if !matches!(a.powered, Obs::Got(true)) => unknown_row(id, "skipped"),
            Obs::Missing => req(id, Status::Blocked, "unsupported", "", dev),
            // Our own advertisement uses a slot, so a full set is a limit, never a block.
            Obs::Got(s) => match (s.active, s.supported) {
                (Some(active), Some(supported)) if active >= supported => req(
                    id,
                    Status::Warning,
                    "no_free_slot",
                    &format!("{active} of {supported} advertising slots in use"),
                    dev,
                ),
                _ => ready(id, dev),
            },
        }
    });

    let scanning = judged("bluez.scanning", &|a| {
        let dev = Some(a.name.as_str());
        let id = "bluez.scanning";
        match (&a.roles, &a.discovery_filters) {
            (Obs::Got(r), _) if !r.is_empty() && !r.iter().any(|x| x.contains("central")) => {
                req(id, Status::Blocked, "no_central_role", "", dev)
            }
            (_, Obs::Got(_)) => ready(id, dev),
            (_, Obs::Missing) => unknown_row(id, "filters_unreported"),
            (_, Obs::Failed(e)) => unknown_row(id, generic(e)),
        }
    });

    let transport = judged("bluez.transport", &|a| {
        let dev = Some(a.name.as_str());
        let id = "bluez.transport";
        if a.gatt_manager {
            ready(id, dev)
        } else if matches!(a.powered, Obs::Got(true)) {
            req(id, Status::Blocked, "no_gatt_manager", "", dev)
        } else {
            unknown_row(id, "skipped")
        }
    });

    let id = "bluez.l2cap";
    let l2cap = if lacks_feature(f, "bluetooth") {
        req(id, Status::Warning, "sandbox_denied", "bluetooth", None)
    } else {
        match &b.l2cap_kernel {
            Obs::Got(true) => ready(id, None),
            Obs::Failed(e) => unknown_row(id, generic(e)),
            Obs::Got(false) | Obs::Missing => unknown_row(id, "kernel_unreported"),
        }
    };

    vec![
        service,
        adapter,
        powered,
        le,
        advertising,
        scanning,
        transport,
        l2cap,
    ]
}

// ---------------------------------------------------------------------------
// NetworkManager
// ---------------------------------------------------------------------------

struct NmRows {
    service: Requirement,
    networking: Requirement,
    wifi_enabled: Requirement,
    hardware: Requirement,
    device: Requirement,
    managed: Requirement,
    firmware: Requirement,
    driver: Requirement,
    permissions: Requirement,
}

impl NmRows {
    fn all(&self) -> [&Requirement; 9] {
        [
            &self.service,
            &self.networking,
            &self.wifi_enabled,
            &self.hardware,
            &self.device,
            &self.managed,
            &self.firmware,
            &self.driver,
            &self.permissions,
        ]
    }

    fn into_vec(self) -> Vec<Requirement> {
        vec![
            self.service,
            self.networking,
            self.wifi_enabled,
            self.hardware,
            self.device,
            self.managed,
            self.firmware,
            self.driver,
            self.permissions,
        ]
    }
}

fn wifi_interface(n: &NmFacts) -> Option<&str> {
    match &n.wifi {
        Obs::Got(w) => w.interface.got().map(String::as_str),
        _ => None,
    }
}

fn switch_row(
    id: &'static str,
    service: &Requirement,
    value: &Obs<bool>,
    off_reason: &str,
) -> Requirement {
    gate(id, &[service]).unwrap_or_else(|| match value {
        Obs::Got(true) => ready(id, None),
        Obs::Got(false) => req(id, Status::Blocked, off_reason, "", None),
        other => unknown_row(id, obs_reason(other)),
    })
}

fn perm_word(p: Perm) -> &'static str {
    match p {
        Perm::Yes => "yes",
        Perm::Auth => "auth",
        Perm::No => "no",
    }
}

fn nm_rows(f: &Facts) -> NmRows {
    let n = &f.nm;
    let service = service_row(
        f,
        "nm.service",
        "org.freedesktop.NetworkManager",
        &n.name,
        Some(&n.version),
    );
    let networking = switch_row("nm.networking", &service, &n.networking_enabled, "disabled");
    let wifi_enabled = switch_row("nm.wifi_enabled", &service, &n.wireless_enabled, "disabled");
    let hardware = switch_row(
        "nm.hardware_enabled",
        &service,
        &n.wireless_hardware_enabled,
        "switch_off",
    );

    let device = gate("nm.device", &[&service]).unwrap_or_else(|| match &n.device_types {
        Obs::Got(types) => match select_wifi_device(types) {
            None => req("nm.device", Status::Blocked, "no_wifi_device", "", None),
            Some(i) => {
                let more = select_wifi_device(&types[i + 1..]).is_some();
                if more {
                    req(
                        "nm.device",
                        Status::Ready,
                        "ok_first_of_many",
                        "more than one Wi-Fi device, the first is used",
                        wifi_interface(n),
                    )
                } else {
                    ready("nm.device", wifi_interface(n))
                }
            }
        },
        other => unknown_row("nm.device", obs_reason(other)),
    });

    let on_device = |id: &'static str, judge: &dyn Fn(&WifiDeviceFacts) -> Requirement| {
        gate(id, &[&service, &device]).unwrap_or_else(|| match &n.wifi {
            Obs::Got(w) => judge(w),
            other => unknown_row(id, obs_reason(other)),
        })
    };
    let dev = wifi_interface(n);

    let managed = on_device("nm.managed", &|w| match &w.managed {
        Obs::Got(true) => ready("nm.managed", dev),
        Obs::Got(false) => req("nm.managed", Status::Blocked, "unmanaged", "", dev),
        other => unknown_row("nm.managed", obs_reason(other)),
    });
    let firmware = on_device("nm.firmware", &|w| match &w.firmware_missing {
        Obs::Got(false) => ready("nm.firmware", dev),
        Obs::Got(true) => req("nm.firmware", Status::Blocked, "firmware_missing", "", dev),
        other => unknown_row("nm.firmware", obs_reason(other)),
    });
    // With the radio switched off the device is "unavailable" by design: the switch rows say so.
    let radio_off = matches!(n.wireless_enabled, Obs::Got(false))
        || matches!(n.wireless_hardware_enabled, Obs::Got(false));
    let driver = on_device("nm.driver", &|w| {
        let id = "nm.driver";
        if matches!(w.plugin_missing, Obs::Got(true)) {
            return req(id, Status::Blocked, "plugin_missing", "", dev);
        }
        match &w.state {
            Obs::Got(NM_STATE_UNAVAILABLE) if !radio_off => {
                let details = match &w.state_reason {
                    Obs::Got(r) => format!("state unavailable, reason {r}"),
                    _ => "state unavailable".to_owned(),
                };
                req(id, Status::Warning, "unavailable", &details, dev)
            }
            Obs::Got(_) => ready(id, dev),
            other => unknown_row(id, obs_reason(other)),
        }
    });

    let permissions = gate("nm.permissions", &[&service]).unwrap_or_else(|| {
        let id = "nm.permissions";
        let Obs::Got(p) = &n.permissions else {
            return unknown_row(id, obs_reason(&n.permissions));
        };
        let mut denied = false;
        let mut auth = false;
        let mut unknown = None;
        let mut details = Vec::new();
        for (key, value) in [
            ("network-control", &p.network_control),
            ("settings.modify.system", &p.settings_modify_system),
        ] {
            match value {
                Obs::Got(Perm::Yes) => {}
                Obs::Got(v) => {
                    denied |= *v == Perm::No;
                    auth |= *v == Perm::Auth;
                    details.push(format!("{key}={}", perm_word(*v)));
                }
                other => unknown = Some(obs_reason(other)),
            }
        }
        let details = details.join(" ");
        if denied {
            req(id, Status::Blocked, "denied", &details, None)
        } else if auth {
            req(id, Status::Warning, "auth_required", &details, None)
        } else if let Some(reason) = unknown {
            unknown_row(id, reason)
        } else {
            ready(id, None)
        }
    });

    NmRows {
        service,
        networking,
        wifi_enabled,
        hardware,
        device,
        managed,
        firmware,
        driver,
        permissions,
    }
}

// ---------------------------------------------------------------------------
// Wi-Fi client, access point, addressing
// ---------------------------------------------------------------------------

fn wifi_client_row(f: &Facts, nm: &NmRows) -> Requirement {
    let id = "wifi.client";
    let dev = wifi_interface(&f.nm);
    if lacks_network(f) {
        return req(id, Status::Blocked, "sandbox_no_network", "network", dev);
    }
    let prerequisites = nm.all();
    if prerequisites.iter().any(|r| r.status == Status::Blocked) {
        req(id, Status::Blocked, "prerequisite_blocked", "", dev)
    } else if prerequisites.iter().any(|r| r.status == Status::Unknown) {
        req(id, Status::Unknown, "prerequisite_uncertain", "", dev)
    } else {
        ready(id, dev)
    }
}

fn wifi_ap_row(f: &Facts, nm: &NmRows, ev: &SessionEvidence) -> Requirement {
    let id = "wifi.ap";
    let n = &f.nm;
    let dev = wifi_interface(n);
    if lacks_network(f) {
        return req(id, Status::Blocked, "sandbox_no_network", "network", dev);
    }
    if let Some(skipped) = gate(id, &[&nm.service, &nm.device]) {
        return skipped;
    }
    let Obs::Got(w) = &n.wifi else {
        return unknown_row(id, obs_reason(&n.wifi));
    };
    let caps = match &w.capabilities {
        Obs::Got(c) => *c,
        other => return unknown_row(id, obs_reason(other)),
    };
    let hex = format!("capabilities {caps:#x}");
    if caps != 0 {
        if caps & CAP_AP == 0 {
            return req(id, Status::Blocked, "no_ap_mode", &hex, dev);
        }
        if caps & CAP_CCMP == 0 || caps & CAP_RSN == 0 {
            return req(id, Status::Blocked, "no_wpa2", &hex, dev);
        }
        // Without FREQ_VALID the band bits say nothing: not a verdict.
        if caps & CAP_FREQ_VALID != 0 && caps & (CAP_FREQ_2GHZ | CAP_FREQ_5GHZ) == 0 {
            return req(id, Status::Blocked, "no_band", &hex, dev);
        }
    }
    let share = match &n.permissions {
        Obs::Got(p) => &p.wifi_share_protected,
        other => return unknown_row(id, obs_reason(other)),
    };
    if matches!(share, Obs::Got(Perm::No)) {
        return req(
            id,
            Status::Blocked,
            "share_denied",
            "wifi.share.protected=no",
            dev,
        );
    }
    if caps == 0 {
        return req(
            id,
            Status::Warning,
            "caps_unreported",
            "capabilities 0x0",
            dev,
        );
    }
    match share {
        Obs::Got(Perm::Auth) => req(
            id,
            Status::Warning,
            "share_auth_required",
            "wifi.share.protected=auth",
            dev,
        ),
        Obs::Got(Perm::Yes) => {
            let details = if ev.hotspot_failed && !ev.hotspot_started {
                "the last hotspot attempt failed"
            } else {
                ""
            };
            req(id, Status::Ready, "ok", details, dev)
        }
        other => unknown_row(id, obs_reason(other)),
    }
}

fn wifi_dhcp_row(f: &Facts, ev: &SessionEvidence) -> Requirement {
    let id = "wifi.dhcp";
    if ev.hotspot_started {
        return ready_with(id, "worked_this_session");
    }
    let shared_failed = match &f.nm.wifi {
        Obs::Got(w) => matches!(
            w.state_reason,
            Obs::Got(NM_REASON_SHARED_START_FAILED | NM_REASON_SHARED_FAILED)
        ),
        _ => false,
    };
    if shared_failed {
        return req(
            id,
            Status::Warning,
            "shared_start_failed",
            "NetworkManager could not start connection sharing",
            wifi_interface(&f.nm),
        );
    }
    // The host's /usr cannot be seen from a sandbox: an Info row, so what cannot be looked for
    // does not lower receiving. The first hotspot settles it, and a failed start still gives the
    // critical `shared_start_failed` above.
    if f.packaging == Packaging::Flatpak {
        return Requirement {
            category: Category::Info,
            ..unknown_row(id, "sandbox_unobservable")
        };
    }
    match &f.dhcp.dnsmasq {
        Obs::Got(true) => ready_with(id, "binary_present"),
        Obs::Got(false) => req(id, Status::Warning, "not_found", "dnsmasq", None),
        other => unknown_row(id, obs_reason(other)),
    }
}

fn ready_with(id: &'static str, reason: &str) -> Requirement {
    req(id, Status::Ready, reason, "", None)
}

// ---------------------------------------------------------------------------
// Firewall and sandbox
// ---------------------------------------------------------------------------

fn firewall_from_facts(f: &Facts) -> Requirement {
    let id = "firewall.integration";
    let fw = &f.firewall;
    match &fw.name {
        Obs::Failed(e) => unknown_row(id, generic(e)),
        Obs::Got(NameState::Activatable) | Obs::Missing => {
            if lacks_talk(f, "org.fedoraproject.FirewallD1") {
                req(
                    id,
                    Status::Warning,
                    "sandbox_denied",
                    "org.fedoraproject.FirewallD1",
                    None,
                )
            } else {
                req(id, Status::Warning, "not_running", "", None)
            }
        }
        Obs::Got(NameState::Owned) => {
            let zone_missing = match &fw.zones {
                Obs::Got(z) => !z.iter().any(|n| n == "nm-shared"),
                _ => false,
            };
            match &fw.polkit {
                Obs::Got(PolkitVerdict::Denied) => {
                    req(id, Status::Blocked, "permission_denied", "", None)
                }
                _ if zone_missing => req(id, Status::Warning, "zone_missing", "nm-shared", None),
                Obs::Got(PolkitVerdict::Challenge) => {
                    req(id, Status::Warning, "permission_auth", "", None)
                }
                Obs::Got(PolkitVerdict::Authorized) => match &fw.zones {
                    Obs::Got(_) => ready(id, None),
                    // The zone list is read only after polkit allowed it: not read, not known.
                    Obs::Missing => unknown_row(id, "permission_unverifiable"),
                    other => unknown_row(id, obs_reason(other)),
                },
                Obs::Failed(ProbeError::Timeout) => unknown_row(id, "timeout"),
                Obs::Failed(ProbeError::BusUnreachable) => unknown_row(id, "bus_unreachable"),
                Obs::Missing | Obs::Failed(_) => unknown_row(id, "permission_unverifiable"),
            }
        }
    }
}

/// firewalld's row, with what this session's hotspot attempts showed: a port that was opened
/// settles it; a consent to an open zone is what makes a refused port usable.
fn firewall_row(f: &Facts, ev: &SessionEvidence) -> Requirement {
    let id = "firewall.integration";
    if ev.firewall == FirewallEvidence::PortOpened {
        return ready(id, None);
    }
    let base = firewall_from_facts(f);
    if base.status == Status::Ready {
        return base;
    }
    if ev.firewall_consent {
        return req(
            id,
            Status::Warning,
            "consent_given",
            "an open firewall zone is allowed for this session",
            None,
        );
    }
    match ev.firewall {
        FirewallEvidence::Refused(BlockReason::FirewallDenied | BlockReason::FirewallMissing) => {
            req(id, Status::Blocked, "denied_this_session", "", None)
        }
        _ => base,
    }
}

fn sandbox_row(f: &Facts) -> Option<Requirement> {
    if f.packaging != Packaging::Flatpak {
        return None;
    }
    let id = "sandbox.permissions";
    let c = match &f.sandbox.context {
        Obs::Got(c) => c,
        other => return Some(unknown_row(id, obs_reason(other))),
    };
    let mut missing = Vec::new();
    if !c.shared.iter().any(|s| s == "network") {
        missing.push("network");
    }
    if !c.features.iter().any(|s| s == "bluetooth") {
        missing.push("bluetooth");
    }
    for (name, word) in [
        ("org.bluez", "bluez"),
        ("org.freedesktop.NetworkManager", "networkmanager"),
        ("org.fedoraproject.FirewallD1", "firewalld"),
        ("org.freedesktop.PolicyKit1", "polkit"),
    ] {
        if !c.can_talk(name) {
            missing.push(word);
        }
    }
    if !missing.is_empty() {
        return Some(req(
            id,
            Status::Warning,
            "missing",
            &format!("missing: {}", missing.join(" ")),
            None,
        ));
    }
    if c.filesystems.iter().any(|s| s == "host" || s == "host-os") {
        return Some(req(
            id,
            Status::Warning,
            "filesystem_host",
            "the sandbox can read the whole file system",
            None,
        ));
    }
    Some(ready(id, None))
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// The rows of one run, in a fixed order: BlueZ, NetworkManager, Wi-Fi, firewall, sandbox.
pub fn evaluate(f: &Facts, ev: &SessionEvidence) -> Vec<Requirement> {
    let mut rows = bluez_rows(f);
    let nm = nm_rows(f);
    let client = wifi_client_row(f, &nm);
    let ap = wifi_ap_row(f, &nm, ev);
    rows.extend(nm.into_vec());
    rows.push(client);
    rows.push(ap);
    rows.push(wifi_dhcp_row(f, ev));
    rows.push(firewall_row(f, ev));
    rows.extend(sandbox_row(f));
    rows
}

fn capability(rows: &[Requirement], key: &str) -> CapabilityStatus {
    let fed = || {
        rows.iter()
            .filter(|r| r.category != Category::Info && r.affects.iter().any(|a| a == key))
    };
    let critical = |r: &&Requirement| r.category == Category::Critical;
    if fed().any(|r| critical(&r) && r.status == Status::Blocked) {
        CapabilityStatus::Unavailable
    } else if fed().any(|r| critical(&r) && matches!(r.status, Status::Unknown | Status::Checking))
    {
        CapabilityStatus::Unknown
    } else if fed().any(|r| {
        r.status == Status::Warning
            || (!critical(&r) && matches!(r.status, Status::Blocked | Status::Unknown))
    }) {
        CapabilityStatus::Degraded
    } else {
        CapabilityStatus::Available
    }
}

/// What each capability amounts to, from the non-info rows that feed it:
/// any critical row Blocked is `unavailable`; else any critical row Unknown or Checking is
/// `unknown`; else any Warning, or a non-critical row Blocked or Unknown, is `degraded`; else
/// `available`.
pub fn derive(rows: &[Requirement]) -> Capabilities {
    Capabilities {
        sending: capability(rows, SENDING),
        receiving: capability(rows, RECEIVING),
        join_network: capability(rows, JOIN_NETWORK),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::requirements::facts::{
        AdvSlots, DhcpFacts, FirewallFacts, NmPermissions, SandboxFacts,
    };

    fn good_adapter() -> BtAdapter {
        BtAdapter {
            name: "hci0".into(),
            powered: Obs::Got(true),
            power_state: Obs::Got("on".into()),
            roles: Obs::Got(vec!["central".into(), "peripheral".into()]),
            advertising: Obs::Got(AdvSlots {
                active: Some(0),
                supported: Some(5),
            }),
            gatt_manager: true,
            discovery_filters: Obs::Got(vec!["Transport".into()]),
        }
    }

    fn good() -> Facts {
        Facts {
            packaging: Packaging::Native,
            sandbox: SandboxFacts::default(),
            bluez: BluezFacts {
                name: Obs::Got(NameState::Owned),
                adapters: Obs::Got(vec![good_adapter()]),
                l2cap_kernel: Obs::Got(true),
            },
            nm: NmFacts {
                name: Obs::Got(NameState::Owned),
                version: Obs::Got("1.46.0".into()),
                networking_enabled: Obs::Got(true),
                wireless_enabled: Obs::Got(true),
                wireless_hardware_enabled: Obs::Got(true),
                device_types: Obs::Got(vec![1, 2]),
                wifi: Obs::Got(WifiDeviceFacts {
                    interface: Obs::Got("wlan0".into()),
                    managed: Obs::Got(true),
                    firmware_missing: Obs::Got(false),
                    plugin_missing: Obs::Got(false),
                    state: Obs::Got(30),
                    state_reason: Obs::Got(0),
                    capabilities: Obs::Got(
                        CAP_CCMP | CAP_RSN | CAP_AP | CAP_FREQ_VALID | CAP_FREQ_5GHZ,
                    ),
                }),
                permissions: Obs::Got(NmPermissions {
                    network_control: Obs::Got(Perm::Yes),
                    settings_modify_system: Obs::Got(Perm::Yes),
                    wifi_share_protected: Obs::Got(Perm::Yes),
                }),
            },
            firewall: FirewallFacts {
                name: Obs::Got(NameState::Owned),
                zones: Obs::Got(vec!["public".into(), "nm-shared".into()]),
                polkit: Obs::Got(PolkitVerdict::Authorized),
            },
            dhcp: DhcpFacts {
                dnsmasq: Obs::Got(true),
            },
        }
    }

    fn caps(f: &Facts) -> Capabilities {
        derive(&evaluate(f, &SessionEvidence::default()))
    }

    fn find<'a>(rows: &'a [Requirement], id: &str) -> &'a Requirement {
        rows.iter().find(|r| r.id == id).unwrap()
    }

    #[test]
    fn everything_good_is_available() {
        let rows = evaluate(&good(), &SessionEvidence::default());
        assert!(rows.iter().all(|r| r.status == Status::Ready), "{rows:#?}");
        let c = derive(&rows);
        assert_eq!(c.sending, CapabilityStatus::Available);
        assert_eq!(c.receiving, CapabilityStatus::Available);
        assert_eq!(c.join_network, CapabilityStatus::Available);
    }

    #[test]
    fn bluez_absent_blocks_sending_and_receiving_only() {
        let mut f = good();
        f.bluez.name = Obs::Missing;
        let c = caps(&f);
        assert_eq!(c.sending, CapabilityStatus::Unavailable);
        assert_eq!(c.receiving, CapabilityStatus::Unavailable);
        assert_eq!(c.join_network, CapabilityStatus::Available);
        let rows = evaluate(&f, &SessionEvidence::default());
        assert_eq!(find(&rows, "bluez.service").reason_code, "not_running");
        assert_eq!(find(&rows, "bluez.adapter").reason_code, "skipped");
    }

    #[test]
    fn nm_absent_blocks_all_three() {
        let mut f = good();
        f.nm.name = Obs::Missing;
        let c = caps(&f);
        assert_eq!(c.sending, CapabilityStatus::Unavailable);
        assert_eq!(c.receiving, CapabilityStatus::Unavailable);
        assert_eq!(c.join_network, CapabilityStatus::Unavailable);
    }

    #[test]
    fn no_ap_mode_blocks_receiving_only() {
        let mut f = good();
        if let Obs::Got(w) = &mut f.nm.wifi {
            w.capabilities = Obs::Got(CAP_CCMP | CAP_RSN);
        }
        let c = caps(&f);
        assert_eq!(c.sending, CapabilityStatus::Available);
        assert_eq!(c.receiving, CapabilityStatus::Unavailable);
        assert_eq!(c.join_network, CapabilityStatus::Available);
    }

    #[test]
    fn firewalld_absent_degrades_receiving() {
        let mut f = good();
        f.firewall.name = Obs::Missing;
        let c = caps(&f);
        assert_eq!(c.sending, CapabilityStatus::Available);
        assert_eq!(c.receiving, CapabilityStatus::Degraded);
    }

    #[test]
    fn nm_auth_is_a_warning_not_a_block() {
        let mut f = good();
        if let Obs::Got(p) = &mut f.nm.permissions {
            p.network_control = Obs::Got(Perm::Auth);
        }
        let rows = evaluate(&f, &SessionEvidence::default());
        let row = find(&rows, "nm.permissions");
        assert_eq!(row.status, Status::Warning);
        assert_eq!(row.reason_code, "auth_required");
        assert_eq!(row.details, "network-control=auth");
        assert_eq!(derive(&rows).sending, CapabilityStatus::Degraded);
    }

    #[test]
    fn flatpak_dnsmasq_is_unknown() {
        let mut f = good();
        f.packaging = Packaging::Flatpak;
        f.dhcp.dnsmasq = Obs::Missing;
        f.sandbox.context = Obs::Got(FlatpakContext {
            system_talk_names: vec![
                "org.bluez".into(),
                "org.freedesktop.NetworkManager".into(),
                "org.fedoraproject.FirewallD1".into(),
                "org.freedesktop.PolicyKit1".into(),
            ],
            features: vec!["bluetooth".into()],
            shared: vec!["network".into()],
            filesystems: vec![],
            sockets: vec![],
        });
        let rows = evaluate(&f, &SessionEvidence::default());
        let dhcp = find(&rows, "wifi.dhcp");
        assert_eq!(
            (dhcp.status, dhcp.reason_code.as_str()),
            (Status::Unknown, "sandbox_unobservable")
        );
        // Info: what cannot be looked for from a sandbox does not lower receiving.
        assert_eq!(dhcp.category, Category::Info);
        assert_eq!(derive(&rows).receiving, CapabilityStatus::Available);
        assert_eq!(derive(&rows).sending, CapabilityStatus::Available);
    }

    #[test]
    fn no_system_bus_is_all_unknown() {
        let f = Facts::failed(Packaging::Native, ProbeError::BusUnreachable);
        let rows = evaluate(&f, &SessionEvidence::default());
        assert!(
            rows.iter().all(|r| r.status == Status::Unknown),
            "{rows:#?}"
        );
        let c = derive(&rows);
        assert_eq!(c.sending, CapabilityStatus::Unknown);
        assert_eq!(c.receiving, CapabilityStatus::Unknown);
        assert_eq!(c.join_network, CapabilityStatus::Unknown);
    }

    #[test]
    fn a_timeout_stays_a_timeout_down_the_chain() {
        let mut f = good();
        f.bluez = BluezFacts::failed(ProbeError::Timeout);
        let rows = evaluate(&f, &SessionEvidence::default());
        assert_eq!(find(&rows, "bluez.service").reason_code, "timeout");
        assert_eq!(find(&rows, "bluez.powered").reason_code, "timeout");
    }

    #[test]
    fn unknown_never_becomes_blocked() {
        let f = Facts::failed(Packaging::Native, ProbeError::Malformed);
        let rows = evaluate(&f, &SessionEvidence::default());
        assert!(rows.iter().all(|r| r.status != Status::Blocked));
    }

    #[test]
    fn evidence_settles_only_its_rows() {
        let mut f = good();
        f.firewall.name = Obs::Missing;
        f.dhcp.dnsmasq = Obs::Got(false);
        let mut ev = SessionEvidence::default();
        ev.apply(crate::requirements::facts::RuntimeEvidence::HotspotStarted);
        ev.apply(crate::requirements::facts::RuntimeEvidence::FirewallPortOpened);
        let rows = evaluate(&f, &ev);
        assert_eq!(find(&rows, "wifi.dhcp").reason_code, "worked_this_session");
        assert_eq!(find(&rows, "firewall.integration").status, Status::Ready);
        assert_eq!(find(&rows, "bluez.adapter").status, Status::Ready);
    }

    #[test]
    fn refusal_blocks_unless_consent_is_given() {
        let mut f = good();
        f.firewall.name = Obs::Missing;
        let mut ev = SessionEvidence::default();
        ev.apply(
            crate::requirements::facts::RuntimeEvidence::FirewallRefused(
                BlockReason::FirewallMissing,
            ),
        );
        let rows = evaluate(&f, &ev);
        assert_eq!(
            find(&rows, "firewall.integration").reason_code,
            "denied_this_session"
        );
        ev.apply(crate::requirements::facts::RuntimeEvidence::FirewallConsent(true));
        let rows = evaluate(&f, &ev);
        assert_eq!(
            find(&rows, "firewall.integration").reason_code,
            "consent_given"
        );
    }

    #[test]
    fn rfkill_is_blocked_and_power_off_is_a_warning() {
        let mut f = good();
        if let Obs::Got(list) = &mut f.bluez.adapters {
            list[0].powered = Obs::Got(false);
            list[0].power_state = Obs::Got("off".into());
        }
        let rows = evaluate(&f, &SessionEvidence::default());
        assert_eq!(find(&rows, "bluez.powered").reason_code, "powered_off");
        assert_eq!(derive(&rows).sending, CapabilityStatus::Degraded);
        if let Obs::Got(list) = &mut f.bluez.adapters {
            list[0].power_state = Obs::Got("off-blocked".into());
        }
        let rows = evaluate(&f, &SessionEvidence::default());
        assert_eq!(find(&rows, "bluez.powered").status, Status::Blocked);
        assert_eq!(derive(&rows).receiving, CapabilityStatus::Unavailable);
    }

    #[test]
    fn the_adapter_is_picked_like_bluer_does() {
        let names = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(select_bt_adapter(&names(&["hci1", "hci0"])), Some(1));
        assert_eq!(select_bt_adapter(&names(&["hci2", "hci1"])), Some(1));
        assert_eq!(select_bt_adapter(&[]), None);
    }

    #[test]
    fn sanitize_strips_and_cuts() {
        assert_eq!(sanitize("a\u{7}b<c>d\n", 10), "abcd");
        assert_eq!(sanitize("abcdef", 3), "abc");
        assert_eq!(sanitize("a\u{200B}b\u{202E}c\u{2066}d\u{FEFF}", 10), "abcd");
        assert_eq!(sanitize_device("wlx00e04c680001"), "wlx\u{2026}");
        assert_eq!(sanitize_device("wlan0"), "wlan0");
        assert!(sanitize_device(&"x".repeat(100)).chars().count() <= MAX_DEVICE);
    }

    #[test]
    fn derive_table() {
        fn row(status: Status, category: Category) -> Requirement {
            Requirement {
                id: "x".into(),
                status,
                category,
                reason_code: "ok".into(),
                details: String::new(),
                device: None,
                verification: String::new(),
                affects: vec![SENDING.into()],
            }
        }
        let s = |rows: &[Requirement]| derive(rows).sending;
        assert_eq!(s(&[]), CapabilityStatus::Available);
        assert_eq!(
            s(&[
                row(Status::Blocked, Category::Critical),
                row(Status::Unknown, Category::Critical)
            ]),
            CapabilityStatus::Unavailable
        );
        assert_eq!(
            s(&[
                row(Status::Checking, Category::Critical),
                row(Status::Warning, Category::Critical)
            ]),
            CapabilityStatus::Unknown
        );
        assert_eq!(
            s(&[row(Status::Blocked, Category::Security)]),
            CapabilityStatus::Degraded
        );
        assert_eq!(
            s(&[row(Status::Unknown, Category::Warning)]),
            CapabilityStatus::Degraded
        );
        assert_eq!(
            s(&[row(Status::Blocked, Category::Info)]),
            CapabilityStatus::Available
        );
        assert_eq!(
            s(&[row(Status::Warning, Category::Critical)]),
            CapabilityStatus::Degraded
        );
    }
}
