//! The one rule for answering an incoming transfer.
//!
//! A transfer that waits for the user has an entry here, with a random token, a deadline and the
//! device name, address and MAC address seen when it opened. Every way of accepting it goes
//! through `Consents::resolve`: the D-Bus methods, the paired-computer and trusted-device
//! auto-accept. The entry is removed before the answer is sent, so a consent is answered once
//! even when several answers race, an expired consent is never accepted, and a prompt that
//! outlived its transfer (the id was reused) cannot answer the new one. Two paths only decline
//! and so do not need a verdict: `expire` (the time is over) and `force_decline` (the name guard).
//!
//! The book has a lock of its own and never takes the `Core` lock: answering must work while a
//! send holds the core for seconds.

use std::{
    collections::HashMap,
    fmt,
    net::IpAddr,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use rqs_lib::channel::TransferAction;

use crate::{allowed, app::LibActions};

/// How long a transfer waits for an answer before it is declined.
pub const CONSENT_TIMEOUT: Duration = Duration::from_secs(300);
/// How long after the user accepted a transfer the sender can still be trusted (`AllowDevice`).
pub const TRUST_GRACE: Duration = Duration::from_secs(30);
/// Transfers waiting for an answer at once; more are declined, so a flood of senders cannot pile
/// up prompts.
const MAX_PENDING: usize = 32;
/// Of those, the places kept for Bluetooth senders (ids `ble-N`): a flood over the network cannot
/// take them, so it cannot get the one phone that sends over Bluetooth declined.
const BLE_RESERVE: usize = 8;
/// Waiting transfers from one network address, like the library's own limit.
const MAX_PER_ADDRESS: usize = 4;
/// The refusal of a flood is worth one log line in this time.
const REFUSAL_LOG_EVERY: Duration = Duration::from_secs(10);

fn is_ble(id: &str) -> bool {
    id.starts_with("ble-") || id.starts_with("ble:")
}

/// What `Consents::open` made of an incoming transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opened {
    /// It waits for an answer; the token goes with the transfer events.
    Token(u64),
    /// Too many wait (in all, from this address, or over the network): decline it.
    Full,
    /// The event comes from an engine that was stopped meanwhile: ignore it.
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Accept,
    /// Accept, and remember the sender (name and the MAC address seen when the consent opened).
    AcceptAndTrust,
    Decline,
}

impl Answer {
    /// The word `AnswerTransfer` takes.
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "accept" => Some(Self::Accept),
            "accept_and_trust" => Some(Self::AcceptAndTrust),
            "decline" => Some(Self::Decline),
            _ => None,
        }
    }
}

/// Who answers. Only an answer of the user lets the sender be trusted afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum By {
    User,
    PairedComputer,
    TrustedDevice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentError {
    /// No transfer with this id waits for an answer (never did, already answered, or gone).
    NotWaiting,
    /// The time to answer is over.
    Expired,
    /// The token belongs to an older transfer that had the same id.
    StaleToken,
    /// The network address of the sender belongs to another device than when the consent opened.
    DeviceChanged,
    /// The network address of the sender could not be read (another network, or IPv6).
    NoMac,
    /// The sender has no name, so it could not be told from another one.
    NoName,
    /// The Quick Share service is stopped.
    NotRunning,
}

impl fmt::Display for ConsentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotWaiting => "this transfer is not waiting for an answer",
            Self::Expired => "the time to answer this transfer is over",
            Self::StaleToken => "the request is out of date: the transfer was replaced",
            Self::DeviceChanged => "the network address now belongs to another device",
            Self::NoMac => {
                "cannot read the network address of this device (only devices on the same network can be trusted)"
            }
            Self::NoName => "the device has no name",
            Self::NotRunning => "service is not running",
        })
    }
}

impl std::error::Error for ConsentError {}

/// What `resolve` learned about the transfer it answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub device: String,
    /// The verified MAC address; only set for `Answer::AcceptAndTrust`.
    pub mac: Option<String>,
}

