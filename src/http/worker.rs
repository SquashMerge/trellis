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
/// Fail accepted queued work after this many recovery faults, but keep retrying.
/// SQLite contention never consumes this allowance.
const STARTUP_FAILURE_LIMIT: u64 = 5;

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
            RuntimeState::RetireRequested { .. } => RuntimePhase::RetireRequested,
            RuntimeState::Closing => RuntimePhase::Closing,
            RuntimeState::Retired => RuntimePhase::Retired,
        }
    }
}

impl DaemonState {
    pub(super) fn worker_now(&self) -> Instant {
        #[cfg(test)]
        if let Some(now) = *self.test_worker_now.lock().unwrap() {
            return now;
        }
        Instant::now()
    }

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
        self.note_state_change();
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

    pub(crate) fn recovery_blocker(&self) -> Option<String> {
        if self.startup_failures.load(Ordering::Acquire) < STARTUP_FAILURE_LIMIT {
            return None;
        }
        self.startup_error()
            .map(|reason| format!("recovery_blocked: {reason}"))
    }

    /// Deterministically pause startup reconciliation after old-root settlement
    /// and before it captures the workspace.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_startup_hook_for_tests(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.startup_hook.lock().unwrap() = Some(hook);
    }

    /// Whether the worker step should run startup reconciliation now. A
    /// standalone scheduler that was never asked to reconcile does so only
    /// when `work_pending` reports accepted work (`None`: undecided, retry).
    pub(super) fn startup_due(
        &self,
        work_pending: impl FnOnce() -> anyhow::Result<Option<bool>>,
    ) -> anyhow::Result<bool> {
        if self
            .recovery_retry_after
            .lock()
            .unwrap()
            .is_some_and(|retry| self.worker_now() < retry)
        {
            return Ok(false);
        }
        match &*self.runtime.lock().unwrap() {
            RuntimeState::Reconciling { .. } => return Ok(true),
            RuntimeState::Activating => {}
            RuntimeState::Ready(_)
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
    /// daemon defaults. Recreation uses the FIFO head, retained watcher
    /// options, then daemon defaults when no explicit options are registered.
    fn startup_options(&self) -> anyhow::Result<IndexOptions> {
        let config = self.config();
        let explicit = config.explicit_options;
        // The FIFO head is read first, even when unused: a typed failure to
        // read it defers this attempt rather than reconciling around it.
        let head = match self.startup_head()? {
            Some(head) if head.request_kind == crate::store::requests::RequestKind::Index => {
                Some(head.options(std::path::Path::new(self.store.workspace_root()))?)
            }
            // A semantic head is never decoded here: an undecodable row would otherwise block
            // startup reconciliation on every attempt. It settles at claim instead.
            _ => None,
        };
        if self.store.is_recreate_pending() {
            if explicit {
                return Ok(config.options.clone());
            }
            let watched = self
                .watcher_work
                .lock()
                .unwrap()
                .as_ref()
                .map(|(_, watcher)| watcher.options());
            return Ok(head.or(watched).unwrap_or_else(|| config.options.clone()));
        }
        if explicit {
            return Ok(config.options.clone());
        }
        Ok(match self.store.recorded_index_options() {
            Ok(Some(recorded)) => recorded,
            Ok(None) => head.unwrap_or_else(|| config.options.clone()),
            Err(error) if crate::store::Store::is_rebuild_failure(&error) => {
                let watched = self
                    .watcher_work
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|(_, watcher)| watcher.options());
                watched.or(head).unwrap_or_else(|| config.options.clone())
            }
            Err(error) => return Err(error),
        })
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

    fn startup_attempt(
        &self,
        retained: Option<(Arc<WriterSession>, Option<IndexOptions>)>,
    ) -> anyhow::Result<Arc<WriterSession>> {
        self.store.settle_foreign_root_requests()?;
        #[cfg(test)]
        if let Some(error) = self.test_startup_error.lock().unwrap().take() {
            return Err(error);
        }
        #[cfg(feature = "test-support")]
        {
            let hook = self.startup_hook.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        if let RuntimeState::Reconciling { attempts, .. } = &mut *self.runtime.lock().unwrap() {
            *attempts += 1;
        }
        #[cfg(feature = "test-support")]
        self.startup_captures.fetch_add(1, Ordering::AcqRel);
        if let Some((session, options)) = retained {
            let options = match options {
                Some(options) => options,
                None => self.startup_options()?,
            };
            let coordinator = crate::index_coordinator::IndexJobCoordinator::prepare_with_session(
                &self.store,
                None,
                session.clone(),
            )?;
            coordinator.run_serving(&options, &Arc::new(AtomicBool::new(false)), |_| {})?;
            return Ok(session);
        }
        let options = self.startup_options()?;
        crate::index_coordinator::establish_serving_session(
            &self.store,
            Some(&options),
            &Arc::new(AtomicBool::new(false)),
        )
    }

    fn confirm_recovery_disposition(&self) -> anyhow::Result<()> {
        let reason = self.recovery_disposition.lock().unwrap().clone();
        if let Some(reason) = reason {
            self.store.fail_queued_recovery_blocked(&reason)?;
            *self.recovery_disposition.lock().unwrap() = None;
        }
        Ok(())
    }

    /// One startup reconciliation attempt; the worker's highest-priority item.
    /// On failure the state waits out a doubling backoff, and only a typed
    /// contention is not reported as a tick failure.
    pub(super) fn run_startup_reconciliation(&self) -> anyhow::Result<()> {
        self.run_reconciliation(None)
    }

    pub(super) fn run_rebuild_reconciliation(
        &self,
        session: Arc<WriterSession>,
        options: Option<IndexOptions>,
    ) -> anyhow::Result<()> {
        self.run_reconciliation(Some((session, options)))
    }

    fn run_reconciliation(
        &self,
        retained: Option<(Arc<WriterSession>, Option<IndexOptions>)>,
    ) -> anyhow::Result<()> {
        {
            let mut state = self.runtime.lock().unwrap();
            let attempts = match &*state {
                RuntimeState::Reconciling { attempts, .. } => *attempts,
                RuntimeState::Activating => 0,
                RuntimeState::Ready(_) if retained.is_some() => 0,
                _ => return Ok(()),
            };
            *state = RuntimeState::Reconciling {
                attempts,
                waiting: false,
            };
        }
        self.note_state_change();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.confirm_recovery_disposition()?;
            self.startup_attempt(retained)
        }))
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
                self.startup_failures.store(0, Ordering::Release);
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
                *self.recovery_retry_after.lock().unwrap() = Some(self.worker_now() + delay);
                if let RuntimeState::Reconciling { waiting, .. } =
                    &mut *self.runtime.lock().unwrap()
                {
                    *waiting = true;
                }
                if crate::store::nonterminal_storage_busy(&error) {
                    self.note_state_change();
                    return Ok(());
                }
                let _ = self.startup_failures.try_update(
                    Ordering::AcqRel,
                    Ordering::Acquire,
                    |failures| Some(failures.saturating_add(1)),
                );
                if self.startup_failures.load(Ordering::Acquire) >= STARTUP_FAILURE_LIMIT {
                    let reason = format!("{error:#}");
                    *self.startup_error.lock().unwrap() = Some(reason.clone());
                    self.note_state_change();
                    *self.recovery_disposition.lock().unwrap() = Some(reason);
                    self.confirm_recovery_disposition()?;
                } else {
                    self.note_state_change();
                }
                Err(error)
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
    /// synchronous fixture without a Tokio runtime drives `worker_step` itself.
    pub(super) fn start_worker(self: &Arc<Self>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if self.worker_started.swap(true, Ordering::AcqRel) {
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
            self.worker_started.store(false, Ordering::Release);
            super::best_effort_queue_stderr(
                std::io::stderr(),
                format_args!("checkout worker failed to start: {error}\n"),
            );
        }
    }

    fn failed_step_retry(&self) -> Instant {
        let now = self.worker_now();
        self.recovery_retry_after
            .lock()
            .unwrap()
            .filter(|retry| *retry > now)
            .unwrap_or(now + STEP_RETRY)
    }

    /// When the worker next has work, if nothing wakes it first.
    fn next_wake(&self) -> Option<Instant> {
        let now = self.worker_now();
        if self.step_progress.swap(false, Ordering::AcqRel) {
            return Some(now);
        }
        if let Some(retry) = *self.recovery_retry_after.lock().unwrap()
            && retry > now
        {
            return Some(retry);
        }
        match &*self.runtime.lock().unwrap() {
            RuntimeState::Activating | RuntimeState::Closing | RuntimeState::Retired => {
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

    /// A purged checkout index has no queue left to drain. Only its exact
    /// directory's absence permits closing; all other failures remain errors.
    /// The worker owns this transition and never closes across an admitted read
    /// or an enqueue already in flight.
    pub(super) fn close_if_index_missing(&self) -> anyhow::Result<bool> {
        if !self.store.index_directory_missing()? {
            return Ok(false);
        }
        let mut state = self.runtime.lock().unwrap();
        if matches!(*state, RuntimeState::Closing | RuntimeState::Retired) {
            return Ok(true);
        }
        if self.leases.load(Ordering::Acquire) != 0 || !self.store.try_begin_closing() {
            // External filesystem loss has no completion signal. Retry using
            // the worker's existing bounded backoff, without reopening the queue.
            *self.recovery_retry_after.lock().unwrap() = Some(Instant::now() + STEP_RETRY);
            return Ok(true);
        }
        // Recheck at the owning transition: a recreated directory may already
        // contain accepted work, which must follow the normal queue path.
        match self.store.index_directory_missing() {
            Ok(true) => {}
            result => {
                self.store.end_closing();
                return result.map(|_| false);
            }
        }
        let held = match std::mem::replace(&mut *state, RuntimeState::Closing) {
            RuntimeState::Ready(session) => Some(session),
            RuntimeState::RetireRequested { session, .. } => session,
            _ => None,
        };
        drop(state);
        self.store.notify_request_queue_unavailable();
        self.close(held);
        Ok(true)
    }

    /// Publish whether accepted FIFO work remains (an unreadable queue counts
    /// as pending). When it drains, wake the lifecycle: a declined release
    /// may now proceed.
    pub(super) fn publish_fifo_pending(&self) {
        let pending = if matches!(
            *self.runtime.lock().unwrap(),
            RuntimeState::Closing | RuntimeState::Retired
        ) {
            false
        } else {
            // A deletion can race the step. Never recreate the directory in
            // this publication probe, even when an admitted read delays close.
            match self.close_if_index_missing() {
                Ok(true) => !self.retired(),
                Ok(false) => !matches!(self.store.earliest_unfinished_request(), Ok(None)),
                Err(_) => true,
            }
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

    /// Semantic work joins the same admission lease, durable FIFO and inbox wake.
    pub(crate) fn enqueue_semantic_import(
        &self,
        options: &crate::store::requests::SemanticImportOptions,
    ) -> anyhow::Result<crate::store::requests::Request> {
        let lease = self.admission_lease()?;
        let row = self
            .store
            .enqueue_semantic_import(std::path::Path::new(self.store.workspace_root()), options)?;
        self.fifo_pending.store(true, Ordering::Release);
        drop(lease);
        self.pending_requests.lock().unwrap().push(row.id.clone());
        *self.recovery_retry_after.lock().unwrap() = None;
        self.inbox.wake();
        Ok(row)
    }

    /// A Trellis-written import joins the same lease, FIFO and inbox wake. Only the producer
    /// import module calls this.
    pub(crate) fn enqueue_generated_semantic_import(
        &self,
        options: &crate::store::requests::GeneratedImportOptions,
    ) -> anyhow::Result<crate::store::requests::Request> {
        let lease = self.admission_lease()?;
        let row = self.store.enqueue_generated_semantic_import(
            std::path::Path::new(self.store.workspace_root()),
            options,
        )?;
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
                if self.root_lost.load(Ordering::Acquire) {
                    anyhow::bail!("root_changed: captured root lost");
                }
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
                self.state_version.fetch_add(1, Ordering::AcqRel);
                self.state_changed.notify_all();
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

    /// A lease for the Architecture owner's read. Unlike a client's lease it never cancels
    /// a requested retirement: nothing is waiting for this work.
    pub(super) fn background_lease(&self) -> Option<AdmissionLease> {
        let state = self.runtime.lock().unwrap();
        if matches!(
            *state,
            RuntimeState::RetireRequested { .. } | RuntimeState::Closing | RuntimeState::Retired
        ) {
            return None;
        }
        self.leases.fetch_add(1, Ordering::AcqRel);
        Some(AdmissionLease {
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
            RuntimeState::Activating | RuntimeState::Reconciling { waiting: true, .. } => None,
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
        self.note_state_change();
        self.inbox.wake();
        RetireOutcome::Requested
    }

    /// The worker's retirement step. A FIFO row accepted around the request
    /// (through any Store) cancels it. Otherwise one attempt at watcher
    /// intent up to the cutoff, then the `Closing` linearization and close.
    pub(super) fn run_retirement(&self) -> anyhow::Result<()> {
        #[cfg(feature = "test-support")]
        {
            let hook = self.retirement_entered_hook.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        if self.close_if_index_missing()? {
            return Ok(());
        }
        // A root replaced since the step's own check takes the same path:
        // settle its accepted rows, mark the loss, then close. A failed check
        // is an ordinary step error, never taken as loss.
        if self.store.root_path_replaced()? {
            return self.close_for_root_loss();
        }
        if self.store.earliest_unfinished_request()?.is_some() {
            self.cancel_retirement(self.runtime.lock().unwrap());
            return Ok(());
        }
        #[cfg(feature = "test-support")]
        if let Some(hook) = self.retire_hook.lock().unwrap().take() {
            hook();
        }
        self.reconcile_through_cutoff();
        // The Closing linearization point: one compare-and-swap on the
        // queue's enqueue word sets its closing bit only if no enqueue
        // (through any Store) is in flight from before its transaction to
        // after its wake. One in flight, or a wake that set `fifo_pending`
        // since the FIFO check above, cancels. An enqueue that starts after
        // the swap waits, durably, for the next activation.
        let held = {
            let mut state = self.runtime.lock().unwrap();
            if !matches!(*state, RuntimeState::RetireRequested { .. }) {
                return Ok(());
            }
            if self.fifo_pending.load(Ordering::Acquire) || !self.store.try_begin_closing() {
                self.cancel_retirement(state);
                return Ok(());
            }
            match std::mem::replace(&mut *state, RuntimeState::Closing) {
                RuntimeState::RetireRequested { session, .. } => session,
                _ => None,
            }
        };
        self.close(held);
        Ok(())
    }

    /// The captured root was lost (Decision 0009 §2): its accepted rows are
    /// settled with a confirmed terminal COMMIT first, then the runtime is
    /// marked root-lost and closes. Until that COMMIT succeeds it keeps its
    /// resources. Recovery for the path runs on a fresh runtime.
    pub(super) fn close_for_root_loss(&self) -> anyhow::Result<()> {
        self.store.fail_changed_root_requests()?;
        self.root_lost.store(true, Ordering::Release);
        let held =
            match std::mem::replace(&mut *self.runtime.lock().unwrap(), RuntimeState::Closing) {
                RuntimeState::Ready(session) => Some(session),
                RuntimeState::RetireRequested { session, .. } => session,
                _ => None,
            };
        self.close(held);
        Ok(())
    }

    /// Run `hook` on the worker as it enters retirement, before its root check.
    #[cfg(feature = "test-support")]
    pub fn set_retirement_entered_hook_for_tests(&self, hook: impl FnOnce() + Send + 'static) {
        *self.retirement_entered_hook.lock().unwrap() = Some(Box::new(hook));
    }

    /// A requested retirement found accepted work: the runtime stays live.
    fn cancel_retirement(&self, mut state: std::sync::MutexGuard<'_, RuntimeState>) {
        if let RuntimeState::RetireRequested { session, .. } = &*state {
            *state = match session {
                Some(session) => RuntimeState::Ready(session.clone()),
                None => RuntimeState::Reconciling {
                    attempts: 0,
                    waiting: false,
                },
            };
        }
        drop(state);
        self.note_state_change();
        self.step_progress.store(true, Ordering::Release);
    }

    /// Close a runtime already in `Closing`. The writer session (and its
    /// index-use guard) is held until the watcher and SQLite witnesses are
    /// closed, so GC never sees the index unused mid-close.
    pub(super) fn close(&self, held: Option<Arc<WriterSession>>) {
        self.cancel_active();
        self.search_fill.lock().unwrap().take();
        self.architecture.0.shut_down();
        self.note_state_change();
        *self.watch_status.lock().unwrap() = None;
        *self.watcher_work.lock().unwrap() = None;
        self.watcher_health.reset();
        #[cfg(feature = "test-support")]
        if let Some(hook) = self.closing_hook.lock().unwrap().take() {
            hook();
        }
        self.store.retire_checkout_sqlite_witnesses();
        drop(held);
        self.store.end_closing();
        *self.runtime.lock().unwrap() = RuntimeState::Retired;
        self.note_state_change();
        if let Some(lifecycle) = self.lifecycle.get() {
            lifecycle();
        }
    }

    /// One attempt at accepted watcher intent up to the cutoff. Its outcome
    /// never blocks retirement: changes it misses, or that arrive after the
    /// cutoff, are captured by the next activation's startup reconciliation.
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
        let _ = watcher.reconcile_through_cutoff(
            &self.store,
            &session,
            &options,
            &self.watcher_cancel,
            cutoff,
        );
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
                .state_changed
                .wait_timeout(state, deadline - now)
                .unwrap()
                .0;
        }
        true
    }

    /// Bump the state version and wake state waiters (read readiness, close).
    pub(super) fn note_state_change(&self) {
        // Under the state lock, so a waiter that checked the version cannot
        // miss this notification.
        let _state = self.runtime.lock().unwrap();
        self.state_version.fetch_add(1, Ordering::AcqRel);
        self.state_changed.notify_all();
    }

    /// The runtime state version a waiter records before re-checking.
    pub fn state_version(&self) -> u64 {
        self.state_version.load(Ordering::Acquire)
    }

    /// Wait until the runtime state changes after `seen`, or `timeout` passes.
    pub fn wait_state_change(&self, seen: u64, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut state = self.runtime.lock().unwrap();
        while self.state_version.load(Ordering::Acquire) == seen {
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            state = self
                .state_changed
                .wait_timeout(state, deadline - now)
                .unwrap()
                .0;
        }
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

    /// The live count of admission leases, which outlives the runtime: a read
    /// admitted before close holds its lease until it finishes.
    pub(crate) fn lease_count(&self) -> Arc<std::sync::atomic::AtomicUsize> {
        Arc::clone(&self.leases)
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
    worker_loop_with_wait(weak, inbox.clone(), |deadline| inbox.wait(deadline));
}

fn worker_loop_with_wait(
    weak: std::sync::Weak<DaemonState>,
    inbox: Arc<WorkerInbox>,
    mut wait: impl FnMut(Option<Instant>),
) {
    let mut deadline = weak.upgrade().map(|state| state.worker_now());
    loop {
        wait(deadline);
        crate::index_coordinator::diagnostic_marker("-", "worker_wake", "");
        if inbox.shutdown.load(Ordering::Acquire) {
            return;
        }
        let Some(state) = weak.upgrade() else {
            return;
        };
        if matches!(*state.runtime.lock().unwrap(), RuntimeState::Retired) {
            return;
        }
        let step = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| state.worker_step()));
        if step.is_err() {
            state.publish_fifo_pending();
        }
        deadline = match step {
            Ok(Ok(())) => {
                if state.maintenance_ready() {
                    state.run_maintenance_unit();
                }
                state.next_wake()
            }
            Ok(Err(error)) => {
                super::best_effort_queue_stderr(
                    std::io::stderr(),
                    format_args!("checkout worker step failed: {error:#}\n"),
                );
                Some(state.failed_step_retry())
            }
            Err(_) => {
                super::best_effort_queue_stderr(
                    std::io::stderr(),
                    format_args!("checkout worker step failed: panicked\n"),
                );
                Some(state.failed_step_retry())
            }
        };
    }
}

#[cfg(test)]
mod recovery_tests;

#[cfg(test)]
mod missing_index_tests {
    #[test]
    fn purge_at_native_spool_admission_inside_step_never_recreates_index() {
        let (_tmp, store, state, options) =
            crate::http::exceptional_recovery_tests::serving_queue_watch_fixture();
        std::fs::write(
            options.workspace_root.join("new.js"),
            "function changed() {}\n",
        )
        .unwrap();
        // Inventory changes can publish locally; changed native configuration
        // keeps this purge at the full spool admission boundary.
        std::fs::write(
            options.workspace_root.join("package.json"),
            r#"{"type":"module"}"#,
        )
        .unwrap();
        store.enqueue_request(&options, None).unwrap();
        let index_dir = store.request_db_path().parent().unwrap().to_owned();
        let removed = index_dir.clone();
        let reached = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_reached = reached.clone();
        store.set_native_spool_hook(move || {
            hook_reached.store(true, std::sync::atomic::Ordering::Release);
            std::fs::remove_dir_all(removed).unwrap();
        });
        let _ = state.worker_step();
        assert!(
            reached.load(std::sync::atomic::Ordering::Acquire),
            "full capture did not reach spool admission"
        );
        assert!(
            !index_dir.exists(),
            "mid-step spool admission recreated the purged index"
        );
        state.publish_fifo_pending();
        state.worker_step().unwrap();
        assert!(state.retired());
        assert!(!index_dir.exists());
    }

    #[test]
    fn writer_admission_after_activation_never_recreates_purged_index() {
        for session in [false, true] {
            let (_tmp, store, _state, _options) =
                crate::http::exceptional_recovery_tests::serving_queue_watch_fixture();
            let index_dir = store.request_db_path().parent().unwrap().to_owned();
            std::fs::remove_dir_all(&index_dir).unwrap();
            if session {
                assert!(store.writer_session().is_err());
            } else {
                assert!(store.writer().is_err());
            }
            assert!(
                !index_dir.exists(),
                "writer admission reprovisioned the index"
            );
        }
    }

    use crate::http::exceptional_recovery_tests::fixture;
    #[cfg(feature = "test-support")]
    use crate::indexer::IndexOptions;

    #[cfg(feature = "test-support")]
    #[test]
    fn purge_between_worker_probe_and_existing_queue_open_does_not_recreate_index() {
        for enqueue in [false, true] {
            let (_tmp, store, state, roots, identity) = fixture();
            state.worker_step().unwrap();
            let index_dir = roots.index_dir(&identity);
            let removed = index_dir.clone();
            store.set_after_queue_root_verify_hook_for_tests(move || {
                std::fs::remove_dir_all(removed).unwrap();
            });
            if enqueue {
                assert!(
                    store
                        .enqueue_request(&IndexOptions::new(identity.root.clone()), None)
                        .is_err()
                );
            } else {
                let _ = state.worker_step();
            }
            assert!(
                !index_dir.exists(),
                "queue admission recreated purged index"
            );
            state.publish_fifo_pending();
            state.worker_step().unwrap();
            assert!(state.retired());
            assert!(!index_dir.exists());
        }
    }

    #[test]
    fn storage_busy_defers_without_authorizing_missing_index_close() {
        let (_tmp, store, state, roots, identity) = fixture();
        // Hold EX before startup creates a serving session's retained SH guard.
        let lock = roots
            .cache
            .join("indexes")
            .join(format!("{}.lock", identity.root_key));
        let exclusive =
            crate::store::topology::UseGuard::acquire_existing(&lock, true, true).unwrap();
        let error = store.earliest_unfinished_request().unwrap_err();
        assert!(
            error.is::<crate::store::topology::StorageBusy>(),
            "{error:#}"
        );
        assert!(!state.close_if_index_missing().unwrap());
        state.publish_fifo_pending();
        assert!(!state.retired());
        assert!(state.fifo_pending());
        drop(exclusive);
        state.worker_step().unwrap();
        state.publish_fifo_pending();
        assert!(!state.fifo_pending());
    }

    #[test]
    fn index_directory_inspection_error_stays_pending() {
        let (_tmp, store, state, roots, identity) = fixture();
        state.worker_step().unwrap();
        let indexes = roots.index_dir(&identity).parent().unwrap().to_owned();
        let moved = indexes.with_extension("retained");
        std::fs::rename(&indexes, &moved).unwrap();
        std::fs::write(&indexes, b"not a directory").unwrap();
        assert!(store.index_directory_missing().is_err());
        assert!(state.worker_step().is_err());
        state.publish_fifo_pending();
        assert!(!state.retired());
        assert!(state.fifo_pending());
        std::fs::remove_file(indexes).unwrap();
    }

    #[test]
    fn queue_or_lock_file_loss_is_not_missing_index_authority() {
        for remove_lock in [false, true] {
            let (_tmp, store, state, roots, identity) = fixture();
            state.worker_step().unwrap();
            let path = if remove_lock {
                roots
                    .cache
                    .join("indexes")
                    .join(format!("{}.lock", identity.root_key))
            } else {
                roots.requests_db(&identity)
            };
            std::fs::remove_file(path).unwrap();
            assert!(!store.index_directory_missing().unwrap());
            // The step may defer without error; neither outcome authorizes close.
            let _ = state.worker_step();
            state.publish_fifo_pending();
            assert!(!state.retired());
            assert!(state.fifo_pending());
        }
    }

    #[test]
    fn missing_index_waits_for_an_enqueue_already_in_flight() {
        let (_tmp, store, state, roots, identity) = fixture();
        state.worker_step().unwrap();
        let enqueue = store.enqueue_in_flight();
        let index_dir = roots.index_dir(&identity);
        std::fs::remove_dir_all(&index_dir).unwrap();
        state.worker_step().unwrap();
        state.publish_fifo_pending();
        assert!(!state.retired());
        assert!(state.fifo_pending());
        assert!(!index_dir.exists());
        drop(enqueue);
        state.worker_step().unwrap();
        state.publish_fifo_pending();
        assert!(state.retired());
        assert!(!state.fifo_pending());
        assert!(!index_dir.exists());
    }

    #[test]
    fn missing_index_directory_retires_without_recreating_the_queue() {
        for remove_root in [false, true] {
            let (_tmp, _store, state, roots, identity) = fixture();
            state.worker_step().unwrap();
            let index_dir = roots.index_dir(&identity);
            std::fs::remove_dir_all(&index_dir).unwrap();
            if remove_root {
                std::fs::remove_dir_all(&identity.root).unwrap();
            }
            let step = state.worker_step();
            state.publish_fifo_pending();
            assert!(state.retired(), "lost index vetoed retirement: {step:?}");
            assert!(!state.fifo_pending());
            assert!(!index_dir.exists(), "queue probe recreated a lost index");
        }
    }

    #[test]
    fn missing_index_directory_defers_close_until_the_admitted_read_finishes() {
        let (_tmp, _store, state, roots, identity) = fixture();
        state.worker_step().unwrap();
        let lease = state.admission_lease().unwrap();
        let index_dir = roots.index_dir(&identity);
        std::fs::remove_dir_all(&index_dir).unwrap();
        let step = state.worker_step();
        state.publish_fifo_pending();
        assert!(!state.retired(), "admitted read must fence a fresh runtime");
        assert!(
            !index_dir.exists(),
            "queue probe recreated a lost index: {step:?}"
        );
        drop(lease);
        state.worker_step().unwrap();
        state.publish_fifo_pending();
        assert!(state.retired());
        assert!(!state.fifo_pending());
        assert!(!index_dir.exists());
    }
}
