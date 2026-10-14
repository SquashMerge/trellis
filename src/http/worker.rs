//! The checkout worker's runtime state and startup reconciliation (Decision 0009).
//!
//! One worker owns every write for a checkout. Startup reconciliation (formerly
//! "H") is its first work item: until it commits a validated head, FIFO claims
//! wait and readers see the last validated head with `catchingUp`.

use super::DaemonState;
use crate::indexer::IndexOptions;
use crate::store::topology::WriterSession;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub(super) const STARTUP_BACKOFF_FIRST: Duration = Duration::from_millis(250);
const STARTUP_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Decision 0009 §2. The worker and readers share this one value; it replaces
/// the separate serving-session slot and the registry's phase flags.
pub(crate) enum RuntimeState {
    /// Not yet asked to reconcile. A daemon runtime begins at once; a
    /// standalone fixture scheduler reconciles when work arrives.
    Activating,
    /// Startup reconciliation is due or running (`waiting: false`), or failed
    /// and is waiting out its backoff (`waiting: true`). `attempts` counts
    /// attempts that began capturing the workspace.
    Reconciling { attempts: u32, waiting: bool },
    /// A validated head committed; this session is the only writer.
    Ready(Arc<WriterSession>),
    /// The captured root is missing or replaced, and its accepted requests are
    /// confirmed terminal. Old-root evidence is never served.
    RootLost,
    /// Release is due (Decision 0009 §5). The worker makes one attempt at
    /// watcher intent up to `cutoff` (the generation when release was
    /// accepted), then closes. A lease or an enqueue cancels it before
    /// `Closing`; reads keep the validated head meanwhile.
    RetireRequested {
        cutoff: Option<u64>,
        session: Option<Arc<WriterSession>>,
    },
    /// The worker is closing the watcher, session and witnesses. Nothing is
    /// admitted on this runtime from here on; callers wait for `Retired`.
    Closing,
    /// Released. No further work, reads or claims on this runtime.
    Retired,
}

impl RuntimeState {
    pub(crate) fn session(&self) -> Option<&Arc<WriterSession>> {
        match self {
            RuntimeState::Ready(session)
            | RuntimeState::RetireRequested {
                session: Some(session),
                ..
            } => Some(session),
            _ => None,
        }
    }
}

/// The outcome of asking a runtime to retire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetireOutcome {
    /// Work, a lease or startup reconciliation keeps the runtime active.
    Declined,
    /// The worker will retire it (or is closing it).
    Requested,
    /// Fully retired; the registry may remove it.
    Retired,
}

/// Status a reader may show; never an admission decision (Decision 0009 §3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimePhase {
    Activating,
    Reconciling { started: bool, waiting: bool },
    Ready,
    RootLost,
    RetireRequested,
    Closing,
    Retired,
}

impl RuntimePhase {
    pub(crate) fn of(state: &RuntimeState) -> Self {
        match state {
            RuntimeState::Activating => RuntimePhase::Activating,
            RuntimeState::Reconciling { attempts, waiting } => RuntimePhase::Reconciling {
                started: *attempts > 0,
                waiting: *waiting,
            },
            RuntimeState::Ready(_) => RuntimePhase::Ready,
            RuntimeState::RootLost => RuntimePhase::RootLost,
            RuntimeState::RetireRequested { .. } => RuntimePhase::RetireRequested,
            RuntimeState::Closing => RuntimePhase::Closing,
            RuntimeState::Retired => RuntimePhase::Retired,
        }
    }
}

impl DaemonState {
    pub fn runtime_phase(&self) -> RuntimePhase {
        RuntimePhase::of(&self.runtime.lock().unwrap())
    }

