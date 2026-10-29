//! Quick Share receiver over BLE (GATT "weave" socket).
//!
//! A phone with Wi-Fi off finds us by the 0xFEF3 advertisement, connects over GATT and talks
//! to an `InboundRequest` through [`bridge`]; the WIFI_HOTSPOT upgrade then takes over, since
//! BLE alone moves about 5 KB/s. Hence the receiver only runs while a `WifiLink` is set and
//! we are visible.
#![cfg_attr(
    not(all(feature = "experimental", target_os = "linux")),
    allow(dead_code)
)]

use std::time::Duration;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::hdl::Visibility;

#[cfg(all(feature = "experimental", target_os = "linux"))]
pub mod advertiser;
pub mod bridge;
#[cfg(all(feature = "experimental", target_os = "linux"))]
pub mod gatt;
pub mod service_data;
pub mod session;
pub mod socket;
pub mod weave;

/// Wait before trying again after the receiver failed to start or stopped.
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// Same visibility rule as mDNS, which also answers while `Temporarily` visible.
fn wanted(visibility: &watch::Receiver<Visibility>, link: &watch::Receiver<bool>) -> bool {
    *link.borrow() && *visibility.borrow() != Visibility::Invisible
}

/// False once a sender is gone.
async fn changed(
    visibility: &mut watch::Receiver<Visibility>,
    link: &mut watch::Receiver<bool>,
) -> bool {
    tokio::select! {
        r = visibility.changed() => r.is_ok(),
        r = link.changed() => r.is_ok(),
    }
}

async fn until_unwanted(
    visibility: &mut watch::Receiver<Visibility>,
    link: &mut watch::Receiver<bool>,
) -> bool {
    while wanted(visibility, link) {
        if !changed(visibility, link).await {
            return false;
        }
    }
    true
}

/// Runs `start(stop)` while a link is set and we are visible. When that stops being true,
/// `stop` is cancelled and the future gets to wind down (finish with `Ok`), so a session in
/// flight can end by itself; it must not outlive its own bound. A failure is logged and
/// retried after `retry`, whatever changes meanwhile, and never ends the supervisor. Returns
/// only when a watch sender is dropped.
pub async fn supervise<F, Fut>(
    mut start: F,
    mut visibility: watch::Receiver<Visibility>,
    mut link: watch::Receiver<bool>,
    retry: Duration,
) where
    F: FnMut(CancellationToken) -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let mut failures = 0u32;
    loop {
        if !wanted(&visibility, &link) {
            failures = 0;
            if !changed(&mut visibility, &mut link).await {
                return;
            }
            continue;
        }
        let stop = CancellationToken::new();
        let failure = {
            let run = start(stop.clone());
            tokio::pin!(run);
            tokio::select! {
                r = &mut run => r,
                alive = until_unwanted(&mut visibility, &mut link) => {
                    if !alive {
                        return;
                    }
                    stop.cancel();
                    // Wound down, or failed on the way: either way we start over.
                    let _ = run.await;
                    continue;
                }
            }
        };
        let Err(e) = failure else { continue };
        if failures == 0 {
            warn!("ble receiver: not running: {e:#}");
        } else {
            debug!("ble receiver: still not running: {e:#}");
        }
        failures = failures.saturating_add(1);
        let backoff = tokio::time::sleep(retry);
        tokio::pin!(backoff);
        loop {
            tokio::select! {
                () = &mut backoff => break,
                alive = changed(&mut visibility, &mut link) => {
                    if !alive {
                        return;
                    }
                    // Only going away cuts the backoff short (the loop above resets it).
                    if !wanted(&visibility, &link) {
                        break;
                    }
                }
            }
        }
    }
}

#[cfg(all(feature = "experimental", target_os = "linux"))]
pub use real::spawn;

#[cfg(all(feature = "experimental", target_os = "linux"))]
mod real {
    use std::sync::Arc;

    use anyhow::Context;
    use tokio::sync::broadcast;
    use tokio_util::task::TaskTracker;

    use super::advertiser::ReceiverAdvertiser;
    use super::service_data::{DEVICE_TYPE_LAPTOP, ReceiverAdvertisement};
    use super::*;
    use crate::channel::ChannelMessage;

    const DRAIN_POLL: Duration = Duration::from_secs(1);

