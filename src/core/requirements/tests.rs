//! Tests of the requirements engine with fake probes. Every scenario starts from
//! `Facts::healthy()`, changes one thing, and asserts the capabilities and the rows that
//! change; the rows and capabilities it does not mention must equal the baseline.
//!
//! No sleeps: the engine is driven with tiny configured durations, a probe that waits on a
//! gate until released, and call counters. The engine's `Instant`s are the real ones (the
//! crate's dev-dependencies have no tokio `test-util`), so the tests never depend on a
//! duration elapsing for their result, except the group timeout, which waits for the engine's
//! own (configured) deadline.

use std::{future::Future, sync::Arc, time::Duration};

use super::{
    SCHEMA_VERSION,
    engine::{EngineConfig, Invalidation, RequirementsEngine},
    facts::{
        BtAdapter, DhcpFacts, Facts, FirewallFacts, NameState, NmFacts, Obs, Perm, PolkitVerdict,
        ProbeError, RuntimeEvidence, SandboxFacts, SessionEvidence, WifiDeviceFacts,
    },
    fake::{
        CAP_AP, CAP_CCMP, CAP_FREQ_5GHZ, CAP_FREQ_VALID, CAP_RSN, FakeRequirementsProbe, Group,
        healthy_adapter,
    },
    model::{
        Capabilities, CapabilityStatus, Category, MAX_DETAILS, MAX_DEVICE, MAX_JSON_BYTES,
        MAX_ROWS, MAX_VERIFICATION, Packaging, Requirement, RequirementsReport, Status,
    },
    policy::{EnableVerdict, enable_verdict},
    rules,
};
use crate::link::{BlockReason, select_wifi_device};
use CapabilityStatus::{Available, Degraded, Unavailable, Unknown};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn eval(f: &Facts) -> Vec<Requirement> {
    rules::evaluate(f, &SessionEvidence::default())
}

fn caps(rows: &[Requirement]) -> Capabilities {
    rules::derive(rows)
}

fn caps3(
    sending: CapabilityStatus,
    receiving: CapabilityStatus,
    join: CapabilityStatus,
) -> Capabilities {
    Capabilities {
        sending,
        receiving,
        join_network: join,
    }
}

fn row<'a>(rows: &'a [Requirement], id: &str) -> &'a Requirement {
    rows.iter()
        .find(|r| r.id == id)
        .unwrap_or_else(|| panic!("no row {id}"))
}

fn base_rows() -> Vec<Requirement> {
    eval(&Facts::healthy())
}

/// The ids whose status or reason differ from `base` (or that exist in only one of them).
fn changed(base: &[Requirement], now: &[Requirement]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for r in now {
        match base.iter().find(|b| b.id == r.id) {
            Some(b) if (b.status, &b.reason_code) == (r.status, &r.reason_code) => {}
            _ => ids.push(r.id.clone()),
        }
    }
    for b in base {
        if !now.iter().any(|r| r.id == b.id) {
            ids.push(b.id.clone());
        }
    }
    ids.sort();
    ids
}

fn assert_changed(base: &[Requirement], now: &[Requirement], expected: &[&str]) {
    let mut want: Vec<String> = expected.iter().map(|s| (*s).to_owned()).collect();
    want.sort();
    assert_eq!(
        changed(base, now),
        want,
        "rows that differ from the baseline"
    );
}

/// Evaluates `f`, asserts the capabilities and exactly which rows differ from the healthy
/// baseline; returns the rows.
fn check(f: &Facts, expected: Capabilities, ids: &[&str]) -> Vec<Requirement> {
    let rows = eval(f);
    assert_eq!(caps(&rows), expected, "capabilities");
    assert_changed(&base_rows(), &rows, ids);
    rows
}

fn status(rows: &[Requirement], id: &str) -> (Status, String) {
    let r = row(rows, id);
    (r.status, r.reason_code.clone())
}

fn adapter_mut(f: &mut Facts) -> &mut BtAdapter {
    match &mut f.bluez.adapters {
        Obs::Got(list) => &mut list[0],
        other => panic!("{other:?}"),
    }
}

fn wifi_mut(f: &mut Facts) -> &mut WifiDeviceFacts {
    match &mut f.nm.wifi {
        Obs::Got(w) => w,
        other => panic!("{other:?}"),
    }
}

/// Everything is cached for an hour, nothing is refreshed by itself, no gaps between runs.
fn quiet_cfg() -> EngineConfig {
    EngineConfig {
        per_call: Duration::from_secs(1),
        total: Duration::from_secs(5),
        ttl: Duration::from_secs(3600),
        debounce: Duration::from_secs(3600),
        min_event_gap: Duration::ZERO,
        min_forced_gap: Duration::ZERO,
        max_parallel_groups: 3,
    }
}

type Engine = Arc<RequirementsEngine<FakeRequirementsProbe>>;

fn engine_with(f: Facts, cfg: EngineConfig) -> (Engine, FakeRequirementsProbe) {
    let probe = FakeRequirementsProbe::new(f);
    (RequirementsEngine::new(probe.clone(), cfg), probe)
}

/// A test that waits for something that never comes fails instead of hanging.
async fn bounded<T>(f: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), f)
        .await
        .expect("the test waited too long")
}

/// Sets the facts, drops the cache like an event would, and returns the next report.
async fn rerun(
    engine: &Engine,
    probe: &FakeRequirementsProbe,
    f: Facts,
) -> Arc<RequirementsReport> {
    probe.set(f);
    engine.invalidate(Invalidation::NetworkManager);
    bounded(engine.report(false)).await
}

// ---------------------------------------------------------------------------
// The baseline
// ---------------------------------------------------------------------------

#[tokio::test]
async fn all_available() {
    let (engine, probe) = engine_with(Facts::healthy(), quiet_cfg());
    let report = bounded(engine.report(false)).await;
    assert_eq!(report.capabilities, caps3(Available, Available, Available));
    assert_eq!(report.requirements.len(), 21);
    for r in &report.requirements {
        // `wifi.dhcp` names how it became Ready (contract table); every other row says `ok`.
        let expected = if r.id == "wifi.dhcp" {
            "binary_present"
        } else {
            "ok"
        };
        assert_eq!(
            (r.status, r.reason_code.as_str()),
            (Status::Ready, expected),
            "{}",
            r.id
        );
    }
    assert_eq!(report.schema_version, SCHEMA_VERSION);
    assert_eq!(report.packaging, Packaging::Native);
    assert_eq!(report.ttl_ms, 3_600_000);
    assert_eq!(report.core_version, env!("CARGO_PKG_VERSION"));
    assert!(report.generated_at_ms > 0);
    for g in [
        Group::Sandbox,
        Group::Bluez,
        Group::Nm,
        Group::Firewall,
        Group::Dhcp,
    ] {
        assert_eq!(probe.calls(g), 1);
    }
}