#[derive(Debug, Clone)]
struct Pending {
    token: u64,
    /// The PIN of the session: another PIN under the same id is another transfer.
    pin: String,
    deadline: Instant,
    device: String,
    ip: Option<IpAddr>,
    mac: Option<String>,
}

/// A transfer the user accepted: its sender can still be trusted for `TRUST_GRACE`.
struct Recent {
    device: String,
    ip: Option<IpAddr>,
    mac: Option<String>,
    until: Instant,
}

#[derive(Default)]
struct Book {
    pending: HashMap<String, Pending>,
    recent: HashMap<String, Recent>,
    last_refusal_log: Option<Instant>,
}

type MacReader = Arc<dyn Fn(IpAddr) -> Option<String> + Send + Sync>;

#[derive(Clone)]
pub struct Consents {
    book: Arc<Mutex<Book>>,
    /// Counts the restarts of the service (`clear`): the events of an older engine are ignored.
    generation: Arc<AtomicU64>,
    actions: LibActions,
    mac: MacReader,
}

fn new_token() -> u64 {
    loop {
        // 0 is what "no consent" looks like on the bus.
        let token: u64 = rand::random();
        if token != 0 {
            return token;
        }
    }
}

impl Consents {
    /// Answers travel through `actions`, the library's event channel of the running engine.
    pub fn new(actions: LibActions) -> Self {
        Self::with_mac_reader(actions, |ip| allowed::mac_for_ip(&ip.to_string()))
    }

