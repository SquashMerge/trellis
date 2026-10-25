//! Loopback-only HTTP API. Database and indexing work runs on blocking workers.
use crate::{
    auth::valid_token,
    dependencies::{Catalog, CatalogOptions},
    indexer::IndexOptions,
    model::*,
    store::Store,
};
use anyhow::Context;
use axum::{
    Json, Router,
    body::{Body, Bytes, to_bytes},
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
#[cfg(feature = "test-support")]
use axum::{extract::DefaultBodyLimit, routing::post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

mod config;
mod worker;
pub use config::BrowserConfig;
pub use worker::{AdmissionLease, RetireOutcome, RuntimePhase};
use worker::{RuntimeState, STARTUP_BACKOFF_FIRST};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexJob {
    pub id: String,
    pub state: String,
    pub progress: IndexProgress,
    pub revision: Option<IndexPin>,
    pub error: Option<Value>,
    pub submitted_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}
struct Jobs {
    current: Option<String>,
    jobs: BTreeMap<String, IndexJob>,
    cancel: CancelFlag,
}
struct DependencyIndex {
    generation: u64,
    cancel: Arc<AtomicBool>,
    stopped: bool,
    worker_running: bool,
    state: &'static str,
    catalog: Option<Arc<Catalog>>,
    warnings: Vec<String>,
}
impl DependencyIndex {
    /// Replace the pending generation, but admit at most one worker driver.
    fn request_build(&mut self) -> bool {
        if self.stopped {
            return false;
        }
        self.cancel.store(true, Ordering::Release);
        self.generation += 1;
        self.cancel = Arc::new(AtomicBool::new(false));
        self.state = "loading";
        self.catalog = None;
        self.warnings.clear();
        !std::mem::replace(&mut self.worker_running, true)
    }
}
#[derive(Debug)]
struct GcPriorityYield;
impl std::fmt::Display for GcPriorityYield {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("maintenance deferred for publication")
    }
}
impl std::error::Error for GcPriorityYield {}

#[derive(Default)]
struct MaintenanceTelemetry {
    preemptions: u64,
    busy_attempts: u64,
    successful_units: u64,
    deferred_since: Option<Instant>,
    max_deferred_ms: u128,
    max_deferred_age_s: u64,
}
/// One bounded notice per minute, even when a worker fails at every 20 ms tick.
#[derive(Default)]
struct MaintenanceErrorLimiter {
    last_notice: Option<Instant>,
}
impl MaintenanceErrorLimiter {
    fn permit(&mut self, now: Instant) -> bool {
        if self
            .last_notice
            .is_some_and(|last| now.saturating_duration_since(last) < Duration::from_secs(60))
        {
            return false;
        }
        self.last_notice = Some(now);
        true
    }
}

/// Never render arbitrary anyhow context, SQLite messages, source, or paths.
/// SQLite extended result codes and OS errno are numeric, bounded categories.
fn maintenance_error_class(error: &anyhow::Error) -> (&'static str, i32) {
    for cause in error.chain() {
        if let Some(sqlite) = cause.downcast_ref::<rusqlite::Error>() {
            return match sqlite {
                rusqlite::Error::SqliteFailure(code, _) => ("sqlite", code.extended_code),
                _ => ("sqlite", 0),
            };
        }
    }
    if error.is::<crate::store::SqliteContention>() {
        return ("sqlite_contention", 0);
    }
    if let Some(io) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<std::io::Error>())
    {
        return ("io", io.raw_os_error().unwrap_or(0));
    }
    ("other", 0)
}
fn maintenance_error_notice(category: &'static str, code: i32) -> String {
    format!("maintenance tick failed: category={category} code={code}\n")
}

impl MaintenanceTelemetry {
    fn observe_due_age(&mut self, age: Option<u64>) {
        if let Some(age) = age {
            self.max_deferred_age_s = self.max_deferred_age_s.max(age.saturating_sub(900));
        }
    }
    fn deferred(&mut self, now: Instant) -> (u128, u128) {
        self.preemptions = self.preemptions.saturating_add(1);
        let age = now
            .duration_since(*self.deferred_since.get_or_insert(now))
            .as_millis();
        self.max_deferred_ms = self.max_deferred_ms.max(age);
        (age, self.max_deferred_ms)
    }
    fn progressed(&mut self, now: Instant) -> (u128, u128) {
        self.successful_units = self.successful_units.saturating_add(1);
        let age = self
            .deferred_since
            .take()
            .map_or(0, |start| now.duration_since(start).as_millis());
        self.max_deferred_ms = self.max_deferred_ms.max(age);
        (age, self.max_deferred_ms)
    }
}

pub struct DaemonState {
    store: Store,
    /// Decision 0009 §2: the checkout's one runtime state, shared by the worker
    /// and every reader. `Ready` holds the only serving writer session.
    runtime: Mutex<RuntimeState>,
    /// The first error of the current startup reconciliation run, cleared when
    /// a validated head commits.
    startup_error: Mutex<Option<String>>,
    /// The next failed startup reconciliation waits this long (doubling, capped).
    startup_backoff: Mutex<Duration>,
    /// The current browser configuration (Decision 0009 §6), replaced whole.
    config: std::sync::RwLock<Arc<BrowserConfig>>,
    /// Explicit index options a new configuration brought, applied by the
    /// worker's next full watcher inventory and then recorded.
    explicit_pending: Mutex<Option<Arc<IndexOptions>>>,
    /// The worker's wake inbox. The queue waker, registered with every Store
    /// for this root, also marks FIFO work pending at the enqueue COMMIT; the
    /// watch waker only wakes.
    inbox: Arc<worker::WorkerInbox>,
    /// Owned here; the Store registry holds it weakly.
    _queue_waker: Arc<dyn Fn() + Send + Sync>,
    watch_waker: Arc<dyn Fn() + Send + Sync>,
    /// Hold the worker at the start of its next step.
    #[cfg(feature = "test-support")]
    worker_hold: std::sync::RwLock<()>,
    /// The last step made progress, so more work may be ready at once.
    step_progress: AtomicBool,
    /// Published watcher intent; readers never take the watcher's lock.
    watch_status: Mutex<Option<worker::WatchStatus>>,
    /// Read and enqueue admission leases held against release.
    leases: Arc<std::sync::atomic::AtomicUsize>,
    /// Published by the worker after each step (and set by the enqueue gate):
    /// accepted FIFO work keeps the runtime from retiring.
    fifo_pending: Arc<AtomicBool>,
    /// The registry's lifecycle wake (Decision 0009 §5).
    lifecycle: Arc<std::sync::OnceLock<Arc<dyn Fn() + Send + Sync>>>,
    /// The runtime closed because its captured root was lost.
    root_lost: AtomicBool,
    /// Every runtime state transition bumps this and signals `state_changed`.
    state_version: AtomicU64,
    state_changed: std::sync::Condvar,
    /// Pause retirement before its `Closing` linearization point, and after it.
    #[cfg(feature = "test-support")]
    retire_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(feature = "test-support")]
    retirement_entered_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(feature = "test-support")]
    closing_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs as a maintenance unit begins, after its priority check passed.
    #[cfg(feature = "test-support")]
    maintenance_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    dependencies: Mutex<DependencyIndex>,
    #[cfg(all(test, feature = "test-support"))]
    dependency_capture_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(all(test, feature = "test-support"))]
    outer_fence_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(feature = "test-support")]
    token: String,
    #[cfg(feature = "test-support")]
    hosts: Vec<String>,
    #[cfg(feature = "test-support")]
    origins: Vec<String>,
    jobs: Mutex<Jobs>,
    worker_started: AtomicBool,
    /// Watcher inventories run on the worker and are never cancelled from
    /// outside it (Decision 0009 §1).
    watcher_cancel: CancelFlag,
    #[cfg(feature = "test-support")]
    startup_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Startup reconciliation captures begun, for the single-owner tests.
    #[cfg(feature = "test-support")]
    startup_captures: AtomicU64,
    maintenance_telemetry: Mutex<MaintenanceTelemetry>,
    maintenance_error_limiter: Mutex<MaintenanceErrorLimiter>,
    #[cfg(test)]
    test_pending_read_failures: AtomicUsize,
    #[cfg(test)]
    test_activation_head_busy_once: AtomicBool,
    pending_requests: Mutex<Vec<String>>,
    job_progress: Mutex<BTreeMap<String, IndexProgress>>,
    /// One worker step at a time. In production only the worker takes it; test
    /// fixtures that drive steps by hand take it to serialize with the worker.
    step_lock: Mutex<()>,
    watcher_work: Mutex<
        Option<(
            std::sync::Weak<crate::store::topology::WriterSession>,
            crate::index_coordinator::WatcherWork,
        )>,
    >,
    #[cfg(feature = "test-support")]
    causal_witness: Mutex<Option<Arc<crate::daemon::causal_witness::CausalWitness>>>,
    #[cfg(feature = "test-support")]
    causal_runtime: Mutex<Option<std::sync::Weak<crate::daemon::registry::CheckoutRuntime>>>,
    #[cfg(feature = "test-support")]
    causal_lineage_ordinal: Arc<AtomicU64>,
    #[cfg(feature = "test-support")]
    causal_startup_reported: Mutex<Option<(uuid::Uuid, uuid::Uuid, u64)>>,
    recovery_retry_after: Mutex<Option<Instant>>,
    /// Watcher health, published by the current watcher through one flag that
    /// lives as long as this state. Readers never wait for watcher work.
    watcher_health: Arc<crate::watch::WatcherHealth>,
    retention_last_run: Mutex<Instant>,
    gc_last_check: Mutex<Instant>,
    #[cfg(test)]
    test_queue_before_stream: crate::store::TestOneShotHook,
    #[cfg(test)]
    test_queue_after_stream: crate::store::TestOneShotHook,
    #[cfg(test)]
    test_queue_after_pending_snapshot: crate::store::TestOneShotHook,
    #[cfg(test)]
    test_queue_after_startup_drain: crate::store::TestOneShotHook,
    #[cfg(test)]
    test_queue_before_preclaim: crate::store::TestOneShotHook,
    #[cfg(test)]
    test_queue_before_watcher_reconcile: crate::store::TestOneShotHook,
    #[cfg(test)]
    test_queue_after_watcher_reconcile: crate::store::TestOneShotHook,
    #[cfg(test)]
    test_queue_current_phase: std::sync::atomic::AtomicU8,
}
impl Drop for DaemonState {
    /// A lossless shutdown: an idle worker waiting without a deadline exits.
    fn drop(&mut self) {
        self.inbox.shut_down();
    }
}

/// Diagnostic logging must never kill the durable queue worker if the
/// launching parent closed its stderr pipe after the server banner.
fn best_effort_queue_stderr(mut output: impl std::io::Write, args: std::fmt::Arguments<'_>) {
    let _ = output.write_fmt(args);
}

pub fn new(
    store: Store,
    index_options: IndexOptions,
    token: String,
    address: SocketAddr,
) -> anyhow::Result<Arc<DaemonState>> {
    let browse_root = index_options.workspace_root.clone();
    new_with_browser_root(store, index_options, token, address, browse_root)
}
pub fn new_with_browser_root(
    store: Store,
    index_options: IndexOptions,
    token: String,
    address: SocketAddr,
    browse_root: std::path::PathBuf,
) -> anyhow::Result<Arc<DaemonState>> {
    new_with_source_roots(store, index_options, token, address, browse_root, vec![])
}
pub fn new_with_source_roots(
    store: Store,
    index_options: IndexOptions,
    token: String,
    address: SocketAddr,
    browse_root: std::path::PathBuf,
    source_roots: Vec<(String, std::path::PathBuf)>,
) -> anyhow::Result<Arc<DaemonState>> {
    new_with_dependency_options(
        store,
        index_options,
        token,
        address,
        browse_root,
        source_roots,
        None,
    )
}
/// Catalog roots are trusted daemon configuration, never request parameters.
pub fn new_with_dependency_options(
    store: Store,
    index_options: IndexOptions,
    token: String,
    address: SocketAddr,
    browse_root: std::path::PathBuf,
    source_roots: Vec<(String, std::path::PathBuf)>,
    catalog_options: Option<CatalogOptions>,
) -> anyhow::Result<Arc<DaemonState>> {
    let config = BrowserConfig::build(
        index_options,
        false,
        &browse_root,
        source_roots,
        catalog_options,
    )?;
    new_with_config(store, config, token, address)
}

/// A checkout scheduler with a prebuilt browser configuration.
pub fn new_with_config(
    store: Store,
    config: BrowserConfig,
    token: String,
    address: SocketAddr,
) -> anyhow::Result<Arc<DaemonState>> {
    anyhow::ensure!(
        address.ip().is_loopback() && address.port() != 0,
        "daemon requires a bound loopback address"
    );
    anyhow::ensure!(valid_token(&token), "invalid bearer token");
    #[cfg(feature = "test-support")]
    let hosts = vec![address.to_string(), format!("localhost:{}", address.port())];
    #[cfg(feature = "test-support")]
    let origins = hosts.iter().map(|h| format!("http://{h}")).collect();
    let inbox = Arc::new(worker::WorkerInbox::default());
    let fifo_pending = Arc::new(AtomicBool::new(false));
    let queue_waker: Arc<dyn Fn() + Send + Sync> = {
        let inbox = inbox.clone();
        let fifo_pending = fifo_pending.clone();
        Arc::new(move || {
            fifo_pending.store(true, Ordering::Release);
            inbox.wake();
        })
    };
    let watch_waker: Arc<dyn Fn() + Send + Sync> = {
        let inbox = inbox.clone();
        Arc::new(move || inbox.wake())
    };
    store.register_queue_waker(&queue_waker);
    Ok(Arc::new(DaemonState {
        store,
        inbox,
        _queue_waker: queue_waker,
        watch_waker,
        #[cfg(feature = "test-support")]
        worker_hold: std::sync::RwLock::new(()),
        step_progress: AtomicBool::new(false),
        watch_status: Mutex::new(None),
        leases: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        fifo_pending,
        lifecycle: Arc::new(std::sync::OnceLock::new()),
        root_lost: AtomicBool::new(false),
        state_version: AtomicU64::new(0),
        state_changed: std::sync::Condvar::new(),
        #[cfg(feature = "test-support")]
        retire_hook: Mutex::new(None),
        #[cfg(feature = "test-support")]
        retirement_entered_hook: Mutex::new(None),
        #[cfg(feature = "test-support")]
        closing_hook: Mutex::new(None),
        #[cfg(feature = "test-support")]
        maintenance_hook: Mutex::new(None),
        runtime: Mutex::new(RuntimeState::Activating),
        startup_error: Mutex::new(None),
        startup_backoff: Mutex::new(STARTUP_BACKOFF_FIRST),
        dependencies: Mutex::new(DependencyIndex {
            generation: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            stopped: false,
            worker_running: false,
            state: if config.dependency_options.is_some() {
                "loading"
            } else {
                "disabled"
            },
            catalog: None,
            warnings: Vec::new(),
        }),
        #[cfg(all(test, feature = "test-support"))]
        dependency_capture_hook: Mutex::new(None),
        #[cfg(all(test, feature = "test-support"))]
        outer_fence_hook: Mutex::new(None),
        #[cfg(feature = "test-support")]
        token,
        #[cfg(feature = "test-support")]
        hosts,
        #[cfg(feature = "test-support")]
        origins,
        config: std::sync::RwLock::new(Arc::new(config)),
        explicit_pending: Mutex::new(None),
        worker_started: AtomicBool::new(false),
        watcher_cancel: Arc::new(AtomicBool::new(false)),
        #[cfg(feature = "test-support")]
        startup_hook: Mutex::new(None),
        #[cfg(feature = "test-support")]
        startup_captures: AtomicU64::new(0),
        maintenance_telemetry: Mutex::new(MaintenanceTelemetry::default()),
        maintenance_error_limiter: Mutex::new(MaintenanceErrorLimiter::default()),
        #[cfg(test)]
        test_pending_read_failures: AtomicUsize::new(0),
        #[cfg(test)]
        test_activation_head_busy_once: AtomicBool::new(false),
        pending_requests: Mutex::new(Vec::new()),
        job_progress: Mutex::new(BTreeMap::new()),
        step_lock: Mutex::new(()),
        watcher_work: Mutex::new(None),
        #[cfg(feature = "test-support")]
        causal_witness: Mutex::new(None),
        #[cfg(feature = "test-support")]
        causal_runtime: Mutex::new(None),
        #[cfg(feature = "test-support")]
        causal_lineage_ordinal: Arc::new(AtomicU64::new(0)),
        #[cfg(feature = "test-support")]
        causal_startup_reported: Mutex::new(None),
        recovery_retry_after: Mutex::new(None),
        watcher_health: Arc::new(crate::watch::WatcherHealth::default()),
        retention_last_run: Mutex::new(Instant::now()),
        gc_last_check: Mutex::new(Instant::now() - Duration::from_secs(3600)),
        #[cfg(test)]
        test_queue_before_stream: crate::store::TestOneShotHook::default(),
        #[cfg(test)]
        test_queue_after_stream: crate::store::TestOneShotHook::default(),
        #[cfg(test)]
        test_queue_after_pending_snapshot: crate::store::TestOneShotHook::default(),
        #[cfg(test)]
        test_queue_after_startup_drain: crate::store::TestOneShotHook::default(),
        #[cfg(test)]
        test_queue_before_preclaim: crate::store::TestOneShotHook::default(),
        #[cfg(test)]
        test_queue_before_watcher_reconcile: crate::store::TestOneShotHook::default(),
        #[cfg(test)]
        test_queue_after_watcher_reconcile: crate::store::TestOneShotHook::default(),
        #[cfg(test)]
        test_queue_current_phase: std::sync::atomic::AtomicU8::new(0),
        jobs: Mutex::new(Jobs {
            current: None,
            jobs: BTreeMap::new(),
            cancel: Arc::new(AtomicBool::new(false)),
        }),
    }))
}
impl DaemonState {
    /// Register a test-only, read-only witness after this checkout activates.
    /// The reverse edge is weak: release may drop the runtime independently.
    #[cfg(feature = "test-support")]
    pub fn set_causal_witness_runtime(
        &self,
        reporter: Arc<crate::daemon::causal_witness::CausalWitness>,
        runtime: std::sync::Weak<crate::daemon::registry::CheckoutRuntime>,
    ) {
        *self.causal_witness.lock().unwrap() = Some(reporter);
        *self.causal_runtime.lock().unwrap() = Some(runtime);
    }

