//! BlueZ GATT service 0xFEF3 for the receiver. Thin on purpose: all protocol logic lives in
//! [`bridge`](super::bridge); this file only moves packets between BlueZ and a bridge.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bluer::UuidExt;
use bluer::gatt::local::{
    Application, Characteristic, CharacteristicNotifier, CharacteristicNotify,
    CharacteristicNotifyMethod, CharacteristicRead, CharacteristicWrite, CharacteristicWriteMethod,
    ReqError, Service,
};
use bluer::{Adapter, Address, Uuid};
use tokio::sync::{Notify, broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::bridge::{BleBridge, GattWrite};
use super::service_data::SERVICE_UUID;
use super::weave::parse_conn_request;
use crate::channel::ChannelMessage;
use crate::hdl::{InboundRequest, MigratableStream};
use crate::manager::drive_inbound;

/// Slot 0: the advertisement, readable once connected.
const SLOT0_UUID: Uuid = Uuid::from_u128(0x0000_0000_0000_3000_8000_0000_0000_0000);
/// Phone to us, write with response.
pub(crate) const TO_PERIPHERAL_UUID: Uuid =
    Uuid::from_u128(0x0000_0100_0004_1000_8000_001a_1100_0101);
/// Us to phone, notify and indicate.
pub(crate) const FROM_PERIPHERAL_UUID: Uuid =
    Uuid::from_u128(0x0000_0100_0004_1000_8000_001a_1100_0102);

/// Writes queued for the bridge; a full queue delays the write response, which is the
/// backpressure on the phone.
const WRITES_QUEUE: usize = 16;
const NOTIFY_QUEUE: usize = 64;
/// Buffer of the in-memory stream between the bridge and the `InboundRequest`.
const STREAM_BUFFER: usize = 64 * 1024;
/// A session that stayed quiet this long (no write from its phone) no longer holds the slot:
/// a new subscription may replace it, and a closing receiver stops waiting for it. Generous,
/// because the phone sends nothing while the user decides whether to accept.
pub const SESSION_IDLE: Duration = Duration::from_secs(120);
/// A new session whose phone has not yet sent its connection request may be replaced once
/// it is this old, so a burst of subscriptions cannot cut a handshake still being written.
const HANDSHAKE_GRACE: Duration = Duration::from_secs(3);
/// Time given to the last notifications to leave before the link is dropped.
const DISCONNECT_GRACE: Duration = Duration::from_millis(500);

pub struct Shared {
    slot0: Vec<u8>,
    bus: broadcast::Sender<ChannelMessage>,
    /// The one live session.
    active: Mutex<Option<Active>>,
    sessions: AtomicU32,
    /// Set when the receiver is winding down: no new session is accepted.
    closing: AtomicBool,
    /// Notified when a connection may have used up the advertising set.
    rearm: Arc<Notify>,
    /// Used to drop the link of a finished session; `None` in tests.
    adapter: Option<Arc<Adapter>>,
    /// Cancelled when the service stops (unlike the receiver's own `stop`, which only ends the
    /// advertising and lets a transfer in flight finish): the session then ends at once.
    service_stop: CancellationToken,
    /// The tasks of the sessions, so that the stop of the service can wait for them.
    tasks: TaskTracker,
}

struct Active {
    number: u32,
    writes: mpsc::Sender<GattWrite>,
    /// The device that sent the connection request. BlueZ does not tell which device a
    /// notification subscription belongs to, so the session is bound to the first device
    /// whose write is a valid weave connection request, and writes from any other are refused.
    owner: Option<Address>,
    started: Instant,
    last_write: Instant,
}

impl Active {
    fn expired(&self, now: Instant) -> bool {
        now.duration_since(self.last_write) >= SESSION_IDLE
    }

    fn replaceable(&self, now: Instant) -> bool {
        self.expired(now)
            || (self.owner.is_none() && now.duration_since(self.started) >= HANDSHAKE_GRACE)
    }
}

impl Shared {
    pub fn new(
        slot0: Vec<u8>,
        bus: broadcast::Sender<ChannelMessage>,
        rearm: Arc<Notify>,
        adapter: Option<Arc<Adapter>>,
        service_stop: CancellationToken,
        tasks: TaskTracker,
    ) -> Arc<Self> {
        Arc::new(Self {
            slot0,
            bus,
            active: Mutex::new(None),
            sessions: AtomicU32::new(0),
            closing: AtomicBool::new(false),
            rearm,
            adapter,
            service_stop,
            tasks,
        })
    }

    fn active(&self) -> MutexGuard<'_, Option<Active>> {
        self.active.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Stops accepting sessions; the one in flight is left alone.
    pub fn close(&self) {
        self.closing.store(true, Ordering::Relaxed);
    }

    /// True while a session exists that is not idle.
    pub fn has_live_session(&self) -> bool {
        self.active()
            .as_ref()
            .is_some_and(|a| !a.expired(Instant::now()))
    }

    /// Makes `writes` the live session unless a better one is running.
    fn admit(&self, number: u32, writes: mpsc::Sender<GattWrite>, now: Instant) -> bool {
        let mut active = self.active();
        if self.closing.load(Ordering::Relaxed)
            || active.as_ref().is_some_and(|a| !a.replaceable(now))
        {
            return false;
        }
        *active = Some(Active {
            number,
            writes,
            owner: None,
            started: now,
            last_write: now,
        });
        true
    }

    /// Ends session `number` if it is still the live one and returns the device that owned
    /// it, whose link is then to be dropped. A newer session of the same phone is not touched.
    fn finish(&self, number: u32) -> Option<Address> {
        let mut active = self.active();
        if active.as_ref().is_some_and(|a| a.number == number) {
            return active.take().and_then(|a| a.owner);
        }
        None
    }

    async fn disconnect(&self, device: Address) {
        let Some(adapter) = &self.adapter else { return };
        tokio::time::sleep(DISCONNECT_GRACE).await;
        // A session that started meanwhile may be the same phone connecting again: leave the
        // link alone, it is dropped when that session ends.
        if self.active().is_some() {
            return;
        }
        match adapter.device(device) {
            Ok(d) => {
                if let Err(e) = d.disconnect().await {
                    debug!("ble receiver: disconnecting {device}: {e}");
                }
            }
            Err(e) => debug!("ble receiver: disconnecting {device}: {e}"),
        }
    }

    /// The write queue for a write from `device`, claiming the session for it if it is the
    /// first valid connection request.
    fn writer_for(
        &self,
        device: Address,
        data: &[u8],
        now: Instant,
    ) -> Result<mpsc::Sender<GattWrite>, ReqError> {
        let mut active = self.active();
        // Writes without a subscription have nowhere to go.
        let a = active.as_mut().ok_or(ReqError::NotPermitted)?;
        match a.owner {
            Some(owner) if owner != device => return Err(ReqError::NotAuthorized),
            Some(_) => {}
            None => {
                if parse_conn_request(data).is_err() {
                    return Err(ReqError::NotPermitted);
                }
                a.owner = Some(device);
            }
        }
        a.last_write = now;
        Ok(a.writes.clone())
    }
}

/// What a read of slot 0 returns at `offset`.
fn slot0_read(slot0: &[u8], offset: u16) -> Result<Vec<u8>, ReqError> {
    slot0
        .get(usize::from(offset)..)
        .map(<[u8]>::to_vec)
        .ok_or(ReqError::InvalidOffset)
}

/// The GATT application to register with BlueZ.
pub fn application(shared: Arc<Shared>) -> Application {
    let on_read = shared.clone();
    let on_write = shared.clone();
    let on_notify = shared;
    Application {
        services: vec![Service {
            uuid: Uuid::from_u16(SERVICE_UUID),
            primary: true,
            characteristics: vec![
                Characteristic {
                    uuid: SLOT0_UUID,
                    read: Some(CharacteristicRead {
                        read: true,
                        fun: Box::new(move |req| {
                            let shared = on_read.clone();
                            Box::pin(async move {
                                // Someone connected, so the set is gone.
                                shared.rearm.notify_one();
                                slot0_read(&shared.slot0, req.offset)
                            })
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                Characteristic {
                    uuid: TO_PERIPHERAL_UUID,
                    write: Some(CharacteristicWrite {
                        // Write with response only: the phone sends one packet at a time, so
                        // BlueZ hands them over in order.
                        write: true,
                        write_without_response: false,
                        method: CharacteristicWriteMethod::Fun(Box::new(move |data, req| {
                            let shared = on_write.clone();
                            Box::pin(async move {
                                let tx =
                                    shared.writer_for(req.device_address, &data, Instant::now())?;
                                tx.send(GattWrite { data, mtu: req.mtu })
                                    .await
                                    .map_err(|_| ReqError::Failed)
                            })
                        })),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                Characteristic {
                    uuid: FROM_PERIPHERAL_UUID,
                    notify: Some(CharacteristicNotify {
                        notify: true,
                        indicate: true,
                        method: CharacteristicNotifyMethod::Fun(Box::new(move |notifier| {
                            Box::pin(serve_session(on_notify.clone(), notifier))
                        })),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// SECURITY: this id is no IP address and has no ':'. The daemon's auto-accept for trusted
/// devices and paired computers is keyed on the IP part of the id (`ip:port`), so it never
/// applies to a BLE session: every one goes through the normal user consent. BLE sessions
/// also bypass the per-IP rate limits, which is why only one can be live (see `serve_session`).
fn session_id(number: u32) -> String {
    format!("ble-{number}")
}

/// One phone subscription is one session, and at most one is live: a subscription while
/// another session is running is refused (see [`Active::replaceable`]), so a nearby device
/// cannot cut a transfer by subscribing. A replaced session's write queue is dropped, which
/// stops its bridge and ends the `InboundRequest` behind it.
async fn serve_session(shared: Arc<Shared>, mut notifier: CharacteristicNotifier) {
    let number = shared.sessions.fetch_add(1, Ordering::Relaxed) + 1;
    let id = session_id(number);
    let (writes_tx, writes_rx) = mpsc::channel(WRITES_QUEUE);
    let (notify_tx, mut notify_rx) = mpsc::channel(NOTIFY_QUEUE);
    let (session_end, bridge_end) = tokio::io::duplex(STREAM_BUFFER);
    if !shared.admit(number, writes_tx, Instant::now()) {
        info!("ble receiver: subscription refused, a session is running");
        return;
    }

    info!("ble receiver: session {id} started");
    shared.rearm.notify_one();
    let log_id = id.clone();
    shared.tasks.spawn(async move {
        let exit = BleBridge::new(writes_rx, notify_tx, bridge_end).run().await;
        info!("ble receiver: session {log_id} ended: {exit:?}");
    });
    let request = InboundRequest::new_over(
        MigratableStream::Duplex(session_end),
        id,
        shared.bus.clone(),
    )
    .with_stop(shared.service_stop.clone());
    shared
        .tasks
        .spawn(drive_inbound(request, shared.bus.clone()));

    // The bridge dropped its sender: the session is over and everything it produced was sent.
    let mut drained = false;
    loop {
        tokio::select! {
            packet = notify_rx.recv() => {
                let Some(packet) = packet else {
                    drained = true;
                    break;
                };
                if let Err(e) = notifier.notify(packet).await {
                    debug!("ble receiver: notify failed: {e}");
                    break;
                }
            }
            () = notifier.stopped() => break,
        }
    }

    let owner = shared.finish(number);
    shared.rearm.notify_one();
    if let (true, Some(owner)) = (drained, owner) {
        shared.disconnect(owner).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared_with(slot0: Vec<u8>, bus: broadcast::Sender<ChannelMessage>) -> Arc<Shared> {
        Shared::new(
            slot0,
            bus,
            Arc::default(),
            None,
            CancellationToken::new(),
            TaskTracker::new(),
        )
    }

    #[test]
    fn session_ids_never_look_like_an_address() {
        assert_eq!(session_id(7), "ble-7");
        assert!(!session_id(7).contains(':'));
    }

    #[test]
    fn application_exposes_the_weave_service() {
        let (bus, _) = broadcast::channel(1);
        let app = application(shared_with(vec![1, 2, 3], bus));
        let [service] = app.services.as_slice() else {
            panic!("one service expected");
        };
        assert!(service.primary);
        assert_eq!(service.uuid, Uuid::from_u16(0xFEF3));

        let uuids: Vec<_> = service.characteristics.iter().map(|c| c.uuid).collect();
        assert_eq!(
            uuids,
            [SLOT0_UUID, TO_PERIPHERAL_UUID, FROM_PERIPHERAL_UUID]
        );
        assert_eq!(
            SLOT0_UUID.to_string(),
            "00000000-0000-3000-8000-000000000000"
        );
        assert_eq!(
            TO_PERIPHERAL_UUID.to_string(),
            "00000100-0004-1000-8000-001a11000101"
        );
        assert_eq!(
            FROM_PERIPHERAL_UUID.to_string(),
            "00000100-0004-1000-8000-001a11000102"
        );

        let [slot0, to, from] = service.characteristics.as_slice() else {
            panic!("three characteristics expected");
        };
        assert!(slot0.read.as_ref().is_some_and(|r| r.read));
        let write = to.write.as_ref().unwrap();
        assert!(write.write && !write.write_without_response);
        let notify = from.notify.as_ref().unwrap();
        assert!(notify.notify && notify.indicate);
    }

    const PHONE: Address = Address::new([1, 1, 1, 1, 1, 1]);
    const OTHER: Address = Address::new([2, 2, 2, 2, 2, 2]);
    const CONN: [u8; 7] = [0x80, 0, 1, 0, 1, 0x01, 0xfd];

    fn shared() -> Arc<Shared> {
        let (bus, _) = broadcast::channel(1);
        shared_with(vec![1, 2, 3, 4], bus)
    }

    fn admit(shared: &Shared, number: u32, now: Instant) -> bool {
        shared.admit(number, mpsc::channel(1).0, now)
    }

    #[test]
    fn slot0_reads_honour_the_offset() {
        let slot0 = [1, 2, 3, 4];
        assert_eq!(slot0_read(&slot0, 0).unwrap(), [1, 2, 3, 4]);
        assert_eq!(slot0_read(&slot0, 2).unwrap(), [3, 4]);
        assert_eq!(slot0_read(&slot0, 4).unwrap(), Vec::<u8>::new());
        assert_eq!(slot0_read(&slot0, 5), Err(ReqError::InvalidOffset));
    }

    #[test]
    fn writes_without_a_session_are_refused() {
        let shared = shared();
        let r = shared.writer_for(PHONE, &CONN, Instant::now());
        assert_eq!(r.err(), Some(ReqError::NotPermitted));
    }

    #[tokio::test]
    async fn only_the_device_that_connected_may_write() {
        let shared = shared();
        let t0 = Instant::now();
        let (tx, mut rx) = mpsc::channel(4);
        assert!(shared.admit(1, tx, t0));

        // Junk cannot claim the session.
        let r = shared.writer_for(OTHER, &[0x38, 1, 2], t0);
        assert_eq!(r.err(), Some(ReqError::NotPermitted));

        let writer = shared.writer_for(PHONE, &CONN, t0).unwrap();
        let write = GattWrite {
            data: vec![9],
            mtu: 23,
        };
        writer.send(write).await.unwrap();
        assert_eq!(rx.recv().await.unwrap().data, [9]);

        for data in [&CONN[..], &[0x38, 1, 2]] {
            let r = shared.writer_for(OTHER, data, t0);
            assert_eq!(r.err(), Some(ReqError::NotAuthorized));
        }
        assert!(shared.writer_for(PHONE, &[0x38, 1, 2], t0).is_ok());
    }

    #[test]
    fn a_running_session_is_kept_and_a_stale_one_replaced() {
        let shared = shared();
        let t0 = Instant::now();
        assert!(admit(&shared, 1, t0));

        // Before the handshake it survives only the grace period.
        assert!(!admit(&shared, 2, t0 + Duration::from_secs(1)));
        assert!(admit(&shared, 3, t0 + HANDSHAKE_GRACE));

        // Once the phone has connected it is kept until it has been quiet for SESSION_IDLE.
        shared.writer_for(PHONE, &CONN, t0).unwrap();
        assert!(!admit(&shared, 4, t0 + HANDSHAKE_GRACE));
        let t1 = t0 + Duration::from_secs(100);
        shared.writer_for(PHONE, &[0x38], t1).unwrap();
        assert!(!admit(
            &shared,
            4,
            t1 + SESSION_IDLE - Duration::from_secs(1)
        ));
        assert_eq!(shared.active().as_ref().map(|a| a.number), Some(3));
        assert!(admit(&shared, 5, t1 + SESSION_IDLE));
        assert_eq!(shared.active().as_ref().map(|a| a.number), Some(5));
    }

    #[test]
    fn finishing_returns_the_owner_unless_a_newer_session_took_over() {
        let shared = shared();
        let t0 = Instant::now();
        assert!(admit(&shared, 1, t0));
        // No owner yet: nobody to disconnect, but the slot is freed.
        assert_eq!(shared.finish(1), None);
        assert!(!shared.has_live_session());

        assert!(admit(&shared, 2, t0));
        shared.writer_for(PHONE, &CONN, t0).unwrap();
        // The old session ending later must not free or disconnect the new one.
        assert_eq!(shared.finish(1), None);
        assert!(shared.has_live_session());
        assert_eq!(shared.finish(2), Some(PHONE));
        assert!(!shared.has_live_session());
    }

    #[test]
    fn a_closing_receiver_takes_no_session() {
        let shared = shared();
        let t0 = Instant::now();
        assert!(admit(&shared, 1, t0));
        assert!(shared.has_live_session());
        shared.close();
        assert!(!admit(&shared, 2, t0 + SESSION_IDLE));
        // The one in flight is untouched.
        assert!(shared.has_live_session());
    }
}