    pub fn with_mac_reader(
        actions: LibActions,
        reader: impl Fn(IpAddr) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            book: Arc::default(),
            generation: Arc::default(),
            actions,
            mac: Arc::new(reader),
        }
    }

    fn book(&self) -> MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The generation to give `open`: the one of the engine that is running now.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// True at most once in `REFUSAL_LOG_EVERY`: the caller logs a refused flood only then.
    pub fn note_refusal(&self, now: Instant) -> bool {
        let mut book = self.book();
        let due = book
            .last_refusal_log
            .is_none_or(|at| now.saturating_duration_since(at) >= REFUSAL_LOG_EVERY);
        if due {
            book.last_refusal_log = Some(now);
        }
        due
    }

    /// Starts waiting for an answer for the transfer `id`, or returns the token of the one that
    /// already waits (the library repeats the event). The deadline is not extended by repeats,
    /// and the MAC address is read only here, never on a repeat. A repeat with another PIN is
    /// another transfer under a reused id: the old entry is dropped and a new token is made.
    /// `generation` is the one of the engine the event comes from (`Consents::generation`).
    pub fn open(
        &self,
        generation: u64,
        id: &str,
        device: &str,
        pin: &str,
        ip: Option<IpAddr>,
        now: Instant,
    ) -> Opened {
        let mut book = self.book();
        if generation != self.generation() {
            return Opened::Stale;
        }
        if let Some(entry) = book.pending.get_mut(id) {
            if pin.is_empty() || entry.pin.is_empty() || entry.pin == pin {
                if entry.device.is_empty() && !device.is_empty() {
                    device.clone_into(&mut entry.device);
                }
                if entry.pin.is_empty() {
                    pin.clone_into(&mut entry.pin);
                }
                return Opened::Token(entry.token);
            }
            book.pending.remove(id);
            book.recent.remove(id);
        }
        let ble = is_ble(id);
        let lan = book.pending.keys().filter(|id| !is_ble(id)).count();
        let from_address = ip.map_or(0, |ip| {
            book.pending.values().filter(|e| e.ip == Some(ip)).count()
        });
        if book.pending.len() >= MAX_PENDING
            || (!ble && lan >= MAX_PENDING - BLE_RESERVE)
            || from_address >= MAX_PER_ADDRESS
        {
            return Opened::Full;
        }
        let token = new_token();
        book.pending.insert(
            id.to_owned(),
            Pending {
                token,
                pin: pin.to_owned(),
                deadline: now + CONSENT_TIMEOUT,
                device: device.to_owned(),
                ip,
                mac: ip.and_then(|ip| (self.mac)(ip)),
            },
        );
        Opened::Token(token)
    }

    /// `open` with the running engine's generation and no PIN, for tests.
    #[cfg(test)]
    pub(crate) fn open_for_test(
        &self,
        id: &str,
        device: &str,
        ip: Option<IpAddr>,
        now: Instant,
    ) -> Option<u64> {
        match self.open(self.generation(), id, device, "", ip, now) {
            Opened::Token(token) => Some(token),
            _ => None,
        }
    }

    /// The token of the consent that waits for `id`.
    pub fn token(&self, id: &str) -> Option<u64> {
        self.book().pending.get(id).map(|e| e.token)
    }

    /// The MAC address read when the consent of `id` opened.
    pub fn opened_mac(&self, id: &str) -> Option<String> {
        self.book().pending.get(id).and_then(|e| e.mac.clone())
    }

    /// Answers the consent of `id`. `token` is the one `TransferUpdated` carried; `None` is the
    /// legacy path (`Accept` and `Decline`), which cannot tell a reused id from the original.
    ///
    /// The entry is removed before the answer is sent, so of two racing answers one gets
    /// `NotWaiting`. An accept after the deadline declines the transfer instead. An
    /// `AcceptAndTrust` that cannot be bound to the MAC address read when the consent opened
    /// fails and leaves the consent pending, so the user can still accept without trusting.
    pub fn resolve(
        &self,
        id: &str,
        token: Option<u64>,
        answer: Answer,
        by: By,
        now: Instant,
    ) -> Result<Resolved, ConsentError> {
        let mut book = self.book();
        let entry = book
            .pending
            .get(id)
            .ok_or(ConsentError::NotWaiting)?
            .clone();
        if token.is_some_and(|t| t != entry.token) {
            return Err(ConsentError::StaleToken);
        }
        if answer != Answer::Decline && now >= entry.deadline {
            book.pending.remove(id);
            drop(book);
            // Nobody can accept it any more.
            let _ = self.actions.send(id, TransferAction::ConsentDecline);
            return Err(ConsentError::Expired);
        }
        let mac = if answer == Answer::AcceptAndTrust {
            Some(self.verified_mac(&entry.device, entry.ip, entry.mac.as_deref())?)
        } else {
            None
        };
        book.pending.remove(id);
        if answer != Answer::Decline && by == By::User {
            book.recent.insert(
                id.to_owned(),
                Recent {
                    device: entry.device.clone(),
                    ip: entry.ip,
                    mac: entry.mac,
                    until: now + TRUST_GRACE,
                },
            );
        }
        drop(book);
        let action = if answer == Answer::Decline {
            TransferAction::ConsentDecline
        } else {
            TransferAction::ConsentAccept
        };
        self.actions
            .send(id, action)
            .map_err(|_| ConsentError::NotRunning)?;
        Ok(Resolved {
            device: entry.device,
            mac,
        })
    }

    /// The sender of `id` for the legacy `AllowDevice`: only while its consent is pending, or
    /// within `TRUST_GRACE` after the user accepted it. Returns the name and the verified MAC
    /// address.
    pub fn allow_trust(&self, id: &str, now: Instant) -> Result<(String, String), ConsentError> {
        let mut book = self.book();
        book.recent.retain(|_, r| now < r.until);
        if let Some(entry) = book.pending.get(id) {
            if now >= entry.deadline {
                return Err(ConsentError::Expired);
            }
            let mac = self.verified_mac(&entry.device, entry.ip, entry.mac.as_deref())?;
            return Ok((entry.device.clone(), mac));
        }
        let recent = book.recent.get(id).ok_or(ConsentError::NotWaiting)?;
        let mac = self.verified_mac(&recent.device, recent.ip, recent.mac.as_deref())?;
        Ok((recent.device.clone(), mac))
    }

    /// The MAC address stored at open, if the address still has it.
    fn verified_mac(
        &self,
        device: &str,
        ip: Option<IpAddr>,
        stored: Option<&str>,
    ) -> Result<String, ConsentError> {
        if device.is_empty() {
            return Err(ConsentError::NoName);
        }
        let (Some(ip), Some(stored)) = (ip, stored) else {
            return Err(ConsentError::NoMac);
        };
        match (self.mac)(ip) {
            None => Err(ConsentError::NoMac),
            Some(current) if current != stored => Err(ConsentError::DeviceChanged),
            Some(_) => Ok(stored.to_owned()),
        }
    }

    /// Declines `id` whether or not a consent waits for it (the name guard: a transfer that
    /// wants to write outside the download folder must not be accepted by anything).
    pub fn force_decline(&self, id: &str) {
        self.book().pending.remove(id);
        let _ = self.actions.send(id, TransferAction::ConsentDecline);
    }

    /// The transfer moved on (or stopped waiting): nobody answers it any more. The sender can
    /// still be trusted for a while if the user accepted it.
    pub fn close(&self, id: &str) {
        self.book().pending.remove(id);
    }

    /// The transfer is over: forgets everything about it.
    pub fn finish(&self, id: &str) {
        let mut book = self.book();
        book.pending.remove(id);
        book.recent.remove(id);
    }

    /// Declines every consent whose time is over and returns their ids. Called by one ticker.
    pub fn expire(&self, now: Instant) -> Vec<String> {
        let expired: Vec<String> = {
            let mut book = self.book();
            book.recent.retain(|_, r| now < r.until);
            let expired: Vec<String> = book
                .pending
                .iter()
                .filter(|(_, e)| now >= e.deadline)
                .map(|(id, _)| id.clone())
                .collect();
            for id in &expired {
                book.pending.remove(id);
            }
            expired
        };
        for id in &expired {
            let _ = self.actions.send(id, TransferAction::ConsentDecline);
        }
        expired
    }

    /// The service restarted: its transfers are gone, and so are their tokens.
    pub fn clear(&self) {
        let mut book = self.book();
        self.generation.fetch_add(1, Ordering::SeqCst);
        book.pending.clear();
        book.recent.clear();
    }
}

