//! Wi-Fi credentials received from a phone, kept in memory only so the user can join the network.
//!
//! The password never reaches a D-Bus signal or property, the history, the settings or a log
//! (`Debug` of everything here is redacted). An entry is dropped after a successful join, when its
//! transfer is declined or cancelled, after `TTL`, and when the oldest of `MAX_ENTRIES` is pushed
//! out; a restart of the daemon forgets them all.

use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use rqs_lib::{TransferState, hdl::info::TransferPayload};

use crate::link::Secret;

pub const TTL: Duration = Duration::from_secs(600);
pub const MAX_ENTRIES: usize = 8;

/// The security types a received network can have; `Unsupported` (WEP, unknown) cannot be joined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Security {
    Open,
    WpaPsk,
    Unsupported,
}

#[derive(Clone)]
pub struct Credentials {
    pub ssid: String,
    pub security: Security,
    password: Secret,
}

impl Credentials {
    /// The key to join with: `None` for an open network.
    pub fn psk(&self) -> Option<&str> {
        (self.security == Security::WpaPsk).then_some(self.password.0.as_str())
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("ssid", &self.ssid)
            .field("security", &self.security)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Why a stored network cannot be joined. The messages are shown to the user and never quote the
/// password.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// No such transfer, or its credentials expired or were already used.
    Unknown,
    Unsupported,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unknown => {
                "These Wi-Fi details are no longer available; ask the phone to send them again"
            }
            Self::Unsupported => "Only WPA/WPA2 and open networks can be joined from here",
        })
    }
}

#[derive(Default)]
pub struct Store {
    entries: HashMap<String, (Instant, Credentials)>,
}

pub type Shared = Arc<Mutex<Store>>;