    /// Begin mandatory startup reconciliation for a daemon checkout runtime.
    pub(crate) fn begin_startup_reconciliation(self: &Arc<Self>) {
        {
            let mut state = self.runtime.lock().unwrap();
            if matches!(*state, RuntimeState::Activating) {
                *state = RuntimeState::Reconciling {
                    attempts: 0,
                    waiting: false,
                };
            }
        }
        self.start_worker();
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn startup_captures_for_tests(&self) -> u64 {
        self.startup_captures.load(Ordering::Acquire)
    }

    /// Push all timed work out of reach: the watcher's periodic inventory
    /// (see `WatcherWork::delay_periodic_inventory_for_tests`) and maintenance.
    /// False without a watcher.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn delay_timed_work_for_tests(&self) -> bool {
        let mut work = self.watcher_work.lock().unwrap();
        let Some((_, watcher)) = work.as_mut() else {
            return false;
        };
        watcher.delay_periodic_inventory_for_tests();
        drop(work);
        *self.retention_last_run.lock().unwrap() = Instant::now() + Duration::from_secs(3600);
        // Make the worker recompute its sleep deadline from the delayed inventory.
        self.inbox.wake();
        true
    }

    /// The worker's deadline while it is blocked with no wake pending
    /// (`Some(None)`: no deadline); `None` while it is running a step.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn worker_asleep_until_for_tests(&self) -> Option<Option<Instant>> {
        *self.inbox.asleep.lock().unwrap()
    }

    pub fn startup_error(&self) -> Option<String> {
        self.startup_error.lock().unwrap().clone()
    }

    /// Deterministically pause startup reconciliation after old-root settlement
    /// and before it captures the workspace.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_startup_hook_for_tests(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.startup_hook.lock().unwrap() = Some(hook);
    }

    /// Whether the queue tick should run startup reconciliation now. A
    /// standalone scheduler that was never asked to reconcile does so only
    /// when `work_pending` reports accepted work (`None`: undecided, retry).
    pub(super) fn startup_due(
        &self,
        work_pending: impl FnOnce() -> anyhow::Result<Option<bool>>,
    ) -> anyhow::Result<bool> {
        match &*self.runtime.lock().unwrap() {
            RuntimeState::Reconciling { .. } => return Ok(true),
            RuntimeState::Activating => {}
            RuntimeState::Ready(_)
            | RuntimeState::RootLost
            | RuntimeState::RetireRequested { .. }
            | RuntimeState::Closing
            | RuntimeState::Retired => {
                return Ok(false);
            }
        }
        if self.store.is_root_replaced() || self.store.is_recreate_pending() {
            return Ok(true);
        }
        Ok(work_pending()?.unwrap_or(false))
    }

    /// The options startup reconciliation captures with. Explicit registered
    /// options win; otherwise recorded inputs, then the FIFO head's, then the
    /// daemon defaults. A recreate without trusted inputs needs the FIFO
    /// head's options (or, for a replaced root, the defaults).
    fn startup_options(&self) -> anyhow::Result<Option<IndexOptions>> {
        let config = self.config();
        let explicit = config.explicit_options;
        // The FIFO head is read first, even when unused: a typed failure to
        // read it defers this attempt rather than reconciling around it.
        let head = self
            .startup_head()?
            .map(|head| head.options(std::path::Path::new(self.store.workspace_root())))
            .transpose()?;
        if self.store.is_recreate_pending() {
            if explicit {
                return Ok(Some(config.options.clone()));
            }
            return Ok(head.or_else(|| {
                self.store
                    .is_root_replaced()
                    .then(|| config.options.clone())
            }));
        }
        if explicit {
            return Ok(Some(config.options.clone()));
        }
        Ok(Some(match self.store.recorded_index_options()? {
            Some(recorded) => recorded,
            None => head.unwrap_or_else(|| config.options.clone()),
        }))
    }

    /// The FIFO head as startup reconciliation sees it.
    pub(super) fn startup_head(&self) -> anyhow::Result<Option<crate::store::requests::Request>> {
        #[cfg(test)]
        if self
            .test_activation_head_busy_once
            .swap(false, Ordering::AcqRel)
        {
            return Err(crate::store::topology::StorageBusy.into());
        }
        self.store.earliest_unfinished_request()
    }

    fn startup_attempt(&self) -> anyhow::Result<Arc<WriterSession>> {
        self.store.settle_foreign_root_requests()?;
        #[cfg(feature = "test-support")]
        if let Some(hook) = self.startup_hook.lock().unwrap().take() {
            hook();
        }
        if let RuntimeState::Reconciling { attempts, .. } = &mut *self.runtime.lock().unwrap() {
            *attempts += 1;
        }
        #[cfg(feature = "test-support")]
        self.startup_captures.fetch_add(1, Ordering::AcqRel);
        let options = self.startup_options()?;
        crate::index_coordinator::establish_serving_session(
            &self.store,
            options.as_ref(),
            &Arc::new(AtomicBool::new(false)),
        )
    }

    /// One startup reconciliation attempt; the worker's highest-priority item.
    /// On failure the state waits out a doubling backoff, and only a typed
    /// contention is not reported as a tick failure.
    pub(super) fn run_startup_reconciliation(&self) -> anyhow::Result<()> {
        {
            let mut state = self.runtime.lock().unwrap();
            let attempts = match &*state {
                RuntimeState::Reconciling { attempts, .. } => *attempts,
                RuntimeState::Activating => 0,
                _ => return Ok(()),
            };
            *state = RuntimeState::Reconciling {
                attempts,
                waiting: false,
            };
        }
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.startup_attempt()))
                .unwrap_or_else(|panic| {
                    let message = panic
                        .downcast_ref::<&str>()
                        .map(|text| (*text).to_owned())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".to_owned());
                    Err(anyhow::anyhow!(
                        "startup reconciliation panicked: {message}"
                    ))
                });
        match outcome {
            Ok(session) => {
                self.replace_serving_session(Some(session));
                *self.startup_error.lock().unwrap() = None;
                *self.startup_backoff.lock().unwrap() = STARTUP_BACKOFF_FIRST;
                *self.recovery_retry_after.lock().unwrap() = None;
                self.step_progress.store(true, Ordering::Release);
                Ok(())
            }
            Err(error) => {
                {
                    let mut first = self.startup_error.lock().unwrap();
                    if first.is_none() {
                        *first = Some(format!("{error:#}"));
                    }
                }
                let delay = {
                    let mut backoff = self.startup_backoff.lock().unwrap();
                    let delay = *backoff;
                    *backoff = backoff.saturating_mul(2).min(STARTUP_BACKOFF_MAX);
                    delay
                };
                *self.recovery_retry_after.lock().unwrap() = Some(Instant::now() + delay);
                if let RuntimeState::Reconciling { waiting, .. } =
                    &mut *self.runtime.lock().unwrap()
                {
                    *waiting = true;
                }
                if crate::store::transient_storage_contention(&error) {
                    Ok(())
                } else {
                    Err(error)
                }
            }
        }
    }
}