// ---------------------------------------------------------------------------
// BlueZ
// ---------------------------------------------------------------------------

const BLUEZ_BELOW_SERVICE: [&str; 6] = [
    "bluez.adapter",
    "bluez.powered",
    "bluez.le",
    "bluez.advertising",
    "bluez.scanning",
    "bluez.transport",
];

fn bluez_ids(extra: &[&'static str]) -> Vec<&'static str> {
    let mut ids = vec!["bluez.service"];
    ids.extend(BLUEZ_BELOW_SERVICE);
    ids.extend(extra);
    ids
}

#[test]
fn bluez_missing_blocks_ble_only() {
    let mut f = Facts::healthy();
    f.bluez.name = Obs::Missing;
    let rows = check(
        &f,
        caps3(Unavailable, Unavailable, Available),
        &bluez_ids(&[]),
    );
    assert_eq!(
        status(&rows, "bluez.service"),
        (Status::Blocked, "not_running".into())
    );
    for id in BLUEZ_BELOW_SERVICE {
        assert_eq!(
            status(&rows, id),
            (Status::Unknown, "skipped".into()),
            "{id}"
        );
    }
    // Nothing of NetworkManager moved.
    assert!(
        rows.iter()
            .filter(|r| r.id.starts_with("nm."))
            .all(|r| r.status == Status::Ready)
    );
}

#[test]
fn bluez_activatable_is_warning() {
    let mut f = Facts::healthy();
    f.bluez.name = Obs::Got(NameState::Activatable);
    let rows = eval(&f);
    assert_eq!(
        status(&rows, "bluez.service"),
        (Status::Warning, "not_running_activatable".into())
    );
    // Not running yet: the rows below cannot be judged, so they are Unknown, never Blocked.
    assert!(rows.iter().all(|r| r.status != Status::Blocked));
    assert_eq!(caps(&rows), caps3(Unknown, Unknown, Available));
    assert_changed(&base_rows(), &rows, &bluez_ids(&[]));
}

#[test]
fn powered_off_warning_rfkill_blocked() {
    // Off: the runtime switches it on by itself.
    let mut f = Facts::healthy();
    let a = adapter_mut(&mut f);
    a.powered = Obs::Got(false);
    a.power_state = Obs::Got("off".into());
    let rows = check(&f, caps3(Degraded, Degraded, Available), &["bluez.powered"]);
    assert_eq!(
        status(&rows, "bluez.powered"),
        (Status::Warning, "powered_off".into())
    );
    assert_eq!(row(&rows, "bluez.powered").device.as_deref(), Some("hci0"));

    // Blocked by rfkill: it cannot be switched on.
    let mut f = Facts::healthy();
    let a = adapter_mut(&mut f);
    a.powered = Obs::Got(false);
    a.power_state = Obs::Got("off-blocked".into());
    let rows = check(
        &f,
        caps3(Unavailable, Unavailable, Available),
        &["bluez.powered"],
    );
    assert_eq!(
        status(&rows, "bluez.powered"),
        (Status::Blocked, "rfkill_blocked".into())
    );
}

#[test]
fn no_bluetooth_adapter_and_no_le() {
    let mut f = Facts::healthy();
    f.bluez.adapters = Obs::Got(vec![]);
    let rows = check(
        &f,
        caps3(Unavailable, Unavailable, Available),
        &bluez_ids(&[])[1..],
    );
    assert_eq!(
        status(&rows, "bluez.adapter"),
        (Status::Blocked, "no_adapter".into())
    );
    assert_eq!(status(&rows, "bluez.service").0, Status::Ready);

    // Only receiving needs the advertising manager.
    let mut f = Facts::healthy();
    adapter_mut(&mut f).advertising = Obs::Missing;
    let rows = check(
        &f,
        caps3(Available, Unavailable, Available),
        &["bluez.advertising"],
    );
    assert_eq!(
        status(&rows, "bluez.advertising"),
        (Status::Blocked, "unsupported".into())
    );

    // A full set of advertising slots is a limit, never a block.
    let mut f = Facts::healthy();
    adapter_mut(&mut f).advertising = Obs::Got(super::facts::AdvSlots {
        active: Some(5),
        supported: Some(5),
    });
    check(
        &f,
        caps3(Available, Degraded, Available),
        &["bluez.advertising"],
    );
}

#[test]
fn bt_prefers_hci0() {
    let names = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        rules::select_bt_adapter(&names(&["hci1", "hci0", "hci2"])),
        Some(1)
    );
    assert_eq!(rules::select_bt_adapter(&names(&["hci2", "hci1"])), Some(1));
    assert_eq!(rules::select_bt_adapter(&names(&["hci0"])), Some(0));
    assert_eq!(rules::select_bt_adapter(&[]), None);

    // hci1 is fine, hci0 is blocked by rfkill: the runtime uses hci0, so the report judges hci0.
    let mut f = Facts::healthy();
    let mut blocked = healthy_adapter("hci0");
    blocked.powered = Obs::Got(false);
    blocked.power_state = Obs::Got("off-blocked".into());
    f.bluez.adapters = Obs::Got(vec![blocked, healthy_adapter("hci1")]);
    let rows = check(
        &f,
        caps3(Unavailable, Unavailable, Available),
        &["bluez.powered"],
    );
    assert_eq!(row(&rows, "bluez.adapter").device.as_deref(), Some("hci0"));

    // hci0 fine, hci1 blocked: nothing is wrong.
    let mut f = Facts::healthy();
    let mut blocked = healthy_adapter("hci1");
    blocked.power_state = Obs::Got("off-blocked".into());
    f.bluez.adapters = Obs::Got(vec![healthy_adapter("hci0"), blocked]);
    check(&f, caps3(Available, Available, Available), &[]);
}

// ---------------------------------------------------------------------------
// NetworkManager
// ---------------------------------------------------------------------------

const NM_ROWS: [&str; 9] = [
    "nm.service",
    "nm.networking",
    "nm.wifi_enabled",
    "nm.hardware_enabled",
    "nm.device",
    "nm.managed",
    "nm.firmware",
    "nm.driver",
    "nm.permissions",
];

#[test]
fn nm_absent_blocks_three() {
    let mut f = Facts::healthy();
    f.nm.name = Obs::Missing;
    let mut ids = NM_ROWS.to_vec();
    ids.extend(["wifi.client", "wifi.ap"]);
    let rows = check(&f, caps3(Unavailable, Unavailable, Unavailable), &ids);
    assert_eq!(
        status(&rows, "nm.service"),
        (Status::Blocked, "not_running".into())
    );
    assert_eq!(
        status(&rows, "wifi.client"),
        (Status::Blocked, "prerequisite_blocked".into())
    );
    // BlueZ did not move.
    assert!(
        rows.iter()
            .filter(|r| r.id.starts_with("bluez."))
            .all(|r| r.status == Status::Ready)
    );
}

#[test]
fn no_wifi_device() {
    let mut f = Facts::healthy();
    f.nm.device_types = Obs::Got(vec![1]);
    f.nm.wifi = Obs::Missing;
    let rows = check(
        &f,
        caps3(Unavailable, Unavailable, Unavailable),
        &[
            "nm.device",
            "nm.managed",
            "nm.firmware",
            "nm.driver",
            "wifi.client",
            "wifi.ap",
        ],
    );
    assert_eq!(
        status(&rows, "nm.device"),
        (Status::Blocked, "no_wifi_device".into())
    );
}

#[test]
fn hw_radio_blocked() {
    let mut f = Facts::healthy();
    f.nm.wireless_hardware_enabled = Obs::Got(false);
    // With the radio off NetworkManager reports the device as unavailable (20): the switch row
    // says why, the driver row must not add a second complaint.
    wifi_mut(&mut f).state = Obs::Got(20);
    let rows = check(
        &f,
        caps3(Unavailable, Unavailable, Unavailable),
        &["nm.hardware_enabled", "wifi.client"],
    );
    assert_eq!(
        status(&rows, "nm.hardware_enabled"),
        (Status::Blocked, "switch_off".into())
    );
    assert_eq!(status(&rows, "nm.driver").0, Status::Ready);

    // The software switch.
    let mut f = Facts::healthy();
    f.nm.wireless_enabled = Obs::Got(false);
    let rows = check(
        &f,
        caps3(Unavailable, Unavailable, Unavailable),
        &["nm.wifi_enabled", "wifi.client"],
    );
    assert_eq!(
        status(&rows, "nm.wifi_enabled"),
        (Status::Blocked, "disabled".into())
    );
}

#[test]
fn unmanaged() {
    let mut f = Facts::healthy();
    wifi_mut(&mut f).managed = Obs::Got(false);
    let rows = check(
        &f,
        caps3(Unavailable, Unavailable, Unavailable),
        &["nm.managed", "wifi.client"],
    );
    assert_eq!(
        status(&rows, "nm.managed"),
        (Status::Blocked, "unmanaged".into())
    );
}

#[test]
fn driver_problems() {
    let mut f = Facts::healthy();
    wifi_mut(&mut f).firmware_missing = Obs::Got(true);
    let rows = check(
        &f,
        caps3(Unavailable, Unavailable, Unavailable),
        &["nm.firmware", "wifi.client"],
    );
    assert_eq!(
        status(&rows, "nm.firmware"),
        (Status::Blocked, "firmware_missing".into())
    );

    // Unavailable with the radio on: a warning with the reason number.
    let mut f = Facts::healthy();
    wifi_mut(&mut f).state = Obs::Got(20);
    wifi_mut(&mut f).state_reason = Obs::Got(42);
    let rows = check(&f, caps3(Degraded, Degraded, Degraded), &["nm.driver"]);
    assert_eq!(
        status(&rows, "nm.driver"),
        (Status::Warning, "unavailable".into())
    );
    assert!(row(&rows, "nm.driver").details.contains("42"));
}

#[test]
fn no_ap_band_cipher_receiving_only() {
    let only_receiving = |caps_bits: u32, reason: &str| {
        let mut f = Facts::healthy();
        wifi_mut(&mut f).capabilities = Obs::Got(caps_bits);
        let rows = check(&f, caps3(Available, Unavailable, Available), &["wifi.ap"]);
        assert_eq!(
            status(&rows, "wifi.ap"),
            (Status::Blocked, reason.into()),
            "{caps_bits:#x}"
        );
    };
    // No access-point mode.
    only_receiving(
        CAP_CCMP | CAP_RSN | CAP_FREQ_VALID | CAP_FREQ_5GHZ,
        "no_ap_mode",
    );
    // No WPA2: either the cipher or the RSN bit is missing.
    only_receiving(CAP_RSN | CAP_AP | CAP_FREQ_VALID | CAP_FREQ_5GHZ, "no_wpa2");
    only_receiving(
        CAP_CCMP | CAP_AP | CAP_FREQ_VALID | CAP_FREQ_5GHZ,
        "no_wpa2",
    );
    // The band bits are valid and name no band.
    only_receiving(CAP_CCMP | CAP_RSN | CAP_AP | CAP_FREQ_VALID, "no_band");

    // Band bits that are not marked valid say nothing: not a block.
    let mut f = Facts::healthy();
    wifi_mut(&mut f).capabilities = Obs::Got(CAP_CCMP | CAP_RSN | CAP_AP);
    check(&f, caps3(Available, Available, Available), &[]);

    // The driver reports nothing at all: a warning on receiving only.
    let mut f = Facts::healthy();
    wifi_mut(&mut f).capabilities = Obs::Got(0);
    let rows = check(&f, caps3(Available, Degraded, Available), &["wifi.ap"]);
    assert_eq!(
        status(&rows, "wifi.ap"),
        (Status::Warning, "caps_unreported".into())
    );
}

#[test]
fn multiple_wifi_use_runtime_selector() {
    assert_eq!(select_wifi_device(&[1, 2, 2]), Some(1));
    assert_eq!(select_wifi_device(&[2, 2]), Some(0));
    assert_eq!(select_wifi_device(&[1, 14]), None);

    let mut f = Facts::healthy();
    f.nm.device_types = Obs::Got(vec![1, 2, 2]);
    let rows = check(&f, caps3(Available, Available, Available), &["nm.device"]);
    let device = row(&rows, "nm.device");
    assert_eq!(
        (device.status, device.reason_code.as_str()),
        (Status::Ready, "ok_first_of_many")
    );
    assert_eq!(device.device.as_deref(), Some("wlan0"));
}

#[test]
fn permissions_yes_auth_no_error() {
    let with = |change: &dyn Fn(&mut super::facts::NmPermissions)| {
        let mut f = Facts::healthy();
        if let Obs::Got(p) = &mut f.nm.permissions {
            change(p);
        }
        f
    };

    // auth: a prompt is possible, not a refusal.
    let f = with(&|p| p.network_control = Obs::Got(Perm::Auth));
    let rows = check(&f, caps3(Degraded, Degraded, Degraded), &["nm.permissions"]);
    assert_eq!(
        status(&rows, "nm.permissions"),
        (Status::Warning, "auth_required".into())
    );
    assert!(rows.iter().all(|r| r.status != Status::Blocked));
    assert!(
        row(&rows, "nm.permissions")
            .details
            .contains("network-control=auth")
    );

    // no: refused.
    let f = with(&|p| p.settings_modify_system = Obs::Got(Perm::No));
    let rows = check(
        &f,
        caps3(Unavailable, Unavailable, Unavailable),
        &["nm.permissions", "wifi.client"],
    );
    assert_eq!(
        status(&rows, "nm.permissions"),
        (Status::Blocked, "denied".into())
    );

    // The call failed: Unknown, never Blocked.
    let f = with(&|p| p.network_control = Obs::Failed(ProbeError::AccessDenied));
    let rows = check(
        &f,
        caps3(Unknown, Unknown, Unknown),
        &["nm.permissions", "wifi.client"],
    );
    assert_eq!(
        status(&rows, "nm.permissions"),
        (Status::Unknown, "query_failed".into())
    );
    assert!(rows.iter().all(|r| r.status != Status::Blocked));

    // The hotspot permission only touches receiving.
    let f = with(&|p| p.wifi_share_protected = Obs::Got(Perm::Auth));
    let rows = check(&f, caps3(Available, Degraded, Available), &["wifi.ap"]);
    assert_eq!(
        status(&rows, "wifi.ap"),
        (Status::Warning, "share_auth_required".into())
    );
    let f = with(&|p| p.wifi_share_protected = Obs::Got(Perm::No));
    let rows = check(&f, caps3(Available, Unavailable, Available), &["wifi.ap"]);
    assert_eq!(
        status(&rows, "wifi.ap"),
        (Status::Blocked, "share_denied".into())
    );
}

// ---------------------------------------------------------------------------
// dnsmasq and the firewall
// ---------------------------------------------------------------------------

#[test]
fn dnsmasq_unknown_in_flatpak() {
    let f = Facts::healthy_flatpak();
    let rows = eval(&f);
    assert_eq!(
        status(&rows, "wifi.dhcp"),
        (Status::Unknown, "sandbox_unobservable".into())
    );
    // An Info row: it does not lower receiving.
    assert_eq!(row(&rows, "wifi.dhcp").category, Category::Info);
    assert_eq!(caps(&rows), caps3(Available, Available, Available));
    assert!(rows.iter().all(|r| r.status != Status::Blocked));
    assert_eq!(row(&rows, "sandbox.permissions").status, Status::Ready);
    // Against the native baseline: the dnsmasq row and the sandbox row itself.
    assert_changed(&base_rows(), &rows, &["wifi.dhcp", "sandbox.permissions"]);

    // A hotspot that started this session settles the row.
    let mut ev = SessionEvidence::default();
    ev.apply(RuntimeEvidence::HotspotStarted);
    let rows = rules::evaluate(&f, &ev);
    assert_eq!(
        status(&rows, "wifi.dhcp"),
        (Status::Ready, "worked_this_session".into())
    );
    assert_eq!(row(&rows, "wifi.dhcp").category, Category::Critical);
    assert_eq!(caps(&rows), caps3(Available, Available, Available));

    // NetworkManager could not start sharing: still the critical warning, sandbox or not.
    let mut f = Facts::healthy_flatpak();
    wifi_mut(&mut f).state_reason = Obs::Got(18);
    let rows = eval(&f);
    assert_eq!(
        status(&rows, "wifi.dhcp"),
        (Status::Warning, "shared_start_failed".into())
    );
    assert_eq!(row(&rows, "wifi.dhcp").category, Category::Critical);
    assert_eq!(caps(&rows), caps3(Available, Degraded, Available));

    // Native: the file is there or it is not, and a failed check is Unknown.
    let mut f = Facts::healthy();
    f.dhcp.dnsmasq = Obs::Got(false);
    let rows = check(&f, caps3(Available, Degraded, Available), &["wifi.dhcp"]);
    assert_eq!(
        status(&rows, "wifi.dhcp"),
        (Status::Warning, "not_found".into())
    );
}

#[test]
fn flatpak_with_every_permission_is_available() {
    // polkit is asked from the sandbox too: only dnsmasq stays unobservable, as an Info row.
    let rows = eval(&Facts::healthy_flatpak());
    assert_eq!(caps(&rows), caps3(Available, Available, Available));
    assert_eq!(
        status(&rows, "firewall.integration"),
        (Status::Ready, "ok".into())
    );
    let dhcp = row(&rows, "wifi.dhcp");
    assert_eq!(
        (dhcp.status, dhcp.category),
        (Status::Unknown, Category::Info)
    );
}

#[test]
fn flatpak_without_polkit_names_it() {
    // Without the talk permission the sandbox hides polkit: the probe sees no owner, so neither
    // the zones nor the permission are read.
    let mut f = Facts::healthy_flatpak();
    match &mut f.sandbox.context {
        Obs::Got(c) => c
            .system_talk_names
            .retain(|n| n != "org.freedesktop.PolicyKit1"),
        other => panic!("{other:?}"),
    }
    f.firewall.zones = Obs::Missing;
    f.firewall.polkit = Obs::Missing;
    let rows = eval(&f);
    let sandbox = row(&rows, "sandbox.permissions");
    assert_eq!(
        (sandbox.status, sandbox.reason_code.as_str()),
        (Status::Warning, "missing")
    );
    assert!(sandbox.details.contains("polkit"), "{}", sandbox.details);
    assert_eq!(
        status(&rows, "firewall.integration"),
        (Status::Unknown, "permission_unverifiable".into())
    );
    assert_eq!(caps(&rows), caps3(Available, Degraded, Available));
}

#[test]
fn firewalld_absent_degrades_receiving() {
    let mut f = Facts::healthy();
    f.firewall.name = Obs::Missing;
    let rows = check(
        &f,
        caps3(Available, Degraded, Available),
        &["firewall.integration"],
    );
    assert_eq!(
        status(&rows, "firewall.integration"),
        (Status::Warning, "not_running".into())
    );
    assert_eq!(
        row(&rows, "firewall.integration").category,
        Category::Security
    );

    // No `nm-shared` zone: also a limit.
    let mut f = Facts::healthy();
    f.firewall.zones = Obs::Got(vec!["public".into()]);
    let rows = check(
        &f,
        caps3(Available, Degraded, Available),
        &["firewall.integration"],
    );
    assert_eq!(status(&rows, "firewall.integration").1, "zone_missing");
}

#[test]
fn firewalld_permission_unknown() {
    // firewalld is there but the permission cannot be checked: Unknown, and receiving is only
    // limited, never Unavailable.
    for polkit in [
        Obs::Missing,
        Obs::Failed(ProbeError::AccessDenied),
        Obs::Failed(ProbeError::Other),
        Obs::Failed(ProbeError::Malformed),
    ] {
        let mut f = Facts::healthy();
        f.firewall.polkit = polkit.clone();
        let rows = check(
            &f,
            caps3(Available, Degraded, Available),
            &["firewall.integration"],
        );
        assert_eq!(
            status(&rows, "firewall.integration"),
            (Status::Unknown, "permission_unverifiable".into()),
            "{polkit:?}"
        );
    }
    let mut f = Facts::healthy();
    f.firewall.polkit = Obs::Failed(ProbeError::Timeout);
    let rows = check(
        &f,
        caps3(Available, Degraded, Available),
        &["firewall.integration"],
    );
    assert_eq!(status(&rows, "firewall.integration").1, "timeout");

    // The user would have to authenticate: a warning.
    let mut f = Facts::healthy();
    f.firewall.polkit = Obs::Got(PolkitVerdict::Challenge);
    let rows = check(
        &f,
        caps3(Available, Degraded, Available),
        &["firewall.integration"],
    );
    assert_eq!(status(&rows, "firewall.integration").1, "permission_auth");

    // Refused: the row is Blocked, but the firewall is a security row and only degrades.
    let mut f = Facts::healthy();
    f.firewall.polkit = Obs::Got(PolkitVerdict::Denied);
    let rows = check(
        &f,
        caps3(Available, Degraded, Available),
        &["firewall.integration"],
    );
    assert_eq!(
        status(&rows, "firewall.integration"),
        (Status::Blocked, "permission_denied".into())
    );
}

#[test]
fn evidence_changes_only_its_rows() {
    let mut f = Facts::healthy();
    f.firewall.name = Obs::Missing;
    let base = eval(&f);

    let run = |events: &[RuntimeEvidence]| {
        let mut ev = SessionEvidence::default();
        for e in events {
            ev.apply(*e);
        }
        rules::evaluate(&f, &ev)
    };

    let rows = run(&[RuntimeEvidence::FirewallRefused(
        BlockReason::FirewallDenied,
    )]);
    assert_eq!(
        status(&rows, "firewall.integration"),
        (Status::Blocked, "denied_this_session".into())
    );
    assert_eq!(caps(&rows), caps3(Available, Degraded, Available));
    assert_changed(&base, &rows, &["firewall.integration"]);

    let rows = run(&[
        RuntimeEvidence::FirewallRefused(BlockReason::FirewallDenied),
        RuntimeEvidence::FirewallConsent(true),
    ]);
    assert_eq!(
        status(&rows, "firewall.integration"),
        (Status::Warning, "consent_given".into())
    );
    assert_changed(&base, &rows, &["firewall.integration"]);

    let rows = run(&[RuntimeEvidence::FirewallPortOpened]);
    assert_eq!(status(&rows, "firewall.integration").0, Status::Ready);
    assert_eq!(caps(&rows), caps3(Available, Available, Available));

    let rows = run(&[RuntimeEvidence::HotspotStarted]);
    assert_eq!(
        status(&rows, "wifi.dhcp"),
        (Status::Ready, "worked_this_session".into())
    );
    assert_eq!(caps(&rows), caps(&base));
}

// ---------------------------------------------------------------------------
// Unknown, malformed, partial
// ---------------------------------------------------------------------------

#[test]
fn malformed_is_unknown() {
    let mut f = Facts::healthy();
    f.nm = NmFacts::failed(ProbeError::Malformed);
    let mut ids = NM_ROWS.to_vec();
    ids.extend(["wifi.client", "wifi.ap"]);
    let rows = check(&f, caps3(Unknown, Unknown, Unknown), &ids);
    assert_eq!(
        status(&rows, "nm.service"),
        (Status::Unknown, "malformed_reply".into())
    );
    assert_eq!(
        status(&rows, "nm.managed"),
        (Status::Unknown, "malformed_reply".into())
    );
    assert_eq!(
        status(&rows, "wifi.client"),
        (Status::Unknown, "prerequisite_uncertain".into())
    );
    assert!(rows.iter().all(|r| r.status != Status::Blocked));

    let mut f = Facts::healthy();
    f.bluez = super::facts::BluezFacts::failed(ProbeError::Malformed);
    let mut ids = bluez_ids(&[]);
    ids.push("bluez.l2cap");
    let rows = check(&f, caps3(Unknown, Unknown, Available), &ids);
    assert_eq!(
        status(&rows, "bluez.service"),
        (Status::Unknown, "malformed_reply".into())
    );
    assert!(rows.iter().all(|r| r.status != Status::Blocked));
}

#[test]
fn unknown_never_blocked() {
    let errors = [
        ProbeError::Timeout,
        ProbeError::BusUnreachable,
        ProbeError::NoOwner,
        ProbeError::AccessDenied,
        ProbeError::Malformed,
        ProbeError::Other,
    ];
    for e in errors {
        // Every question failed, natively and in a sandbox that could not be read.
        for packaging in [Packaging::Native, Packaging::Flatpak] {
            let mut f = Facts::failed(packaging, e);
            f.sandbox = SandboxFacts {
                context: Obs::Failed(e),
            };
            let rows = eval(&f);
            for r in &rows {
                assert_eq!(r.status, Status::Unknown, "{e:?} {packaging:?} {}", r.id);
            }
            assert_eq!(
                caps(&rows),
                caps3(Unknown, Unknown, Unknown),
                "{e:?} {packaging:?}"
            );
        }
        // One group failed on a healthy machine.
        for which in 0..5 {
            let mut f = Facts::healthy_flatpak();
            match which {
                0 => {
                    f.sandbox = SandboxFacts {
                        context: Obs::Failed(e),
                    }
                }
                1 => f.bluez = super::facts::BluezFacts::failed(e),
                2 => f.nm = NmFacts::failed(e),
                3 => f.firewall = FirewallFacts::failed(e),
                _ => f.dhcp = DhcpFacts::failed(e),
            }
            let rows = eval(&f);
            assert!(
                rows.iter().all(|r| r.status != Status::Blocked),
                "{e:?}: {rows:#?}"
            );
            let c = caps(&rows);
            for s in [c.sending, c.receiving, c.join_network] {
                assert_ne!(s, Unavailable, "{e:?}");
            }
        }
    }
}

#[tokio::test]
async fn group_timeout_within_total() {
    let cfg = EngineConfig {
        per_call: Duration::from_millis(50),
        total: Duration::from_millis(150),
        ..quiet_cfg()
    };
    let (engine, probe) = engine_with(Facts::healthy(), cfg);
    probe.hold(Group::Nm); // never released: NetworkManager does not answer
    let started = std::time::Instant::now();
    let report = bounded(engine.report(false)).await;
    assert!(started.elapsed() < Duration::from_secs(10));

    let mut ids = NM_ROWS.to_vec();
    ids.extend(["wifi.client", "wifi.ap"]);
    assert_eq!(report.capabilities, caps3(Unknown, Unknown, Unknown));
    assert_changed(&base_rows(), &report.requirements, &ids);
    assert_eq!(
        status(&report.requirements, "nm.service"),
        (Status::Unknown, "timeout".into())
    );
    assert_eq!(
        status(&report.requirements, "wifi.ap"),
        (Status::Unknown, "timeout".into())
    );
    assert!(
        report
            .requirements
            .iter()
            .all(|r| r.status != Status::Blocked)
    );
    // The other groups did their work.
    assert_eq!(probe.calls(Group::Bluez), 1);
    assert_eq!(probe.calls(Group::Dhcp), 1);
}

#[tokio::test]
async fn service_drop_and_return() {
    let (engine, probe) = engine_with(Facts::healthy(), quiet_cfg());
    let up = bounded(engine.report(false)).await;
    assert_eq!(up.capabilities, caps3(Available, Available, Available));

    // NetworkManager vanishes: nothing changes until an event drops the cache.
    probe.update(|f| f.nm.name = Obs::Missing);
    assert!(Arc::ptr_eq(&up, &bounded(engine.report(false)).await));
    engine.invalidate(Invalidation::ServiceOwner);
    let down = bounded(engine.report(false)).await;
    assert_eq!(
        down.capabilities,
        caps3(Unavailable, Unavailable, Unavailable)
    );
    let mut ids = NM_ROWS.to_vec();
    ids.extend(["wifi.client", "wifi.ap"]);
    assert_changed(&up.requirements, &down.requirements, &ids);
    assert!(!down.same_content(&up));

    // It comes back: the same report as at the start.
    probe.update(|f| f.nm = Facts::healthy().nm);
    engine.invalidate(Invalidation::ServiceOwner);
    let back = bounded(engine.report(false)).await;
    assert!(back.same_content(&up));
    assert_eq!(probe.calls(Group::Nm), 3);
    // BlueZ was asked again too (a run asks every group) but its rows never moved.
    assert_eq!(
        changed(&up.requirements, &down.requirements)
            .iter()
            .filter(|i| i.starts_with("bluez"))
            .count(),
        0
    );
}

#[tokio::test]
async fn partial_only_affected_capability_changes() {
    let (engine, probe) = engine_with(Facts::healthy(), quiet_cfg());
    let base = bounded(engine.report(false)).await;
    let healthy = Facts::healthy;

    // firewalld only: receiving.
    let mut f = healthy();
    f.firewall.name = Obs::Missing;
    let r = rerun(&engine, &probe, f).await;
    assert_eq!(r.capabilities, caps3(Available, Degraded, Available));
    assert_changed(
        &base.requirements,
        &r.requirements,
        &["firewall.integration"],
    );

    // The dnsmasq check failed: receiving only.
    let mut f = healthy();
    f.dhcp = DhcpFacts::failed(ProbeError::Timeout);
    let r = rerun(&engine, &probe, f).await;
    assert_eq!(r.capabilities, caps3(Available, Unknown, Available));
    assert_changed(&base.requirements, &r.requirements, &["wifi.dhcp"]);

    // No adapter: both Bluetooth directions, not joining a network.
    let mut f = healthy();
    f.bluez.adapters = Obs::Got(vec![]);
    let r = rerun(&engine, &probe, f).await;
    assert_eq!(r.capabilities, caps3(Unavailable, Unavailable, Available));

    // The sandbox facts do not matter natively.
    let mut f = healthy();
    f.sandbox = SandboxFacts {
        context: Obs::Failed(ProbeError::Timeout),
    };
    let r = rerun(&engine, &probe, f).await;
    assert!(r.same_content(&base));

    // Back to the start: the same content, a new run.
    let r = rerun(&engine, &probe, healthy()).await;
    assert!(r.same_content(&base));
}

// ---------------------------------------------------------------------------
// Engine: cache, joining, dedup, debounce
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ttl_cache() {
    // Inside the TTL: the cache.
    let (engine, probe) = engine_with(Facts::healthy(), quiet_cfg());
    let a = bounded(engine.report(false)).await;
    let b = bounded(engine.report(false)).await;
    assert!(Arc::ptr_eq(&a, &b));
    assert_eq!(probe.calls(Group::Bluez), 1);

    // A forced call right after a run returns that run when the gap says so.
    let cfg = EngineConfig {
        min_forced_gap: Duration::from_secs(3600),
        ..quiet_cfg()
    };
    let (engine, probe) = engine_with(Facts::healthy(), cfg);
    let a = bounded(engine.report(false)).await;
    let forced = bounded(engine.report(true)).await;
    assert!(Arc::ptr_eq(&a, &forced));
    assert_eq!(probe.calls(Group::Bluez), 1);

    // Without a gap a forced call runs again.
    let (engine, probe) = engine_with(Facts::healthy(), quiet_cfg());
    let a = bounded(engine.report(false)).await;
    let forced = bounded(engine.report(true)).await;
    assert!(!Arc::ptr_eq(&a, &forced));
    assert_eq!(probe.calls(Group::Bluez), 2);

    // A TTL of zero is always out of date.
    let cfg = EngineConfig {
        ttl: Duration::ZERO,
        ..quiet_cfg()
    };
    let (engine, probe) = engine_with(Facts::healthy(), cfg);
    bounded(engine.report(false)).await;
    bounded(engine.report(false)).await;
    assert_eq!(probe.calls(Group::Bluez), 2);

    // An event drops the cache.
    let (engine, probe) = engine_with(Facts::healthy(), quiet_cfg());
    bounded(engine.report(false)).await;
    engine.invalidate(Invalidation::Bluetooth);
    bounded(engine.report(false)).await;
    assert_eq!(probe.calls(Group::Bluez), 2);
}

#[tokio::test]
async fn force_joins_inflight() {
    let (engine, probe) = engine_with(Facts::healthy(), quiet_cfg());
    probe.hold(Group::Bluez);
    // One task: the first call starts the run, the forced call finds it going, and only then
    // is the probe released.
    let (a, b, ()) = bounded(async {
        tokio::join!(engine.report(false), engine.report(true), async {
            probe.wait_entered(Group::Bluez).await;
            probe.release(Group::Bluez);
        })
    })
    .await;
    assert!(Arc::ptr_eq(&a, &b), "the forced call joined the run");
    assert_eq!(probe.calls(Group::Bluez), 1);
    assert_eq!(a.capabilities, caps3(Available, Available, Available));
}

#[tokio::test]
async fn dedup_one_run() {
    let (engine, probe) = engine_with(Facts::healthy(), quiet_cfg());
    probe.hold(Group::Nm);
    let (a, b, c, d, ()) = bounded(async {
        tokio::join!(
            engine.report(false),
            engine.report(false),
            engine.report(true),
            engine.report(false),
            async {
                probe.wait_entered(Group::Nm).await;
                probe.release(Group::Nm);
            }
        )
    })
    .await;
    assert!(Arc::ptr_eq(&a, &b) && Arc::ptr_eq(&b, &c) && Arc::ptr_eq(&c, &d));
    for g in [
        Group::Sandbox,
        Group::Bluez,
        Group::Nm,
        Group::Firewall,
        Group::Dhcp,
    ] {
        assert_eq!(probe.calls(g), 1, "{g:?}");
    }
}

#[tokio::test]
async fn debounce() {
    let cfg = EngineConfig {
        debounce: Duration::from_millis(30),
        ..quiet_cfg()
    };
    let (engine, probe) = engine_with(Facts::healthy(), cfg);
    // Nothing is refreshed before somebody asked for a report.
    engine.invalidate(Invalidation::Bluetooth);
    assert_eq!(probe.calls(Group::Bluez), 0);

    let first = bounded(engine.report(false)).await;
    let mut rx = engine.subscribe();
    // A burst of events: one refresh.
    probe.update(|f| f.firewall.name = Obs::Missing);
    engine.invalidate(Invalidation::Bluetooth);
    engine.invalidate(Invalidation::NetworkManager);
    engine.invalidate(Invalidation::ServiceOwner);
    bounded(rx.changed()).await.unwrap();
    assert_eq!(probe.calls(Group::Bluez), 2, "one run for the three events");

    let published = rx.borrow().clone().unwrap();
    assert_eq!(
        published.capabilities,
        caps3(Available, Degraded, Available)
    );
    assert!(!published.same_content(&first));
    // The refresh filled the cache.
    let cached = bounded(engine.report(false)).await;
    assert!(Arc::ptr_eq(&cached, &published));
    assert_eq!(probe.calls(Group::Bluez), 2);
}

#[tokio::test]
async fn evidence_invalidates_and_is_kept() {
    let (engine, probe) = engine_with(Facts::healthy_flatpak(), quiet_cfg());
    let first = bounded(engine.report(false)).await;
    assert_eq!(
        status(&first.requirements, "wifi.dhcp").1,
        "sandbox_unobservable"
    );
    engine.record(RuntimeEvidence::HotspotStarted);
    let second = bounded(engine.report(false)).await;
    assert_eq!(
        status(&second.requirements, "wifi.dhcp"),
        (Status::Ready, "worked_this_session".into())
    );
    assert_changed(&first.requirements, &second.requirements, &["wifi.dhcp"]);
    assert_eq!(probe.calls(Group::Dhcp), 2);
    // Kept for the next runs.
    engine.invalidate(Invalidation::NetworkManager);
    let third = bounded(engine.report(false)).await;
    assert!(third.same_content(&second));
}

// ---------------------------------------------------------------------------
// Policy, text limits, JSON
// ---------------------------------------------------------------------------

#[test]
fn enable_verdict_table() {
    let all = [Available, Degraded, Unavailable, Unknown];
    for s in all {
        for r in all {
            let expected = if s == Unavailable && r == Unavailable {
                EnableVerdict::Reject
            } else if s == Available && r == Available {
                EnableVerdict::Allow
            } else if s == Unknown || r == Unknown {
                EnableVerdict::AllowUncertain
            } else {
                EnableVerdict::AllowLimited
            };
            for j in all {
                assert_eq!(
                    enable_verdict(&caps3(s, r, j)),
                    expected,
                    "{s:?} {r:?} (join {j:?})"
                );
            }
        }
    }

    // The scenarios of the mandate.
    let verdict = |f: &Facts| enable_verdict(&caps(&eval(f)));
    assert_eq!(verdict(&Facts::healthy()), EnableVerdict::Allow);
    let mut f = Facts::healthy();
    f.bluez.name = Obs::Missing;
    assert_eq!(verdict(&f), EnableVerdict::Reject);
    let mut f = Facts::healthy();
    f.nm.name = Obs::Missing;
    assert_eq!(verdict(&f), EnableVerdict::Reject);
    let mut f = Facts::healthy();
    wifi_mut(&mut f).capabilities = Obs::Got(CAP_CCMP | CAP_RSN);
    assert_eq!(verdict(&f), EnableVerdict::AllowLimited);
    let mut f = Facts::healthy();
    f.firewall.name = Obs::Missing;
    assert_eq!(verdict(&f), EnableVerdict::AllowLimited);
    assert_eq!(verdict(&Facts::healthy_flatpak()), EnableVerdict::Allow);
    assert_eq!(
        verdict(&Facts::failed(
            Packaging::Native,
            ProbeError::BusUnreachable
        )),
        EnableVerdict::AllowUncertain
    );
}

#[test]
fn details_sanitized_bounded() {
    // The function itself.
    assert_eq!(rules::sanitize("  a<b>\u{7}c\n ", 50), "abc");
    assert_eq!(
        rules::sanitize(&"x".repeat(500), MAX_DETAILS)
            .chars()
            .count(),
        MAX_DETAILS
    );
    assert_eq!(rules::sanitize("\u{1b}[31m<script>", 50), "[31mscript");

    // Hostile facts: a version string, an interface name with a MAC and a long one.
    let mut f = Facts::healthy();
    f.nm.version = Obs::Got(format!("<b>1.46\n{}", "9".repeat(10_000)));
    wifi_mut(&mut f).interface = Obs::Got("wlx001122334455".into());
    let rows = eval(&f);
    let device = row(&rows, "nm.managed").device.clone().unwrap();
    assert!(!device.contains("001122334455"), "{device}");
    assert!(!row(&rows, "nm.service").details.contains(['<', '>', '\n']));

    let mut f = Facts::healthy();
    wifi_mut(&mut f).interface = Obs::Got(format!("wl\u{7}an{}", "z".repeat(500)));
    adapter_mut(&mut f).name = format!("hci{}", "y".repeat(100));
    f.nm.version = Obs::Got("1.46.0".repeat(100));
    let rows = eval(&f);
    assert!(rows.len() <= MAX_ROWS);
    for r in &rows {
        assert!(r.details.chars().count() <= MAX_DETAILS, "{}", r.id);
        assert!(
            r.verification.chars().count() <= MAX_VERIFICATION,
            "{}",
            r.id
        );
        if let Some(d) = &r.device {
            assert!(d.chars().count() <= MAX_DEVICE, "{}", r.id);
        }
        for text in [
            &r.details,
            &r.verification,
            r.device.as_ref().unwrap_or(&String::new()),
        ] {
            assert!(
                !text.chars().any(|c| c.is_control() || c == '<' || c == '>'),
                "{}",
                r.id
            );
        }
    }
    assert!(row(&rows, "nm.service").details.len() <= "version ".len() + 32);

    // The whole document stays small.
    let report = RequirementsReport {
        schema_version: SCHEMA_VERSION,
        generated_at_ms: 1,
        ttl_ms: 1,
        packaging: Packaging::Native,
        core_version: "x".into(),
        capabilities: caps(&rows),
        requirements: rows,
    };
    assert!(report.to_json().unwrap().len() < MAX_JSON_BYTES);
}

#[test]
fn golden_json_v1() {
    let report = RequirementsReport {
        schema_version: 1,
        generated_at_ms: 1_793_260_800_000,
        ttl_ms: 30_000,
        packaging: Packaging::Flatpak,
        core_version: "1.4.1".into(),
        capabilities: caps3(Degraded, Degraded, Degraded),
        requirements: vec![
            Requirement {
                id: "nm.permissions".into(),
                status: Status::Warning,
                category: Category::Critical,
                reason_code: "auth_required".into(),
                details: "network-control=auth".into(),
                device: None,
                verification: "GetPermissions without interaction".into(),
                affects: vec!["sending".into(), "receiving".into(), "join_network".into()],
            },
            Requirement {
                id: "wifi.dhcp".into(),
                status: Status::Unknown,
                category: Category::Info,
                reason_code: "sandbox_unobservable".into(),
                details: String::new(),
                device: Some("wlan0".into()),
                verification: "dnsmasq at the standard paths".into(),
                affects: vec!["receiving".into()],
            },
        ],
    };
    let golden = concat!(
        r#"{"schema_version":1,"generated_at_ms":1793260800000,"ttl_ms":30000,"#,
        r#""packaging":"flatpak","core_version":"1.4.1","#,
        r#""capabilities":{"sending":"degraded","receiving":"degraded","join_network":"degraded"},"#,
        r#""requirements":["#,
        r#"{"id":"nm.permissions","status":"warning","category":"critical","reason_code":"auth_required","#,
        r#""details":"network-control=auth","device":null,"verification":"GetPermissions without interaction","#,
        r#""affects":["sending","receiving","join_network"]},"#,
        r#"{"id":"wifi.dhcp","status":"unknown","category":"info","reason_code":"sandbox_unobservable","#,
        r#""details":"","device":"wlan0","verification":"dnsmasq at the standard paths","#,
        r#""affects":["receiving"]}]}"#
    );
    assert_eq!(report.to_json().unwrap(), golden);
    let back: RequirementsReport = serde_json::from_str(golden).unwrap();
    assert_eq!(back, report);
}

#[test]
fn json_keys_v1() {
    // The keys of a real report: new fields may be added, these must stay.
    let rows = base_rows();
    let report = RequirementsReport {
        schema_version: SCHEMA_VERSION,
        generated_at_ms: 1,
        ttl_ms: 30_000,
        packaging: Packaging::Native,
        core_version: "1.4.1".into(),
        capabilities: caps(&rows),
        requirements: rows,
    };
    let v: serde_json::Value = serde_json::from_str(&report.to_json().unwrap()).unwrap();
    let keys = |v: &serde_json::Value| {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    assert_eq!(
        keys(&v),
        [
            "capabilities",
            "core_version",
            "generated_at_ms",
            "packaging",
            "requirements",
            "schema_version",
            "ttl_ms"
        ]
    );
    assert_eq!(
        keys(&v["capabilities"]),
        ["join_network", "receiving", "sending"]
    );
    assert_eq!(
        keys(&v["requirements"][0]),
        [
            "affects",
            "category",
            "details",
            "device",
            "id",
            "reason_code",
            "status",
            "verification"
        ]
    );
    assert_eq!(v["capabilities"]["sending"], "available");
}