    /// Called only after a worker step returned and released its locks.
    #[cfg(feature = "test-support")]
    fn report_causal_startup_ready(self: &Arc<Self>) {
        let runtime = self.causal_runtime.lock().unwrap().clone();
        if let Some(runtime) = runtime.and_then(|runtime| runtime.upgrade()) {
            runtime.report_causal_startup_ready(self);
        }
    }

    #[cfg(feature = "test-support")]
    pub fn causal_lineage_for(
        &self,
        session: &Arc<crate::store::topology::WriterSession>,
    ) -> Option<crate::daemon::causal_witness::Lineage> {
        let serving = self.runtime.lock().unwrap();
        if !serving
            .session()
            .is_some_and(|owner| Arc::ptr_eq(owner, session))
        {
            return None;
        }
        let work = self.watcher_work.lock().unwrap();
        let (owner, scheduler) = work.as_ref()?;
        owner
            .upgrade()
            .filter(|owner| Arc::ptr_eq(owner, session))?;
        scheduler.causal_lineage()
    }

    #[cfg(feature = "test-support")]
    pub fn causal_startup_already_reported(
        &self,
        lineage: crate::daemon::causal_witness::Lineage,
    ) -> bool {
        self.causal_startup_reported.lock().unwrap().as_ref()
            == Some(&(
                lineage.owner_incarnation,
                lineage.watch_epoch,
                lineage.ordinal,
            ))
    }

    #[cfg(feature = "test-support")]
    pub fn emit_causal_startup_ready_once(
        &self,
        lineage: crate::daemon::causal_witness::Lineage,
        pin: IndexPin,
    ) {
        let Some(reporter) = self.causal_witness.lock().unwrap().clone() else {
            return;
        };
        let key = (
            lineage.owner_incarnation,
            lineage.watch_epoch,
            lineage.ordinal,
        );
        let mut reported = self.causal_startup_reported.lock().unwrap();
        if reported.as_ref() != Some(&key) {
            reporter.h_ready(lineage, pin);
            *reported = Some(key);
        }
    }

    pub fn retain_serving_session(
        self: &Arc<Self>,
        session: Arc<crate::store::topology::WriterSession>,
    ) {
        // Re-retention must not race an in-flight tick that sampled an old,
        // invalid holder and would replace this verified session afterward.
        {
            let _stream = self.step_lock.lock().unwrap();
            self.replace_serving_session(Some(session));
        }
        self.start_worker();
    }

    /// Offline CLI fixtures must drain worker lanes before inspecting SQLite.
    /// Production registry release instead uses the nonblocking try-release gate.
    #[doc(hidden)]
    pub fn release_checkout_runtime_for_fixture(&self) {
        let _stream = self.step_lock.lock().unwrap();
        self.set_runtime_state(RuntimeState::Retired);
        self.inbox.wake();
    }

    /// A daemon-local writer session is usable only after its startup reconciliation commits.
    /// A stale session cannot admit new claims or selected responses.
    pub fn checkout_serving_owner(&self) -> Option<Arc<crate::store::topology::WriterSession>> {
        let owner = self.runtime.lock().unwrap().session()?.clone();
        owner.verify().ok()?;
        Some(owner)
    }

    pub fn checkout_root_loss_retired(&self) -> bool {
        self.root_lost.load(Ordering::Acquire)
    }

    fn retired(&self) -> bool {
        matches!(*self.runtime.lock().unwrap(), RuntimeState::Retired)
    }

    /// Hold what a worker step holds mid-inventory (the step and the watcher),
    /// to show readers never wait for either.
    #[cfg(any(test, feature = "test-support"))]
    pub fn hold_worker_step_for_tests(&self, hold: impl FnOnce()) {
        let _step = self.step_lock.lock().unwrap();
        let _watcher = self.watcher_work.lock().unwrap();
        hold();
    }

    /// Watcher health as the current watcher published it. A sticky flag, not
    /// a sample: an inventory in flight never delays or refuses this read.
    pub fn checkout_watcher_degraded(&self) -> bool {
        self.watcher_health.degraded()
    }

    /// One coherent watcher-health witness, for measurement.
    pub fn checkout_watcher_witness(&self) -> crate::watch::WatcherWitness {
        self.watcher_health.witness()
    }

    /// A pending watcher hint (including the periodic inventory deadline) keeps
    /// the selected checkout in the catching-up state until the worker
    /// acknowledges it. Reads the published status only (#138 item 2): ingress
    /// increments the live generation before the worker drains it.
    pub fn checkout_watch_pending(&self) -> bool {
        match &*self.watch_status.lock().unwrap() {
            Some(status) => {
                status.other_intent
                    || status.generation.load(Ordering::SeqCst) != status.acknowledged
            }
            // A serving session's first watcher has not run its full inventory.
            None => self.runtime.lock().unwrap().session().is_some(),
        }
    }

