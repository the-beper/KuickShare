//! A probe that answers from a `Facts` value, for the unit tests of the requirements engine.
//!
//! The knobs, all deterministic (no sleeps):
//! - `set` / `update` change the facts between runs;
//! - `calls(group)` counts how often a group was probed;
//! - `hold(group)` makes the probe of that group wait on a gate once it has been entered;
//!   `wait_entered(group)` returns when a probe is inside, `release(group)` opens the gate for
//!   good. A group that is held and never released is a hanging service (a timeout).
//!
//! `Facts::healthy()` is the baseline every test changes one thing of.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use tokio::sync::Semaphore;

use super::{
    engine::{Budget, RequirementsProbe},
    facts::{
        AdvSlots, BluezFacts, BtAdapter, DhcpFacts, Facts, FirewallFacts, FlatpakContext,
        NameState, NmFacts, NmPermissions, Obs, Perm, PolkitVerdict, SandboxFacts, WifiDeviceFacts,
    },
    model::Packaging,
};

/// `NM_WIFI_DEVICE_CAP_CIPHER_CCMP`
pub(crate) const CAP_CCMP: u32 = 0x8;
/// `NM_WIFI_DEVICE_CAP_RSN`
pub(crate) const CAP_RSN: u32 = 0x20;
/// `NM_WIFI_DEVICE_CAP_AP`
pub(crate) const CAP_AP: u32 = 0x40;
/// `NM_WIFI_DEVICE_CAP_FREQ_VALID`
pub(crate) const CAP_FREQ_VALID: u32 = 0x100;
/// `NM_WIFI_DEVICE_CAP_FREQ_5GHZ`
pub(crate) const CAP_FREQ_5GHZ: u32 = 0x400;
/// What a healthy access-point-capable device reports.
pub(crate) const CAPS_HEALTHY: u32 = CAP_CCMP | CAP_RSN | CAP_AP | CAP_FREQ_VALID | CAP_FREQ_5GHZ;

/// A powered adapter with every manager.
pub(crate) fn healthy_adapter(name: &str) -> BtAdapter {
    BtAdapter {
        name: name.into(),
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

impl Facts {
    /// Every requirement met on a native install: all rows Ready, all capabilities available.
    pub(crate) fn healthy() -> Self {
        Self {
            packaging: Packaging::Native,
            sandbox: SandboxFacts::default(),
            bluez: BluezFacts {
                name: Obs::Got(NameState::Owned),
                adapters: Obs::Got(vec![healthy_adapter("hci0")]),
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
                    capabilities: Obs::Got(CAPS_HEALTHY),
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

    /// The same machine seen from a Flatpak sandbox with every permission the manifest asks for:
    /// dnsmasq and polkit cannot be observed from there.
    pub(crate) fn healthy_flatpak() -> Self {
        let mut f = Self::healthy();
        f.packaging = Packaging::Flatpak;
        f.sandbox = SandboxFacts {
            context: Obs::Got(FlatpakContext {
                system_talk_names: vec![
                    "org.bluez".into(),
                    "org.freedesktop.NetworkManager".into(),
                    "org.fedoraproject.FirewallD1".into(),
                ],
                features: vec!["bluetooth".into()],
                shared: vec!["network".into()],
                filesystems: vec!["xdg-download".into()],
                sockets: vec![],
            }),
        };
        f.dhcp = DhcpFacts::default();
        f.firewall.polkit = Obs::Missing;
        f
    }
}

/// The probe groups of the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Group {
    Sandbox = 0,
    Bluez = 1,
    Nm = 2,
    Firewall = 3,
    Dhcp = 4,
}

struct Inner {
    facts: Mutex<Option<Facts>>,
    calls: [AtomicUsize; 5],
    /// A probe adds a permit when it is entered.
    entered: [Arc<Semaphore>; 5],
    holds: Mutex<[Option<Arc<Semaphore>>; 5]>,
}

/// The fake. Cheap to clone: the engine gets one clone, the test keeps another.
#[derive(Clone)]
pub(crate) struct FakeRequirementsProbe {
    inner: Arc<Inner>,
}

impl FakeRequirementsProbe {
    pub(crate) fn new(facts: Facts) -> Self {
        let inner = Inner {
            facts: Mutex::new(Some(facts)),
            calls: Default::default(),
            entered: std::array::from_fn(|_| Arc::new(Semaphore::new(0))),
            holds: Mutex::new(Default::default()),
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Replaces the facts; the next probe call sees them.
    pub(crate) fn set(&self, facts: Facts) {
        *self.inner.facts.lock().unwrap() = Some(facts);
    }

    /// Changes the facts in place.
    pub(crate) fn update(&self, change: impl FnOnce(&mut Facts)) {
        change(self.inner.facts.lock().unwrap().as_mut().unwrap());
    }

    pub(crate) fn facts(&self) -> Facts {
        self.inner.facts.lock().unwrap().clone().unwrap()
    }

    /// How often the group was probed.
    pub(crate) fn calls(&self, g: Group) -> usize {
        self.inner.calls[g as usize].load(Ordering::SeqCst)
    }

    /// From now on, probing the group waits for `release`.
    pub(crate) fn hold(&self, g: Group) {
        self.inner.holds.lock().unwrap()[g as usize] = Some(Arc::new(Semaphore::new(0)));
    }

    /// Opens the gate of the group and removes it: waiting and later probes go through.
    pub(crate) fn release(&self, g: Group) {
        if let Some(gate) = self.inner.holds.lock().unwrap()[g as usize].take() {
            gate.add_permits(Semaphore::MAX_PERMITS / 2);
        }
    }

    /// Returns once a probe of the group has been entered (one entry per call).
    pub(crate) async fn wait_entered(&self, g: Group) {
        if let Ok(permit) = self.inner.entered[g as usize].acquire().await {
            permit.forget();
        }
    }

    async fn visit(&self, g: Group) -> Facts {
        self.inner.calls[g as usize].fetch_add(1, Ordering::SeqCst);
        self.inner.entered[g as usize].add_permits(1);
        let gate = self.inner.holds.lock().unwrap()[g as usize].clone();
        if let Some(gate) = gate {
            // A gate that is never released is a service that never answers.
            if let Ok(permit) = gate.acquire().await {
                permit.forget();
            }
        }
        self.facts()
    }
}

impl RequirementsProbe for FakeRequirementsProbe {
    fn packaging(&self) -> Packaging {
        self.facts().packaging
    }

    async fn sandbox(&self) -> SandboxFacts {
        self.visit(Group::Sandbox).await.sandbox
    }

    async fn bluez(&self, _b: Budget) -> BluezFacts {
        self.visit(Group::Bluez).await.bluez
    }

    async fn network_manager(&self, _b: Budget) -> NmFacts {
        self.visit(Group::Nm).await.nm
    }

    async fn firewall(&self, _b: Budget) -> FirewallFacts {
        self.visit(Group::Firewall).await.firewall
    }

    async fn dhcp(&self, _p: Packaging) -> DhcpFacts {
        self.visit(Group::Dhcp).await.dhcp
    }
}