/// The worker's inbox (Decision 0009 §8): a sticky wake flag under a condition
/// variable. Setting it cannot fail, so wakes coalesce but are never lost; the
/// worker re-reads durable state after every wake.
#[derive(Default)]
pub(super) struct WorkerInbox {
    wake: std::sync::Mutex<bool>,
    changed: std::sync::Condvar,
    shutdown: AtomicBool,
    /// While the worker is blocked with no wake pending: its deadline.
    #[cfg(feature = "test-support")]
    asleep: std::sync::Mutex<Option<Option<Instant>>>,
}

impl WorkerInbox {
    pub(super) fn wake(&self) {
        *self.wake.lock().unwrap() = true;
        self.changed.notify_one();
    }

    /// A wake arrived that the worker has not consumed yet: newer work.
    pub(super) fn wake_pending(&self) -> bool {
        *self.wake.lock().unwrap()
    }

    pub(super) fn shut_down(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.wake();
    }

    /// Block until woken or `deadline` passes (`None`: until woken).
    fn wait(&self, deadline: Option<Instant>) {
        let mut woken = self.wake.lock().unwrap();
        #[cfg(feature = "test-support")]
        if !*woken {
            *self.asleep.lock().unwrap() = Some(deadline);
        }
        while !*woken && !self.shutdown.load(Ordering::Acquire) {
            match deadline {
                None => woken = self.changed.wait(woken).unwrap(),
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        break;
                    }
                    woken = self.changed.wait_timeout(woken, deadline - now).unwrap().0;
                }
            }
        }
        #[cfg(feature = "test-support")]
        {
            *self.asleep.lock().unwrap() = None;
        }
        *woken = false;
    }
}