    /// A deterministic fixture can submit an accepted source hint through the
    /// same bounded watcher ingress as notify, without relying on OS delivery.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn submit_watch_event_for_tests(&self, event: notify::Result<notify::Event>) -> bool {
        let work = self.watcher_work.lock().unwrap();
        let Some((_, scheduler)) = work.as_ref() else {
            return false;
        };
        scheduler.submit_watch_event_for_tests(event);
        true
    }

    /// Arm a queue-read race only after CLI enqueue has committed. Enqueue
    /// itself opens the queue, so arming earlier would pause the wrong read.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_queue_root_verify_after_enqueue_for_tests(
        &self,
        hook: impl FnOnce() + Send + 'static,
    ) {
        let store = self.store.clone();
        self.store.set_after_enqueue_commit_hook_for_tests(move || {
            store.set_after_queue_root_verify_hook_for_tests(hook);
        });
    }

    /// Offline snapshot fixtures opt out of the daemon's queue and maintenance ticks.
    /// See #111 for the separate read-during-reconciliation product fix.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn retain_serving_session_without_tick_for_tests(
        self: &Arc<Self>,
        session: Arc<crate::store::topology::WriterSession>,
    ) {
        self.replace_serving_session(Some(session));
    }
    /// Losing the serving session sends the runtime back to startup
    /// reconciliation (Decision 0009 §2); gaining one makes it Ready.
    fn replace_serving_session(&self, next: Option<Arc<crate::store::topology::WriterSession>>) {
        self.set_runtime_state(match next {
            Some(session) => RuntimeState::Ready(session),
            None => RuntimeState::Reconciling {
                attempts: 0,
                waiting: false,
            },
        });
    }

    /// The watcher belongs to one serving session: any session change drops it.
    fn set_runtime_state(&self, next: RuntimeState) {
        let mut current = self.runtime.lock().unwrap();
        let changed = match (current.session(), next.session()) {
            (Some(old), Some(next)) => !Arc::ptr_eq(old, next),
            (None, None) => false,
            _ => true,
        };
        if changed {
            let mut work = self.watcher_work.lock().unwrap();
            *work = None;
            *self.watch_status.lock().unwrap() = None;
            // The replaced watcher is gone; a new one publishes its own state.
            self.watcher_health.reset();
        }
        *current = next;
        drop(current);
        self.note_state_change();
    }
    /// Run one maintenance unit on the worker thread, after its higher-priority
    /// step (Decision 0009 §4), and report a failure at a bounded rate.
    pub(super) fn run_maintenance_unit(&self) {
        let _step = self.step_lock.lock().unwrap();
        let failure = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.maintenance_tick()
        })) {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(maintenance_error_class(&error)),
            Err(_) => Some(("worker_join", 0)),
        };
        if let Some((category, code)) = failure
            && self
                .maintenance_error_limiter
                .lock()
                .unwrap()
                .permit(Instant::now())
        {
            let notice = maintenance_error_notice(category, code);
            best_effort_queue_stderr(std::io::stderr(), format_args!("{notice}"));
        }
    }

    /// Why maintenance must yield now, from the worker's own published state
    /// (#138 item 3): it runs on the worker thread, so no lock is probed.
    fn maintenance_priority_reason(
        &self,
        session: &Arc<crate::store::topology::WriterSession>,
        _options: &IndexOptions,
    ) -> Option<&'static str> {
        if self.inbox.wake_pending() {
            return Some("wake_pending");
        }
        if self.store.root_path_replaced().unwrap_or(true) || session.verify().is_err() {
            return Some("root_or_writer");
        }
        if self.fifo_pending.load(Ordering::Acquire)
            || !self.pending_requests.lock().unwrap().is_empty()
        {
            return Some("local_fifo");
        }
        if self.store.has_recorded_completion(session).unwrap_or(true) {
            return Some("terminal_ack");
        }
        if !matches!(&*self.runtime.lock().unwrap(),
            RuntimeState::Ready(owner) if Arc::ptr_eq(owner, session))
        {
            return Some("session_replaced");
        }
        if self.checkout_watch_pending() {
            return Some("accepted_watcher");
        }
        None
    }

    fn maintenance_priority(
        &self,
        session: &Arc<crate::store::topology::WriterSession>,
        options: &IndexOptions,
    ) -> bool {
        self.maintenance_priority_reason(session, options).is_none()
    }

    /// Targeted indexed debt probe, only for explicitly enabled diagnostics.
    /// A BUSY/unknown read never blocks or changes foreground work.
    fn maintenance_oldest_age(
        &self,
        session: &Arc<crate::store::topology::WriterSession>,
    ) -> (String, Option<u64>) {
        if !crate::index_coordinator::diagnostics_enabled() {
            return ("disabled".into(), None);
        }
        let Ok(leader) = session.writer_guard() else {
            return ("unknown".into(), None);
        };
        match self.store.maintenance_oldest_due_age_secs(leader) {
            Ok(Some(age)) => (age.to_string(), Some(age)),
            Ok(None) => ("none_or_clock_held".into(), None),
            Err(_) => ("unknown".into(), None),
        }
    }

    fn maintenance_deferred_reason(
        &self,
        session: &Arc<crate::store::topology::WriterSession>,
        options: &IndexOptions,
    ) -> &'static str {
        if let Some(reason) = self.maintenance_priority_reason(session, options) {
            return reason;
        }
        match self.store.open_maintenance_queue_probe() {
            Ok(probe) => match probe.check() {
                crate::store::MaintenanceQueueState::Clear => "writer_or_gate_uncertain",
                crate::store::MaintenanceQueueState::Pending => "external_fifo",
                crate::store::MaintenanceQueueState::Unknown => "queue_probe_unknown",
            },
            Err(_) => "queue_probe_unknown",
        }
    }

    fn maintenance_deferred(
        &self,
        session: &Arc<crate::store::topology::WriterSession>,
        reason: &str,
    ) {
        let (oldest, measured_age) = self.maintenance_oldest_age(session);
        let mut stats = self.maintenance_telemetry.lock().unwrap();
        stats.busy_attempts = self.store.maintenance_sqlite_busy_attempts();
        stats.observe_due_age(measured_age);
        let (age, max_age) = stats.deferred(Instant::now());
        let details = format!(
            "reason={reason} preemptions={} busy_attempts={} successful_units={} deferred_ms={age} max_deferred_ms={max_age} max_deferred_age_s={} oldest_due_age_s={oldest}",
            stats.preemptions,
            stats.busy_attempts,
            stats.successful_units,
            stats.max_deferred_age_s
        );
        drop(stats);
        crate::index_coordinator::diagnostic_marker(
            &session.incarnation().to_string(),
            "deferred",
            &details,
        );
    }

    fn gc_tick(
        &self,
        session: &Arc<crate::store::topology::WriterSession>,
        options: &IndexOptions,
    ) -> anyhow::Result<()> {
        if self.gc_last_check.lock().unwrap().elapsed() < Duration::from_secs(3600)
            || !self.maintenance_priority(session, options)
        {
            return Ok(());
        }
        let probe = match self.store.open_maintenance_queue_probe() {
            Ok(probe) => probe,
            Err(_) => return Ok(()),
        };
        if probe.check() != crate::store::MaintenanceQueueState::Clear {
            return Ok(());
        }
        let Some(permit) = self.store.maintenance_try_enter() else {
            return Ok(());
        };
        drop(permit); // Never hold the gate across the whole directory scan.
        let Ok(leader) = session.writer_guard() else {
            return Ok(());
        };
        let mut priority = |stage| -> anyhow::Result<()> {
            if stage == crate::store::topology::GcStage::AfterFirstDbUnlink
                || stage == crate::store::topology::GcStage::AfterParentSync
            {
                // Once unlink begins, finish the candidate's guarded sequence.
                return Ok(());
            }
            let Some(unit) = self.store.maintenance_try_enter() else {
                return Err(anyhow::Error::new(GcPriorityYield));
            };
            let clear = self.maintenance_priority(session, options)
                && probe.check() == crate::store::MaintenanceQueueState::Clear;
            drop(unit);
            if !clear {
                return Err(anyhow::Error::new(GcPriorityYield));
            }
            Ok(())
        };
        let started = Instant::now();
        crate::index_coordinator::diagnostic_marker(
            &session.incarnation().to_string(),
            "start",
            "kind=gc",
        );
        let result = self.store.automatic_gc_cooperative(leader, &mut priority);
        let outcome = if result.is_ok() {
            "completed"
        } else if result
            .as_ref()
            .is_err_and(|error| error.is::<GcPriorityYield>())
        {
            "deferred"
        } else {
            "error"
        };
        crate::index_coordinator::diagnostic_marker(
            &session.incarnation().to_string(),
            "end",
            &format!(
                "kind=gc outcome={outcome} duration_us={}",
                started.elapsed().as_micros()
            ),
        );
        // A daily attempt stamp may already have committed. Any remaining
        // candidate waits until the next authorized daily attempt after yield.
        *self.gc_last_check.lock().unwrap() = Instant::now();
        match result {
            Ok(_) => Ok(()),
            Err(error) if error.is::<GcPriorityYield>() => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn maintenance_tick(&self) -> anyhow::Result<()> {
        if self.retired() {
            return Ok(());
        }
        use crate::store::MaintenanceOutcome;
        if self.retention_last_run.lock().unwrap().elapsed() < Duration::from_secs(60) {
            return Ok(());
        }
        let session = match &*self.runtime.lock().unwrap() {
            RuntimeState::Ready(session) => Some(session.clone()),
            _ => None,
        };
        let Some(session) = session else {
            return Ok(());
        };
        // Read selected options only outside a maintenance writer. A changed
        // watcher/options binding is intent until the worker replaces it.
        let options = match self.store.recorded_index_options() {
            Ok(Some(options)) => options,
            Ok(None) => self.config().options.clone(),
            Err(_) => return Ok(()),
        };
        if let Some(reason) = self.maintenance_priority_reason(&session, &options) {
            self.maintenance_deferred(&session, reason);
            *self.retention_last_run.lock().unwrap() =
                Instant::now() - Duration::from_secs(60) + Duration::from_millis(250);
            return Ok(());
        }
        let started = Instant::now();
        crate::index_coordinator::diagnostic_marker(
            &session.incarnation().to_string(),
            "start",
            &format!(
                "kind=retention oldest_due_age_s={}",
                self.maintenance_oldest_age(&session).0
            ),
        );
        #[cfg(feature = "test-support")]
        if let Some(hook) = self.maintenance_hook.lock().unwrap().take() {
            hook();
        }
        let result =
            crate::index_coordinator::cooperative_maintenance_unit(&self.store, &session, || {
                self.maintenance_priority(&session, &options)
            });
        let outcome = match &result {
            Ok(crate::store::MaintenanceOutcome::Progress) => "progress",
            Ok(crate::store::MaintenanceOutcome::Deferred) => "deferred",
            Ok(crate::store::MaintenanceOutcome::Idle) => "idle",
            Err(_) => "error",
        };
        crate::index_coordinator::diagnostic_marker(
            &session.incarnation().to_string(),
            "end",
            &format!(
                "kind=retention outcome={outcome} duration_us={}",
                started.elapsed().as_micros()
            ),
        );
        match result {
            Ok(MaintenanceOutcome::Progress) => {
                let (oldest, measured_age) = self.maintenance_oldest_age(&session);
                let mut stats = self.maintenance_telemetry.lock().unwrap();
                stats.busy_attempts = self.store.maintenance_sqlite_busy_attempts();
                if let Some(age) = measured_age {
                    stats.max_deferred_age_s =
                        stats.max_deferred_age_s.max(age.saturating_sub(900));
                }
                let (deferred, max_deferred) = stats.progressed(Instant::now());
                let details = format!(
                    "successful_units={} preemptions={} busy_attempts={} deferred_ms={deferred} max_deferred_ms={max_deferred} max_deferred_age_s={} oldest_due_age_s={oldest}",
                    stats.successful_units,
                    stats.preemptions,
                    stats.busy_attempts,
                    stats.max_deferred_age_s
                );
                drop(stats);
                crate::index_coordinator::diagnostic_marker(
                    &session.incarnation().to_string(),
                    "progress",
                    &details,
                );
                // Keep making one small unit at the next idle tick, after
                // re-arbitrating every accepted watcher and external FIFO.
            }
            Ok(MaintenanceOutcome::Deferred) => {
                let reason = self.maintenance_deferred_reason(&session, &options);
                // Deferred also covers a newly accepted queue row or an unknown
                // probe. Do not invent an exact SQLite BUSY count from it.
                self.maintenance_deferred(&session, reason);
                *self.retention_last_run.lock().unwrap() =
                    Instant::now() - Duration::from_secs(60) + Duration::from_millis(250);
            }
            Ok(MaintenanceOutcome::Idle) => {
                *self.retention_last_run.lock().unwrap() = Instant::now();
            }
            Err(error) => {
                *self.retention_last_run.lock().unwrap() = Instant::now();
                return Err(error);
            }
        }
        self.gc_tick(&session, &options)?;
        Ok(())
    }

    /// Exercise the independent low-priority maintenance lane at its deadline
    /// in integration tests, without waiting for wall-clock time.
    #[doc(hidden)]
    pub fn force_retention_idle_tick_for_tests(self: &Arc<Self>) -> anyhow::Result<()> {
        // A standalone fixture can retain an owner without running the native
        // stream. Verify/ack its initial full watcher inventory before claiming
        // a genuine idle maintenance opportunity.
        {
            let _stream = self.step_lock.lock().unwrap();
            let session = self.runtime.lock().unwrap().session().cloned();
            if let Some(session) = session {
                let options = self
                    .store
                    .recorded_index_options()?
                    .unwrap_or_else(|| self.config().options.clone());
                let mut work = self.watcher_work.lock().unwrap();
                if work.is_none() {
                    let mut scheduler = crate::index_coordinator::WatcherWork::publishing(
                        &self.store,
                        &session,
                        &options,
                        self.watcher_health.clone(),
                    )?;
                    // A late OS event from before this forced fixture tick must
                    // not defer pin expiration as accepted watcher work (#131).
                    scheduler.disable_native_watcher_for_tests();
                    scheduler.set_waker(self.watch_waker.clone());
                    let reconciled = scheduler.reconcile_due(
                        &self.store,
                        &session,
                        &options,
                        &Arc::new(AtomicBool::new(false)),
                        true,
                    );
                    self.publish_watch_status(&scheduler, &options);
                    reconciled?;
                    *work = Some((Arc::downgrade(&session), scheduler));
                }
            }
        }
        *self.retention_last_run.lock().unwrap() = Instant::now() - Duration::from_secs(60);
        // The worker is the only writer (Decision 0009 §1): serialize with it.
        let _step = self.step_lock.lock().unwrap();
        self.maintenance_tick()
    }

    /// Drive the serial root-loss transition in a standalone fixture without
    /// conflating it with the independent low-priority maintenance lane.
    #[doc(hidden)]
    pub fn force_root_transition_tick_for_tests(self: &Arc<Self>) -> anyhow::Result<()> {
        self.worker_step()
    }

    /// Inspect the three mutually exclusive roles at a root-loss tick boundary.
    #[doc(hidden)]
    pub fn root_loss_retirement_for_tests(&self) -> (bool, bool, bool) {
        (
            self.runtime.lock().unwrap().session().is_some(),
            self.watcher_work.lock().unwrap().is_some(),
            false,
        )
    }

    /// Whether accepted work is waiting for a standalone scheduler that was
    /// never asked to reconcile. Typed contention defers the decision.
    fn durable_work_pending(&self, pending_local: bool) -> anyhow::Result<Option<bool>> {
        if pending_local {
            return Ok(Some(true));
        }
        match self.startup_head() {
            Ok(head) => Ok(Some(head.is_some())),
            Err(error) if crate::store::transient_storage_contention(&error) => {
                *self.recovery_retry_after.lock().unwrap() =
                    Some(Instant::now() + Duration::from_millis(250));
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// The watcher's effective options: explicit options a new `serve`
    /// registration brought, else the FIFO head's persisted inputs, else the
    /// daemon defaults (Decision 0009 §6).
    fn selected_watch_options(
        &self,
        explicit: Option<&Arc<IndexOptions>>,
    ) -> anyhow::Result<IndexOptions> {
        Ok(match explicit {
            Some(options) => IndexOptions::clone(options),
            None => self
                .store
                .recorded_index_options()?
                .unwrap_or_else(|| self.config().options.clone()),
        })
    }

    // Store's cached terminal completion is settled before this callback runs.
    // The claim fence samples the gate again at the owning transition.
    fn reconcile_preclaim_gate(
        &self,
        session: &Arc<crate::store::topology::WriterSession>,
    ) -> anyhow::Result<bool> {
        let explicit = self.explicit_pending.lock().unwrap().clone();
        let selected = self.selected_watch_options(explicit.as_ref())?;
        // A registration here is unseen by this sample; the claim fence sees it.
        #[cfg(test)]
        self.test_queue_before_preclaim.run();
        let gate_due = self
            .watcher_work
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(owner, work)| {
                owner
                    .upgrade()
                    .is_some_and(|owner| Arc::ptr_eq(&owner, session))
                    && work.gate_pending(&selected)
            });
        if !gate_due {
            return Ok(true);
        }
        #[cfg(test)]
        self.test_queue_before_watcher_reconcile.run();
        let mut guard = self.watcher_work.lock().unwrap();
        let pending = if let Some((owner, work)) = guard.as_mut()
            && owner
                .upgrade()
                .is_some_and(|owner| Arc::ptr_eq(&owner, session))
            && work.gate_pending(&selected)
        {
            let accounted =
                work.reconcile_queue_gate(&self.store, session, &selected, &self.watcher_cancel)?;
            self.publish_watch_status(work, &selected);
            if accounted {
                self.finalize_watcher_gate(work, &selected, explicit.as_ref());
            }
            // The gate's attempt is spent; a failed one defers this claim to
            // the next step, which claims unless the options changed again.
            !accounted || work.gate_pending(&selected)
        } else {
            false
        };
        drop(guard);
        #[cfg(test)]
        self.test_queue_after_watcher_reconcile.run();
        Ok(!pending)
    }

    fn worker_step(self: &Arc<Self>) -> anyhow::Result<()> {
        #[cfg(feature = "test-support")]
        let _held = self.worker_hold.read().unwrap();
        if self.retired() {
            return Ok(());
        }
        #[cfg(test)]
        self.test_queue_before_stream.run();
        let _stream = self.step_lock.lock().unwrap();
        #[cfg(test)]
        self.test_queue_current_phase.store(1, Ordering::Release);
        if self.retired() {
            return Ok(());
        }
        #[cfg(test)]
        self.test_queue_after_stream.run();
        // Queue openers may provision a missing index directory. Observe its
        // loss first, while this worker owns the serial boundary.
        if self.close_if_index_missing()? {
            return Ok(());
        }
        // A lost captured root has no claim or publication authority. Resolve
        // its accepted queue rows before releasing local scheduler resources;
        // until that COMMIT is confirmed the runtime keeps them and makes no
        // other progress.
        if self.store.root_path_replaced()? {
            return self.close_for_root_loss();
        }
        // Retirement is due: the worker retires this runtime at this boundary
        // unless accepted FIFO work cancels it (Decision 0009 §5).
        if matches!(
            *self.runtime.lock().unwrap(),
            RuntimeState::RetireRequested { .. }
        ) {
            return self.run_retirement();
        }
        // Release the damaged file's writer before exclusive recreation, but keep
        // the watcher's actual options until a replacement session is admitted.
        if self.store.is_recreate_pending() && self.runtime.lock().unwrap().session().is_some() {
            *self.runtime.lock().unwrap() = RuntimeState::Reconciling {
                attempts: 0,
                waiting: false,
            };
            self.note_state_change();
        }
        if self
            .recovery_retry_after
            .lock()
            .unwrap()
            .is_some_and(|retry| Instant::now() < retry)
        {
            return Ok(());
        }
        if !self.store.is_ready_disposition() {
            let retained = self.runtime.lock().unwrap().session().cloned();
            if let Some(session) = retained {
                let options = self
                    .watcher_work
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|(_, watcher)| watcher.options());
                self.run_rebuild_reconciliation(session, options)?;
                if self.runtime.lock().unwrap().session().is_none() {
                    return Ok(());
                }
            }
        }
        let mut pending = self.pending_requests.lock().unwrap();
        pending.retain(|id| {
            #[cfg(test)]
            if self
                .test_pending_read_failures
                .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    (count > 0).then(|| count - 1)
                })
                .is_ok()
            {
                return true;
            }
            match self.store.request_by_id(id) {
                Ok(Some(row)) => row.finished_at.is_none(),
                Ok(None) | Err(_) => true,
            }
        });
        let pending_local = !pending.is_empty();
        drop(pending);
        #[cfg(test)]
        self.test_queue_after_pending_snapshot.run();
        // Startup reconciliation is the worker's first item: FIFO waits for it.
        if self.runtime.lock().unwrap().session().is_none() {
            if !self.startup_due(|| self.durable_work_pending(pending_local))? {
                return Ok(());
            }
            self.run_startup_reconciliation()?;
            if self.runtime.lock().unwrap().session().is_none() {
                return Ok(());
            }
        }
        let retained = self.runtime.lock().unwrap().session().cloned();
        if let Some(ref session) = retained
            && session.verify().is_ok()
        {
            #[cfg(test)]
            self.test_queue_current_phase.store(2, Ordering::Release);
            let gate_deferred = std::cell::Cell::new(false);
            let gate_error = std::cell::Cell::new(false);
            let processed = match crate::index_coordinator::drain_one_request_with_watcher(
                &self.store,
                session,
                &self.watcher_work,
                |id, p| {
                    self.job_progress.lock().unwrap().insert(id.to_owned(), p);
                },
                |claim| loop {
                    let claim_ready = self
                        .reconcile_preclaim_gate(session)
                        .inspect_err(|_| gate_error.set(true))?;
                    if !claim_ready {
                        gate_deferred.set(true);
                        return Ok(None);
                    }
                    let mut guard = self.watcher_work.lock().unwrap();
                    if let Some((owner, work)) = guard.as_mut()
                        && owner
                            .upgrade()
                            .is_some_and(|owner| Arc::ptr_eq(&owner, session))
                    {
                        // A `serve` registration publishes its options under
                        // this lock: hold it, and the callback's ingress lock,
                        // through claim_request's durable COMMIT. A registration
                        // is then either seen here or follows the claim. Each
                        // retry spends the gate's attempt or applies new options.
                        let explicit = self.explicit_pending.lock().unwrap();
                        let current = self
                            .selected_watch_options(explicit.as_ref())
                            .inspect_err(|_| gate_error.set(true))?;
                        if let Some(request) = work.claim_if_gate_clear(&current, claim)? {
                            return Ok(request);
                        }
                        continue;
                    }
                    return claim();
                },
            ) {
                Ok(processed) => processed,
                Err(error) => {
                    // A failed gate capture retains this watcher and its retry
                    // deadline; only request/completion failures retire the writer.
                    if gate_error.get() {
                        return Err(error);
                    }
                    // A cached terminal result must be resolved by this same
                    // incarnation. Otherwise release the stale writer so a
                    // successor can reclaim an unfinished running head.
                    if !self.store.has_recorded_completion(session)?
                        || !crate::index_coordinator::retryable_completion_error(&error)
                    {
                        self.replace_serving_session(None);
                    }
                    return Err(error);
                }
            };
            #[cfg(test)]
            self.test_queue_current_phase.store(3, Ordering::Release);
            if processed > 0 {
                self.start_dependency_index();
                self.step_progress.store(true, Ordering::Release);
            }
            #[cfg(test)]
            self.test_queue_after_startup_drain.run();
            // A verified drain normally consumes every FIFO head. If one is
            // still queued, the guarded publish-BUSY path deferred it. Avoid
            // recapturing the workspace every 20 ms under sustained readers.
            if processed == 0
                && !gate_deferred.get()
                && self
                    .store
                    .earliest_unfinished_request()
                    .is_ok_and(|head| head.is_some_and(|request| request.state == "queued"))
            {
                *self.recovery_retry_after.lock().unwrap() =
                    Some(Instant::now() + Duration::from_millis(250));
            }
            if (processed > 0 || self.store.earliest_unfinished_request()?.is_none())
                && !self.store.has_recorded_completion(session)?
            {
                // The idle watcher follows the selected head's persisted inputs.
                // Daemon defaults cannot silently supersede a FIFO head's options;
                // explicit options a new `serve` registration brought are applied
                // once, by the next full inventory (Decision 0009 §6).
                let explicit = self.explicit_pending.lock().unwrap().clone();
                let selected_options = self.selected_watch_options(explicit.as_ref())?;
                {
                    let mut work = self.watcher_work.lock().unwrap();
                    if !work.as_ref().is_some_and(|(owner, _)| {
                        owner
                            .upgrade()
                            .is_some_and(|owner| Arc::ptr_eq(&owner, session))
                    }) {
                        let mut scheduler = crate::index_coordinator::WatcherWork::publishing(
                            &self.store,
                            session,
                            &selected_options,
                            self.watcher_health.clone(),
                        )?;
                        #[cfg(feature = "test-support")]
                        if let Some(reporter) = self.causal_witness.lock().unwrap().clone() {
                            scheduler.attach_causal_witness(
                                reporter,
                                session.incarnation(),
                                self.causal_lineage_ordinal.clone(),
                            );
                        }
                        scheduler.set_waker(self.watch_waker.clone());
                        self.publish_watch_status(&scheduler, &selected_options);
                        *work = Some((Arc::downgrade(session), scheduler));
                    }
                    if let Some((_, scheduler)) = work.as_mut() {
                        let successor_waiting = self.store.earliest_unfinished_request()?.is_some();
                        if processed > 0 && successor_waiting {
                            scheduler.set_queue_gate();
                        } else if !successor_waiting {
                            // With no successor, a later idle edit uses ordinary debounce.
                            scheduler.clear_queue_gate();
                        }
                        #[cfg(test)]
                        self.test_queue_current_phase.store(4, Ordering::Release);
                        #[cfg(test)]
                        self.test_queue_before_watcher_reconcile.run();
                        let reconciled = if scheduler.gate_pending(&selected_options) {
                            scheduler.reconcile_queue_gate(
                                &self.store,
                                session,
                                &selected_options,
                                &self.watcher_cancel,
                            )
                        } else {
                            scheduler.reconcile_due(
                                &self.store,
                                session,
                                &selected_options,
                                &self.watcher_cancel,
                                false,
                            )
                        };
                        self.publish_watch_status(scheduler, &selected_options);
                        let accounted = reconciled?;
                        if accounted {
                            self.finalize_watcher_gate(
                                scheduler,
                                &selected_options,
                                explicit.as_ref(),
                            );
                        }
                        #[cfg(test)]
                        self.test_queue_current_phase.store(5, Ordering::Release);
                        #[cfg(test)]
                        self.test_queue_after_watcher_reconcile.run();
                        // ACK only a verified, acknowledged full inventory.
                        // Neither a failed FIFO probe nor a newer accepted hint
                        // can be called settled. The reporter only try_sends here.
                    }
                }
            }
            return Ok(());
        }
        Ok(())
    }
    fn finalize_watcher_gate(
        &self,
        work: &crate::index_coordinator::WatcherWork,
        selected: &IndexOptions,
        explicit: Option<&Arc<IndexOptions>>,
    ) {
        // A later registration must not be cleared by an older completed gate.
        if let Some(applied) = explicit {
            let mut pending = self.explicit_pending.lock().unwrap();
            if pending
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, applied))
            {
                *pending = None;
            }
        }
        #[cfg(not(feature = "test-support"))]
        let _ = (work, selected);
        #[cfg(feature = "test-support")]
        if let Some(reporter) = self.causal_witness.lock().unwrap().clone()
            && !work.accepted_watch_intent(selected)
            && matches!(self.store.earliest_unfinished_request(), Ok(None))
            && let (Some(lineage), Some(generation), Some(certified_pin)) = (
                work.causal_lineage(),
                work.accounted_watch_generation(),
                work.accounted_watch_pin(),
            )
            && let Ok(session) = self.retained_serving_session()
            && self.store.verify_writer_session(&session).is_ok()
            && self
                .store
                .index_baseline()
                .is_ok_and(|pin| pin == certified_pin)
            && self.store.verify_writer_session(&session).is_ok()
        {
            reporter.watch_ack(lineage, generation, certified_pin, true);
        }
    }

    pub fn retained_serving_session(
        &self,
    ) -> anyhow::Result<Arc<crate::store::topology::WriterSession>> {
        self.runtime
            .lock()
            .unwrap()
            .session()
            .cloned()
            .context("index_not_ready: no verified daemon serving session")
    }

    /// Start a replacement generation without doing filesystem or database work on the caller.
    /// A previous generation can finish, but can never publish over its replacement.
    pub fn start_dependency_index(self: &Arc<Self>) {
        let config = self.config();
        self.start_dependency_index_for(&config);
    }

    /// Schedule a catalog build if `config` is still the current version and
    /// enables the catalog. The check and the schedule share the catalog lock
    /// with a configuration swap's reset, so they cannot interleave with it.
    pub(crate) fn start_dependency_index_for(
        self: &Arc<Self>,
        config: &Arc<BrowserConfig>,
    ) -> bool {
        {
            let mut index = self.dependencies.lock().unwrap();
            if !Arc::ptr_eq(&self.config(), config) || config.dependency_options.is_none() {
                return false;
            }
            if !index.request_build() {
                return !index.stopped;
            }
        }
        let state = self.clone();
        tokio::spawn(async move {
            loop {
                let (generation, cancel) = {
                    let mut index = state.dependencies.lock().unwrap();
                    if index.stopped {
                        index.worker_running = false;
                        return;
                    }
                    (index.generation, index.cancel.clone())
                };
                let worker = state.clone();
                let worker_cancel = cancel.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let result = (|| {
                        // Each generation builds with the configuration current
                        // when it starts; a replaced one bumps the generation.
                        let config = worker.config();
                        let options = config
                            .dependency_options
                            .clone()
                            .ok_or_else(|| anyhow::anyhow!("dependency catalog disabled"))?;
                        let revision = worker.store.status()?.revision;
                        Catalog::build(
                            &config.options.workspace_root,
                            revision,
                            &options,
                            &worker_cancel,
                        )
                    })();
                    worker.publish_dependency_index(generation, &worker_cancel, result);
                })
                .await;
                if result.is_err() {
                    // Error publication does not access the database.
                    state.publish_dependency_index(
                        generation,
                        &cancel,
                        Err(anyhow::anyhow!("worker failed")),
                    );
                }
                let mut index = state.dependencies.lock().unwrap();
                if index.stopped || index.generation == generation {
                    index.worker_running = false;
                    return;
                }
                // Refresh bursts coalesce to the latest generation. Only this driver
                // can launch catalog work, and its previous blocking worker has exited.
            }
        });
        true
    }
    fn publish_dependency_index(
        &self,
        generation: u64,
        cancel: &AtomicBool,
        result: anyhow::Result<Catalog>,
    ) {
        // Called on a blocking worker for successful builds. No parsing, source I/O,
        // or database work while holding the catalog lock.
        let result = result.and_then(|catalog| {
            anyhow::ensure!(
                self.store.status()?.revision == catalog.workspace_revision,
                "revision conflict"
            );
            Ok(catalog)
        });
        let mut index = self.dependencies.lock().unwrap();
        if index.stopped || index.generation != generation || cancel.load(Ordering::Acquire) {
            return;
        }
        match result {
            Ok(catalog) => {
                index.state = "ready";
                index.catalog = Some(Arc::new(catalog));
            }
            Err(_) => {
                index.state = "failed";
                index.catalog = None;
                index.warnings = vec![
                    "Dependency catalog build failed or workspace changed; refresh to retry".into(),
                ];
            }
        }
    }
    /// Presentation-only snapshot. The caller must use a matching cached workspace revision.
    pub(crate) fn catalog_snapshot(&self, revision: IndexPin) -> Option<Arc<Catalog>> {
        self.dependencies
            .lock()
            .unwrap()
            .catalog
            .as_ref()
            .filter(|catalog| catalog.workspace_revision == revision)
            .cloned()
    }
    pub fn cancel_active(&self) {
        {
            let mut dependencies = self.dependencies.lock().unwrap();
            dependencies.stopped = true;
            dependencies.cancel.store(true, Ordering::Release);
            dependencies.generation += 1;
            dependencies.catalog = None;
            if self.config().dependency_options.is_some() {
                dependencies.state = "failed";
                dependencies.warnings =
                    vec!["Dependency indexing cancelled during shutdown".into()];
            }
        }
        self.jobs
            .lock()
            .unwrap()
            .cancel
            .store(true, Ordering::Release);
    }
}
fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"error":{"code":code,"message":message}})),
    )
        .into_response()
}
#[derive(Debug)]
struct ApiError(StatusCode, &'static str, &'static str);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        error(self.0, self.1, self.2)
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        // IOERR is broken SQLite locking/storage, not ordinary BUSY. Even an
        // outer storage_busy context must never mask an IOERR_RDLOCK as 409.
        if e.chain().any(|cause| cause.downcast_ref::<rusqlite::Error>().is_some_and(|error| {
            matches!(error, rusqlite::Error::SqliteFailure(info, _) if info.code == rusqlite::ErrorCode::SystemIoFailure)
        })) {
            return Self(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", "Operation failed");
        }
        if e.chain().any(|cause| cause.downcast_ref::<rusqlite::Error>().is_some_and(|error| {
            matches!(error, rusqlite::Error::SqliteFailure(info, _) if matches!(info.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
        })) {
            return Self(StatusCode::CONFLICT, "storage_busy", "Storage is busy");
        }
        // A spilled large publication holds every reader out until COMMIT;
        // say so at once instead of a generic busy (#210).
        if e.chain().any(|cause| {
            cause
                .downcast_ref::<crate::store::RebuildInProgress>()
                .is_some()
        }) {
            return Self(
                StatusCode::SERVICE_UNAVAILABLE,
                "rebuild_in_progress",
                "The index is being rebuilt; retry after it finishes",
            );
        }
        if let Some(directory) = e.downcast_ref::<BrowseDirectoryError>() {
            return browse_directory_error(&directory.0);
        }
        if let Some(invalid) = e.downcast_ref::<crate::navigation::InvalidRequest>() {
            Self(
                StatusCode::BAD_REQUEST,
                "invalid_navigation_request",
                invalid.0,
            )
        } else if e
            .downcast_ref::<crate::files_view::InvalidRequest>()
            .is_some()
        {
            invalid()
        } else if let Some(invalid) = e.downcast_ref::<crate::class_diagram::InvalidRequest>() {
            Self(StatusCode::BAD_REQUEST, "invalid_class_request", invalid.0)
        } else if e
            .chain()
            .any(|cause| cause.downcast_ref::<crate::store::PinExpired>().is_some())
        {
            Self(StatusCode::CONFLICT, "pin_expired", "The index pin expired")
        } else if e
            .chain()
            .any(|cause| cause.to_string().starts_with("revision conflict"))
        {
            Self(
                StatusCode::CONFLICT,
                "revision_conflict",
                "The index revision changed",
            )
        } else if e
            .chain()
            .any(|cause| cause.to_string() == "native declaration target missing")
        {
            invalid()
        } else {
            // Context must not hide an exact allowlisted refusal from the public API.
            for cause in e.chain() {
                let text = cause.to_string();
                for (prefix, status, code) in [
                    ("root_changed", StatusCode::CONFLICT, "root_changed"),
                    (
                        "root_key_collision",
                        StatusCode::SERVICE_UNAVAILABLE,
                        "root_key_collision",
                    ),
                    (
                        "workspace_id_changed",
                        StatusCode::CONFLICT,
                        "workspace_id_changed",
                    ),
                    ("storage_busy", StatusCode::CONFLICT, "storage_busy"),
                    (
                        "index_not_ready",
                        StatusCode::SERVICE_UNAVAILABLE,
                        "index_not_ready",
                    ),
                    (
                        "incompatible_index",
                        StatusCode::SERVICE_UNAVAILABLE,
                        "incompatible_index",
                    ),
                    (
                        "recovery_required",
                        StatusCode::SERVICE_UNAVAILABLE,
                        "recovery_required",
                    ),
                    (
                        "unsafe_index",
                        StatusCode::SERVICE_UNAVAILABLE,
                        "unsafe_index",
                    ),
                    ("not_found", StatusCode::NOT_FOUND, "not_found"),
                    (
                        "invalid_source_syntax",
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "invalid_source_syntax",
                    ),
                    (
                        "deadline_exceeded",
                        StatusCode::REQUEST_TIMEOUT,
                        "deadline_exceeded",
                    ),
                    ("cancelled", StatusCode::REQUEST_TIMEOUT, "cancelled"),
                    (
                        "bounded_work",
                        StatusCode::SERVICE_UNAVAILABLE,
                        "bounded_work",
                    ),
                    (
                        "response_too_large",
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "response_too_large",
                    ),
                    ("corrupt", StatusCode::SERVICE_UNAVAILABLE, "corrupt"),
                ] {
                    if text.starts_with(prefix) {
                        return Self(status, code, "Storage is unavailable");
                    }
                }
            }
            Self(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Operation failed",
            )
        }
    }
}
fn invalid() -> ApiError {
    ApiError(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        "Invalid request",
    )
}
fn missing() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not_found", "Not found")
}
async fn db<T: Send + 'static>(
    s: Arc<DaemonState>,
    f: impl FnOnce(&Store) -> anyhow::Result<T> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(move || f(&s.store))
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Operation failed",
            )
        })?
        .map_err(Into::into)
}
#[cfg(feature = "test-support")]
async fn guard(State(s): State<Arc<DaemonState>>, req: Request, next: Next) -> Response {
    guard_common(&s.token, &s.hosts, &s.origins, req, next).await
}

