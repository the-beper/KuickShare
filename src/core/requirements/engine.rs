//! Runs the probes, judges the facts, and caches the report.
//!
//! At most one run is in flight. A run is spawned, so a caller that goes away does not cancel it;
//! every other caller, `force` included, joins it. The engine state is a plain mutex that is never
//! held across an await; nothing here touches the `Core` mutex, so a slow D-Bus service cannot
//! hold up the rest of the daemon.
//!
//! Time: 2 s per D-Bus call and 6 s for the whole run (a group of probes that is not done by then
//! gives Unknown rows with the reason `timeout`), a 30 s TTL, and an event-driven refresh: every
//! `invalidate` drops the cache, and after a trailing debounce one run starts, at most one every
//! `min_event_gap`. There is no polling, and nothing refreshes until somebody asked for a report
//! once.

use std::{
    future::Future,
    sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError, Weak},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::{
    sync::{Semaphore, watch},
    time::{Instant, sleep, timeout_at},
};

use super::{
    SCHEMA_VERSION,
    facts::{
        BluezFacts, DhcpFacts, Facts, FirewallFacts, NmFacts, ProbeError, RuntimeEvidence,
        SandboxFacts, SessionEvidence,
    },
    model::{MAX_ROWS, Packaging, RequirementsReport},
    rules,
};

/// What a probe may spend.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    /// The most one D-Bus call may take.
    pub per_call: Duration,
    /// When the whole run ends; the engine stops waiting then.
    pub deadline: Instant,
}

/// Where the facts come from. Probes never fail: what could not be observed is in the facts
/// (`Obs::Failed`). They must be read-only: no hotspot, no change of Wi-Fi or Bluetooth state,
/// no scan, no firewall change, no profile saved, no credential prompt, no package install.
pub trait RequirementsProbe: Send + Sync + 'static {
    fn packaging(&self) -> Packaging;
    fn sandbox(&self) -> impl Future<Output = SandboxFacts> + Send;
    fn bluez(&self, b: Budget) -> impl Future<Output = BluezFacts> + Send;
    fn network_manager(&self, b: Budget) -> impl Future<Output = NmFacts> + Send;
    fn firewall(&self, b: Budget) -> impl Future<Output = FirewallFacts> + Send;
    fn dhcp(&self, p: Packaging) -> impl Future<Output = DhcpFacts> + Send;
}

#[derive(Clone, Copy, Debug)]
pub struct EngineConfig {
    pub per_call: Duration,
    pub total: Duration,
    pub ttl: Duration,
    /// Quiet time after the last event before a refresh starts.
    pub debounce: Duration,
    /// Least time between the starts of two refreshes caused by events.
    pub min_event_gap: Duration,
    /// A forced call within this time of the last run returns that run's report.
    pub min_forced_gap: Duration,
    pub max_parallel_groups: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            per_call: Duration::from_secs(2),
            total: Duration::from_secs(6),
            ttl: Duration::from_secs(30),
            debounce: Duration::from_millis(1500),
            min_event_gap: Duration::from_secs(5),
            min_forced_gap: Duration::from_secs(2),
            max_parallel_groups: 3,
        }
    }
}

/// Why the cache was dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalidation {
    /// A service we look at appeared on, or vanished from, the bus.
    ServiceOwner,
    /// An adapter, its power, or its roles changed.
    Bluetooth,
    /// A device, a switch, or a device state of NetworkManager changed.
    NetworkManager,
    /// The runtime learned something by acting (`RuntimeEvidence`).
    Evidence,
}

type Published = Option<Arc<RequirementsReport>>;
/// Receives the report of one run.
type Slot = watch::Receiver<Published>;

#[derive(Default)]
struct State {
    report: Option<Arc<RequirementsReport>>,
    /// Until when `report` is valid; `None` once an event dropped it.
    fresh_until: Option<Instant>,
    last_started: Option<Instant>,
    last_finished: Option<Instant>,
    inflight: Option<Slot>,
    evidence: SessionEvidence,
    /// Counts the events; a run that ends after one is not trusted for the TTL.
    generation: u64,
    /// A task waits to refresh after the events.
    scheduled: bool,
}

