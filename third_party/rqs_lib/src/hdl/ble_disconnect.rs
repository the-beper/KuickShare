//! Closing a BlueZ connection that nobody waits for any more.

use std::future::Future;
use std::time::Duration;

use bluer::Device;
use tokio::time::timeout;

/// BlueZ was seen finishing a disconnect 0.3-0.6 s after 2 s.
pub(crate) const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Best effort: a link that cannot be closed is BlueZ's to clean up.
pub(crate) async fn disconnect(device: &Device) {
    match timeout(DISCONNECT_TIMEOUT, device.disconnect()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => debug!("BLE disconnect failed: {e}"),
        Err(_) => debug!(
            "no answer to the BLE disconnect within {DISCONNECT_TIMEOUT:?}; BlueZ closes the link later"
        ),
    }
}

/// What [`DisconnectOnDrop`] closes. A trait so that a test can count the calls: a `Device` needs BlueZ.
pub(crate) trait Link: Send + 'static {
    fn close(self) -> impl Future<Output = ()> + Send;
}

impl Link for Device {
    async fn close(self) {
        disconnect(&self).await;
    }
}

/// Closes the link, or cancels the connect BlueZ still has pending, when dropped while armed
/// (the `Option` is `Some`). A future that is dropped mid-connect, by a deadline or a cancelled
/// send, would otherwise leave the connect pending and the next one failing with "operation in
/// progress".
pub(crate) struct DisconnectOnDrop<L: Link>(pub(crate) Option<L>);

impl<L: Link> Drop for DisconnectOnDrop<L> {
    fn drop(&mut self) {
        if let (Some(link), Ok(rt)) = (self.0.take(), tokio::runtime::Handle::try_current()) {
            rt.spawn(link.close());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct Counted(Arc<AtomicUsize>);

    impl Link for Counted {
        async fn close(self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The disconnect runs in a spawned task, which gets to run at the next yield.
    async fn settle() {
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn an_armed_guard_closes_the_link_when_dropped() {
        let closed = Arc::new(AtomicUsize::new(0));
        drop(DisconnectOnDrop(Some(Counted(closed.clone()))));
        settle().await;
        assert_eq!(closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_disarmed_guard_leaves_the_link_alone() {
        let closed = Arc::new(AtomicUsize::new(0));
        let mut guard = DisconnectOnDrop(Some(Counted(closed.clone())));
        guard.0 = None;
        drop(guard);
        settle().await;
        assert_eq!(closed.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_future_dropped_midway_closes_the_link() {
        let closed = Arc::new(AtomicUsize::new(0));
        let read = {
            let closed = closed.clone();
            async move {
                let _guard = DisconnectOnDrop(Some(Counted(closed)));
                std::future::pending::<()>().await;
            }
        };
        // Polls `read` once, then drops it: the zero deadline wins like a window deadline does.
        assert!(timeout(Duration::ZERO, read).await.is_err());
        settle().await;
        assert_eq!(closed.load(Ordering::SeqCst), 1);
    }
}