async fn guard_common(
    token: &str,
    hosts: &[String],
    origins: &[String],
    mut req: Request,
    next: Next,
) -> Response {
    let headers = req.headers();
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    let origin = headers.get(header::ORIGIN);
    let mut response = if headers.get_all(header::HOST).iter().count() != 1
        || !host.is_some_and(|h| hosts.iter().any(|v| v == h))
    {
        error(StatusCode::FORBIDDEN, "invalid_host", "Host is not allowed")
    } else if headers.get_all(header::ORIGIN).iter().count() > 1
        || origin.is_some_and(|h| {
            !h.to_str()
                .ok()
                .is_some_and(|v| origins.iter().any(|o| o == v))
        })
    {
        error(
            StatusCode::FORBIDDEN,
            "invalid_origin",
            "Origin is not allowed",
        )
    } else if (req.uri().path() == "/api" || req.uri().path().starts_with("/api/"))
        && (headers.get_all(header::AUTHORIZATION).iter().count() != 1
            || !headers
                .get(header::AUTHORIZATION)
                .and_then(|h| h.to_str().ok())
                .and_then(|h| h.strip_prefix("Bearer "))
                .is_some_and(|t| bool::from(t.as_bytes().ct_eq(token.as_bytes()))))
    {
        error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Bearer authentication required",
        )
    } else {
        let body = std::mem::replace(req.body_mut(), Body::empty());
        match to_bytes(body, 1024 * 1024).await {
            Ok(bytes) => {
                *req.body_mut() = Body::from(bytes);
                next.run(req).await
            }
            Err(_) => error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_too_large",
                "Request body exceeds limit",
            ),
        }
    };
    if response.status().is_client_error()
        && !response
            .headers()
            .get(header::CONTENT_TYPE)
            .is_some_and(|v| v == "application/json")
    {
        response = error(response.status(), "invalid_request", "Invalid request");
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response.headers_mut().insert(header::CONTENT_SECURITY_POLICY,"default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; font-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
}
#[cfg(feature = "test-support")]
pub fn router(state: Arc<DaemonState>) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    include_str!("../web/index.html"),
                )
            }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../web/app.js"),
                )
            }),
        )
        .route(
            "/viewer-metrics.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../web/viewer-metrics.js"),
                )
            }),
        )
        .route(
            "/files.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../web/files.js"),
                )
            }),
        )
        .route(
            "/files.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../web/files.css"),
                )
            }),
        )
        .route(
            "/shell.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../web/shell.js"),
                )
            }),
        )
        .route(
            "/fonts/jetbrains-mono-latin.woff2",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "font/woff2")],
                    &include_bytes!("../web/fonts/jetbrains-mono-latin.woff2")[..],
                )
            }),
        )
        .route(
            "/fonts/inter-variable.woff2",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "font/woff2")],
                    &include_bytes!("../web/fonts/inter-variable.woff2")[..],
                )
            }),
        )
        .route(
            "/fonts/manrope-variable.woff2",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "font/woff2")],
                    &include_bytes!("../web/fonts/manrope-variable.woff2")[..],
                )
            }),
        )
        .route(
            "/fonts/JetBrainsMono-OFL.txt",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    include_str!("../web/fonts/JetBrainsMono-OFL.txt"),
                )
            }),
        )
        .route(
            "/fonts/Inter-OFL.txt",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    include_str!("../web/fonts/Inter-OFL.txt"),
                )
            }),
        )
        .route(
            "/fonts/Manrope-OFL.txt",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    include_str!("../web/fonts/Manrope-OFL.txt"),
                )
            }),
        )
        .route(
            "/brand/trellis-mark-transparent-v1.png",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "image/png")],
                    &include_bytes!("../web/brand/trellis-mark-transparent-v1.png")[..],
                )
            }),
        )
        .route(
            "/brand/trellis-logo-dark-v1.webp",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "image/webp")],
                    &include_bytes!("../web/brand/trellis-logo-dark-v1.webp")[..],
                )
            }),
        )
        .route(
            "/style.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../web/style.css"),
                )
            }),
        )
        .route(
            "/healthz",
            get(|| async { Json(json!({"ok":true,"version":env!("CARGO_PKG_VERSION")})) }),
        )
        .route("/favicon.ico", get(|| async { StatusCode::NO_CONTENT }))
        .route("/api/status", get(status))
        .route("/api/index", post(start_index))
        .route("/api/jobs/current", get(current_job))
        .route("/api/jobs/{id}", get(job))
        .route("/api/jobs/{id}/cancel", post(cancel_job))
        .route("/api/dependencies", get(dependency_status))
        .route("/api/dependencies/refresh", post(dependency_refresh))
        .route("/api/dependencies/symbols", get(dependency_symbols))
        .route("/api/dependencies/source", get(dependency_source))
        .route("/api/rust-sources", get(rust_source_roots))
        .route("/api/rust-sources/tree", get(rust_source_tree))
        .route("/api/rust-sources/file", get(rust_source_file))
        .route("/api/tree", get(tree))
        .route("/api/files", get(files))
        .route("/api/methods", get(methods))
        .route("/api/symbols", get(file_symbols))
        .route("/api/syntax-matches", post(syntax_matches))
        .route("/api/sequence", post(sequence))
        .route("/api/classes", get(classes))
        .route("/api/class-diagram", post(class_diagram))
        .route("/api/navigation", post(navigation))
        .route(
            "/classes.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../web/classes.js"),
                )
            }),
        )
        .route(
            "/classes.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../web/classes.css"),
                )
            }),
        )
        .route(
            "/navigation.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../web/navigation.js"),
                )
            }),
        )
        .route(
            "/navigation.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../web/navigation.css"),
                )
            }),
        )
        .route(
            "/sequence.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../web/sequence.js"),
                )
            }),
        )
        .route(
            "/sequence-pele.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../web/sequence-pele.js"),
                )
            }),
        )
        .route(
            "/vendor/pele-1.10.0-trellis.1.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_bytes!("../web/vendor/pele-1.10.0-trellis.1.js"),
                )
            }),
        )
        .route("/api/source", get(source))
        .fallback(|| async { missing().into_response() })
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            evidence_response_guard,
        ))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}