/// What a reader may know about watcher intent without the watcher's lock:
/// the live ingress generation, the generation last acknowledged by a
/// verified capture, and any other pending intent the worker computed.
pub(super) struct WatchStatus {
    pub(super) generation: Arc<std::sync::atomic::AtomicU64>,
    pub(super) acknowledged: u64,
    pub(super) other_intent: bool,
}

/// A failed or panicked step is retried after this delay.
const STEP_RETRY: Duration = Duration::from_millis(250);
/// Retention runs at most this often while idle (a deferral shortens it).
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60);

impl DaemonState {
    /// Start this checkout's worker thread, or wake it if it is running. A
    /// synchronous fixture without a Tokio runtime drives `queue_tick` itself.
    pub(super) fn start_worker(self: &Arc<Self>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if self.queue_tick_started.swap(true, Ordering::AcqRel) {
            self.inbox.wake();
            return;
        }
        let weak = Arc::downgrade(self);
        let inbox = self.inbox.clone();
        let spawned = std::thread::Builder::new()
            .name("trellis-checkout-worker".into())
            .spawn(move || {
                // Steps start async helpers (dependency indexing) on the daemon runtime.
                let _runtime = runtime.enter();
                worker_loop(weak, inbox)
            });
        if let Err(error) = spawned {
            self.queue_tick_started.store(false, Ordering::Release);
            super::best_effort_queue_stderr(
                std::io::stderr(),
                format_args!("checkout worker failed to start: {error}\n"),
            );
        }
    }

    /// When the worker next has work, if nothing wakes it first.
    fn next_wake(&self) -> Option<Instant> {
        let now = Instant::now();
        if self.step_progress.swap(false, Ordering::AcqRel) {
            return Some(now);
        }
        if let Some(retry) = *self.recovery_retry_after.lock().unwrap()
            && retry > now
        {
            return Some(retry);
        }
        match &*self.runtime.lock().unwrap() {
            RuntimeState::Activating
            | RuntimeState::RootLost
            | RuntimeState::Closing
            | RuntimeState::Retired => {
                return None;
            }
            RuntimeState::Reconciling { .. } | RuntimeState::RetireRequested { .. } => {
                return Some(now);
            }
            RuntimeState::Ready(_) => {}
        }
        // An accepted head that made no progress (and set no deferral) is
        // retried rather than left until the next inventory.
        if self.fifo_pending.load(Ordering::Acquire) {
            return Some(now + STEP_RETRY);
        }
        let watcher_due = match &*self.watcher_work.lock().unwrap() {
            Some((_, watcher)) => watcher.next_due(),
            None => now,
        };
        Some(watcher_due.min(self.maintenance_due_at()))
    }

    /// The next retention unit is due 60 s after the last idle one, or soon
    /// after a deferral (the unit sets this clock).
    fn maintenance_due_at(&self) -> Instant {
        *self.retention_last_run.lock().unwrap() + MAINTENANCE_INTERVAL
    }