#[cfg(test)]
mod tests {
    use rqs_lib::channel::{ChannelMessage, Message};
    use tokio::sync::broadcast;

    use super::*;

    const ID: &str = "10.0.0.5:4000";
    const MAC: &str = "aa:bb:cc:dd:ee:01";

    struct Fixture {
        consents: Consents,
        sent: broadcast::Receiver<ChannelMessage>,
        mac: Arc<Mutex<Option<String>>>,
        t0: Instant,
    }

    impl Fixture {
        fn new() -> Self {
            let actions = LibActions::default();
            let (tx, sent) = broadcast::channel(64);
            actions.set(Some(tx));
            let mac = Arc::new(Mutex::new(Some(MAC.to_owned())));
            let reader = mac.clone();
            Self {
                consents: Consents::with_mac_reader(actions, move |_| {
                    reader.lock().unwrap().clone()
                }),
                sent,
                mac,
                t0: Instant::now(),
            }
        }

        fn open(&self, id: &str) -> u64 {
            self.consents
                .open_for_test(id, "Pixel", Some("10.0.0.5".parse().unwrap()), self.t0)
                .unwrap()
        }

        fn answer(
            &self,
            id: &str,
            token: Option<u64>,
            answer: Answer,
        ) -> Result<Resolved, ConsentError> {
            self.consents.resolve(id, token, answer, By::User, self.t0)
        }

        /// The actions sent so far, as (id, accepted).
        fn sent(&mut self) -> Vec<(String, bool)> {
            let mut out = Vec::new();
            while let Ok(message) = self.sent.try_recv() {
                let Message::Lib { action } = message.msg else {
                    continue;
                };
                out.push((message.id, matches!(action, TransferAction::ConsentAccept)));
            }
            out
        }
    }

    #[test]
    fn accept_needs_pending() {
        let mut f = Fixture::new();
        assert_eq!(
            f.answer(ID, None, Answer::Accept),
            Err(ConsentError::NotWaiting)
        );
        assert_eq!(
            f.answer(ID, None, Answer::Decline),
            Err(ConsentError::NotWaiting)
        );
        f.open(ID);
        f.consents.close(ID); // the transfer moved on
        assert_eq!(
            f.answer(ID, None, Answer::Accept),
            Err(ConsentError::NotWaiting)
        );
        assert!(f.sent().is_empty(), "nothing may reach the library");
    }

