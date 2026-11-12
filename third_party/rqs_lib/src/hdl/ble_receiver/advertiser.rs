//! BlueZ advertisement of the receiver (service data under 0xFEF3).

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use bluer::adv::{Advertisement, AdvertisementHandle, SecondaryChannel, Type};
use bluer::{Adapter, Uuid, UuidExt};
use futures::FutureExt;
use tokio::sync::Notify;
use tokio::time::Instant;

use super::service_data::{SERVICE_UUID, build_legacy_header};

const MIN_INTERVAL: Duration = Duration::from_millis(100);
const MAX_INTERVAL: Duration = Duration::from_millis(150);
/// A connectable set is used up by a connection; this is the safety net behind the kicks.
const REREGISTER_EVERY: Duration = Duration::from_secs(30);
/// A re-registration is kept at least this long, whatever the kicks.
const MIN_HOLD: Duration = Duration::from_secs(1);
const RETRY_AFTER: Duration = Duration::from_secs(5);
const MAX_RETRY: Duration = Duration::from_secs(60);

/// Bytes the controller must be able to advertise for `payload_len` bytes of service data:
/// the AD structure header (length, type, UUID16) and the flags BlueZ adds.
pub fn required_adv_length(payload_len: usize) -> usize {
    payload_len + 4 + 3
}

pub fn advertisement(service_data: Vec<u8>, legacy: bool) -> Advertisement {
    Advertisement {
        // Connectable: the phone connects to read slot 0 and open the weave socket.
        advertisement_type: Type::Peripheral,
        service_data: [(Uuid::from_u16(SERVICE_UUID), service_data)].into(),
        discoverable: Some(true),
        min_interval: Some(MIN_INTERVAL),
        max_interval: Some(MAX_INTERVAL),
        // Only full service data needs an extended advertising set. 23 bytes of base64
        // legacy header (plus flags and AD overhead) fit in the classic 31-byte PDU.
        secondary_channel: (!legacy).then_some(SecondaryChannel::OneM),
        ..Default::default()
    }
}

/// Registers the advertisement; dropping the handle unregisters it.
pub trait Register {
    type Handle;
    fn register(&self) -> impl Future<Output = anyhow::Result<Self::Handle>> + Send;
}

struct BluerRegister {
    adapter: Arc<Adapter>,
    service_data: Vec<u8>,
    legacy: bool,
}

impl Register for BluerRegister {
    type Handle = AdvertisementHandle;

    async fn register(&self) -> anyhow::Result<AdvertisementHandle> {
        Ok(self
            .adapter
            .advertise(advertisement(self.service_data.clone(), self.legacy))
            .await?)
    }
}

#[derive(Clone, Copy)]
pub struct Timing {
    pub reregister_every: Duration,
    /// Shortest time a registration is kept, so a burst of kicks makes one re-registration.
    pub min_hold: Duration,
    pub retry_after: Duration,
    pub max_retry: Duration,
}

pub const TIMING: Timing = Timing {
    reregister_every: REREGISTER_EVERY,
    min_hold: MIN_HOLD,
    retry_after: RETRY_AFTER,
    max_retry: MAX_RETRY,
};

pub struct ReceiverAdvertiser {
    adapter: Arc<Adapter>,
    service_data: Vec<u8>,
    rearm: Arc<Notify>,
}

impl ReceiverAdvertiser {
    /// `rearm` is notified whenever the set may have been used up by a connection.
    pub fn new(adapter: Arc<Adapter>, service_data: Vec<u8>, rearm: Arc<Notify>) -> Self {
        Self {
            adapter,
            service_data,
            rearm,
        }
    }

    /// Advertises until the future is dropped, which deregisters the advertisement.
    /// Fails if the very first registration does (after the one retry of
    /// [`retry_as_legacy`]); later failures are retried.
    pub async fn run(self) -> anyhow::Result<Infallible> {
        let full = self.service_data;
        let max = match self.adapter.supported_advertising_capabilities().await {
            Ok(Some(caps)) => Some(usize::from(caps.max_advertisement_length)),
            _ => None,
        };
        let mut payload = payload_for(max, &full)?;
        loop {
            let (len, legacy) = (payload.data.len(), payload.legacy);
            let register = BluerRegister {
                adapter: self.adapter.clone(),
                service_data: payload.data,
                legacy,
            };
            let outcome = keep_advertising(&register, &self.rearm, TIMING, || {
                info!(
                    "ble receiver: advertising {len} bytes on 0x{SERVICE_UUID:04X} (legacy={legacy})"
                );
            })
            .await;
            match outcome {
                Err(e) if retry_as_legacy(max, legacy, &e) => {
                    warn!(
                        "ble receiver: the extended advertisement was refused ({e:#}); trying the legacy header"
                    );
                    payload = legacy_payload(&full);
                }
                outcome => return outcome,
            }
        }
    }
}

