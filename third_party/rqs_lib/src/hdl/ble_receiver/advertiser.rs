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

use super::service_data::SERVICE_UUID;

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

pub fn advertisement(service_data: Vec<u8>) -> Advertisement {
    Advertisement {
        // Connectable: the phone connects to read slot 0 and open the weave socket.
        advertisement_type: Type::Peripheral,
        service_data: [(Uuid::from_u16(SERVICE_UUID), service_data)].into(),
        discoverable: Some(true),
        min_interval: Some(MIN_INTERVAL),
        max_interval: Some(MAX_INTERVAL),
        // The service data does not fit a legacy PDU (31 bytes): without a secondary channel
        // BlueZ may refuse the set (`Invalid Parameters`, seen upstream, not reproduced here).
        secondary_channel: Some(SecondaryChannel::OneM),
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
}

impl Register for BluerRegister {
    type Handle = AdvertisementHandle;

    async fn register(&self) -> anyhow::Result<AdvertisementHandle> {
        Ok(self
            .adapter
            .advertise(advertisement(self.service_data.clone()))
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
    /// Fails if the very first registration does; later failures are retried.
    pub async fn run(self) -> anyhow::Result<Infallible> {
        // The payload does not fit a legacy advertisement; only extended advertising carries it.
        if let Ok(Some(caps)) = self.adapter.supported_advertising_capabilities().await {
            let need = required_adv_length(self.service_data.len());
            if usize::from(caps.max_advertisement_length) < need {
                bail!(
                    "controller advertises at most {} bytes, {need} needed (no extended advertising?)",
                    caps.max_advertisement_length
                );
            }
        }
        let len = self.service_data.len();
        let register = BluerRegister {
            adapter: self.adapter,
            service_data: self.service_data,
        };
        keep_advertising(&register, &self.rearm, TIMING, || {
            info!("ble receiver: advertising {len} bytes on 0x{SERVICE_UUID:04X}");
        })
        .await
    }
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
        let adv = advertisement(vec![1, 2, 3]);
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
    fn a_typical_payload_needs_extended_advertising() {
        use crate::hdl::ble_receiver::service_data::ReceiverAdvertisement;
        let data = ReceiverAdvertisement::new([b'A'; 4], 3, [0; 16], "my laptop").to_bytes();
        assert!(required_adv_length(data.len()) > 31);
        assert!(required_adv_length(data.len()) < 251);
    }
}
