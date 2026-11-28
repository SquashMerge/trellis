//! `DaemonState`: its constructors and its methods. The struct and its fields stay in
//! `http.rs`.

use super::*;

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
    let fifo = Arc::new(worker::FifoPublication::default());
    let watch_status = Arc::new(Mutex::new(None));
    let queue_waker: Arc<dyn Fn() + Send + Sync> = {
        let inbox = inbox.clone();
        let fifo = fifo.clone();
        let watch_status = watch_status.clone();
        Arc::new(move || {
            fifo.accept(&watch_status);
            inbox.wake(worker::WakeSource::Queue);
        })
    };
    let watch_waker: Arc<dyn Fn() + Send + Sync> = {
        let inbox = inbox.clone();
        Arc::new(move || inbox.wake(worker::WakeSource::Watch))
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
        watch_status,
        leases: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        fifo,
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
        #[cfg(feature = "test-support")]
        dependency_hook: Mutex::new(None),
        #[cfg(feature = "test-support")]
        native_watcher: AtomicBool::new(crate::testing::native_watcher_requested()),
        runtime: Mutex::new(RuntimeState::Activating),
        startup_error: Mutex::new(None),
        startup_failures: AtomicU64::new(0),
        recovery_disposition: Mutex::new(None),
        recovery_blocked: Mutex::new(None),
        #[cfg(test)]
        test_worker_now: Mutex::new(None),
        #[cfg(test)]
        test_startup_error: Mutex::new(None),
        startup_backoff: Mutex::new(STARTUP_BACKOFF_FIRST),
        dependencies: Mutex::new(DependencyIndex {
            generation: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            stopped: false,
            worker_running: false,
            worker: None,
            state: if config.dependency_options.is_some() {
                "loading"
            } else {
                "disabled"
            },
            catalog: None,
            warnings: Vec::new(),
        }),
        #[cfg(test)]
        dependency_idle: tokio::sync::Notify::new(),
        #[cfg(test)]
        dependency_build_hook: Mutex::new(None),
        #[cfg(test)]
        dependency_stop_hook: Mutex::new(None),
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
        search_fill: Mutex::new(None),
        architecture: architecture::ArchitectureHandle(architecture::ArchitectureOwner::new()),
        watcher_cancel: Arc::new(AtomicBool::new(false)),
        #[cfg(feature = "test-support")]
        startup_hook: Mutex::new(None),
        #[cfg(feature = "test-support")]
        hold_startup_backoff: AtomicBool::new(false),
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
        #[cfg(feature = "test-support")]
        causal_watch_reported: Mutex::new(None),
        recovery_retry_after: Mutex::new(None),
        watcher_health: Arc::new(crate::watch::WatcherHealth::default()),
        retention_last_run: Mutex::new(Instant::now()),
        #[cfg(feature = "test-support")]
        timed_work_delayed: AtomicBool::new(false),
        startup_release_pass: AtomicBool::new(false),
        gc_last_check: Mutex::new(Instant::now() - Duration::from_secs(3600)),
        gc_run: Mutex::new(None),
        gc_run_next: AtomicU64::new(0),
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
        #[cfg(test)]
        test_session_change_before_watch_status: crate::store::TestOneShotHook::default(),
        #[cfg(test)]
        test_watch_pending_before_state: crate::store::TestOneShotHook::default(),
        #[cfg(test)]
        test_quiet_between_reads: crate::store::TestOneShotHook::default(),
        #[cfg(test)]
        test_fifo_between_observation_and_publication: crate::store::TestOneShotHook::default(),
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
        self.inbox.wake(worker::WakeSource::Test);
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

    pub(super) fn retired(&self) -> bool {
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

    /// Accept `event` through the watcher's ingress while holding the worker step,
    /// so the hint stays pending until `hold` returns (#703). False without a
    /// watcher, and then `hold` does not run.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn hold_worker_with_watch_hint_for_tests(
        &self,
        event: notify::Result<notify::Event>,
        hold: impl FnOnce(),
    ) -> bool {
        let _step = self.step_lock.lock().unwrap();
        let work = self.watcher_work.lock().unwrap();
        let Some((_, scheduler)) = work.as_ref() else {
            return false;
        };
        scheduler.submit_watch_event_for_tests(event);
        hold();
        true
    }

    /// Install every later watcher with asynchronous OS delivery, for a test of
    /// real watcher delivery. Call it before startup begins. Without it a
    /// test-support build's watchers take synthetic ingress only (#703, #720).
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn enable_native_watcher_for_tests(&self) {
        self.native_watcher.store(true, Ordering::Release);
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
        // The status lock is released before the runtime state is read. A session change
        // holds the state while it clears this status, so a reader that kept the status
        // while it waited for the state would deadlock with the worker (#1078).
        let published = self
            .watch_status
            .lock()
            .unwrap()
            .as_ref()
            .map(worker::WatchStatus::pending);
        if let Some(pending) = published {
            return pending;
        }
        #[cfg(test)]
        self.test_watch_pending_before_state.run();
        // A serving session's first watcher has not run its full inventory.
        self.runtime.lock().unwrap().session().is_some()
    }

    /// Whether the checkout is `Ready` with nothing pending: no accepted FIFO work, and a
    /// watcher of the current serving session whose published status holds no
    /// unacknowledged intent. It is the admission condition of an automatic producer run
    /// (#1082).
    ///
    /// One coherent read, unlike separate samples of the phase, the FIFO flag and
    /// [`Self::checkout_watch_pending`]:
    /// - The runtime state is held while the status is read, and a session change clears the
    ///   status under that lock, so a published status belongs to the session seen here. A
    ///   runtime with no serving session, or a session whose watcher has not published yet,
    ///   has an inventory ahead of it and is never quiet.
    /// - The FIFO flag and the watch status are both published under the watch status lock,
    ///   which is held across both reads. A request accepted, or a hint acknowledged, while
    ///   this read runs is published wholly before it or wholly after it, never between.
    ///
    /// The watcher's live ingress generation is not published under a lock: a hint accepted
    /// after this read is a later event, which the worker publishes.
    ///
    /// Lock order: the runtime state, then the watch status, as `set_runtime_state` and
    /// `request_retire` take them. Never the reverse (#1078).
    pub(crate) fn ready_and_quiet(&self) -> bool {
        let state = self.runtime.lock().unwrap();
        if !matches!(*state, RuntimeState::Ready(_)) {
            return false;
        }
        let status = self.watch_status.lock().unwrap();
        if self.fifo_pending() {
            return false;
        }
        #[cfg(test)]
        self.test_quiet_between_reads.run();
        status.as_ref().is_some_and(|status| !status.pending())
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

    /// [`Self::submit_watch_event_for_tests`], with the hint due at once: the
    /// worker's next iteration reconciles it without a debounce iteration first.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn submit_due_watch_event_for_tests(&self, event: notify::Result<notify::Event>) -> bool {
        let mut work = self.watcher_work.lock().unwrap();
        let Some((_, scheduler)) = work.as_mut() else {
            return false;
        };
        scheduler.submit_watch_event_for_tests(event);
        scheduler.force_periodic_inventory_for_tests();
        true
    }

    /// Make the periodic inventory due now, with no hint accepted, and wake the
    /// worker to run it. False without a watcher.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn run_periodic_inventory_for_tests(&self) -> bool {
        {
            let mut work = self.watcher_work.lock().unwrap();
            let Some((_, scheduler)) = work.as_mut() else {
                return false;
            };
            scheduler.force_periodic_inventory_for_tests();
        }
        self.inbox.wake(worker::WakeSource::Test);
        true
    }

    /// Run `hook` once on the worker, after its next watcher reconciliation has
    /// published and before it acknowledges the hint. False without a watcher.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_before_watch_ack_hook_for_tests(
        &self,
        hook: impl FnOnce(&dyn Fn(notify::Event)) + Send + 'static,
    ) -> bool {
        let mut work = self.watcher_work.lock().unwrap();
        let Some((_, watcher)) = work.as_mut() else {
            return false;
        };
        watcher.set_before_ack_hook_for_tests(hook);
        true
    }

    /// Run `hook` once on the worker, after its next watcher reconciliation
    /// returns and before it publishes the watch status. False without a watcher.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_after_watch_reconcile_hook_for_tests(
        &self,
        hook: impl FnOnce() + Send + 'static,
    ) -> bool {
        let mut work = self.watcher_work.lock().unwrap();
        let Some((_, watcher)) = work.as_mut() else {
            return false;
        };
        watcher.set_after_reconcile_hook_for_tests(hook);
        true
    }

    /// Select explicit index options for the watcher, as a `serve` registration
    /// applied in place does, and wake the worker.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn select_watch_options_for_tests(&self, options: IndexOptions) {
        *self.explicit_pending.lock().unwrap() = Some(Arc::new(options));
        self.inbox.wake(worker::WakeSource::Test);
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
    /// Lose the serving session as a failed completion does: its watcher and published
    /// watch status go with it, and the runtime returns to startup reconciliation.
    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn lose_serving_session_for_tests(&self) {
        self.replace_serving_session(None);
    }

    /// Run `hook` once on the worker, after a step's FIFO drain and before it installs or
    /// reconciles the serving session's watcher.
    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn set_after_startup_drain_hook_for_tests(
        &self,
        hook: impl FnOnce() + Send + 'static,
    ) {
        self.test_queue_after_startup_drain.set(hook);
    }

    /// Losing the serving session sends the runtime back to startup
    /// reconciliation (Decision 0009 §2); gaining one makes it Ready.
    pub(super) fn replace_serving_session(
        &self,
        next: Option<Arc<crate::store::topology::WriterSession>>,
    ) {
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
            #[cfg(test)]
            self.test_session_change_before_watch_status.run();
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
    pub(super) fn maintenance_priority_reason(
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
        if self.fifo.pending.load(Ordering::Acquire)
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
        // A yield records no completed scan (#1316): the next hourly tick, past
        // the attempt fence, resumes the remaining candidates.
        *self.gc_last_check.lock().unwrap() = Instant::now();
        match result {
            Ok(_) => Ok(()),
            Err(error) if error.is::<GcPriorityYield>() => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// `trellis gc --run` (#1316): submit one GC pass to this checkout's worker,
    /// the only GC runner, and wait up to `wait` for its report. One request is
    /// pending at a time. The waiter withdraws only its own ticket, on every exit.
    pub fn run_gc_now(
        &self,
        wait: Duration,
    ) -> anyhow::Result<crate::store::topology::GcRunReport> {
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        let id = self.gc_run_next.fetch_add(1, Ordering::AcqRel);
        {
            let mut slot = self.gc_run.lock().unwrap();
            anyhow::ensure!(
                slot.is_none(),
                "storage_busy: another gc --run request is pending"
            );
            *slot = Some(GcRunTicket { id, reply });
        }
        let _withdraw = WithdrawGcRun {
            slot: &self.gc_run,
            id,
        };
        self.inbox.wake(worker::WakeSource::Lifecycle);
        match answer.recv_timeout(wait) {
            Ok(report) => report,
            Err(_) => anyhow::bail!("deadline_exceeded: the checkout worker did not run gc --run"),
        }
    }

    /// Run a submitted `gc --run` pass, on the worker between steps, once the
    /// runtime is Ready. It holds the step like any unit and never yields: the
    /// user asked for this pass.
    pub(super) fn run_requested_gc(&self) {
        let session = match &*self.runtime.lock().unwrap() {
            RuntimeState::Ready(session) => session.clone(),
            _ => return,
        };
        let Some(ticket) = self.gc_run.lock().unwrap().take() else {
            return;
        };
        let _step = self.step_lock.lock().unwrap();
        let report = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.store.gc_run(session.writer_guard()?)
        }))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("worker_join: gc --run panicked")));
        // The waiter may have given up; its ticket is answered either way.
        let _ = ticket.reply.try_send(report);
    }

    /// A retiring worker answers a pending `gc --run` rather than leaving it to
    /// its waiter's deadline.
    pub(super) fn fail_requested_gc(&self) {
        if let Some(ticket) = self.gc_run.lock().unwrap().take() {
            let _ = ticket
                .reply
                .try_send(Err(anyhow::anyhow!("index_not_ready: checkout released")));
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
                if self.startup_release_pass.swap(false, Ordering::AcqRel) {
                    return Ok(());
                }
            }
            Err(error) => {
                *self.retention_last_run.lock().unwrap() = Instant::now();
                self.startup_release_pass.store(false, Ordering::Release);
                return Err(error);
            }
        }
        // The pass after reconciliation only settles owed releases (#1291): GC
        // keeps its cadence, first checked one interval after that pass.
        if self.startup_release_pass.load(Ordering::Acquire) {
            return Ok(());
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
                    // A late OS event from before this forced fixture tick must
                    // not defer pin expiration as accepted watcher work (#131).
                    let mut scheduler =
                        crate::index_coordinator::WatcherWork::publishing_with_native(
                            &self.store,
                            &session,
                            &options,
                            self.watcher_health.clone(),
                            false,
                        )?;
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
                self.finalize_watcher_gate(explicit.as_ref());
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

    pub(crate) fn set_search_fill(
        &self,
        registration: crate::daemon::search_fill::FillRegistration,
    ) {
        *self.search_fill.lock().unwrap() = Some(registration);
    }

    pub(crate) fn search_fill_building(&self, pin: IndexPin) -> bool {
        self.search_fill
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|fill| fill.building_for(pin))
    }

    pub(crate) fn search_root_key(&self) -> String {
        self.store.root_key().to_owned()
    }

    #[cfg(feature = "test-support")]
    pub fn search_head_slot_for_tests(
        &self,
    ) -> Option<std::sync::Arc<crate::daemon::search_head::HeadSlot>> {
        self.search_fill
            .lock()
            .unwrap()
            .as_ref()
            .map(|fill| fill.head_slot())
    }

    pub(crate) fn search_head_slot(
        &self,
        request: bool,
    ) -> Option<std::sync::Arc<crate::daemon::search_head::HeadSlot>> {
        let slot = self
            .search_fill
            .lock()
            .unwrap()
            .as_ref()
            .map(|fill| fill.head_slot());
        if request && let Some(slot) = &slot {
            slot.request();
        }
        slot
    }

    pub(crate) fn search_head_update(
        &self,
        previous: Option<IndexPin>,
        admit: &mut dyn FnMut(usize, usize, bool) -> anyhow::Result<()>,
        check: &mut dyn FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<Option<crate::daemon::search_head::HeadUpdate>> {
        use crate::daemon::search_head::{HeadChange, HeadUpdate};
        if !matches!(*self.runtime.lock().unwrap(), RuntimeState::Ready(_)) {
            return Err(crate::daemon::search_fill::SearchDeferred.into());
        }
        let response = self.store.evidence_response()?;
        let pin = response.status()?.revision;
        let mut check = || check();
        let update = match previous {
            Some(previous) if previous == pin => HeadUpdate {
                pin,
                declarations: response.mcp_search_head_count(pin, &mut check)?,
                change: HeadChange::Delta {
                    removed_version_ids: Vec::new(),
                    added: std::sync::Arc::from([]),
                },
            },
            Some(previous) => match response.mcp_search_head_delta_admitted(
                previous,
                pin,
                &mut check,
                &mut |added, removed| admit(added, removed, false),
            ) {
                Ok(delta) => HeadUpdate {
                    pin: delta.pin,
                    declarations: delta.head_card_count,
                    change: HeadChange::Delta {
                        removed_version_ids: delta.removed_version_ids,
                        added: delta.added,
                    },
                },
                Err(error) if error.is::<crate::store::SearchDeltaColdRebuild>() => {
                    let (pin, cards) =
                        response.mcp_search_head_admitted(Some(pin), &mut check, &mut |count| {
                            admit(count, 0, true)
                        })?;
                    HeadUpdate {
                        pin,
                        declarations: cards.len(),
                        change: HeadChange::Full(cards),
                    }
                }
                Err(error) => return Err(error),
            },
            None => {
                let (pin, cards) =
                    response.mcp_search_head_admitted(Some(pin), &mut check, &mut |count| {
                        admit(count, 0, true)
                    })?;
                HeadUpdate {
                    pin,
                    declarations: cards.len(),
                    change: HeadChange::Full(cards),
                }
            }
        };
        response.finish(())?;
        Ok(Some(update))
    }

    pub(crate) fn search_fill_page(
        &self,
        expected: Option<IndexPin>,
        after: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<crate::store::HeadSearchCards> {
        // A fill is not a read admission lease: disposable work must never
        // cancel retirement or retain a writer session while loading weights.
        if !matches!(*self.runtime.lock().unwrap(), RuntimeState::Ready(_)) {
            return Err(crate::daemon::search_fill::SearchDeferred.into());
        }
        self.store.head_search_cards(expected, after, limit)
    }

    pub(super) fn worker_step(self: &Arc<Self>) -> anyhow::Result<()> {
        #[cfg(feature = "test-support")]
        let _held = self.worker_hold.read().unwrap();
        let result = self.worker_step_inner();
        self.publish_fifo_pending();
        // A path that replaced the watcher's signals without publishing its
        // status (a FIFO or import drain, an error exit) publishes it here.
        {
            let work = self.watcher_work.lock().unwrap();
            if let Some((_, watcher)) = work.as_ref()
                && watcher.status_stale()
            {
                self.publish_watch_status(watcher, &watcher.options());
            }
        }
        // The authoritative worker has released its step/watcher/publication
        // guards. The fill owner only receives a coalescing in-memory hint.
        if let Some(fill) = self.search_fill.lock().unwrap().as_ref() {
            fill.notify();
        }
        self.architecture_hint_after_step();
        #[cfg(feature = "test-support")]
        if result.is_ok() {
            self.report_causal_startup_ready();
            self.report_causal_watch_ack();
        }
        result
    }

    fn worker_step_inner(self: &Arc<Self>) -> anyhow::Result<()> {
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
            .is_some_and(|retry| self.worker_now() < retry)
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
            self.start_dependency_index();
        }
        let retained = self.runtime.lock().unwrap().session().cloned();
        if let Some(ref session) = retained
            && session.verify().is_ok()
        {
            #[cfg(all(feature = "test-support", unix))]
            if let Some(path) = std::env::var_os("TRELLIS_TEST_RETAINED_AFTER_STARTUP_SOCKET") {
                let path = std::path::PathBuf::from(path);
                if !path.exists() {
                    let listener = std::os::unix::net::UnixListener::bind(&path)?;
                    std::fs::write(path.with_extension("ready"), [])?;
                    let (_stream, _) = listener.accept()?;
                }
            }
            crate::index_coordinator::enqueue_retained_runs(&self.store)?;
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
                        #[cfg(feature = "test-support")]
                        let native = self.native_watcher.load(Ordering::Acquire);
                        #[cfg(not(feature = "test-support"))]
                        let native = true;
                        let mut scheduler =
                            crate::index_coordinator::WatcherWork::publishing_with_native(
                                &self.store,
                                session,
                                &selected_options,
                                self.watcher_health.clone(),
                                native,
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
                        #[cfg(feature = "test-support")]
                        if let Some(hook) = scheduler.take_after_reconcile_hook_for_tests() {
                            hook();
                        }
                        self.publish_watch_status(scheduler, &selected_options);
                        let accounted = reconciled?;
                        if accounted {
                            self.finalize_watcher_gate(explicit.as_ref());
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
    pub(super) fn finalize_watcher_gate(&self, explicit: Option<&Arc<IndexOptions>>) {
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
    }

    /// Certify the public status only after the owning step publishes it. The
    /// request sequence prevents a same-pin startup ACK certifying a later no-op.
    #[cfg(feature = "test-support")]
    fn report_causal_watch_ack(&self) {
        let Some(reporter) = self.causal_witness.lock().unwrap().clone() else {
            return;
        };
        if self.fifo_pending() || self.checkout_watch_pending() {
            return;
        }
        let explicit = self.explicit_pending.lock().unwrap().clone();
        let Ok(selected) = self.selected_watch_options(explicit.as_ref()) else {
            return;
        };
        let guard = self.watcher_work.lock().unwrap();
        let Some((_, work)) = guard.as_ref() else {
            return;
        };
        if work.accepted_watch_intent(&selected)
            || !matches!(self.store.earliest_unfinished_request(), Ok(None))
        {
            return;
        }
        let (Some(lineage), Some(generation), Some(pin)) = (
            work.causal_lineage(),
            work.accounted_watch_generation(),
            work.accounted_watch_pin(),
        ) else {
            return;
        };
        let Ok(request) = self.store.current_request() else {
            return;
        };
        if request
            .as_ref()
            .is_some_and(|row| row.finished_at.is_none())
        {
            return;
        }
        let request_seq = request.map_or(0, |row| row.seq);
        let Ok(session) = self.retained_serving_session() else {
            return;
        };
        if lineage.owner_incarnation != session.incarnation()
            || self.store.verify_writer_session(&session).is_err()
            || !self
                .store
                .index_baseline()
                .is_ok_and(|current| current == pin)
            || self.store.verify_writer_session(&session).is_err()
        {
            return;
        }
        let ack = crate::daemon::causal_witness::WatchAck {
            lineage,
            generation,
            pin,
            request_seq,
        };
        let mut reported = self.causal_watch_reported.lock().unwrap();
        if reported.as_ref() != Some(&ack) {
            reporter.watch_ack(lineage, generation, pin, true, request_seq);
            *reported = Some(ack);
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
        let mut index = self.dependencies.lock().unwrap();
        if !Arc::ptr_eq(&self.config(), config) || config.dependency_options.is_none() {
            return false;
        }
        if !index.request_build() {
            return !index.stopped;
        }
        let state = self.clone();
        // Install the handle under the admission lock, so close cannot miss a
        // newly admitted driver. No Tokio progress is needed to join it.
        match std::thread::Builder::new()
            .name("trellis-catalog".into())
            .spawn(move || state.run_dependency_index())
        {
            Ok(worker) => {
                index.worker = Some(worker);
                true
            }
            Err(_) => {
                index.worker_running = false;
                index.state = "failed";
                index.warnings = vec!["Dependency catalog worker could not start".into()];
                false
            }
        }
    }

    fn run_dependency_index(&self) {
        loop {
            let (generation, cancel, config) = {
                let mut index = self.dependencies.lock().unwrap();
                let config = self.config();
                if index.stopped || config.dependency_options.is_none() {
                    index.worker_running = false;
                    #[cfg(test)]
                    self.dependency_idle.notify_one();
                    return;
                }
                (index.generation, index.cancel.clone(), config)
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let result = (|| {
                    // Configuration, generation and cancellation were captured
                    // together before this blocking build was admitted.
                    let options = config
                        .dependency_options
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("dependency catalog disabled"))?;
                    #[cfg(feature = "test-support")]
                    self.run_dependency_hook("before_revision");
                    let revision = self.store.status()?.revision;
                    #[cfg(test)]
                    {
                        let hook = self.dependency_build_hook.lock().unwrap().take();
                        if let Some(hook) = hook {
                            hook();
                        }
                    }
                    Catalog::build(&config.options.workspace_root, revision, &options, &cancel)
                })();
                self.publish_dependency_index(generation, &cancel, result);
            }));
            if result.is_err() {
                // Error publication does not access the database.
                self.publish_dependency_index(
                    generation,
                    &cancel,
                    Err(anyhow::anyhow!("worker failed")),
                );
            }
            #[cfg(feature = "test-support")]
            self.run_dependency_hook("published");
            let mut index = self.dependencies.lock().unwrap();
            if index.stopped || index.generation == generation {
                index.worker_running = false;
                #[cfg(test)]
                self.dependency_idle.notify_one();
                return;
            }
            // Refresh bursts coalesce to the latest generation. Only this driver
            // can launch catalog work, and its previous build has exited.
        }
    }
    /// Observe or pause catalog builds (#700). Installed before startup begins.
    #[cfg(feature = "test-support")]
    pub(crate) fn set_dependency_hook_for_tests(&self, hook: DependencyHook) {
        *self.dependency_hook.lock().unwrap() = Some(hook);
    }
    #[cfg(feature = "test-support")]
    fn run_dependency_hook(&self, phase: &'static str) {
        let hook = self.dependency_hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(phase);
        }
    }
    pub(super) fn publish_dependency_index(
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
        let worker = {
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
            dependencies.worker.take()
        };
        #[cfg(test)]
        if let Some(hook) = self.dependency_stop_hook.lock().unwrap().take() {
            hook();
        }
        // The driver needs the catalog lock to finish. Join before close drops
        // SQLite witnesses, and never while holding that lock.
        if let Some(worker) = worker {
            let _ = worker.join();
        }
        self.jobs
            .lock()
            .unwrap()
            .cancel
            .store(true, Ordering::Release);
    }
}

/// One submitted `trellis gc --run` pass and where its report goes (#1316).
pub(super) struct GcRunTicket {
    id: u64,
    reply: std::sync::mpsc::SyncSender<anyhow::Result<crate::store::topology::GcRunReport>>,
}

/// Clears the pending ticket only if it is still the waiter's own.
struct WithdrawGcRun<'a> {
    slot: &'a Mutex<Option<GcRunTicket>>,
    id: u64,
}
impl Drop for WithdrawGcRun<'_> {
    fn drop(&mut self) {
        let mut slot = self.slot.lock().unwrap_or_else(|error| error.into_inner());
        if slot.as_ref().is_some_and(|ticket| ticket.id == self.id) {
            *slot = None;
        }
    }
}