    /// Maintenance is the lowest class (Decision 0009 §4): it runs only when
    /// the step made no progress, no wake is pending and its unit is due.
    fn maintenance_ready(&self) -> bool {
        !self.step_progress.load(Ordering::Acquire)
            && !self.inbox.wake_pending()
            && matches!(*self.runtime.lock().unwrap(), RuntimeState::Ready(_))
            && Instant::now() >= self.maintenance_due_at()
    }

    /// Publish whether accepted FIFO work remains (an unreadable queue counts
    /// as pending). When it drains, wake the lifecycle: a declined release
    /// may now proceed.
    fn publish_fifo_pending(&self) {
        let pending = match &*self.runtime.lock().unwrap() {
            // Nothing is claimed on a retired runtime; never reopen its queue.
            RuntimeState::Closing | RuntimeState::Retired => false,
            RuntimeState::RootLost => !matches!(self.store.old_root_unfinished_request(), Ok(None)),
            _ => !matches!(self.store.earliest_unfinished_request(), Ok(None)),
        };
        let was = self.fifo_pending.swap(pending, Ordering::AcqRel);
        if was
            && !pending
            && let Some(lifecycle) = self.lifecycle.get()
        {
            lifecycle();
        }
    }

    /// Hold the worker at the start of its next step until the guard drops.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn hold_worker_for_tests(&self) -> std::sync::RwLockWriteGuard<'_, ()> {
        self.worker_hold.write().unwrap()
    }

    /// Make the next retention unit due now, and run `hook` as it begins.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn on_next_maintenance_unit_for_tests(&self, hook: impl FnOnce() + Send + 'static) {
        *self.maintenance_hook.lock().unwrap() = Some(Box::new(hook));
        *self.retention_last_run.lock().unwrap() = Instant::now() - MAINTENANCE_INTERVAL;
    }

    /// Hold retirement just before its `Closing` linearization point.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_retire_hook_for_tests(&self, hook: impl FnOnce() + Send + 'static) {
        *self.retire_hook.lock().unwrap() = Some(Box::new(hook));
    }

    /// Hold retirement after `Closing`, before `Retired`.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_closing_hook_for_tests(&self, hook: impl FnOnce() + Send + 'static) {
        *self.closing_hook.lock().unwrap() = Some(Box::new(hook));
    }

    /// Accept one index request durably and wake the worker. The only enqueue
    /// path for browser and CLI requests (#138 item 23). A newly accepted
    /// request starts its own attempt at once, overriding an earlier deferral.
    pub(crate) fn enqueue_index(
        &self,
        options: &IndexOptions,
        expected: Option<crate::model::IndexPin>,
    ) -> anyhow::Result<crate::store::requests::Request> {
        let lease = self.admission_lease()?;
        let row = self.store.enqueue_request(options, expected)?;
        self.fifo_pending.store(true, Ordering::Release);
        drop(lease);
        self.pending_requests.lock().unwrap().push(row.id.clone());
        *self.recovery_retry_after.lock().unwrap() = None;
        self.inbox.wake();
        Ok(row)
    }

    /// Take an admission lease unless the runtime is retired. Release declines
    /// while a lease is held, and both decide under the runtime state lock.
    /// A lease taken while retirement is requested cancels it (Decision 0009
    /// §5): the runtime returns to its prior state with its resources live.
    /// From `Closing` on, nothing is admitted here; use a fresh runtime.
    pub(crate) fn admission_lease(&self) -> anyhow::Result<AdmissionLease> {
        let mut state = self.runtime.lock().unwrap();
        match &*state {
            RuntimeState::Closing | RuntimeState::Retired => {
                anyhow::bail!("index_not_ready: checkout released")
            }
            RuntimeState::RetireRequested { session, .. } => {
                *state = match session {
                    Some(session) => RuntimeState::Ready(session.clone()),
                    None => RuntimeState::Reconciling {
                        attempts: 0,
                        waiting: false,
                    },
                };
                self.inbox.wake();
            }
            _ => {}
        }
        self.leases.fetch_add(1, Ordering::AcqRel);
        Ok(AdmissionLease {
            count: self.leases.clone(),
            lifecycle: self.lifecycle.clone(),
        })
    }

    /// Ask the worker to retire this runtime. `idle` decides, under the state
    /// lock, whether nothing keeps it active (Decision 0009 §5). Startup
    /// reconciliation in progress, a lease or accepted FIFO work declines.
    pub(crate) fn request_retire(&self, idle: impl FnOnce(RuntimePhase) -> bool) -> RetireOutcome {
        let mut state = self.runtime.lock().unwrap();
        let session = match &*state {
            RuntimeState::Retired => return RetireOutcome::Retired,
            RuntimeState::Closing | RuntimeState::RetireRequested { .. } => {
                return RetireOutcome::Requested;
            }
            RuntimeState::Reconciling { waiting: false, .. } => return RetireOutcome::Declined,
            RuntimeState::Ready(session) => Some(session.clone()),
            RuntimeState::Activating
            | RuntimeState::Reconciling { waiting: true, .. }
            | RuntimeState::RootLost => None,
        };
        if self.leases.load(Ordering::Acquire) != 0
            || self.fifo_pending.load(Ordering::Acquire)
            || !idle(RuntimePhase::of(&state))
        {
            return RetireOutcome::Declined;
        }
        let cutoff = self
            .watch_status
            .lock()
            .unwrap()
            .as_ref()
            .map(|status| status.generation.load(Ordering::SeqCst));
        *state = RuntimeState::RetireRequested { cutoff, session };
        drop(state);
        self.inbox.wake();
        RetireOutcome::Requested
    }

    /// The worker's retirement step. A FIFO row accepted around the request
    /// (through any Store) cancels it. Otherwise one attempt at watcher
    /// intent up to the cutoff, then the `Closing` linearization and close.
    pub(super) fn run_retirement(&self) -> anyhow::Result<()> {
        // A lost root's accepted requests are already settled; new-root work
        // belongs to a fresh runtime.
        let root_lost = self.store.root_path_replaced().unwrap_or(true);
        if !root_lost && self.store.earliest_unfinished_request()?.is_some() {
            let mut state = self.runtime.lock().unwrap();
            if let RuntimeState::RetireRequested { session, .. } = &*state {
                *state = match session {
                    Some(session) => RuntimeState::Ready(session.clone()),
                    None => RuntimeState::Reconciling {
                        attempts: 0,
                        waiting: false,
                    },
                };
            }
            self.step_progress.store(true, Ordering::Release);
            return Ok(());
        }
        self.reconcile_through_cutoff();
        #[cfg(feature = "test-support")]
        if let Some(hook) = self.retire_hook.lock().unwrap().take() {
            hook();
        }
        {
            let mut state = self.runtime.lock().unwrap();
            if !matches!(*state, RuntimeState::RetireRequested { .. }) {
                // Cancelled by a lease or an enqueue: resources stay live.
                return Ok(());
            }
            *state = RuntimeState::Closing;
        }
        *self.watch_status.lock().unwrap() = None;
        *self.watcher_work.lock().unwrap() = None;
        self.watcher_health.store(false, Ordering::Release);
        #[cfg(feature = "test-support")]
        if let Some(hook) = self.closing_hook.lock().unwrap().take() {
            hook();
        }
        self.store.retire_checkout_sqlite_witnesses();
        *self.runtime.lock().unwrap() = RuntimeState::Retired;
        self.retired_changed.notify_all();
        if let Some(lifecycle) = self.lifecycle.get() {
            lifecycle();
        }
        Ok(())
    }

    /// One attempt at accepted watcher intent up to the cutoff. Its outcome
    /// never blocks retirement: changes it misses are captured by the next
    /// activation's mandatory startup reconciliation.
    fn reconcile_through_cutoff(&self) {
        let (cutoff, session) = match &*self.runtime.lock().unwrap() {
            RuntimeState::RetireRequested {
                cutoff: Some(cutoff),
                session: Some(session),
            } => (*cutoff, session.clone()),
            _ => return,
        };
        let options = match self.store.recorded_index_options() {
            Ok(options) => options.unwrap_or_else(|| self.config().options.clone()),
            Err(_) => return,
        };
        let mut work = self.watcher_work.lock().unwrap();
        let Some((_, watcher)) = work.as_mut() else {
            return;
        };
        if watcher.acknowledged_generation() >= cutoff
            && !watcher.intent_besides_generation(&options)
        {
            return;
        }
        let _ = watcher.reconcile_due(&self.store, &session, &options, &self.watcher_cancel, true);
    }

    /// Wait until this runtime is `Retired` (or `timeout` passes). Close is
    /// bounded local work: watcher, session and witness handles.
    pub(crate) fn wait_retired(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.runtime.lock().unwrap();
        while !matches!(*state, RuntimeState::Retired) {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = self
                .retired_changed
                .wait_timeout(state, deadline - now)
                .unwrap()
                .0;
        }
        true
    }

    /// Register the registry's lifecycle wake, signalled on retirement, when
    /// accepted FIFO work drains and when the last lease is released.
    pub(crate) fn set_lifecycle_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        let _ = self.lifecycle.set(wake);
    }

    pub(crate) fn fifo_pending(&self) -> bool {
        self.fifo_pending.load(Ordering::Acquire)
    }

    pub(crate) fn leases(&self) -> usize {
        self.leases.load(Ordering::Acquire)
    }

    /// Publish the watcher's status after the worker changed it.
    pub(super) fn publish_watch_status(
        &self,
        watcher: &crate::index_coordinator::WatcherWork,
        options: &IndexOptions,
    ) {
        *self.watch_status.lock().unwrap() = Some(WatchStatus {
            generation: watcher.generation_handle(),
            acknowledged: watcher.acknowledged_generation(),
            other_intent: watcher.intent_besides_generation(options),
        });
    }
}