// Only endpoints that include a native snapshot need this outer response fence.
// External-source-only routes keep their independent policy.
#[cfg(feature = "test-support")]
fn native_response_route(path: &str) -> bool {
    matches!(
        path,
        "/api/status"
            | "/api/tree"
            | "/api/files"
            | "/api/methods"
            | "/api/symbols"
            | "/api/syntax-matches"
            | "/api/sequence"
            | "/api/classes"
            | "/api/class-diagram"
            | "/api/navigation"
            | "/api/source"
            | "/api/dependencies"
            | "/api/dependencies/symbols"
            | "/api/dependencies/source"
            | "/api/rust-sources/tree"
    )
}
#[cfg(feature = "test-support")]
async fn evidence_response_guard(
    State(state): State<Arc<DaemonState>>,
    request: Request,
    next: Next,
) -> Response {
    if !native_response_route(request.uri().path()) {
        return next.run(request).await;
    }
    // The snapshot is short: retain its protected writer observation through
    // serialization. An ordinary same-incarnation publish does not reject an
    // already-materialized coherent unpinned response.
    let admitted = db(state.clone(), |store| {
        let snapshot = store.evidence_response()?;
        Ok(snapshot.into_fence(crate::store::EvidenceFencePolicy::T03))
    })
    .await;
    let response = next.run(request).await;
    if !response.status().is_success() {
        // Preserve validation/materialization error precedence, but still check T03.
        if let Ok(fence) = admitted {
            let _ = fence.finish(());
        }
        return response;
    }
    let fence = match admitted {
        Ok(fence) => fence,
        Err(error) => return error.into_response(),
    };
    #[cfg(test)]
    if let Some(hook) = state.outer_fence_hook.lock().unwrap().as_ref() {
        hook();
    }
    match fence.finish(response) {
        Ok(response) => response,
        Err(error) => ApiError::from(error).into_response(),
    }
}
fn dependency_stale() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "stale_catalog",
        "Dependency catalog or source changed; refresh the catalog",
    )
}
#[cfg(feature = "test-support")]
async fn dependency_work<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "dependency_error",
            "Dependency worker failed",
        )
    })?
}
impl DaemonState {
    #[cfg(feature = "test-support")]
    fn requested_catalog(&self, id: &str) -> Result<Arc<Catalog>, ApiError> {
        let revision = self.store.status()?.revision;
        self.catalog_snapshot(revision)
            .filter(|catalog| catalog.id == id)
            .ok_or_else(dependency_stale)
    }
    #[cfg(feature = "test-support")]
    fn check_catalog(&self, catalog: &Arc<Catalog>) -> Result<(), ApiError> {
        let current = self.requested_catalog(&catalog.id)?;
        if !Arc::ptr_eq(&current, catalog) {
            return Err(dependency_stale());
        }
        Ok(())
    }
}
#[cfg(feature = "test-support")]
async fn dependency_status(State(s): State<Arc<DaemonState>>) -> Result<Response, ApiError> {
    dependency_work(move || dependency_status_response(&s, || {}).map_err(ApiError::from)).await
}
#[cfg(feature = "test-support")]
fn dependency_status_response(
    s: &DaemonState,
    after_capture: impl FnOnce(),
) -> anyhow::Result<Response> {
    let response = s.store.evidence_response()?;
    let revision = response.status()?.revision;
    after_capture();
    #[cfg(test)]
    if let Some(hook) = s.dependency_capture_hook.lock().unwrap().as_ref() {
        hook();
    }
    let (state, catalog, mut warnings) = {
        let index = s.dependencies.lock().unwrap();
        (index.state, index.catalog.clone(), index.warnings.clone())
    };
    let payload = if let Some(catalog) = catalog {
        if catalog.workspace_revision == revision {
            json!({"state":state,"workspaceRevision":revision,
                "catalogId":catalog.id,"packages":catalog.packages,
                "symbolCount":catalog.symbols.len(),"warnings":catalog.warnings})
        } else {
            warnings.push("Workspace changed; refresh the dependency catalog".into());
            json!({"state":"failed","workspaceRevision":revision,
                "catalogId":null,"packages":[],"symbolCount":0,"warnings":warnings})
        }
    } else {
        json!({"state":state,"workspaceRevision":revision,
            "catalogId":null,"packages":[],"symbolCount":0,"warnings":warnings})
    };
    // IntoResponse serializes the owned JSON before the final T03 check. No
    // evidence body can escape on a failed fence, including the failed-catalog branch.
    let result = Json(payload).into_response();
    response.finish(result)
}
#[cfg(feature = "test-support")]
async fn dependency_refresh(
    State(s): State<Arc<DaemonState>>,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let body: Value = serde_json::from_slice(&body).map_err(|_| invalid())?;
    if !body.as_object().is_some_and(|object| object.is_empty()) {
        return Err(invalid());
    }
    if s.config().dependency_options.is_none() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "dependencies_disabled",
            "Dependency catalog is disabled",
        ));
    }
    if s.dependencies.lock().unwrap().stopped {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down",
            "Daemon is shutting down",
        ));
    }
    s.start_dependency_index();
    Ok((StatusCode::ACCEPTED, Json(json!({"state":"loading"}))))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DependencySymbolsQuery {
    catalog_id: String,
    package_id: Option<String>,
    #[serde(default)]
    q: String,
    #[serde(default)]
    offset: usize,
    #[serde(default = "dependency_limit")]
    limit: usize,
}
fn dependency_limit() -> usize {
    100
}
#[cfg(feature = "test-support")]
async fn dependency_symbols(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<DependencySymbolsQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(q) = query.map_err(|_| invalid())?;
    if q.catalog_id.is_empty()
        || q.catalog_id.len() > 8192
        || q.q.len() > 8192
        || q.package_id.as_ref().is_some_and(|id| id.len() > 8192)
        || !(1..=200).contains(&q.limit)
        || q.offset > 50_000
    {
        return Err(invalid());
    }
    dependency_work(move || {
        let catalog = s.requested_catalog(&q.catalog_id)?;
        if q.package_id
            .as_ref()
            .is_some_and(|id| !catalog.packages.iter().any(|p| &p.id == id))
        {
            return Err(missing());
        }
        let search = q.q.to_lowercase();
        let mut matches = catalog
            .symbols
            .iter()
            .filter(|symbol| {
                q.package_id
                    .as_ref()
                    .is_none_or(|id| &symbol.package_id == id)
                    && (search.is_empty()
                        || symbol.qualified_name.to_lowercase().contains(&search)
                        || symbol.name.to_lowercase().contains(&search))
            })
            .skip(q.offset);
        let items: Vec<_> = matches.by_ref().take(q.limit).collect();
        let next_offset = matches.next().map(|_| q.offset + items.len());
        let response = Json(
            json!({"catalogId":catalog.id,"workspaceRevision":catalog.workspace_revision,
            "items":items,"nextOffset":next_offset}),
        );
        s.check_catalog(&catalog)?;
        Ok(response)
    })
    .await
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DependencySourceQuery {
    catalog_id: String,
    source_ref: String,
}
#[cfg(feature = "test-support")]
async fn dependency_source(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<DependencySourceQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(q) = query.map_err(|_| invalid())?;
    if q.catalog_id.is_empty()
        || q.catalog_id.len() > 8192
        || q.source_ref.is_empty()
        || q.source_ref.len() > 8192
    {
        return Err(invalid());
    }
    dependency_work(move || {
        let catalog = s.requested_catalog(&q.catalog_id)?;
        let source = catalog.sources.get(&q.source_ref).ok_or_else(missing)?;
        // This descriptor is retained from discovery. No user-selected paths or reopened roots.
        let text = source.directory.read_file(&source.path).map_err(rust_source_error)?;
        let hash = hex::encode(Sha256::digest(text.as_bytes()));
        if hash != source.hash { return Err(dependency_stale()); }
        let package = catalog.packages.iter().find(|package| package.id == source.package_id).ok_or_else(missing)?;
        let definitions: Vec<_> = catalog.symbols.iter().filter(|symbol| symbol.source_ref == q.source_ref)
            .map(|symbol| json!({"id":symbol.id,"name":symbol.name,"kind":symbol.kind,
                "parent":symbol.parent,"path":symbol.path,"range":symbol.range})).collect();
        let file = SourceFile { path: source.path.clone(), hash: hash.clone(), language: "rust".into(), text };
        let response = Json(json!({"id":q.source_ref,"rootId":package.id,"rootLabel":package.name,
            "path":source.path,"hash":hash,"file":file,"definitions":definitions,
            "warnings":["Definitional candidates only; not confirmed callees. Separate from the workspace graph and source-sharing scope."]}));
        s.check_catalog(&catalog)?;
        Ok(response)
    }).await
}

#[cfg(feature = "test-support")]
async fn rust_source_roots(State(s): State<Arc<DaemonState>>) -> Json<Value> {
    Json(
        json!({"roots": s.config().rust_sources.iter().map(|r| json!({"id": r.label, "label": r.label, "path": r.directory.root.to_string_lossy()})).collect::<Vec<_>>()}),
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RustTreeQuery {
    root: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    offset: usize,
    #[serde(default = "file_limit")]
    limit: usize,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RustFileQuery {
    root: String,
    path: String,
}
fn rust_source_error(e: std::io::Error) -> ApiError {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::NotFound => missing(),
        ErrorKind::FileTooLarge => ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "source_too_large",
            "Source exceeds 2 MiB limit",
        ),
        ErrorKind::PermissionDenied => ApiError(
            StatusCode::FORBIDDEN,
            "source_forbidden",
            "Source access is not allowed",
        ),
        ErrorKind::InvalidInput | ErrorKind::InvalidData | ErrorKind::NotADirectory => {
            browse_invalid()
        }
        _ if e.raw_os_error() == Some(libc::ELOOP) => ApiError(
            StatusCode::FORBIDDEN,
            "symlink_forbidden",
            "Symlink traversal is not allowed",
        ),
        _ => ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "source_error",
            "Source could not be read",
        ),
    }
}
#[cfg(feature = "test-support")]
async fn rust_source_tree(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<RustTreeQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<crate::file_tree::Page>, ApiError> {
    let Query(q) = query.map_err(|_| invalid())?;
    if !crate::file_tree::valid_path(&q.path)
        || !(1..=200).contains(&q.limit)
        || q.offset > crate::file_tree::SCAN_LIMIT
    {
        return Err(browse_invalid());
    }
    let revision = db(s.clone(), |s| s.status()).await?.revision;
    tokio::task::spawn_blocking(move || {
        let config = s.config();
        let root = config
            .rust_sources
            .iter()
            .find(|r| r.label == q.root)
            .ok_or_else(missing)?;
        let (items, next_offset, truncated) = root
            .directory
            .list(&q.path, q.offset, q.limit)
            .map_err(rust_source_error)?;
        Ok(Json(crate::file_tree::Page {
            root: root.directory.root.to_string_lossy().into_owned(),
            indexed_workspace: String::new(),
            path: q.path,
            revision,
            items,
            next_offset,
            truncated,
        }))
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "source_error",
            "Source worker failed",
        )
    })?
}
#[cfg(feature = "test-support")]
async fn rust_source_file(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<RustFileQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<crate::rust_sources::Snapshot>, ApiError> {
    let Query(q) = query.map_err(|_| invalid())?;
    if q.path.is_empty() || !crate::file_tree::valid_path(&q.path) || !q.path.ends_with(".rs") {
        return Err(browse_invalid());
    }
    tokio::task::spawn_blocking(move || {
        let config = s.config();
        let root = config
            .rust_sources
            .iter()
            .find(|r| r.label == q.root)
            .ok_or_else(missing)?;
        root.snapshot(&q.path).map(Json).map_err(rust_source_error)
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "source_error",
            "Source worker failed",
        )
    })?
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TreeQuery {
    #[serde(default)]
    path: String,
    #[serde(default)]
    offset: usize,
    #[serde(default = "file_limit")]
    limit: usize,
}
#[derive(Debug)]
struct BrowseDirectoryError(std::io::Error);
impl std::fmt::Display for BrowseDirectoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("browse directory unavailable")
    }
}
impl std::error::Error for BrowseDirectoryError {}

fn browse_directory_error(e: &std::io::Error) -> ApiError {
    match e.kind() {
        std::io::ErrorKind::NotFound => ApiError(
            StatusCode::NOT_FOUND,
            "directory_missing",
            "Directory no longer exists; refresh its parent",
        ),
        std::io::ErrorKind::PermissionDenied => ApiError(
            StatusCode::FORBIDDEN,
            "directory_forbidden",
            "Permission denied while listing directory",
        ),
        std::io::ErrorKind::NotADirectory | std::io::ErrorKind::InvalidInput => ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_directory",
            "Path must be a real directory; symlink traversal is not allowed",
        ),
        _ if e.raw_os_error() == Some(libc::ELOOP) => ApiError(
            StatusCode::FORBIDDEN,
            "symlink_forbidden",
            "Symlink traversal is not allowed",
        ),
        _ => ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "directory_error",
            "Directory could not be read; check permissions and refresh",
        ),
    }
}