    #[test]
    fn answer_works_once() {
        let mut f = Fixture::new();
        let token = f.open(ID);
        assert_eq!(
            f.consents.open_for_test(ID, "Pixel", None, f.t0),
            Some(token)
        );
        let resolved = f.answer(ID, Some(token), Answer::Accept).unwrap();
        assert_eq!(resolved.device, "Pixel");
        assert_eq!(
            f.answer(ID, Some(token), Answer::Accept),
            Err(ConsentError::NotWaiting)
        );
        assert_eq!(
            f.answer(ID, None, Answer::Decline),
            Err(ConsentError::NotWaiting)
        );
        assert_eq!(f.sent(), [(ID.to_owned(), true)]);
    }

    #[test]
    fn decline_is_always_possible_while_pending() {
        let mut f = Fixture::new();
        let token = f.open(ID);
        // No MAC and a late hour do not matter to a decline.
        *f.mac.lock().unwrap() = None;
        let late = f.t0 + CONSENT_TIMEOUT + Duration::from_secs(5);
        assert!(
            f.consents
                .resolve(ID, Some(token), Answer::Decline, By::User, late)
                .is_ok()
        );
        assert_eq!(f.sent(), [(ID.to_owned(), false)]);
    }

    #[test]
    fn expired_consent_cannot_be_accepted() {
        let mut f = Fixture::new();
        let token = f.open(ID);
        let just_before = f.t0 + CONSENT_TIMEOUT - Duration::from_secs(1);
        let at_deadline = f.t0 + CONSENT_TIMEOUT;
        for answer in [Answer::Accept, Answer::AcceptAndTrust] {
            let other = "10.0.0.5:4001";
            let other_token = f.open(other);
            assert_eq!(
                f.consents
                    .resolve(other, Some(other_token), answer, By::User, at_deadline),
                Err(ConsentError::Expired)
            );
            // It was declined on the spot: a retry finds nothing to replay.
            assert_eq!(
                f.consents
                    .resolve(other, Some(other_token), answer, By::User, just_before),
                Err(ConsentError::NotWaiting)
            );
            assert_eq!(f.sent(), [(other.to_owned(), false)]);
        }
        // The ticker declines the ones nobody touched.
        assert!(f.consents.expire(just_before).is_empty());
        assert_eq!(f.consents.expire(at_deadline), [ID]);
        assert_eq!(f.sent(), [(ID.to_owned(), false)]);
        assert_eq!(
            f.consents.resolve(
                ID,
                Some(token),
                Answer::Accept,
                By::User,
                at_deadline + Duration::from_secs(1)
            ),
            Err(ConsentError::NotWaiting)
        );
        assert!(f.sent().is_empty());
    }

    #[test]
    fn repeated_events_do_not_extend_the_deadline() {
        let f = Fixture::new();
        let token = f.open(ID);
        let later = f.t0 + CONSENT_TIMEOUT - Duration::from_secs(1);
        assert_eq!(
            f.consents.open_for_test(ID, "Pixel", None, later),
            Some(token)
        );
        assert_eq!(f.consents.expire(f.t0 + CONSENT_TIMEOUT), [ID]);
    }

    #[test]
    fn stale_token_cannot_accept_reused_id() {
        let mut f = Fixture::new();
        let old = f.open(ID);
        f.answer(ID, Some(old), Answer::Decline).unwrap();
        f.consents.finish(ID);
        f.sent();
        // The same id again: another transfer.
        let new = f.open(ID);
        assert_ne!(old, new);
        for answer in [Answer::Accept, Answer::AcceptAndTrust, Answer::Decline] {
            assert_eq!(
                f.answer(ID, Some(old), answer),
                Err(ConsentError::StaleToken)
            );
            assert_eq!(f.answer(ID, Some(0), answer), Err(ConsentError::StaleToken));
        }
        assert!(f.sent().is_empty(), "a stale prompt must not answer");
        assert!(f.answer(ID, Some(new), Answer::Accept).is_ok());
        assert_eq!(f.sent(), [(ID.to_owned(), true)]);
    }