/// A read or enqueue admission held against release. Dropping the last one
/// wakes the lifecycle, which may now release an idle checkout.
pub struct AdmissionLease {
    count: Arc<std::sync::atomic::AtomicUsize>,
    lifecycle: Arc<std::sync::OnceLock<Arc<dyn Fn() + Send + Sync>>>,
}

impl Drop for AdmissionLease {
    fn drop(&mut self) {
        if self.count.fetch_sub(1, Ordering::AcqRel) == 1
            && let Some(lifecycle) = self.lifecycle.get()
        {
            lifecycle();
        }
    }
}

fn worker_loop(weak: std::sync::Weak<DaemonState>, inbox: Arc<WorkerInbox>) {
    let mut deadline = Some(Instant::now());
    loop {
        inbox.wait(deadline);
        if inbox.shutdown.load(Ordering::Acquire) {
            return;
        }
        let Some(state) = weak.upgrade() else {
            return;
        };
        if matches!(*state.runtime.lock().unwrap(), RuntimeState::Retired) {
            return;
        }
        let step = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| state.queue_tick()));
        state.publish_fifo_pending();
        deadline = match step {
            Ok(Ok(())) => {
                // The witness reads phase only after the step released its locks.
                state.report_causal_h_ready();
                if state.maintenance_ready() {
                    state.run_maintenance_unit();
                }
                state.next_wake()
            }
            Ok(Err(error)) => {
                super::best_effort_queue_stderr(
                    std::io::stderr(),
                    format_args!("queue tick failed: {error:#}\n"),
                );
                Some(Instant::now() + STEP_RETRY)
            }
            Err(_) => {
                super::best_effort_queue_stderr(
                    std::io::stderr(),
                    format_args!("queue tick worker failed: panicked\n"),
                );
                Some(Instant::now() + STEP_RETRY)
            }
        };
    }
}