#[cfg(feature = "test-support")]
async fn tree(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<TreeQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<crate::file_tree::Page>, ApiError> {
    let Query(q) = query.map_err(|_| browse_invalid())?;
    if !crate::file_tree::valid_path(&q.path)
        || !(1..=200).contains(&q.limit)
        || q.offset > crate::file_tree::SCAN_LIMIT
    {
        return Err(browse_invalid());
    }
    tokio::task::spawn_blocking(move || {
        s.store.verify_root()?;
        let config = s.config();
        let (mut items, next_offset, truncated) = config
            .browser
            .list(&q.path, q.offset, q.limit)
            .map_err(|e| browse_directory_error(&e))?;
        let (revision, indexed_workspace) =
            s.store.tree_metadata(&config.browser.root, &mut items)?;
        for item in items
            .iter_mut()
            .filter(|e| e.kind == "file" && e.indexed_path.is_none())
        {
            let absolute = config.browser.root.join(&item.path);
            let reason = if !absolute.starts_with(&indexed_workspace) {
                "Outside indexed workspace"
            } else {
                match absolute
                    .extension()
                    .and_then(|extension| extension.to_str())
                {
                    Some("ts" | "tsx") => "TypeScript indexing not supported yet",
                    Some("js" | "mjs" | "cjs" | "rs" | "java" | "py") => {
                        "Not indexed yet (may be excluded or size-limited)"
                    }
                    _ => "Unsupported source type",
                }
            };
            item.unindexed_reason = Some(reason.into());
        }
        s.store.verify_root()?;
        Ok(Json(crate::file_tree::Page {
            root: config.browser.root.to_string_lossy().into_owned(),
            indexed_workspace,
            path: q.path,
            revision,
            items,
            next_offset,
            truncated,
        }))
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "directory_error",
            "Directory worker failed",
        )
    })?
}
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PinQuery {
    index_generation: Option<String>,
    index_revision: Option<String>,
}
impl PinQuery {
    fn pin(&self) -> Result<Option<IndexPin>, ApiError> {
        match (&self.index_generation, &self.index_revision) {
            (None, None) => Ok(None),
            (Some(generation), Some(revision)) => {
                let revision = revision.parse::<u64>().map_err(|_| invalid())?;
                let value = json!({"indexGeneration":generation,"indexRevision":revision});
                Ok(Some(serde_json::from_value(value).map_err(|_| invalid())?))
            }
            _ => Err(invalid()),
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FilesQuery {
    #[serde(flatten)]
    pin: PinQuery,
    #[serde(default)]
    offset: usize,
    #[serde(default = "file_limit")]
    limit: usize,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileSymbolsQuery {
    path: String,
    #[serde(flatten)]
    pin: PinQuery,
    #[serde(default)]
    offset: usize,
    #[serde(default = "file_limit")]
    limit: usize,
}
impl FileSymbolsQuery {
    fn validate(&self) -> Result<IndexPin, ApiError> {
        if !crate::files_view::valid_path(&self.path)
            || !(1..=200).contains(&self.limit)
            || self.offset > i64::MAX as usize
        {
            return Err(invalid());
        }
        self.pin.pin()?.ok_or_else(invalid)
    }
}
#[cfg(feature = "test-support")]
async fn file_symbols(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<FileSymbolsQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<crate::files_view::Outline>, ApiError> {
    let Query(q) = query.map_err(|_| invalid())?;
    let pin = q.validate()?;
    Ok(Json(
        db(s, move |s| {
            s.file_symbols_at(&q.path, pin, q.offset, q.limit)
        })
        .await?
        .ok_or_else(missing)?,
    ))
}
#[cfg(feature = "test-support")]
async fn syntax_matches(
    State(s): State<Arc<DaemonState>>,
    body: Bytes,
) -> Result<Json<crate::files_view::SyntaxMatches>, ApiError> {
    let q: crate::files_view::SyntaxMatchesRequest =
        serde_json::from_slice(&body).map_err(|_| invalid())?;
    q.validate()?;
    Ok(Json(
        db(s, move |s| s.syntax_matches_at(&q))
            .await?
            .ok_or_else(missing)?,
    ))
}
fn file_limit() -> usize {
    200
}
fn browse_invalid() -> ApiError {
    ApiError(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        "Invalid browse or sequence request",
    )
}
#[cfg(feature = "test-support")]
async fn files(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<FilesQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(q) = query.map_err(|_| browse_invalid())?;
    if !(1..=200).contains(&q.limit) || q.offset > i64::MAX as usize {
        return Err(browse_invalid());
    }
    let pin = q.pin.pin()?;
    Ok(Json(
        db(s, move |s| s.files_at(pin, q.offset, q.limit)).await?,
    ))
}
#[cfg(feature = "test-support")]
async fn methods(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<SourceQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(q) = query.map_err(|_| browse_invalid())?;
    if q.path.is_empty()
        || q.path.len() > 8192
        || q.path.contains(['\0', '\\', ':'])
        || q.path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err(browse_invalid());
    }
    let pin = q.pin.pin()?;
    Ok(Json(
        db(s, move |s| s.methods_at(&q.path, pin))
            .await?
            .ok_or_else(missing)?,
    ))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SequenceRequest {
    seed: String,
    expected_revision: IndexPin,
    #[serde(default)]
    show_all: bool,
}
#[cfg(feature = "test-support")]
async fn sequence(
    State(s): State<Arc<DaemonState>>,
    body: Bytes,
) -> Result<Json<crate::behavior::SequenceView>, ApiError> {
    let q: SequenceRequest = serde_json::from_slice(&body).map_err(|_| browse_invalid())?;
    if q.seed.is_empty() || q.seed.len() > 8192 || q.seed.contains('\0') {
        return Err(browse_invalid());
    }
    let result = tokio::task::spawn_blocking(move || {
        let mut view = s
            .store
            .sequence_at(&q.seed, q.expected_revision, q.show_all)?;
        if let Some(view) = view.as_mut()
            && let Some(catalog) = s.catalog_snapshot(view.revision)
            && let Some((_, file)) = s.store.source_at(&view.seed.path, Some(view.revision))?
        {
            crate::dependency_links::annotate(view, &file, &catalog);
        }
        Ok::<_, anyhow::Error>(view)
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Operation failed",
        )
    })?
    .map_err(|e| {
        if e.chain()
            .any(|cause| cause.downcast_ref::<crate::store::PinExpired>().is_some())
            || e.to_string().starts_with("revision conflict")
            || e.to_string().starts_with("index_not_ready")
            || e.to_string().starts_with("incompatible_index")
        {
            e.into()
        } else {
            browse_invalid()
        }
    })?;
    Ok(Json(result.ok_or_else(missing)?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClassesQuery {
    path: Option<String>,
    #[serde(default)]
    q: String,
    #[serde(flatten)]
    pin: PinQuery,
    #[serde(default)]
    offset: usize,
    #[serde(default = "class_limit")]
    limit: usize,
}
fn class_limit() -> usize {
    100
}
#[cfg(feature = "test-support")]
async fn classes(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<ClassesQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<crate::class_diagram::ClassPage>, ApiError> {
    let Query(q) = query.map_err(|_| invalid())?;
    let pin = q.pin.pin()?;
    Ok(Json(
        db(s, move |s| {
            s.classes_at(q.path.as_deref(), &q.q, pin, q.offset, q.limit)
        })
        .await?,
    ))
}
#[cfg(feature = "test-support")]
async fn navigation(
    State(s): State<Arc<DaemonState>>,
    body: Bytes,
) -> Result<Json<crate::navigation::NavigationResult>, ApiError> {
    if body.len() > 32 * 1024 {
        return Err(invalid());
    }
    let request: crate::navigation::NavigationRequest =
        serde_json::from_slice(&body).map_err(|_| invalid())?;
    request.validate()?;
    Ok(Json(db(s, move |s| s.navigation_at(&request)).await?))
}

#[cfg(feature = "test-support")]
async fn class_diagram(
    State(s): State<Arc<DaemonState>>,
    body: Bytes,
) -> Result<Json<crate::class_diagram::ClassDiagram>, ApiError> {
    let request: crate::class_diagram::ClassDiagramRequest =
        serde_json::from_slice(&body).map_err(|_| invalid())?;
    request.validate().map_err(ApiError::from)?;
    Ok(Json(db(s, move |s| s.class_diagram_at(&request)).await?))
}

#[cfg(feature = "test-support")]
async fn status(
    State(s): State<Arc<DaemonState>>,
    uri: axum::http::Uri,
) -> Result<Json<IndexStatus>, ApiError> {
    if uri.query().is_some() {
        return Err(invalid());
    }
    Ok(Json(db(s, |s| s.status()).await?))
}
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IndexRequest {
    expected_revision: Option<IndexPin>,
}
impl From<crate::store::requests::Request> for IndexJob {
    fn from(row: crate::store::requests::Request) -> Self {
        let error = row
            .error_code
            .map(|code| json!({"code":code,"message":"Index job failed"}));
        Self {
            id: row.id,
            state: row.state,
            progress: IndexProgress::default(),
            revision: row.revision,
            error,
            submitted_at: row.submitted_at,
            started_at: row.started_at,
            finished_at: row.finished_at,
        }
    }
}
async fn start_index(
    State(s): State<Arc<DaemonState>>,
    body: Bytes,
) -> Result<(StatusCode, Json<IndexJob>), ApiError> {
    let request: IndexRequest = if body.is_empty() {
        IndexRequest::default()
    } else {
        let value: Value = serde_json::from_slice(&body).map_err(|_| invalid())?;
        if value.get("expectedRevision").is_some_and(Value::is_null) {
            return Err(invalid());
        }
        serde_json::from_value(value).map_err(|_| invalid())?
    };
    if s.store.is_recreate_pending() && request.expected_revision.is_some() {
        return Err(ApiError::from(anyhow::anyhow!(
            "revision conflict: exceptional recovery has no decodable prior pin"
        )));
    }
    let options = s.config().options.clone();
    let scheduler = s.clone();
    let row = db(s.clone(), move |_| {
        scheduler.enqueue_index(&options, request.expected_revision)
    })
    .await?;
    let job = IndexJob::from(row);
    // A later durable admission supersedes the legacy exceptional display slot.
    s.jobs.lock().unwrap().current = None;
    s.start_worker();
    Ok((StatusCode::ACCEPTED, Json(job)))
}
// The exceptional index-only recovery worker predates requests.db. Until the
// root/recovery slice moves it into the queue, its current ID remains readable
// through the authenticated routes; ordinary jobs always use durable rows.
async fn current_job(
    State(s): State<Arc<DaemonState>>,
) -> Result<Json<Option<IndexJob>>, ApiError> {
    let legacy = {
        let jobs = s.jobs.lock().unwrap();
        jobs.current
            .as_ref()
            .and_then(|id| jobs.jobs.get(id))
            .cloned()
    };
    if legacy.is_some() {
        db(s, |store| store.verify_root()).await?;
        return Ok(Json(legacy));
    }
    let row = db(s.clone(), |store| store.current_request()).await?;
    Ok(Json(row.map(|row| {
        let mut job = IndexJob::from(row);
        if job.finished_at.is_none()
            && let Some(progress) = s.job_progress.lock().unwrap().get(&job.id)
        {
            job.progress = progress.clone();
        }
        job
    })))
}
async fn job(
    State(s): State<Arc<DaemonState>>,
    Path(id): Path<String>,
) -> Result<Json<IndexJob>, ApiError> {
    let legacy = s.jobs.lock().unwrap().jobs.get(&id).cloned();
    if let Some(legacy) = legacy {
        db(s, |store| store.verify_root()).await?;
        return Ok(Json(legacy));
    }
    let row = db(s.clone(), move |store| store.request_by_id(&id))
        .await?
        .ok_or_else(missing)?;
    let mut job = IndexJob::from(row);
    if job.finished_at.is_none()
        && let Some(progress) = s.job_progress.lock().unwrap().get(&job.id)
    {
        job.progress = progress.clone();
    }
    Ok(Json(job))
}
async fn cancel_job(
    State(s): State<Arc<DaemonState>>,
    Path(id): Path<String>,
) -> Result<Json<IndexJob>, ApiError> {
    let legacy = s.jobs.lock().unwrap().jobs.get(&id).cloned();
    if let Some(legacy) = legacy {
        db(s, |store| store.verify_root()).await?;
        if legacy.finished_at.is_some() {
            return Ok(Json(legacy));
        }
        return Err(ApiError(
            StatusCode::CONFLICT,
            "request_not_cancellable",
            "Accepted requests cannot be cancelled",
        ));
    }
    let row = db(s.clone(), move |store| store.request_by_id(&id))
        .await?
        .ok_or_else(missing)?;
    if row.finished_at.is_none() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "request_not_cancellable",
            "Accepted requests cannot be cancelled",
        ));
    }
    // Terminal responses are immutable durable rows. Do not mix in late
    // process-local progress when cancel is read after the initial GET.
    Ok(Json(IndexJob::from(row)))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceQuery {
    path: String,
    #[serde(flatten)]
    pin: PinQuery,
}
#[cfg(feature = "test-support")]
async fn source(
    State(s): State<Arc<DaemonState>>,
    Query(q): Query<SourceQuery>,
) -> Result<Json<Value>, ApiError> {
    if q.path.is_empty()
        || q.path.len() > 8192
        || q.path.contains(['\0', '\\', ':'])
        || q.path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err(invalid());
    }
    let pin = q.pin.pin()?;
    let (revision, file) = db(s, move |s| s.source_at(&q.path, pin))
        .await?
        .ok_or_else(missing)?;
    Ok(Json(json!({"revision":revision,"file":file})))
}

#[cfg(all(test, feature = "test-support"))]
mod live_tests;

#[cfg(test)]
mod serving_holder_tests;

#[cfg(test)]
mod exceptional_recovery_tests;

#[cfg(test)]
mod rebuild_recovery_tests;

#[cfg(all(test, feature = "test-support"))]
mod normal_post_capture_cancellation_tests;

#[cfg(all(test, feature = "test-support"))]
mod dependency_lifecycle_tests;

#[cfg(test)]
mod maintenance_telemetry_tests;

/// The socket-only daemon has no HTTP state. This router is constructed only
/// after a validated explicit serve registration binds a loopback listener.
type SelectedResponseHook = Arc<dyn Fn(&'static str) + Send + Sync>;

#[derive(Clone)]
pub struct ProvisionedBrowser {
    registry: Arc<tokio::sync::Mutex<crate::daemon::registry::CheckoutRegistry>>,
    token: String,
    hosts: Vec<String>,
    origins: Vec<String>,
    response_hook: Arc<Mutex<Option<SelectedResponseHook>>>,
    metrics_limit: Arc<std::sync::atomic::AtomicUsize>,
    metrics_deadline_ms: Arc<std::sync::atomic::AtomicU64>,
    metrics_test_cancel: Arc<std::sync::atomic::AtomicBool>,
    metrics_clock_skew_ms: Arc<std::sync::atomic::AtomicU64>,
    #[cfg(feature = "test-support")]
    metrics_clock: Arc<Mutex<Option<Instant>>>,
}

impl ProvisionedBrowser {
    pub fn new(
        registry: Arc<tokio::sync::Mutex<crate::daemon::registry::CheckoutRegistry>>,
        token: String,
        address: SocketAddr,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            address.ip().is_loopback() && address.port() != 0,
            "bound loopback address required"
        );
        anyhow::ensure!(valid_token(&token), "invalid bearer token");
        let hosts = vec![address.to_string(), format!("localhost:{}", address.port())];
        let origins = hosts.iter().map(|host| format!("http://{host}")).collect();
        Ok(Self {
            registry,
            token,
            hosts,
            origins,
            response_hook: Arc::new(Mutex::new(None)),
            metrics_limit: Arc::new(std::sync::atomic::AtomicUsize::new(64 * 1024 * 1024)),
            metrics_deadline_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            metrics_test_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            metrics_clock_skew_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            #[cfg(feature = "test-support")]
            metrics_clock: Arc::new(Mutex::new(None)),
        })
    }

    #[doc(hidden)]
    pub fn set_selected_response_hook_for_tests(&self, hook: SelectedResponseHook) {
        *self.response_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(feature = "test-support")]
    pub fn set_metrics_controls_for_tests(
        &self,
        limit: usize,
        deadline: Option<Duration>,
        cancel: bool,
    ) {
        use std::sync::atomic::Ordering;
        self.metrics_limit.store(limit, Ordering::Relaxed);
        self.metrics_deadline_ms.store(
            deadline.map_or(0, |d| d.as_millis().max(1) as u64),
            Ordering::Relaxed,
        );
        self.metrics_test_cancel.store(cancel, Ordering::Relaxed);
    }

    #[cfg(feature = "test-support")]
    pub fn set_metrics_clock_skew_for_tests(&self, milliseconds: u64) {
        self.metrics_clock_skew_ms
            .store(milliseconds, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(feature = "test-support")]
    pub fn set_metrics_clock_for_tests(&self, now: Instant) {
        *self.metrics_clock.lock().unwrap() = Some(now);
    }

    pub fn router(self) -> Router {
        let state = Arc::new(self);
        Router::new()
            .route(
                "/",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                        include_str!("../web/index.html"),
                    )
                }),
            )
            .route(
                "/app.js",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                        include_str!("../web/app.js"),
                    )
                }),
            )
            .route(
                "/viewer-metrics.js",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                        include_str!("../web/viewer-metrics.js"),
                    )
                }),
            )
            .route(
                "/files.js",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                        include_str!("../web/files.js"),
                    )
                }),
            )
            .route(
                "/files.css",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                        include_str!("../web/files.css"),
                    )
                }),
            )
            .route(
                "/shell.js",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                        include_str!("../web/shell.js"),
                    )
                }),
            )
            .route(
                "/style.css",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                        include_str!("../web/style.css"),
                    )
                }),
            )
            .route(
                "/classes.js",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                        include_str!("../web/classes.js"),
                    )
                }),
            )
            .route(
                "/classes.css",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                        include_str!("../web/classes.css"),
                    )
                }),
            )
            .route(
                "/navigation.css",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                        include_str!("../web/navigation.css"),
                    )
                }),
            )
            .route(
                "/navigation.js",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                        include_str!("../web/navigation.js"),
                    )
                }),
            )
            .route(
                "/sequence.js",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                        include_str!("../web/sequence.js"),
                    )
                }),
            )
            .route(
                "/sequence-pele.js",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                        include_str!("../web/sequence-pele.js"),
                    )
                }),
            )
            .route(
                "/vendor/pele-1.10.0-trellis.1.js",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                        include_bytes!("../web/vendor/pele-1.10.0-trellis.1.js"),
                    )
                }),
            )
            .route(
                "/fonts/jetbrains-mono-latin.woff2",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "font/woff2")],
                        &include_bytes!("../web/fonts/jetbrains-mono-latin.woff2")[..],
                    )
                }),
            )
            .route(
                "/fonts/inter-variable.woff2",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "font/woff2")],
                        &include_bytes!("../web/fonts/inter-variable.woff2")[..],
                    )
                }),
            )
            .route(
                "/fonts/manrope-variable.woff2",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "font/woff2")],
                        &include_bytes!("../web/fonts/manrope-variable.woff2")[..],
                    )
                }),
            )
            .route(
                "/fonts/JetBrainsMono-OFL.txt",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                        include_str!("../web/fonts/JetBrainsMono-OFL.txt"),
                    )
                }),
            )
            .route(
                "/fonts/Inter-OFL.txt",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                        include_str!("../web/fonts/Inter-OFL.txt"),
                    )
                }),
            )
            .route(
                "/fonts/Manrope-OFL.txt",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                        include_str!("../web/fonts/Manrope-OFL.txt"),
                    )
                }),
            )
            .route(
                "/brand/trellis-mark-transparent-v1.png",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "image/png")],
                        &include_bytes!("../web/brand/trellis-mark-transparent-v1.png")[..],
                    )
                }),
            )
            .route(
                "/brand/trellis-logo-dark-v1.webp",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "image/webp")],
                        &include_bytes!("../web/brand/trellis-logo-dark-v1.webp")[..],
                    )
                }),
            )
            .route("/favicon.ico", get(|| async { StatusCode::NO_CONTENT }))
            .route(
                "/healthz",
                get(|| async { Json(json!({"ok":true,"version":env!("CARGO_PKG_VERSION")})) }),
            )
            .route(
                "/api/checkouts",
                get(|State(state): State<Arc<Self>>| async move {
                    let probe = state.registry.lock().await.browser_checkouts_probe();
                    let mut checkouts = tokio::task::spawn_blocking(move || probe.check())
                        .await
                        .map_err(|_| {
                            ApiError(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "internal_error",
                                "Operation failed",
                            )
                        })?;
                    enrich_checkout_git(&mut checkouts).await;
                    checkout_list_response(checkouts)
                }),
            )
            .route(
                "/api/daemon/status",
                get(|State(state): State<Arc<Self>>| async move {
                    let registry = state.registry.lock().await;
                    Json(json!({"activeCheckouts":registry.active_count()}))
                }),
            )
            .route(
                "/api/checkouts/{root_key}/{*suffix}",
                get(provisioned_core)
                    .post(provisioned_core)
                    .put(provisioned_core)
                    .delete(provisioned_core),
            )
            .layer(middleware::from_fn_with_state(
                state.clone(),
                provision_guard,
            ))
            .with_state(state)
    }
}