/// The service data to register and whether it needs the legacy set (the classic 31-byte PDU).
#[derive(Debug, PartialEq, Eq)]
pub struct Payload {
    pub data: Vec<u8>,
    pub legacy: bool,
}

fn legacy_payload(full: &[u8]) -> Payload {
    Payload {
        data: build_legacy_header(full),
        legacy: true,
    }
}

/// Picks the payload for a controller that can advertise `max_len` bytes (`None`: BlueZ did not
/// say). The full record is kept whenever it fits or the capacity is unknown (the extended
/// advertising path that works today); only a known shortfall selects the legacy header, with
/// the full record still served from GATT slot 0.
pub fn payload_for(max_len: Option<usize>, full: &[u8]) -> anyhow::Result<Payload> {
    match max_len {
        Some(max) if max < required_adv_length(full.len()) => {
            let payload = legacy_payload(full);
            let needed = required_adv_length(payload.data.len());
            if max < needed {
                bail!(
                    "controller advertises at most {max} bytes, even a legacy Nearby header needs {needed}"
                );
            }
            Ok(payload)
        }
        _ => Ok(Payload {
            data: full.to_vec(),
            legacy: false,
        }),
    }
}

/// With the capacity unknown the extended record was tried blind; a refusal of its parameters
/// (BlueZ answers `InvalidArguments` or `InvalidLength`, *(inferred)* from the error kinds bluer
/// knows) gets one retry with the legacy header. A known capacity was already respected by [`payload_for`].
fn retry_as_legacy(max_len: Option<usize>, already_legacy: bool, error: &anyhow::Error) -> bool {
    max_len.is_none()
        && !already_legacy
        && error.downcast_ref::<bluer::Error>().is_some_and(|e| {
            matches!(
                e.kind,
                bluer::ErrorKind::InvalidArguments | bluer::ErrorKind::InvalidLength
            )
        })
}