    /// Starts the supervisor on `tracker`. Nothing here can fail the caller: BlueZ problems
    /// only show up as warnings.
    pub fn spawn(
        tracker: &TaskTracker,
        ctk: CancellationToken,
        endpoint_id: [u8; 4],
        identity: [u8; 16],
        bus: broadcast::Sender<ChannelMessage>,
        visibility: watch::Receiver<Visibility>,
        link: watch::Receiver<bool>,
    ) {
        tracker.spawn(async move {
            // The sessions outlive the receiver's own `stop` (a transfer in flight finishes when
            // the advertising ends), but not the service: `ctk` ends them, and the stop of the
            // service waits for them, within `STOP_WIND_DOWN`.
            let sessions = TaskTracker::new();
            let start = {
                let (ctk, sessions) = (ctk.clone(), sessions.clone());
                move |stop| {
                    run_receiver(
                        endpoint_id,
                        identity,
                        bus.clone(),
                        stop,
                        ctk.clone(),
                        sessions.clone(),
                    )
                }
            };
            tokio::select! {
                () = ctk.cancelled() => {}
                () = supervise(start, visibility, link, RETRY_AFTER) => {}
            }
            crate::manager::wind_down(&sessions).await;
        });
    }

    async fn run_receiver(
        endpoint_id: [u8; 4],
        identity: [u8; 16],
        bus: broadcast::Sender<ChannelMessage>,
        stop: CancellationToken,
        service_stop: CancellationToken,
        sessions: TaskTracker,
    ) -> anyhow::Result<()> {
        let session = bluer::Session::new().await.context("connecting to BlueZ")?;
        let adapter = Arc::new(
            session
                .default_adapter()
                .await
                .context("no Bluetooth adapter")?,
        );
        if !adapter.is_powered().await? {
            // Left on afterwards: other users of the adapter may have come to rely on it.
            adapter.set_powered(true).await?;
        }

        let name = crate::DEVICE_NAME
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let data =
            ReceiverAdvertisement::new(endpoint_id, DEVICE_TYPE_LAPTOP, identity, &name).to_bytes();
        let rearm = Arc::new(tokio::sync::Notify::new());
        let shared = super::gatt::Shared::new(
            data.clone(),
            bus,
            rearm.clone(),
            Some(adapter.clone()),
            service_stop,
            sessions,
        );
        // Both registrations are undone when this future is dropped.
        let _gatt = adapter
            .serve_gatt_application(super::gatt::application(shared.clone()))
            .await
            .context("registering the GATT service")?;
        tokio::select! {
            r = ReceiverAdvertiser::new(adapter, data, rearm).run() => {
                let Err(e) = r;
                return Err(e);
            }
            () = stop.cancelled() => {}
        }
        // Advertising is off. Let a transfer in flight finish; the session bounds itself
        // (see `SESSION_IDLE`), and a dropped future still ends it at once.
        shared.close();
        while shared.has_live_session() {
            tokio::time::sleep(DRAIN_POLL).await;
        }
        drop(session);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A fake receiver: counts starts, and stops (drops) when told.
    #[derive(Default)]
    struct Counters {
        starts: AtomicUsize,
        drops: AtomicUsize,
    }

    struct Guard(Arc<Counters>);

    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    type Start = std::pin::Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;

    /// Runs until stopped. After `stop` it takes `wind_down` to finish, like a session in flight.
    fn fake_draining(
        counters: &Arc<Counters>,
        fail: bool,
        wind_down: Duration,
    ) -> impl FnMut(CancellationToken) -> Start + use<> {
        let counters = counters.clone();
        move |stop| {
            let counters = counters.clone();
            Box::pin(async move {
                counters.starts.fetch_add(1, Ordering::SeqCst);
                let _guard = Guard(counters);
                if fail {
                    anyhow::bail!("no adapter");
                }
                stop.cancelled().await;
                if !wind_down.is_zero() {
                    tokio::time::sleep(wind_down).await;
                }
                Ok(())
            })
        }
    }

    fn fake(
        counters: &Arc<Counters>,
        fail: bool,
    ) -> impl FnMut(CancellationToken) -> Start + use<> {
        fake_draining(counters, fail, Duration::ZERO)
    }

    #[tokio::test]
    async fn runs_only_while_visible_and_a_link_is_set() {
        let counters = Arc::new(Counters::default());
        let (vis_tx, vis_rx) = watch::channel(Visibility::Invisible);
        let (link_tx, link_rx) = watch::channel(false);
        let task = tokio::spawn(supervise(
            fake(&counters, false),
            vis_rx,
            link_rx,
            Duration::from_secs(60),
        ));
        settle().await;
        assert_eq!(counters.starts.load(Ordering::SeqCst), 0);

        // Visible alone is not enough, and neither is the link alone.
        vis_tx.send_replace(Visibility::Visible);
        settle().await;
        assert_eq!(counters.starts.load(Ordering::SeqCst), 0);

        link_tx.send_replace(true);
        settle().await;
        assert_eq!(counters.starts.load(Ordering::SeqCst), 1);
        assert_eq!(counters.drops.load(Ordering::SeqCst), 0);

        // Either turning off stops it, and turning it back on starts it again.
        vis_tx.send_replace(Visibility::Invisible);
        settle().await;
        assert_eq!(counters.drops.load(Ordering::SeqCst), 1);
        vis_tx.send_replace(Visibility::Temporarily);
        settle().await;
        assert_eq!(counters.starts.load(Ordering::SeqCst), 2);
        link_tx.send_replace(false);
        settle().await;
        assert_eq!(counters.drops.load(Ordering::SeqCst), 2);

        // Dropping a sender ends the supervisor.
        drop(link_tx);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("supervisor kept running")
            .unwrap();
    }

    #[tokio::test]
    async fn a_failing_start_is_retried_and_never_ends_the_supervisor() {
        let counters = Arc::new(Counters::default());
        let (_vis_tx, vis_rx) = watch::channel(Visibility::Visible);
        let (_link_tx, link_rx) = watch::channel(true);
        let task = tokio::spawn(supervise(
            fake(&counters, true),
            vis_rx,
            link_rx,
            Duration::from_millis(50),
        ));
        settle().await;
        assert_eq!(counters.starts.load(Ordering::SeqCst), 1);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(counters.starts.load(Ordering::SeqCst) >= 2);
        assert!(!task.is_finished());
    }

    #[tokio::test]
    async fn a_session_in_flight_is_left_to_finish_before_a_restart() {
        let counters = Arc::new(Counters::default());
        let (vis_tx, vis_rx) = watch::channel(Visibility::Visible);
        let (_link_tx, link_rx) = watch::channel(true);
        let wind_down = Duration::from_millis(300);
        tokio::spawn(supervise(
            fake_draining(&counters, false, wind_down),
            vis_rx,
            link_rx,
            Duration::from_secs(60),
        ));
        settle().await;
        assert_eq!(counters.starts.load(Ordering::SeqCst), 1);

        vis_tx.send_replace(Visibility::Invisible);
        settle().await;
        // Told to stop, but not cut off.
        assert_eq!(counters.drops.load(Ordering::SeqCst), 0);

        // Visible again while it winds down: the restart waits for it, so GATT is never doubled.
        vis_tx.send_replace(Visibility::Visible);
        settle().await;
        assert_eq!(counters.starts.load(Ordering::SeqCst), 1);
        tokio::time::sleep(wind_down * 2).await;
        assert_eq!(counters.drops.load(Ordering::SeqCst), 1);
        assert_eq!(counters.starts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn an_unrelated_change_does_not_cut_the_backoff_short() {
        let counters = Arc::new(Counters::default());
        let (vis_tx, vis_rx) = watch::channel(Visibility::Visible);
        let (_link_tx, link_rx) = watch::channel(true);
        tokio::spawn(supervise(
            fake(&counters, true),
            vis_rx,
            link_rx,
            Duration::from_millis(300),
        ));
        settle().await;
        assert_eq!(counters.starts.load(Ordering::SeqCst), 1);

        vis_tx.send_replace(Visibility::Temporarily);
        settle().await;
        assert_eq!(counters.starts.load(Ordering::SeqCst), 1);

        // Going invisible and back is a new start, backoff or not.
        vis_tx.send_replace(Visibility::Invisible);
        settle().await;
        vis_tx.send_replace(Visibility::Visible);
        settle().await;
        assert_eq!(counters.starts.load(Ordering::SeqCst), 2);
    }
}