async fn provision_guard(
    State(state): State<Arc<ProvisionedBrowser>>,
    req: Request,
    next: Next,
) -> Response {
    guard_common(&state.token, &state.hosts, &state.origins, req, next).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GitDiffQuery {
    path: String,
}

fn git_query(route: BrowserCoreRoute, uri: &axum::http::Uri) -> Result<Option<String>, ApiError> {
    if matches!(route, BrowserCoreRoute::GitStatus) {
        return if uri.query().is_none() {
            Ok(None)
        } else {
            Err(invalid())
        };
    }
    let Query(query) = Query::<GitDiffQuery>::try_from_uri(uri).map_err(|_| invalid())?;
    crate::git_status::validate_path(&query.path).map_err(git_request_error)?;
    Ok(Some(query.path))
}

fn git_request_error(error: crate::git_status::GitRequestError) -> ApiError {
    match error {
        crate::git_status::GitRequestError::InvalidPath => ApiError(
            StatusCode::BAD_REQUEST,
            "invalid_git_path",
            "Invalid Git path",
        ),
        crate::git_status::GitRequestError::NotFound => missing(),
    }
}

async fn git_answer(root: &std::path::Path, path: Option<&str>) -> Result<Response, ApiError> {
    match path {
        Some(path) => Ok(Json(
            crate::git_status::diff(root, path)
                .await
                .map_err(git_request_error)?,
        )
        .into_response()),
        None => Ok(Json(crate::git_status::status(root).await).into_response()),
    }
}

async fn enrich_checkout_git(checkouts: &mut [Value]) {
    use crate::{
        git_status::{GitReason, GitSummary},
        store::topology::WorkspaceIdentity,
    };
    let deadline = tokio::time::Instant::now() + crate::git_status::READ_DEADLINE;
    for row in checkouts {
        if tokio::time::Instant::now() >= deadline {
            row["git"] = json!(GitSummary::Unavailable {
                reason: GitReason::DeadlineExceeded
            });
            continue;
        }
        let identity = row["workspaceRoot"]
            .as_str()
            .filter(|_| row["state"] != "unavailable")
            .and_then(|root| {
                let root = std::path::Path::new(root);
                WorkspaceIdentity::discover_unattached(Some(root), root)
                    .and_then(WorkspaceIdentity::attach_existing_marker_readonly)
                    .ok()
            })
            .filter(|identity| row["rootKey"].as_str() == Some(identity.root_key.as_str()));
        let git = match identity {
            None => GitSummary::Unavailable {
                reason: GitReason::Unreadable,
            },
            Some(identity) => {
                let result =
                    tokio::time::timeout_at(deadline, crate::git_status::summary(&identity.root))
                        .await;
                if identity.verify_readonly().is_err() {
                    GitSummary::Unavailable {
                        reason: GitReason::Unreadable,
                    }
                } else {
                    result.unwrap_or(GitSummary::Unavailable {
                        reason: GitReason::DeadlineExceeded,
                    })
                }
            }
        };
        row["git"] = json!(git);
    }
}

fn checkout_list_response(checkouts: Vec<Value>) -> Result<Response, ApiError> {
    struct Bounded(Vec<u8>);
    impl std::io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > crate::git_status::RESPONSE_LIMIT.saturating_sub(self.0.len()) {
                return Err(std::io::Error::other("response_too_large"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Bounded(Vec::new());
    serde_json::to_writer(&mut writer, &json!({"checkouts": checkouts})).map_err(|_| {
        ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "response_too_large",
            "Response is too large",
        )
    })?;
    Ok(([(header::CONTENT_TYPE, "application/json")], writer.0).into_response())
}

#[derive(Clone, Copy)]
enum BrowserCoreRoute {
    GitStatus,
    GitDiff,
    Metrics,
    RefreshDependencies,
    Dependencies,
    DependencySymbols,
    DependencySource,
    RustSources,
    RustSourceTree,
    RustSourceFile,
    Status,
    Tree,
    Files,
    Methods,
    FileSymbols,
    SyntaxMatches,
    Classes,
    Sequence,
    ClassDiagram,
    Navigation,
    Source,
    Index,
    CurrentJob,
    Job,
}

impl BrowserCoreRoute {
    fn parse(method: &axum::http::Method, suffix: &str) -> Option<Self> {
        use axum::http::Method;
        match (method, suffix) {
            (&Method::GET, "git") => Some(Self::GitStatus),
            (&Method::GET, "git/diff") => Some(Self::GitDiff),
            (&Method::GET, "metrics") => Some(Self::Metrics),
            (&Method::POST, "dependencies/refresh") => Some(Self::RefreshDependencies),
            (&Method::GET, "dependencies") => Some(Self::Dependencies),
            (&Method::GET, "dependencies/symbols") => Some(Self::DependencySymbols),
            (&Method::GET, "dependencies/source") => Some(Self::DependencySource),
            (&Method::GET, "rust-sources") => Some(Self::RustSources),
            (&Method::GET, "rust-sources/tree") => Some(Self::RustSourceTree),
            (&Method::GET, "rust-sources/file") => Some(Self::RustSourceFile),
            (&Method::GET, "status") => Some(Self::Status),
            (&Method::GET, "tree") => Some(Self::Tree),
            (&Method::GET, "files") => Some(Self::Files),
            (&Method::GET, "methods") => Some(Self::Methods),
            (&Method::GET, "symbols") => Some(Self::FileSymbols),
            (&Method::POST, "syntax-matches") => Some(Self::SyntaxMatches),
            (&Method::GET, "classes") => Some(Self::Classes),
            (&Method::POST, "sequence") => Some(Self::Sequence),
            (&Method::POST, "class-diagram") => Some(Self::ClassDiagram),
            (&Method::POST, "navigation") => Some(Self::Navigation),
            (&Method::GET, "source") => Some(Self::Source),
            (&Method::POST, "index") => Some(Self::Index),
            (&Method::GET, "jobs/current") => Some(Self::CurrentJob),
            _ => {
                let (family, tail) = suffix.split_once('/')?;
                let single_id = !tail.is_empty() && !tail.contains('/');
                match (method, family) {
                    (&Method::GET, "jobs") if single_id => Some(Self::Job),
                    (&Method::POST, "jobs") => {
                        let id = tail.strip_suffix("/cancel")?;
                        (!id.is_empty() && !id.contains('/')).then_some(Self::Job)
                    }
                    _ => None,
                }
            }
        }
    }
}

/// The provisioned listener has one selection boundary for core checkout routes.
/// The direct single-checkout router is a test-support fixture only.
async fn provisioned_core(
    State(browser): State<Arc<ProvisionedBrowser>>,
    Path((root_key, suffix)): Path<(String, String)>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    use crate::daemon::registry::SelectionError;
    let Some(route) = BrowserCoreRoute::parse(&method, &suffix) else {
        return missing().into_response();
    };
    let is_git = matches!(
        route,
        BrowserCoreRoute::GitStatus | BrowserCoreRoute::GitDiff
    );
    let git_path = if is_git {
        match git_query(route, &uri) {
            Ok(path) => path,
            Err(error) => return error.into_response(),
        }
    } else {
        None
    };
    let identity = {
        let mut registry = browser.registry.lock().await;
        match registry.browser_identity(&root_key) {
            Ok(identity) => identity,
            Err(reason) => {
                return browser_selection_error(reason);
            }
        }
    };
    let root = identity.root.to_string_lossy().to_string();
    // Git reads are observational: they neither attach a browser lifetime nor
    // activate an index runtime. The common selected-root fence still applies.
    let answer: Result<(Response, bool), ApiError> = if is_git {
        let result = if identity.verify_readonly().is_err() {
            Err(ApiError::from(anyhow::anyhow!("root_changed")))
        } else {
            git_answer(&identity.root, git_path.as_deref()).await
        };
        let hook = browser.response_hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook("after_handler");
        }
        let catching_up = browser
            .registry
            .lock()
            .await
            .runtime(&root_key)
            .is_some_and(|runtime| runtime.catching_up());
        result.map(|response| (response, catching_up))
    } else {
        let runtime = {
            let mut registry = browser.registry.lock().await;
            match registry.browser_request_at(&identity, Instant::now()) {
                Ok(_) => registry.activate(&root_key).map_err(|error| {
                    if crate::store::transient_storage_contention(&error) {
                        ApiError::from(error)
                    } else {
                        ApiError(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "store_unavailable",
                            "Storage is unavailable",
                        )
                    }
                }),
                Err(SelectionError::CheckoutCapacity) => Err(ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "checkout_capacity",
                    "Checkout capacity reached",
                )),
                Err(error) => Err(ApiError::from(anyhow::anyhow!("{}", error.reason()))),
            }
        };
        match runtime {
            Err(error) => Err(error),
            Ok(runtime) => {
                let hook = browser.response_hook.lock().unwrap().clone();
                #[cfg(feature = "test-support")]
                let frozen_now = *browser.metrics_clock.lock().unwrap();
                let result = provisioned_core_answer(
                    &runtime,
                    BrowserCoreRequest {
                        route,
                        method: &method,
                        suffix: &suffix,
                        uri: &uri,
                        body,
                    },
                    hook.clone(),
                    MetricsHttpControls {
                        limit: browser
                            .metrics_limit
                            .load(std::sync::atomic::Ordering::Relaxed),
                        deadline_ms: browser
                            .metrics_deadline_ms
                            .load(std::sync::atomic::Ordering::Relaxed),
                        test_cancel: browser.metrics_test_cancel.clone(),
                        clock_skew_ms: browser.metrics_clock_skew_ms.clone(),
                        #[cfg(feature = "test-support")]
                        frozen_now,
                    },
                )
                .await;
                if let Some(hook) = hook {
                    hook("after_handler");
                }
                result
            }
        }
    };
    let (mut response, catching_up) = match answer {
        Ok(pair) => pair,
        Err(error) => {
            let catching_up = browser
                .registry
                .lock()
                .await
                .runtime(&root_key)
                .map_or(!is_git, |runtime| runtime.catching_up());
            (error.into_response(), catching_up)
        }
    };
    if identity.verify_readonly().is_err() {
        let outcome = response
            .headers()
            .get("X-Trellis-Mutation-Outcome")
            .and_then(|value| value.to_str().ok());
        response = match outcome {
            Some(outcome) => selected_mutation_error(
                ApiError(
                    StatusCode::CONFLICT,
                    "root_changed",
                    "Selected checkout changed",
                ),
                outcome,
            ),
            None => ApiError::from(anyhow::anyhow!("root_changed: selected checkout changed"))
                .into_response(),
        };
    }
    if let Ok(value) = root.parse() {
        response.headers_mut().insert("X-Trellis-Workspace", value);
    }
    response.headers_mut().insert(
        "X-Trellis-Catching-Up",
        if catching_up { "true" } else { "false" }.parse().unwrap(),
    );
    response
}

fn browser_selection_error(reason: crate::daemon::registry::SelectionError) -> Response {
    let status = match reason {
        crate::daemon::registry::SelectionError::NotCheckout
        | crate::daemon::registry::SelectionError::NotAbsolute => StatusCode::BAD_REQUEST,
        crate::daemon::registry::SelectionError::IdentityChanged => StatusCode::CONFLICT,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, Json(json!({"error":{"code":"workspace_selection_failed", "reason":reason.reason(), "message":"Checkout selection failed"}}))).into_response()
}

async fn selected_evidence<T: Send + 'static>(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    hook: Option<SelectedResponseHook>,
    work: impl FnOnce(&crate::daemon::registry::CheckoutEvidence) -> anyhow::Result<T> + Send + 'static,
) -> Result<(T, bool), ApiError> {
    let runtime = runtime.clone();
    tokio::task::spawn_blocking(move || {
        let (response, catching_up) = runtime.evidence_response()?;
        if let Some(hook) = &hook {
            hook("before_work");
        }
        let result = work(&response)?;
        if let Some(hook) = hook {
            hook("before_read_finish");
        }
        Ok::<_, anyhow::Error>((response.finish(result)?, catching_up))
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Operation failed",
        )
    })?
    .map_err(ApiError::from)
}

async fn selected_catalog_read<T: Send + 'static>(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    hook: Option<SelectedResponseHook>,
    work: impl FnOnce(&crate::daemon::registry::CheckoutEvidence) -> Result<T, ApiError>
    + Send
    + 'static,
) -> Result<(T, bool), ApiError> {
    let runtime = runtime.clone();
    tokio::task::spawn_blocking(move || {
        let (response, catching_up) = runtime.evidence_response().map_err(ApiError::from)?;
        if let Some(hook) = &hook {
            hook("before_work");
        }
        let value = work(&response)?;
        if let Some(hook) = hook {
            hook("before_read_finish");
        }
        Ok((response.finish(value).map_err(ApiError::from)?, catching_up))
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Operation failed",
        )
    })?
}

fn selected_catalog(
    state: &DaemonState,
    revision: IndexPin,
    id: &str,
) -> Result<Arc<Catalog>, ApiError> {
    state
        .catalog_snapshot(revision)
        .filter(|catalog| catalog.id == id)
        .ok_or_else(dependency_stale)
}

async fn selected_refresh_preflight(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    hook: Option<SelectedResponseHook>,
) -> Result<(IndexPin, bool), ApiError> {
    let runtime = runtime.clone();
    tokio::task::spawn_blocking(move || {
        runtime.require_mutation_ready()?;
        let (response, catching_up) = runtime.evidence_response()?;
        response.require_mutation_ready()?;
        let revision = response.status()?.revision;
        response.finish(())?;
        if let Some(hook) = hook {
            hook("before_mutation");
        }
        runtime.require_mutation_ready()?;
        response.finish(())?;
        Ok::<_, anyhow::Error>((revision, catching_up))
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Operation failed",
        )
    })?
    .map_err(ApiError::from)
}

async fn selected_refresh_postflight(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    revision: IndexPin,
) -> Result<(), ApiError> {
    let runtime = runtime.clone();
    tokio::task::spawn_blocking(move || {
        runtime.require_mutation_ready()?;
        let (response, _) = runtime.evidence_response()?;
        response.require_mutation_ready()?;
        anyhow::ensure!(
            response.status()?.revision == revision,
            "revision conflict: refresh basis changed"
        );
        response.finish(())?;
        runtime.require_mutation_ready()
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Operation failed",
        )
    })?
    .map_err(ApiError::from)
}

fn selected_mutation_error(error: ApiError, outcome: &str) -> Response {
    let mut response = (
        error.0,
        Json(json!({
            "error": {"code": error.1, "message": error.2},
            "mutationOutcome": outcome
        })),
    )
        .into_response();
    response
        .headers_mut()
        .insert("X-Trellis-Mutation-Outcome", outcome.parse().unwrap());
    response
}

fn metrics_query(uri: &axum::http::Uri) -> Result<crate::store::MetricsOptions, ApiError> {
    use std::collections::BTreeMap;
    fn decode(value: &str) -> Result<String, ApiError> {
        if value.len() > 8192 {
            return Err(invalid());
        }
        let mut bytes = Vec::with_capacity(value.len());
        let input = value.as_bytes();
        let mut i = 0;
        while i < input.len() {
            match input[i] {
                b'%' => {
                    let hex = input.get(i + 1..i + 3).ok_or_else(invalid)?;
                    let hex = std::str::from_utf8(hex).map_err(|_| invalid())?;
                    bytes.push(u8::from_str_radix(hex, 16).map_err(|_| invalid())?);
                    i += 3;
                }
                b'+' => {
                    bytes.push(b' ');
                    i += 1;
                }
                byte => {
                    bytes.push(byte);
                    i += 1;
                }
            }
        }
        String::from_utf8(bytes).map_err(|_| invalid())
    }
    let mut fields = BTreeMap::new();
    if let Some(query) = uri.query() {
        if query.len() > 8192 {
            return Err(invalid());
        }
        for pair in query.split('&') {
            let (name, value) = pair.split_once('=').ok_or_else(invalid)?;
            let name = decode(name)?;
            if !matches!(
                name.as_str(),
                "scope"
                    | "path"
                    | "language"
                    | "sourceSetId"
                    | "sid"
                    | "indexGeneration"
                    | "indexRevision"
                    | "aggregate"
                    | "metrics"
                    | "topCognitive"
            ) || fields.insert(name, decode(value)?).is_some()
            {
                return Err(invalid());
            }
        }
    }
    let mut options = crate::store::MetricsOptions::default();
    if let Some(value) = fields.remove("scope") {
        options.scope = value;
    }
    options.path = fields.remove("path");
    options.language = fields.remove("language");
    options.source_set_id = fields.remove("sourceSetId");
    options.sid = fields.remove("sid");
    if let Some(value) = fields.remove("aggregate") {
        options.aggregate = value;
    }
    if let Some(value) = fields.remove("topCognitive") {
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid());
        }
        options.top_cognitive = Some(value.parse().map_err(|_| invalid())?);
    }
    let generation = fields.remove("indexGeneration");
    let revision = fields.remove("indexRevision");
    match (generation, revision) {
        (None, None) => (),
        (Some(generation), Some(revision))
            if !revision.is_empty() && revision.bytes().all(|b| b.is_ascii_digit()) =>
        {
            let revision = revision.parse::<u64>().map_err(|_| invalid())?;
            options.pin = Some(
                serde_json::from_value(json!({
                    "indexGeneration": generation, "indexRevision": revision
                }))
                .map_err(|_| invalid())?,
            );
        }
        _ => return Err(invalid()),
    }
    if let Some(names) = fields.remove("metrics") {
        if names.is_empty() {
            return Err(invalid());
        }
        options.metrics = names.split(',').map(str::to_owned).collect();
    }
    options.validate().map_err(|_| invalid())?;
    for value in [
        options.source_set_id.as_deref(),
        options.path.as_deref(),
        options.sid.as_deref(),
    ] {
        if value.is_some_and(|v| v.contains(['\\', '\0'])) {
            return Err(invalid());
        }
    }
    if options.source_set_id.as_ref().is_some_and(|id| {
        let Some(identity) = id.strip_prefix("source-set:v1:") else {
            return true;
        };
        if let Some(hash) = identity.strip_prefix("path-") {
            return hash.len() != 64
                || !hash
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
        }
        !uuid::Uuid::parse_str(identity).is_ok_and(|uuid| uuid.to_string() == identity)
    }) || options.path.as_ref().is_some_and(|path| path.contains(':'))
        || options.sid.as_ref().is_some_and(|v| {
            v.len() != 39
                || !v.starts_with("sid:v1:")
                || !v[7..]
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
        || options
            .language
            .as_ref()
            .is_some_and(|v| !matches!(v.as_str(), "java" | "python" | "javascript" | "rust"))
    {
        return Err(invalid());
    }
    Ok(options)
}

struct MetricsHttpControls {
    limit: usize,
    deadline_ms: u64,
    test_cancel: Arc<std::sync::atomic::AtomicBool>,
    clock_skew_ms: Arc<std::sync::atomic::AtomicU64>,
    #[cfg(feature = "test-support")]
    frozen_now: Option<Instant>,
}

impl MetricsHttpControls {
    fn now(&self) -> Instant {
        #[cfg(feature = "test-support")]
        if let Some(now) = self.frozen_now {
            return now;
        }
        Instant::now()
    }
}

struct BrowserCoreRequest<'a> {
    route: BrowserCoreRoute,
    method: &'a axum::http::Method,
    suffix: &'a str,
    uri: &'a axum::http::Uri,
    body: Bytes,
}