    #[test]
    fn concurrent_answers_resolve_once() {
        let mut f = Fixture::new();
        let token = f.open(ID);
        let answers = [Answer::Accept, Answer::AcceptAndTrust, Answer::Decline];
        let threads: Vec<_> = (0..24)
            .map(|n| {
                let consents = f.consents.clone();
                let t0 = f.t0;
                std::thread::spawn(move || {
                    consents
                        .resolve(ID, Some(token), answers[n % 3], By::User, t0)
                        .is_ok()
                })
            })
            .collect();
        let won = threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(|ok| *ok)
            .count();
        assert_eq!(won, 1, "exactly one answer may win");
        assert_eq!(f.sent().len(), 1, "and only one action reaches the library");
    }

    #[test]
    fn accept_and_trust_needs_same_mac() {
        let mut f = Fixture::new();
        let token = f.open(ID);

        *f.mac.lock().unwrap() = Some("aa:bb:cc:dd:ee:02".into());
        assert_eq!(
            f.answer(ID, Some(token), Answer::AcceptAndTrust),
            Err(ConsentError::DeviceChanged)
        );
        *f.mac.lock().unwrap() = None;
        assert_eq!(
            f.answer(ID, Some(token), Answer::AcceptAndTrust),
            Err(ConsentError::NoMac)
        );
        assert!(f.sent().is_empty(), "a refused trust accepts nothing");
        assert_eq!(f.consents.token(ID), Some(token), "still pending");

        *f.mac.lock().unwrap() = Some(MAC.into());
        let resolved = f.answer(ID, Some(token), Answer::AcceptAndTrust).unwrap();
        assert_eq!(resolved.mac.as_deref(), Some(MAC));
        assert_eq!(f.sent(), [(ID.to_owned(), true)]);
    }

    #[test]
    fn accept_and_trust_needs_a_mac_at_open_and_a_name() {
        let f = Fixture::new();
        *f.mac.lock().unwrap() = None;
        let token = f.open(ID); // no MAC known when the consent opens
        *f.mac.lock().unwrap() = Some(MAC.into()); // one that appears later is not the one seen
        assert_eq!(
            f.answer(ID, Some(token), Answer::AcceptAndTrust),
            Err(ConsentError::NoMac)
        );
        let nameless = f
            .consents
            .open_for_test("10.0.0.6:4000", "", Some("10.0.0.6".parse().unwrap()), f.t0)
            .unwrap();
        assert_eq!(
            f.answer("10.0.0.6:4000", Some(nameless), Answer::AcceptAndTrust),
            Err(ConsentError::NoName)
        );
        // Without an address (Bluetooth) there is no MAC either.
        let ble = f
            .consents
            .open_for_test("ble:AB", "Pixel", None, f.t0)
            .unwrap();
        assert_eq!(
            f.answer("ble:AB", Some(ble), Answer::AcceptAndTrust),
            Err(ConsentError::NoMac)
        );
    }

    #[test]
    fn restart_forgets_tokens() {
        let mut f = Fixture::new();
        let token = f.open(ID);
        f.answer(ID, Some(token), Answer::Accept).unwrap();
        f.sent();
        let second = f.open("10.0.0.6:4000");
        f.consents.clear();
        assert_eq!(f.consents.token("10.0.0.6:4000"), None);
        assert_eq!(
            f.answer("10.0.0.6:4000", Some(second), Answer::Accept),
            Err(ConsentError::NotWaiting)
        );
        assert_eq!(
            f.consents.allow_trust(ID, f.t0),
            Err(ConsentError::NotWaiting)
        );
        assert!(f.sent().is_empty());
    }