impl Store {
    /// Remembers the credentials of a finished transfer; any other payload is ignored. Returns
    /// whether something was stored. WEP and unknown types are kept without their password so that
    /// a join can say why it fails.
    pub fn insert(&mut self, id: &str, payload: &TransferPayload, now: Instant) -> bool {
        use rqs_lib::sharing_nearby::wifi_credentials_metadata::SecurityType;
        let TransferPayload::Wifi {
            ssid,
            password,
            security_type,
        } = payload
        else {
            return false;
        };
        let (security, password) = match security_type {
            SecurityType::Open => (Security::Open, String::new()),
            SecurityType::WpaPsk => (Security::WpaPsk, password.clone()),
            SecurityType::Wep | SecurityType::UnknownSecurityType => {
                (Security::Unsupported, String::new())
            }
        };
        self.prune(now);
        while self.entries.len() >= MAX_ENTRIES && !self.entries.contains_key(id) {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
        self.entries.insert(
            id.to_owned(),
            (
                now,
                Credentials {
                    ssid: ssid.clone(),
                    security,
                    password: Secret(password),
                },
            ),
        );
        true
    }

    /// Applies a transfer event: an inbound transfer that finished with a Wi-Fi payload is stored
    /// (returns `true`); a rejected or cancelled one is forgotten. `Disconnected` follows
    /// `Finished` and changes nothing.
    pub fn on_event(
        &mut self,
        id: &str,
        state: &TransferState,
        inbound: bool,
        payload: Option<&TransferPayload>,
        now: Instant,
    ) -> bool {
        match state {
            TransferState::Finished if inbound => {
                payload.is_some_and(|payload| self.insert(id, payload, now))
            }
            TransferState::Rejected | TransferState::Cancelled => {
                self.remove(id);
                false
            }
            _ => false,
        }
    }

    /// The credentials of a transfer that can be joined; they stay stored until `remove`.
    pub fn get(&mut self, id: &str, now: Instant) -> Result<Credentials, Error> {
        self.prune(now);
        let (_, credentials) = self.entries.get(id).ok_or(Error::Unknown)?;
        match credentials.security {
            Security::Unsupported => Err(Error::Unsupported),
            Security::Open | Security::WpaPsk => Ok(credentials.clone()),
        }
    }

    pub fn remove(&mut self, id: &str) {
        self.entries.remove(id);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Drops the entries older than `TTL`.
    pub fn prune(&mut self, now: Instant) {
        self.entries
            .retain(|_, (at, _)| now.saturating_duration_since(*at) < TTL);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use rqs_lib::sharing_nearby::wifi_credentials_metadata::SecurityType;

    use super::*;

    fn wifi(ssid: &str, security_type: SecurityType) -> TransferPayload {
        TransferPayload::Wifi {
            ssid: ssid.into(),
            password: "hunter2-secret".into(),
            security_type,
        }
    }

    #[test]
    fn stores_and_returns_wpa_and_open_networks() {
        let (mut store, now) = (Store::default(), Instant::now());
        assert!(store.insert("a", &wifi("Home", SecurityType::WpaPsk), now));
        assert!(store.insert("b", &wifi("Cafe", SecurityType::Open), now));
        let home = store.get("a", now).unwrap();
        assert_eq!(
            (home.ssid.as_str(), home.psk()),
            ("Home", Some("hunter2-secret"))
        );
        let cafe = store.get("b", now).unwrap();
        assert_eq!((cafe.ssid.as_str(), cafe.psk()), ("Cafe", None));
        // Reading does not consume.
        assert!(store.get("a", now).is_ok());
        assert_eq!(store.get("nope", now).unwrap_err(), Error::Unknown);
    }

    #[test]
    fn ignores_other_payloads() {
        let (mut store, now) = (Store::default(), Instant::now());
        assert!(!store.insert("a", &TransferPayload::Text("x".into()), now));
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn wep_and_unknown_are_rejected_and_their_password_is_not_kept() {
        let (mut store, now) = (Store::default(), Instant::now());
        for kind in [SecurityType::Wep, SecurityType::UnknownSecurityType] {
            store.insert("a", &wifi("Old", kind), now);
            let e = store.get("a", now).unwrap_err();
            assert_eq!(e, Error::Unsupported);
            assert!(!e.to_string().contains("hunter2-secret"));
            assert_eq!(store.entries["a"].1.password.0, "");
        }
    }

    #[test]
    fn entries_expire_after_the_ttl() {
        let (mut store, now) = (Store::default(), Instant::now());
        store.insert("a", &wifi("Home", SecurityType::WpaPsk), now);
        assert!(store.get("a", now + TTL - Duration::from_secs(1)).is_ok());
        assert_eq!(store.get("a", now + TTL).unwrap_err(), Error::Unknown);
        assert_eq!(store.len(), 0);
        store.insert("b", &wifi("Home", SecurityType::WpaPsk), now);
        store.prune(now + TTL);
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn the_oldest_entry_is_pushed_out_at_the_bound() {
        let (mut store, now) = (Store::default(), Instant::now());
        for i in 0..MAX_ENTRIES + 3 {
            let at = now + Duration::from_secs(i as u64);
            store.insert(&format!("t{i}"), &wifi("Home", SecurityType::WpaPsk), at);
            assert!(store.len() <= MAX_ENTRIES);
        }
        let end = now + Duration::from_secs(20);
        assert_eq!(store.get("t0", end).unwrap_err(), Error::Unknown);
        assert_eq!(store.get("t2", end).unwrap_err(), Error::Unknown);
        assert!(store.get("t3", end).is_ok());
        assert!(store.get(&format!("t{}", MAX_ENTRIES + 2), end).is_ok());
        // Replacing an entry does not push another one out.
        store.insert("t3", &wifi("Home", SecurityType::WpaPsk), end);
        assert_eq!(store.len(), MAX_ENTRIES);
    }

    #[test]
    fn events_store_on_finished_inbound_and_forget_on_rejected_or_cancelled() {
        let (mut store, now) = (Store::default(), Instant::now());
        let p = wifi("Home", SecurityType::WpaPsk);
        let event = |store: &mut Store, state, inbound, payload| {
            store.on_event("a", &state, inbound, payload, now)
        };
        assert!(!event(&mut store, TransferState::Finished, false, Some(&p)));
        assert!(!event(&mut store, TransferState::Finished, true, None));
        assert!(!event(
            &mut store,
            TransferState::ReceivingFiles,
            true,
            Some(&p)
        ));
        assert_eq!(store.len(), 0);
        assert!(event(&mut store, TransferState::Finished, true, Some(&p)));
        // The Disconnected that follows keeps it.
        assert!(!event(
            &mut store,
            TransferState::Disconnected,
            true,
            Some(&p)
        ));
        assert!(store.get("a", now).is_ok());
        for gone in [TransferState::Rejected, TransferState::Cancelled] {
            event(&mut store, TransferState::Finished, true, Some(&p));
            event(&mut store, gone, true, None);
            assert_eq!(store.get("a", now).unwrap_err(), Error::Unknown);
        }
    }

    #[test]
    fn remove_and_clear_forget_entries() {
        let (mut store, now) = (Store::default(), Instant::now());
        store.insert("a", &wifi("Home", SecurityType::WpaPsk), now);
        store.insert("b", &wifi("Home", SecurityType::WpaPsk), now);
        store.remove("a");
        assert_eq!(store.get("a", now).unwrap_err(), Error::Unknown);
        assert!(store.get("b", now).is_ok());
        store.clear();
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn debug_does_not_show_the_password() {
        let (mut store, now) = (Store::default(), Instant::now());
        store.insert("a", &wifi("Home", SecurityType::WpaPsk), now);
        let credentials = store.get("a", now).unwrap();
        let shown = format!(
            "{credentials:?} {:?} {:#?}",
            Some(&credentials),
            wifi("Home", SecurityType::WpaPsk)
        );
        assert!(!shown.contains("hunter2-secret"));
        assert!(shown.contains("Home") && shown.contains("redacted"));
    }
}