async fn provisioned_core_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    request: BrowserCoreRequest<'_>,
    hook: Option<SelectedResponseHook>,
    metrics: MetricsHttpControls,
) -> Result<(Response, bool), ApiError> {
    use axum::http::Method;
    let BrowserCoreRequest {
        route,
        method,
        suffix,
        uri,
        body,
    } = request;
    match route {
        BrowserCoreRoute::GitStatus | BrowserCoreRoute::GitDiff => {
            unreachable!("Git reads bypass index activation")
        }
        BrowserCoreRoute::Metrics => {
            let options = metrics_query(uri)?;
            let runtime = runtime.clone();
            // The guard belongs to this request future, not to the blocking worker.
            struct CancelOnDrop(Arc<std::sync::atomic::AtomicBool>);
            impl Drop for CancelOnDrop {
                fn drop(&mut self) {
                    self.0.store(true, std::sync::atomic::Ordering::Release);
                }
            }
            let guard = CancelOnDrop(Arc::new(std::sync::atomic::AtomicBool::new(false)));
            let cancellation = guard.0.clone();
            let (body, catching_up) = tokio::task::spawn_blocking(move || {
                let duration = if metrics.deadline_ms != 0 {
                    Duration::from_millis(metrics.deadline_ms)
                } else if matches!(options.scope.as_str(), "revision" | "directory") {
                    Duration::from_secs(300)
                } else {
                    Duration::from_secs(30)
                };
                let deadline = metrics.now() + duration;
                let work_hook = hook.clone();
                let finish_hook = hook.clone();
                let serialize_hook = hook.clone();
                let after_finish_hook = hook.clone();
                let check_hook = hook.clone();
                let mut before_work = || {
                    if let Some(hook) = &work_hook {
                        hook("before_work");
                    }
                };
                let mut before_finish = || {
                    if let Some(hook) = &finish_hook {
                        hook("before_read_finish");
                    }
                };
                let mut after_finish = || {
                    if let Some(hook) = &after_finish_hook {
                        hook("after_read_finish");
                    }
                };
                let mut on_progress = || {
                    if let Some(hook) = &hook {
                        #[cfg(feature = "test-support")]
                        if crate::metrics::in_parser_progress_for_tests() {
                            hook("metrics_parser_progress");
                            return;
                        }
                        hook("metrics_read_progress");
                    }
                };
                let mut on_write = |written: usize| {
                    if let Some(hook) = &serialize_hook {
                        hook(if written >= metrics.limit.saturating_sub(80) {
                            "metrics_serialize_last"
                        } else {
                            "metrics_serialize"
                        });
                    }
                };
                let result = runtime.metrics_response(
                    &options,
                    &mut || {
                        if cancellation.load(std::sync::atomic::Ordering::Acquire)
                            || metrics
                                .test_cancel
                                .load(std::sync::atomic::Ordering::Relaxed)
                        {
                            if let Some(hook) = &check_hook {
                                hook("metrics_cancelled");
                            }
                            anyhow::bail!("cancelled");
                        }
                        if metrics.now()
                            + Duration::from_millis(
                                metrics
                                    .clock_skew_ms
                                    .load(std::sync::atomic::Ordering::Relaxed),
                            )
                            >= deadline
                        {
                            anyhow::bail!("deadline_exceeded");
                        }
                        Ok(())
                    },
                    crate::daemon::registry::MetricsResponseHooks {
                        before_work: &mut before_work,
                        before_finish: &mut before_finish,
                        after_finish: &mut after_finish,
                        on_write: &mut on_write,
                        on_progress: &mut on_progress,
                    },
                    metrics.limit,
                );
                if let Some(hook) = &hook {
                    hook("metrics_worker_done");
                }
                result
            })
            .await
            .map_err(|_| {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "Operation failed",
                )
            })??;
            drop(guard);
            Ok((
                (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    body,
                )
                    .into_response(),
                catching_up,
            ))
        }
        BrowserCoreRoute::RefreshDependencies => {
            let request: Value = serde_json::from_slice(&body).map_err(|_| invalid())?;
            if !request.as_object().is_some_and(|object| object.is_empty()) {
                return Err(invalid());
            }
            let state = runtime.browser_scheduler()?;
            // One configuration version for the whole request (Decision 0009 §6).
            let config = state.config();
            let (revision, catching_up) = selected_refresh_preflight(runtime, hook).await?;
            if config.dependency_options.is_none() {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    "dependencies_disabled",
                    "Dependency catalog is disabled",
                ));
            }
            if state.dependencies.lock().unwrap().stopped {
                return Err(ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "shutting_down",
                    "Daemon is shutting down",
                ));
            }
            // Check once more immediately before scheduling work. Once scheduled,
            // a later root or head change cannot undo the requested refresh.
            selected_refresh_postflight(runtime, revision).await?;
            runtime.require_mutation_ready()?;
            // Scheduled only for the configuration version this request
            // checked; a registration in between refuses it.
            if !state.start_dependency_index_for(&config) {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    "configuration_changed",
                    "The checkout configuration changed; retry the refresh",
                ));
            }
            let mut response = match selected_refresh_postflight(runtime, revision).await {
                Ok(()) => (StatusCode::ACCEPTED, Json(json!({"state":"loading"}))).into_response(),
                Err(error) => selected_mutation_error(error, "committed"),
            };
            response
                .headers_mut()
                .insert("X-Trellis-Mutation-Outcome", "committed".parse().unwrap());
            Ok((response, catching_up))
        }
        BrowserCoreRoute::Dependencies => {
            if uri.query().is_some() {
                return Err(invalid());
            }
            let state = runtime.browser_scheduler()?;
            let (payload, catching_up) = selected_catalog_read(runtime, hook, move |r| {
                let revision = r.status().map_err(ApiError::from)?.revision;
                let (index_state, catalog, mut warnings) = {
                    let index = state.dependencies.lock().unwrap();
                    (index.state, index.catalog.clone(), index.warnings.clone())
                };
                Ok(if let Some(catalog) = catalog {
                    if catalog.workspace_revision == revision {
                        json!({"state":index_state,"workspaceRevision":revision,
                            "catalogId":catalog.id,"packages":catalog.packages,
                            "symbolCount":catalog.symbols.len(),"warnings":catalog.warnings})
                    } else {
                        warnings.push("Workspace changed; refresh the dependency catalog".into());
                        json!({"state":"failed","workspaceRevision":revision,
                            "catalogId":null,"packages":[],"symbolCount":0,"warnings":warnings})
                    }
                } else {
                    json!({"state":index_state,"workspaceRevision":revision,
                        "catalogId":null,"packages":[],"symbolCount":0,"warnings":warnings})
                })
            })
            .await?;
            Ok((Json(payload).into_response(), catching_up))
        }
        BrowserCoreRoute::DependencySymbols => {
            let Query(q) =
                Query::<DependencySymbolsQuery>::try_from_uri(uri).map_err(|_| invalid())?;
            if q.catalog_id.is_empty()
                || q.catalog_id.len() > 8192
                || q.q.len() > 8192
                || q.package_id.as_ref().is_some_and(|id| id.len() > 8192)
                || !(1..=200).contains(&q.limit)
                || q.offset > 50_000
            {
                return Err(invalid());
            }
            let state = runtime.browser_scheduler()?;
            let (payload, catching_up) = selected_catalog_read(runtime, hook, move |r| {
                let revision = r.status().map_err(ApiError::from)?.revision;
                let catalog = selected_catalog(&state, revision, &q.catalog_id)?;
                if q.package_id.as_ref().is_some_and(|id| !catalog.packages.iter().any(|p| &p.id == id)) { return Err(missing()); }
                let search = q.q.to_lowercase();
                let mut matches = catalog.symbols.iter().filter(|symbol| {
                    q.package_id.as_ref().is_none_or(|id| &symbol.package_id == id)
                    && (search.is_empty() || symbol.qualified_name.to_lowercase().contains(&search)
                        || symbol.name.to_lowercase().contains(&search))
                }).skip(q.offset);
                let items: Vec<_> = matches.by_ref().take(q.limit).collect();
                let next_offset = matches.next().map(|_| q.offset + items.len());
                let payload = json!({"catalogId":catalog.id,"workspaceRevision":catalog.workspace_revision,
                    "items":items,"nextOffset":next_offset});
                if !state.catalog_snapshot(revision).is_some_and(|current| Arc::ptr_eq(&current, &catalog)) { return Err(dependency_stale()); }
                Ok(payload)
            }).await?;
            Ok((Json(payload).into_response(), catching_up))
        }
        BrowserCoreRoute::DependencySource => {
            let Query(q) =
                Query::<DependencySourceQuery>::try_from_uri(uri).map_err(|_| invalid())?;
            if q.catalog_id.is_empty()
                || q.catalog_id.len() > 8192
                || q.source_ref.is_empty()
                || q.source_ref.len() > 8192
            {
                return Err(invalid());
            }
            let state = runtime.browser_scheduler()?;
            let (payload, catching_up) = selected_catalog_read(runtime, hook, move |r| {
                let revision = r.status().map_err(ApiError::from)?.revision;
                let catalog = selected_catalog(&state, revision, &q.catalog_id)?;
                let source = catalog.sources.get(&q.source_ref).ok_or_else(missing)?;
                let text = source.directory.read_file(&source.path).map_err(rust_source_error)?;
                let hash = hex::encode(Sha256::digest(text.as_bytes()));
                if hash != source.hash { return Err(dependency_stale()); }
                let package = catalog.packages.iter().find(|p| p.id == source.package_id).ok_or_else(missing)?;
                let definitions: Vec<_> = catalog.symbols.iter().filter(|symbol| symbol.source_ref == q.source_ref)
                    .map(|symbol| json!({"id":symbol.id,"name":symbol.name,"kind":symbol.kind,
                        "parent":symbol.parent,"path":symbol.path,"range":symbol.range})).collect();
                let file = SourceFile { path: source.path.clone(), hash: hash.clone(), language: "rust".into(), text };
                let payload = json!({"id":q.source_ref,"rootId":package.id,"rootLabel":package.name,
                    "path":source.path,"hash":hash,"file":file,"definitions":definitions,
                    "warnings":["Definitional candidates only; not confirmed callees. Separate from the workspace graph and source-sharing scope."]});
                if !state.catalog_snapshot(revision).is_some_and(|current| Arc::ptr_eq(&current, &catalog)) { return Err(dependency_stale()); }
                Ok(payload)
            }).await?;
            Ok((Json(payload).into_response(), catching_up))
        }
        BrowserCoreRoute::RustSources => {
            if uri.query().is_some() {
                return Err(invalid());
            }
            let state = runtime.browser_scheduler()?;
            // One configuration version for the whole request (Decision 0009 §6).
            let config = state.config();
            let (payload, catching_up) = selected_catalog_read(runtime, hook, move |_| {
                Ok(json!({"roots":config.rust_sources.iter().map(|r| json!({"id":r.label,"label":r.label,
                    "path":r.directory.root.to_string_lossy()})).collect::<Vec<_>>() }))
            }).await?;
            Ok((Json(payload).into_response(), catching_up))
        }
        BrowserCoreRoute::RustSourceTree => {
            let Query(q) = Query::<RustTreeQuery>::try_from_uri(uri).map_err(|_| invalid())?;
            if !crate::file_tree::valid_path(&q.path)
                || !(1..=200).contains(&q.limit)
                || q.offset > crate::file_tree::SCAN_LIMIT
            {
                return Err(browse_invalid());
            }
            let state = runtime.browser_scheduler()?;
            // One configuration version for the whole request (Decision 0009 §6).
            let config = state.config();
            let (page, catching_up) = selected_catalog_read(runtime, hook, move |r| {
                let revision = r.status().map_err(ApiError::from)?.revision;
                let root = config
                    .rust_sources
                    .iter()
                    .find(|root| root.label == q.root)
                    .ok_or_else(missing)?;
                let (items, next_offset, truncated) = root
                    .directory
                    .list(&q.path, q.offset, q.limit)
                    .map_err(rust_source_error)?;
                Ok(crate::file_tree::Page {
                    root: root.directory.root.to_string_lossy().into_owned(),
                    indexed_workspace: String::new(),
                    path: q.path,
                    revision,
                    items,
                    next_offset,
                    truncated,
                })
            })
            .await?;
            Ok((Json(page).into_response(), catching_up))
        }
        BrowserCoreRoute::RustSourceFile => {
            let Query(q) = Query::<RustFileQuery>::try_from_uri(uri).map_err(|_| invalid())?;
            if q.path.is_empty()
                || !crate::file_tree::valid_path(&q.path)
                || !q.path.ends_with(".rs")
            {
                return Err(browse_invalid());
            }
            let state = runtime.browser_scheduler()?;
            // One configuration version for the whole request (Decision 0009 §6).
            let config = state.config();
            let (file, catching_up) = selected_catalog_read(runtime, hook, move |_| {
                let root = config
                    .rust_sources
                    .iter()
                    .find(|root| root.label == q.root)
                    .ok_or_else(missing)?;
                root.snapshot(&q.path).map_err(rust_source_error)
            })
            .await?;
            Ok((Json(file).into_response(), catching_up))
        }
        BrowserCoreRoute::Status => {
            if uri.query().is_some() {
                return Err(invalid());
            }
            let runtime = runtime.clone();
            let (status, catching_up) = tokio::task::spawn_blocking(move || {
                let (response, catching_up) = runtime.evidence_response()?;
                let mut status = response.status()?;
                status.catching_up = catching_up;
                if let Some(hook) = hook {
                    hook("before_read_finish");
                }
                Ok::<_, anyhow::Error>((response.finish(status)?, catching_up))
            })
            .await
            .map_err(|_| {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "Operation failed",
                )
            })??;
            Ok((Json(status).into_response(), catching_up))
        }
        BrowserCoreRoute::Tree => {
            let Query(q) = Query::<TreeQuery>::try_from_uri(uri).map_err(|_| browse_invalid())?;
            if !crate::file_tree::valid_path(&q.path)
                || !(1..=200).contains(&q.limit)
                || q.offset > crate::file_tree::SCAN_LIMIT
            {
                return Err(browse_invalid());
            }
            let state = runtime.browser_scheduler()?;
            // One configuration version for the whole request (Decision 0009 §6).
            let config = state.config();
            let (page, catching_up) = selected_evidence(runtime, hook, move |response| {
                let (mut items, next_offset, truncated) = config
                    .browser
                    .list(&q.path, q.offset, q.limit)
                    .map_err(BrowseDirectoryError)?;
                let (revision, indexed_workspace) =
                    response.tree_metadata(&config.browser.root, &mut items)?;
                for item in items
                    .iter_mut()
                    .filter(|item| item.kind == "file" && item.indexed_path.is_none())
                {
                    let absolute = config.browser.root.join(&item.path);
                    let reason = if !absolute.starts_with(&indexed_workspace) {
                        "Outside indexed workspace"
                    } else {
                        match absolute.extension().and_then(|e| e.to_str()) {
                            Some("ts" | "tsx") => "TypeScript indexing not supported yet",
                            Some("js" | "mjs" | "cjs" | "rs" | "java" | "py") => {
                                "Not indexed yet (may be excluded or size-limited)"
                            }
                            _ => "Unsupported source type",
                        }
                    };
                    item.unindexed_reason = Some(reason.into());
                }
                Ok(crate::file_tree::Page {
                    root: config.browser.root.to_string_lossy().into_owned(),
                    indexed_workspace,
                    path: q.path,
                    revision,
                    items,
                    next_offset,
                    truncated,
                })
            })
            .await?;
            Ok((Json(page).into_response(), catching_up))
        }
        BrowserCoreRoute::Files => {
            let Query(q) = Query::<FilesQuery>::try_from_uri(uri).map_err(|_| browse_invalid())?;
            if !(1..=200).contains(&q.limit) || q.offset > i64::MAX as usize {
                return Err(browse_invalid());
            }
            let pin = q.pin.pin()?;
            let (value, catching_up) =
                selected_evidence(runtime, hook, move |r| r.files_at(pin, q.offset, q.limit))
                    .await?;
            Ok((Json(value).into_response(), catching_up))
        }
        BrowserCoreRoute::FileSymbols => {
            let Query(q) = Query::<FileSymbolsQuery>::try_from_uri(uri).map_err(|_| invalid())?;
            let pin = q.validate()?;
            let (value, catching_up) = selected_evidence(runtime, hook, move |r| {
                r.file_symbols_at(&q.path, pin, q.offset, q.limit)
            })
            .await?;
            Ok((
                Json(value.ok_or_else(missing)?).into_response(),
                catching_up,
            ))
        }
        BrowserCoreRoute::SyntaxMatches => {
            let q: crate::files_view::SyntaxMatchesRequest =
                serde_json::from_slice(&body).map_err(|_| invalid())?;
            q.validate()?;
            let (value, catching_up) =
                selected_evidence(runtime, hook, move |r| r.syntax_matches_at(&q)).await?;
            Ok((
                Json(value.ok_or_else(missing)?).into_response(),
                catching_up,
            ))
        }
        BrowserCoreRoute::Methods => {
            let Query(q) = Query::<SourceQuery>::try_from_uri(uri).map_err(|_| browse_invalid())?;
            if q.path.is_empty()
                || q.path.len() > 8192
                || q.path.contains(['\0', '\\', ':'])
                || q.path
                    .split('/')
                    .any(|p| p.is_empty() || p == "." || p == "..")
            {
                return Err(browse_invalid());
            }
            let pin = q.pin.pin()?;
            let (value, catching_up) =
                selected_evidence(runtime, hook, move |r| r.methods_at(&q.path, pin)).await?;
            Ok((
                Json(value.ok_or_else(missing)?).into_response(),
                catching_up,
            ))
        }
        BrowserCoreRoute::Classes => {
            let Query(q) = Query::<ClassesQuery>::try_from_uri(uri).map_err(|_| invalid())?;
            let pin = q.pin.pin()?;
            let (value, catching_up) = selected_evidence(runtime, hook, move |r| {
                r.classes_at(q.path.as_deref(), &q.q, pin, q.offset, q.limit)
            })
            .await?;
            Ok((Json(value).into_response(), catching_up))
        }
        BrowserCoreRoute::Sequence => {
            let q: SequenceRequest = serde_json::from_slice(&body).map_err(|_| browse_invalid())?;
            if q.seed.is_empty() || q.seed.len() > 8192 || q.seed.contains('\0') {
                return Err(browse_invalid());
            }
            let (value, catching_up) = selected_evidence(runtime, hook, move |r| {
                r.sequence_at(&q.seed, q.expected_revision, q.show_all)
            })
            .await?;
            Ok((
                Json(value.ok_or_else(missing)?).into_response(),
                catching_up,
            ))
        }
        BrowserCoreRoute::ClassDiagram => {
            let q: crate::class_diagram::ClassDiagramRequest =
                serde_json::from_slice(&body).map_err(|_| invalid())?;
            q.validate()?;
            let (value, catching_up) =
                selected_evidence(runtime, hook, move |r| r.class_diagram_at(&q)).await?;
            Ok((Json(value).into_response(), catching_up))
        }
        BrowserCoreRoute::Navigation => {
            if body.len() > 32 * 1024 {
                return Err(invalid());
            }
            let q: crate::navigation::NavigationRequest =
                serde_json::from_slice(&body).map_err(|_| invalid())?;
            q.validate()?;
            let (value, catching_up) =
                selected_evidence(runtime, hook, move |r| r.navigation_at(&q)).await?;
            Ok((Json(value).into_response(), catching_up))
        }
        BrowserCoreRoute::Source => {
            let Query(q) = Query::<SourceQuery>::try_from_uri(uri).map_err(|_| invalid())?;
            if q.path.is_empty()
                || q.path.len() > 8192
                || q.path.contains(['\0', '\\', ':'])
                || q.path
                    .split('/')
                    .any(|part| part.is_empty() || part == "." || part == "..")
            {
                return Err(invalid());
            }
            let pin = q.pin.pin()?;
            let runtime = runtime.clone();
            let (file, catching_up) = tokio::task::spawn_blocking(move || {
                let (response, catching_up) = runtime.evidence_response()?;
                let file = response.source_at(&q.path, pin)?;
                Ok::<_, anyhow::Error>((response.finish(file)?, catching_up))
            })
            .await
            .map_err(|_| {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "Operation failed",
                )
            })??;
            let (revision, file) = file.ok_or_else(missing)?;
            Ok((
                Json(json!({"revision":revision,"file":file})).into_response(),
                catching_up,
            ))
        }
        BrowserCoreRoute::Index | BrowserCoreRoute::CurrentJob => {
            let state = runtime.browser_scheduler()?;
            let response = if method == Method::POST {
                start_index(State(state), body).await?.into_response()
            } else {
                current_job(State(state)).await?.into_response()
            };
            Ok((response, runtime.catching_up()))
        }
        BrowserCoreRoute::Job => {
            let state = runtime.browser_scheduler()?;
            let tail = suffix.strip_prefix("jobs/").unwrap();
            let response = if method == Method::GET && !tail.contains('/') {
                job(State(state), Path(tail.to_owned()))
                    .await?
                    .into_response()
            } else if method == Method::POST && tail.ends_with("/cancel") {
                let id = tail.strip_suffix("/cancel").unwrap();
                if id.contains('/') {
                    return Err(missing());
                }
                cancel_job(State(state), Path(id.to_owned()))
                    .await?
                    .into_response()
            } else {
                return Err(missing());
            };
            Ok((response, runtime.catching_up()))
        }
    }
}

#[cfg(test)]
mod browser_core_route_tests {
    use super::BrowserCoreRoute;
    use axum::http::Method;

    #[test]
    fn git_checkout_list_refuses_an_oversized_complete_envelope() {
        let row =
            serde_json::json!({"workspaceRoot": "x".repeat(crate::git_status::RESPONSE_LIMIT)});
        let error = super::checkout_list_response(vec![row]).unwrap_err();
        assert_eq!(error.0, axum::http::StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error.1, "response_too_large");
    }

    #[test]
    fn dynamic_routes_match_only_their_supported_method_and_shape() {
        for (method, suffix) in [
            (Method::GET, "git"),
            (Method::GET, "git/diff"),
            (Method::GET, "jobs/id"),
            (Method::POST, "jobs/id/cancel"),
        ] {
            assert!(
                BrowserCoreRoute::parse(&method, suffix).is_some(),
                "{method} {suffix}"
            );
        }
    }
}