    #[test]
    fn legacy_trust_only_while_pending_or_just_after_the_user_accepted() {
        let f = Fixture::new();
        // Nothing waits: an outbound or finished transfer, or an id nobody knows.
        assert_eq!(
            f.consents.allow_trust(ID, f.t0),
            Err(ConsentError::NotWaiting)
        );

        let token = f.open(ID);
        assert_eq!(
            f.consents.allow_trust(ID, f.t0),
            Ok(("Pixel".to_owned(), MAC.to_owned()))
        );
        f.answer(ID, Some(token), Answer::Accept).unwrap();
        let soon = f.t0 + TRUST_GRACE - Duration::from_secs(1);
        assert!(f.consents.allow_trust(ID, soon).is_ok());
        let late = f.t0 + TRUST_GRACE;
        assert_eq!(
            f.consents.allow_trust(ID, late),
            Err(ConsentError::NotWaiting)
        );

        // A finished transfer is forgotten at once.
        let token = f.open("10.0.0.7:1");
        f.answer("10.0.0.7:1", Some(token), Answer::Accept).unwrap();
        f.consents.close("10.0.0.7:1"); // it moved on: the grace period goes on
        assert!(f.consents.allow_trust("10.0.0.7:1", f.t0).is_ok());
        f.consents.finish("10.0.0.7:1");
        assert_eq!(
            f.consents.allow_trust("10.0.0.7:1", f.t0),
            Err(ConsentError::NotWaiting)
        );

        // An automatic accept is no reason to trust the sender.
        let token = f.open("10.0.0.8:1");
        f.consents
            .resolve(
                "10.0.0.8:1",
                Some(token),
                Answer::Accept,
                By::PairedComputer,
                f.t0,
            )
            .unwrap();
        assert_eq!(
            f.consents.allow_trust("10.0.0.8:1", f.t0),
            Err(ConsentError::NotWaiting)
        );
    }

    #[test]
    fn the_name_guard_declines_with_or_without_a_consent() {
        let mut f = Fixture::new();
        f.open(ID);
        f.consents.force_decline(ID);
        f.consents.force_decline("10.0.0.9:1");
        assert_eq!(
            f.sent(),
            [(ID.to_owned(), false), ("10.0.0.9:1".to_owned(), false)]
        );
        assert_eq!(f.consents.token(ID), None);
    }

    #[test]
    fn a_flood_of_senders_cannot_pile_up_prompts() {
        let f = Fixture::new();
        for n in 0..MAX_PENDING - BLE_RESERVE {
            assert!(
                f.consents
                    .open_for_test(&format!("10.0.1.{n}:1"), "x", None, f.t0)
                    .is_some()
            );
        }
        assert!(
            f.consents
                .open_for_test("10.0.2.1:1", "x", None, f.t0)
                .is_none()
        );
        // The places kept for Bluetooth stay usable, and then the book is full.
        for n in 0..BLE_RESERVE {
            assert!(
                f.consents
                    .open_for_test(&format!("ble-{n}"), "x", None, f.t0)
                    .is_some()
            );
        }
        assert!(
            f.consents
                .open_for_test("ble-99", "x", None, f.t0)
                .is_none()
        );
        // One that already waits is still found.
        assert!(
            f.consents
                .open_for_test("10.0.1.0:1", "x", None, f.t0)
                .is_some()
        );
    }

    fn open_pin(f: &Fixture, id: &str, pin: &str) -> Opened {
        let ip = "10.0.0.5".parse().ok();
        f.consents
            .open(f.consents.generation(), id, "Pixel", pin, ip, f.t0)
    }

    #[test]
    fn a_new_session_under_a_reused_id_gets_a_new_token() {
        let mut f = Fixture::new();
        let Opened::Token(old) = open_pin(&f, ID, "1234") else {
            panic!("not opened");
        };
        // The library repeats the event, without or with the same PIN: same consent.
        assert_eq!(open_pin(&f, ID, ""), Opened::Token(old));
        assert_eq!(open_pin(&f, ID, "1234"), Opened::Token(old));
        // Another PIN: another transfer, whatever the id says.
        let Opened::Token(new) = open_pin(&f, ID, "9999") else {
            panic!("not opened");
        };
        assert_ne!(old, new);
        assert_eq!(
            f.answer(ID, Some(old), Answer::Accept),
            Err(ConsentError::StaleToken)
        );
        assert!(f.answer(ID, Some(new), Answer::Accept).is_ok());
        assert_eq!(f.sent(), [(ID.to_owned(), true)]);
    }