pub struct RequirementsEngine<P: RequirementsProbe> {
    probe: Arc<P>,
    cfg: EngineConfig,
    state: StdMutex<State>,
    published: watch::Sender<Published>,
    me: Weak<Self>,
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Runs `work` for one group of probes: at most `gate` groups at a time, and not past `deadline`.
async fn group<T>(
    gate: &Semaphore,
    deadline: Instant,
    work: impl Future<Output = T>,
    on_timeout: impl FnOnce() -> T,
) -> T {
    let run = async {
        let _permit = gate.acquire().await.ok();
        work.await
    };
    timeout_at(deadline, run)
        .await
        .unwrap_or_else(|_| on_timeout())
}

impl<P: RequirementsProbe> RequirementsEngine<P> {
    pub fn new(probe: P, cfg: EngineConfig) -> Arc<Self> {
        let (published, _) = watch::channel(None);
        Arc::new_cyclic(|me| Self {
            probe: Arc::new(probe),
            cfg,
            state: StdMutex::new(State::default()),
            published,
            me: me.clone(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The report: the cache when it is younger than the TTL (and `force` is not set, or the last
    /// run is very recent), otherwise a run (a run already going is joined).
    pub async fn report(self: &Arc<Self>, force: bool) -> Arc<RequirementsReport> {
        let slot = {
            let mut st = self.lock();
            if let Some(slot) = &st.inflight {
                slot.clone()
            } else {
                match st.report.clone() {
                    Some(report) if !self.must_run(&st, Instant::now(), force) => return report,
                    _ => self.start(&mut st),
                }
            }
        };
        self.wait(slot).await
    }

    /// A report from a run that started after this call: a run already going that began earlier
    /// is waited for, then one more is made. For decisions that must not rest on older facts
    /// than the request (no cache, no minimum gap).
    pub async fn report_from_new_run(self: &Arc<Self>) -> Arc<RequirementsReport> {
        let requested = Instant::now();
        loop {
            let (slot, ours) = {
                let mut st = self.lock();
                match st.inflight.clone() {
                    Some(slot) => (slot, st.last_started.is_some_and(|t| t >= requested)),
                    None => (self.start(&mut st), true),
                }
            };
            let report = self.wait(slot).await;
            if ours {
                return report;
            }
        }
    }

    fn must_run(&self, st: &State, now: Instant, force: bool) -> bool {
        let fresh = st.fresh_until.is_some_and(|until| now < until);
        if !force {
            return !fresh;
        }
        let just_ran = st
            .last_finished
            .is_some_and(|t| now.saturating_duration_since(t) < self.cfg.min_forced_gap);
        !(fresh && just_ran)
    }

    /// Spawns a run; its report reaches `report` callers through the returned slot.
    fn start(self: &Arc<Self>, st: &mut State) -> Slot {
        let (tx, rx) = watch::channel(None);
        st.inflight = Some(rx.clone());
        st.last_started = Some(Instant::now());
        let (evidence, generation) = (st.evidence, st.generation);
        let engine = Arc::clone(self);
        tokio::spawn(async move {
            let worker = Arc::clone(&engine);
            // A run of its own, so a panic in a probe cannot leave the engine waiting for ever.
            let report = match tokio::spawn(async move { worker.collect(evidence).await }).await {
                Ok(report) => report,
                Err(e) => {
                    tracing::warn!("a requirements run failed: {e}");
                    let facts = Facts::failed(engine.probe.packaging(), ProbeError::Other);
                    engine.build(&facts, &evidence)
                }
            };
            let report = Arc::new(report);
            engine.finish(&report, generation);
            // Nobody waits when every caller went away.
            let _ = tx.send(Some(report));
        });
        rx
    }

    async fn wait(&self, mut slot: Slot) -> Arc<RequirementsReport> {
        let done = slot
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|report| report.clone());
        match done {
            Some(report) => report,
            None => {
                let (cached, evidence) = {
                    let st = self.lock();
                    (st.report.clone(), st.evidence)
                };
                cached.unwrap_or_else(|| {
                    let facts = Facts::failed(self.probe.packaging(), ProbeError::Other);
                    Arc::new(self.build(&facts, &evidence))
                })
            }
        }
    }

    fn finish(&self, report: &Arc<RequirementsReport>, generation: u64) {
        let trusted = {
            let mut st = self.lock();
            let now = Instant::now();
            st.report = Some(Arc::clone(report));
            st.last_finished = Some(now);
            st.inflight = None;
            let trusted = generation == st.generation;
            st.fresh_until = trusted.then(|| now + self.cfg.ttl);
            trusted
        };
        self.published.send_replace(Some(Arc::clone(report)));
        if !trusted {
            // An event came while the run was going: its facts may be old.
            self.schedule();
        }
    }

    async fn collect(&self, evidence: SessionEvidence) -> RequirementsReport {
        let packaging = self.probe.packaging();
        let budget = Budget {
            per_call: self.cfg.per_call,
            deadline: Instant::now() + self.cfg.total,
        };
        let gate = Semaphore::new(self.cfg.max_parallel_groups.max(1));
        let timed_out = ProbeError::Timeout;
        let (sandbox, bluez, nm, firewall, dhcp) = tokio::join!(
            group(&gate, budget.deadline, self.probe.sandbox(), || {
                SandboxFacts {
                    context: super::facts::Obs::Failed(timed_out),
                }
            }),
            group(&gate, budget.deadline, self.probe.bluez(budget), || {
                BluezFacts::failed(timed_out)
            }),
            group(
                &gate,
                budget.deadline,
                self.probe.network_manager(budget),
                || { NmFacts::failed(timed_out) }
            ),
            group(&gate, budget.deadline, self.probe.firewall(budget), || {
                FirewallFacts::failed(timed_out)
            }),
            group(&gate, budget.deadline, self.probe.dhcp(packaging), || {
                DhcpFacts::failed(timed_out)
            }),
        );
        let facts = Facts {
            packaging,
            sandbox,
            bluez,
            nm,
            firewall,
            dhcp,
        };
        self.build(&facts, &evidence)
    }

    fn build(&self, facts: &Facts, evidence: &SessionEvidence) -> RequirementsReport {
        let mut requirements = rules::evaluate(facts, evidence);
        requirements.truncate(MAX_ROWS);
        let capabilities = rules::derive(&requirements);
        RequirementsReport {
            schema_version: SCHEMA_VERSION,
            generated_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, millis),
            ttl_ms: millis(self.cfg.ttl),
            packaging: facts.packaging,
            core_version: env!("CARGO_PKG_VERSION").to_owned(),
            capabilities,
            requirements,
        }
    }

    /// Drops the cache. A refresh follows after the debounce, if a report was ever asked for.
    pub fn invalidate(&self, why: Invalidation) {
        tracing::debug!(?why, "requirements cache dropped");
        {
            let mut st = self.lock();
            st.generation += 1;
            st.fresh_until = None;
        }
        self.schedule();
    }

    /// What the runtime learned by acting. It changes only the rows it concerns.
    pub fn record(&self, e: RuntimeEvidence) {
        self.lock().evidence.apply(e);
        self.invalidate(Invalidation::Evidence);
    }

    /// Every new report, after each run.
    pub fn subscribe(&self) -> watch::Receiver<Option<Arc<RequirementsReport>>> {
        self.published.subscribe()
    }

    /// Starts the task that refreshes after the events, unless one waits already.
    fn schedule(&self) {
        let Some(engine) = self.me.upgrade() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        {
            let mut st = self.lock();
            // Nobody has asked for a report yet: nobody needs a refresh.
            if st.report.is_none() || st.scheduled {
                return;
            }
            st.scheduled = true;
        }
        runtime.spawn(engine.refresh_later());
    }

    /// The trailing debounce, then one run; again if an event came meanwhile.
    async fn refresh_later(self: Arc<Self>) {
        loop {
            let seen = self.lock().generation;
            sleep(self.cfg.debounce).await;
            if self.lock().generation != seen {
                continue;
            }
            let gap = self.lock().last_started.map(|started| {
                (started + self.cfg.min_event_gap).saturating_duration_since(Instant::now())
            });
            if let Some(gap) = gap
                && !gap.is_zero()
            {
                sleep(gap).await;
            }
            let _ = self.report(false).await;
            let mut st = self.lock();
            if st.fresh_until.is_some() {
                st.scheduled = false;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A probe that counts its runs and can be slow.
    struct Counting {
        runs: Arc<AtomicUsize>,
        bluez_delay: Duration,
    }

    impl RequirementsProbe for Counting {
        fn packaging(&self) -> Packaging {
            Packaging::Native
        }
        async fn sandbox(&self) -> SandboxFacts {
            SandboxFacts::default()
        }
        async fn bluez(&self, _b: Budget) -> BluezFacts {
            self.runs.fetch_add(1, Ordering::SeqCst);
            sleep(self.bluez_delay).await;
            BluezFacts::default()
        }
        async fn network_manager(&self, _b: Budget) -> NmFacts {
            NmFacts::default()
        }
        async fn firewall(&self, _b: Budget) -> FirewallFacts {
            FirewallFacts::default()
        }
        async fn dhcp(&self, _p: Packaging) -> DhcpFacts {
            DhcpFacts::default()
        }
    }

    fn cfg() -> EngineConfig {
        EngineConfig {
            per_call: Duration::from_millis(200),
            total: Duration::from_millis(300),
            ttl: Duration::from_millis(400),
            debounce: Duration::from_millis(10),
            min_event_gap: Duration::ZERO,
            min_forced_gap: Duration::ZERO,
            max_parallel_groups: 3,
        }
    }

    fn engine(delay: Duration) -> (Arc<RequirementsEngine<Counting>>, Arc<AtomicUsize>) {
        let runs = Arc::new(AtomicUsize::new(0));
        let probe = Counting {
            runs: runs.clone(),
            bluez_delay: delay,
        };
        (RequirementsEngine::new(probe, cfg()), runs)
    }

    #[tokio::test]
    async fn callers_share_one_run_and_the_cache_has_a_ttl() {
        let (engine, runs) = engine(Duration::from_millis(50));
        let (a, b, c) = tokio::join!(
            engine.report(false),
            engine.report(true),
            engine.report(false)
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(Arc::ptr_eq(&a, &b) && Arc::ptr_eq(&b, &c));
        engine.report(false).await;
        assert_eq!(runs.load(Ordering::SeqCst), 1, "served from the cache");
        sleep(Duration::from_millis(450)).await;
        engine.report(false).await;
        assert_eq!(runs.load(Ordering::SeqCst), 2, "the TTL ran out");
    }

    #[tokio::test]
    async fn a_slow_group_gives_timeout_rows() {
        let (engine, _) = engine(Duration::from_secs(30));
        let started = std::time::Instant::now();
        let report = engine.report(false).await;
        assert!(started.elapsed() < Duration::from_secs(3));
        let row = report
            .requirements
            .iter()
            .find(|r| r.id == "bluez.service")
            .map(|r| r.reason_code.clone());
        assert_eq!(row.as_deref(), Some("timeout"));
    }

    #[tokio::test]
    async fn an_event_drops_the_cache_and_refreshes_once() {
        let (engine, runs) = engine(Duration::ZERO);
        engine.report(false).await;
        engine.invalidate(Invalidation::Bluetooth);
        engine.invalidate(Invalidation::Bluetooth);
        engine.invalidate(Invalidation::NetworkManager);
        sleep(Duration::from_millis(150)).await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            2,
            "one refresh for the three events"
        );
        engine.report(false).await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            2,
            "the refresh filled the cache"
        );
    }

    #[tokio::test]
    async fn nothing_refreshes_before_a_first_request() {
        let (engine, runs) = engine(Duration::ZERO);
        engine.invalidate(Invalidation::Bluetooth);
        sleep(Duration::from_millis(100)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }
}