/// Keeps one registration alive, replacing it every `reregister_every` and as soon as `rearm`
/// fires (a connection uses the connectable set up and BlueZ does not start it again).
/// The old registration is dropped before the new one is made, so there is never more than
/// one. After the first success a failure is retried with a growing delay that `rearm`
/// does not shorten. `on_first` runs after the first success.
pub async fn keep_advertising<R: Register>(
    register: &R,
    rearm: &Notify,
    timing: Timing,
    on_first: impl FnOnce(),
) -> anyhow::Result<Infallible> {
    let mut on_first = Some(on_first);
    let mut failures = 0u32;
    loop {
        match register.register().await {
            Ok(handle) => {
                let registered = Instant::now();
                failures = 0;
                if let Some(f) = on_first.take() {
                    f();
                }
                tokio::select! {
                    () = tokio::time::sleep(timing.reregister_every) => {}
                    () = rearm.notified() => {}
                }
                tokio::time::sleep_until(registered + timing.min_hold).await;
                // Kicks that came in meanwhile are served by the registration below.
                rearm.notified().now_or_never();
                drop(handle);
            }
            Err(e) if on_first.is_some() => {
                return Err(e).context("registering the advertisement");
            }
            Err(e) => {
                let delay = timing
                    .retry_after
                    .saturating_mul(1u32 << failures.min(10))
                    .min(timing.max_retry);
                failures = failures.saturating_add(1);
                warn!("ble receiver: advertisement failed ({e}); retrying in {delay:?}");
                tokio::time::sleep(delay).await;
                rearm.notified().now_or_never();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::hdl::ble_receiver::service_data::parse_legacy_header;

    #[derive(Default)]
    struct Fake {
        attempts: Mutex<Vec<Instant>>,
        live: AtomicUsize,
        max_live: AtomicUsize,
        /// Attempts from this one on (1-based) fail.
        fail_from: AtomicUsize,
    }

    struct FakeHandle(Arc<Fake>);

    impl Drop for FakeHandle {
        fn drop(&mut self) {
            self.0.live.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl Fake {
        fn attempts(&self) -> usize {
            self.attempts.lock().unwrap().len()
        }
    }

    struct FakeRegister(Arc<Fake>);

    impl Register for FakeRegister {
        type Handle = FakeHandle;

        async fn register(&self) -> anyhow::Result<FakeHandle> {
            let n = {
                let mut attempts = self.0.attempts.lock().unwrap();
                attempts.push(Instant::now());
                attempts.len()
            };
            tokio::task::yield_now().await;
            let fail_from = self.0.fail_from.load(Ordering::SeqCst);
            if fail_from != 0 && n >= fail_from {
                bail!("BlueZ said no");
            }
            let live = self.0.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.0.max_live.fetch_max(live, Ordering::SeqCst);
            Ok(FakeHandle(self.0.clone()))
        }
    }

    fn timing() -> Timing {
        Timing {
            reregister_every: Duration::from_secs(60),
            min_hold: Duration::from_millis(30),
            retry_after: Duration::from_millis(40),
            max_retry: Duration::from_millis(160),
        }
    }

    fn start(
        fake: &Arc<Fake>,
        timing: Timing,
    ) -> (
        Arc<Notify>,
        tokio::task::JoinHandle<anyhow::Result<Infallible>>,
    ) {
        let rearm = Arc::new(Notify::new());
        let task = tokio::spawn({
            let (fake, rearm) = (fake.clone(), rearm.clone());
            async move { keep_advertising(&FakeRegister(fake), &rearm, timing, || {}).await }
        });
        (rearm, task)
    }

    #[tokio::test]
    async fn a_consumed_set_is_advertised_again_within_the_hold_time() {
        let fake = Arc::new(Fake::default());
        let (rearm, task) = start(&fake, timing());
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(fake.attempts(), 1);

        let kicked = Instant::now();
        rearm.notify_one();
        while fake.attempts() < 2 {
            assert!(
                kicked.elapsed() < Duration::from_secs(1),
                "not re-registered"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(kicked.elapsed() < Duration::from_millis(500));
        task.abort();
    }

    #[tokio::test]
    async fn a_burst_of_kicks_never_doubles_the_registration() {
        let fake = Arc::new(Fake::default());
        let (rearm, task) = start(&fake, timing());
        tokio::time::sleep(Duration::from_millis(10)).await;
        for _ in 0..10 {
            rearm.notify_one();
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        // One re-registration for the whole burst, and never two live at once.
        assert_eq!(fake.attempts(), 2);
        assert_eq!(fake.max_live.load(Ordering::SeqCst), 1);
        task.abort();
    }

    #[tokio::test]
    async fn the_timer_is_the_safety_net() {
        let fake = Arc::new(Fake::default());
        let t = Timing {
            reregister_every: Duration::from_millis(40),
            ..timing()
        };
        let (_rearm, task) = start(&fake, t);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(fake.attempts() >= 3);
        assert_eq!(fake.max_live.load(Ordering::SeqCst), 1);
        task.abort();
    }

    #[tokio::test]
    async fn failures_back_off_and_kicks_do_not_shorten_it() {
        let fake = Arc::new(Fake::default());
        fake.fail_from.store(2, Ordering::SeqCst);
        let (rearm, task) = start(&fake, timing());
        tokio::time::sleep(Duration::from_millis(10)).await;
        rearm.notify_one();
        let begin = Instant::now();
        // Attempts after the kick fail at +30ms (hold), then wait 40, 80, 160, 160...
        // The window is measured, not counted in sleeps: on a loaded machine 40 sleeps of
        // 10 ms take longer than 400 ms and let the next attempt (at about 470 ms) in.
        while begin.elapsed() < Duration::from_millis(400) {
            rearm.notify_one();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let n = fake.attempts();
        // 400ms: at most hold + 40 + 80 + 160 gaps, so a handful of attempts, not a loop.
        assert!(
            (3..=5).contains(&n),
            "{n} attempts in {:?}",
            begin.elapsed()
        );
        let at = fake.attempts.lock().unwrap().clone();
        let gaps: Vec<_> = at.windows(2).skip(1).map(|w| w[1] - w[0]).collect();
        assert!(gaps[0] >= Duration::from_millis(40));
        assert!(gaps.windows(2).all(|g| g[1] >= g[0]));
        assert!(!task.is_finished());
        task.abort();
    }

    #[tokio::test]
    async fn the_first_failure_is_reported() {
        let fake = Arc::new(Fake::default());
        fake.fail_from.store(1, Ordering::SeqCst);
        let (_rearm, task) = start(&fake, timing());
        let r = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(r.is_err());
        assert_eq!(fake.attempts(), 1);
    }

    #[test]
    fn advertisement_is_connectable_fast_and_carries_the_service_data() {
        let adv = advertisement(vec![1, 2, 3], false);
        assert_eq!(adv.advertisement_type, Type::Peripheral);
        assert_eq!(adv.discoverable, Some(true));
        assert_eq!(adv.min_interval, Some(Duration::from_millis(100)));
        assert_eq!(adv.max_interval, Some(Duration::from_millis(150)));
        assert_eq!(
            adv.service_data.get(&Uuid::from_u16(0xFEF3)),
            Some(&vec![1, 2, 3])
        );
        assert_eq!(adv.service_data.len(), 1);
        assert_eq!(adv.secondary_channel, Some(SecondaryChannel::OneM));
    }

    #[test]
    fn legacy_header_disables_secondary_channel_and_fits() {
        let header = build_legacy_header(b"full advertisement");
        let adv = advertisement(header.clone(), true);
        assert!(required_adv_length(header.len()) <= 31);
        assert_eq!(adv.secondary_channel, None);
        assert_eq!(
            adv.service_data.get(&Uuid::from_u16(SERVICE_UUID)),
            Some(&header)
        );
    }

    #[test]
    fn a_typical_payload_needs_extended_advertising() {
        use crate::hdl::ble_receiver::service_data::ReceiverAdvertisement;
        let data = ReceiverAdvertisement::new([b'A'; 4], 3, [0; 16], "my laptop").to_bytes();
        assert!(required_adv_length(data.len()) > 31);
        assert!(required_adv_length(data.len()) < 251);
    }

    const FULL_LEN: usize = 132;

    #[test]
    fn the_full_record_is_used_when_it_fits_or_the_capacity_is_unknown() {
        let full = vec![7u8; FULL_LEN];
        let expected = Payload {
            data: full.clone(),
            legacy: false,
        };
        assert_eq!(payload_for(None, &full).unwrap(), expected);
        assert_eq!(
            payload_for(Some(required_adv_length(FULL_LEN)), &full).unwrap(),
            expected
        );
    }

    #[test]
    fn a_controller_that_cannot_carry_the_record_gets_the_legacy_header() {
        let full = vec![7u8; FULL_LEN];
        let payload = payload_for(Some(required_adv_length(FULL_LEN) - 1), &full).unwrap();
        assert!(payload.legacy);
        assert_eq!(payload.data.len(), 23);
        assert!(parse_legacy_header(&payload.data).is_some());
        // 31 bytes, the classic PDU, is exactly enough for it.
        assert!(payload_for(Some(31), &full).unwrap().legacy);
    }

    #[test]
    fn a_controller_too_small_even_for_the_legacy_header_is_an_error() {
        let full = vec![7u8; FULL_LEN];
        let error = payload_for(Some(required_adv_length(23) - 1), &full).unwrap_err();
        assert!(error.to_string().contains("legacy"), "{error}");
    }

    fn refused(kind: bluer::ErrorKind) -> anyhow::Error {
        anyhow::Error::from(bluer::Error {
            kind,
            message: "refused".into(),
        })
        .context("registering the advertisement")
    }

    #[test]
    fn only_a_blind_extended_registration_refused_for_its_parameters_is_retried() {
        let invalid = refused(bluer::ErrorKind::InvalidArguments);
        assert!(retry_as_legacy(None, false, &invalid));
        assert!(retry_as_legacy(
            None,
            false,
            &refused(bluer::ErrorKind::InvalidLength)
        ));
        // The capacity was known, or the legacy header was already the one refused.
        assert!(!retry_as_legacy(Some(251), false, &invalid));
        assert!(!retry_as_legacy(None, true, &invalid));
        // Other failures are not about the size.
        assert!(!retry_as_legacy(
            None,
            false,
            &refused(bluer::ErrorKind::Failed)
        ));
        assert!(!retry_as_legacy(None, false, &anyhow::anyhow!("no bus")));
    }
}