    #[test]
    fn a_repeat_never_fills_in_the_mac_address() {
        let f = Fixture::new();
        *f.mac.lock().unwrap() = None;
        let token = f.open(ID); // no MAC when the consent opened
        *f.mac.lock().unwrap() = Some(MAC.into());
        assert_eq!(
            f.consents
                .open_for_test(ID, "Pixel", Some("10.0.0.5".parse().unwrap()), f.t0),
            Some(token)
        );
        assert_eq!(f.consents.opened_mac(ID), None);
    }

    #[test]
    fn events_of_a_stopped_engine_are_ignored() {
        let f = Fixture::new();
        let old = f.consents.generation();
        f.consents.clear();
        assert_ne!(f.consents.generation(), old);
        assert_eq!(
            f.consents.open(old, ID, "Pixel", "", None, f.t0),
            Opened::Stale
        );
        assert_eq!(f.consents.token(ID), None);
        assert!(matches!(
            f.consents
                .open(f.consents.generation(), ID, "Pixel", "", None, f.t0),
            Opened::Token(_)
        ));
    }

    #[test]
    fn one_address_cannot_hold_more_than_a_few_prompts() {
        let f = Fixture::new();
        let ip = "10.0.0.5".parse().ok();
        for n in 0..MAX_PER_ADDRESS {
            assert!(
                f.consents
                    .open_for_test(&format!("10.0.0.5:{n}"), "x", ip, f.t0)
                    .is_some()
            );
        }
        assert_eq!(
            f.consents
                .open(f.consents.generation(), "10.0.0.5:99", "x", "", ip, f.t0),
            Opened::Full
        );
        // Another address is not affected.
        let other = "10.0.0.6".parse().ok();
        assert!(
            f.consents
                .open_for_test("10.0.0.6:1", "x", other, f.t0)
                .is_some()
        );
    }

    #[test]
    fn a_flood_over_the_network_leaves_room_for_bluetooth() {
        let f = Fixture::new();
        let mut taken = 0;
        for n in 0..MAX_PENDING * 2 {
            let ip = IpAddr::from([10, 1, (n / 4) as u8, (n % 250) as u8 + 1]);
            if f.consents
                .open_for_test(&format!("{ip}:1"), "x", Some(ip), f.t0)
                .is_some()
            {
                taken += 1;
            }
        }
        assert_eq!(taken, MAX_PENDING - BLE_RESERVE);
        assert!(
            f.consents
                .open_for_test("ble-1", "Pixel", None, f.t0)
                .is_some()
        );
        // The reserve is for Bluetooth: it can fill the book, but not past it.
        for n in 2..=MAX_PENDING {
            f.consents
                .open_for_test(&format!("ble-{n}"), "Pixel", None, f.t0);
        }
        assert_eq!(
            f.consents
                .open(f.consents.generation(), "ble-99", "", "", None, f.t0),
            Opened::Full
        );
    }

    #[test]
    fn a_refused_flood_is_logged_once_in_a_while() {
        let f = Fixture::new();
        assert!(f.consents.note_refusal(f.t0));
        assert!(!f.consents.note_refusal(f.t0 + Duration::from_secs(1)));
        assert!(f.consents.note_refusal(f.t0 + REFUSAL_LOG_EVERY));
    }

    #[test]
    fn answers_are_parsed() {
        assert_eq!(Answer::parse("accept"), Some(Answer::Accept));
        assert_eq!(
            Answer::parse("accept_and_trust"),
            Some(Answer::AcceptAndTrust)
        );
        assert_eq!(Answer::parse("decline"), Some(Answer::Decline));
        assert_eq!(Answer::parse("Accept"), None);
        assert_eq!(Answer::parse(""), None);
    }
}
