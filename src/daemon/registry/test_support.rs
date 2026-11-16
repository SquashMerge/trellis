//! The `test-support` hooks, gates, clocks and counters of the registry and the checkout
//! runtime.
use super::{CheckoutRegistry, CheckoutRuntime};
use crate::http;
use crate::model::IndexPin;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

impl CheckoutRegistry {
    /// Pause the next activation's startup reconciliation, installed before
    /// its worker starts.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_next_startup_hook_for_tests(&mut self, hook: Arc<dyn Fn() + Send + Sync>) {
        self.test_next_startup_hook = Some(hook);
    }

    /// Hold the next activation's failed-startup backoff open until it
    /// retires, installed before its worker starts.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn hold_next_startup_backoff_for_tests(&mut self) {
        self.test_hold_next_startup_backoff = true;
    }

    /// Every later activation's Architecture owner builds only once a read asks, as before
    /// #798, so a metrics test can time a read that computes its own rows. Installed before its
    /// worker starts.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn architecture_on_demand_for_tests(&mut self) {
        self.test_architecture_on_demand = true;
    }

    /// Observe or pause the next activation's dependency catalog builds,
    /// installed before its worker starts (#700).
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_next_dependency_hook_for_tests(
        &mut self,
        hook: Arc<dyn Fn(&'static str) + Send + Sync>,
    ) {
        self.test_next_dependency_hook = Some(hook);
    }

    /// Give every later activation's watchers real OS delivery, installed before
    /// its worker starts. A test-support build's watchers otherwise take
    /// synthetic ingress only (#703, #720); only a test of real watcher delivery
    /// opts in.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn enable_native_watcher_for_tests(&mut self) {
        self.test_native_watcher = true;
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_after_due_release_for_tests(&mut self, hook: impl FnOnce() + Send + 'static) {
        self.test_after_due_release = Some(Box::new(hook));
    }
}

impl CheckoutRuntime {
    /// Invoked through a weak edge AFTER a queue tick releases all stream and
    /// watcher locks. This is a snapshot of the current verified startup reconciliation owner, not a
    /// claim that startup reconciliation ran again when a serving watcher gets a new epoch.
    #[cfg(feature = "test-support")]
    pub(crate) fn report_causal_startup_ready(&self, scheduler: &Arc<http::DaemonState>) {
        let Some(owner) = scheduler.checkout_serving_owner() else {
            return;
        };
        let Ok(resources) = self.resources() else {
            return;
        };
        if !Arc::ptr_eq(&resources.scheduler, scheduler) {
            return;
        }
        let Some(lineage) = scheduler.causal_lineage_for(&owner) else {
            return;
        };
        if lineage.owner_incarnation != owner.incarnation()
            || scheduler.causal_startup_already_reported(lineage)
            || owner.verify().is_err()
        {
            return;
        }
        let Ok(pin) = resources.store.index_baseline() else {
            return;
        };
        let still_ready = scheduler
            .checkout_serving_owner()
            .is_some_and(|current| Arc::ptr_eq(&current, &owner));
        if still_ready
            && owner.verify().is_ok()
            && scheduler.causal_lineage_for(&owner).is_some_and(|current| {
                current.owner_incarnation == lineage.owner_incarnation
                    && current.watch_epoch == lineage.watch_epoch
                    && current.ordinal == lineage.ordinal
            })
        {
            scheduler.emit_causal_startup_ready_once(lineage, pin);
        }
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn notify_on_terminal_request_for_tests(
        &self,
        id: &str,
        notify: Arc<tokio::sync::Notify>,
    ) -> anyhow::Result<()> {
        self.resources()?
            .store
            .notify_on_terminal_request_for_tests(id, notify);
        Ok(())
    }

    /// Hold the checkout worker at its serial boundary while a fixture edits paths.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn hold_worker_step_for_tests(&self, hold: impl FnOnce()) {
        self.resources()
            .unwrap()
            .scheduler
            .hold_worker_step_for_tests(hold);
    }

    /// Hold the checkout worker with `event` accepted as a pending watcher hint (#703).
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn hold_worker_with_watch_hint_for_tests(
        &self,
        event: notify::Result<notify::Event>,
        hold: impl FnOnce(),
    ) -> bool {
        self.resources()
            .unwrap()
            .scheduler
            .hold_worker_with_watch_hint_for_tests(event, hold)
    }

    /// Make this checkout's next completion COMMIT durable but report an error.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn fail_next_completion_after_commit_for_tests(&self) {
        if let Ok(resources) = self.resources() {
            resources
                .store
                .fail_next_completion_after_commit_for_tests();
        }
    }

    /// Move the retention clock past `pin`'s grace and release it.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn release_past_grace_for_tests(&self, pin: IndexPin) -> anyhow::Result<()> {
        let resources = self.resources()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64;
        resources
            .store
            .set_retention_clock_for_tests(now + 1_800, 1_800);
        let session = resources.scheduler.retained_serving_session()?;
        // The worker's own maintenance can hold the writer: contention is typed and retried.
        let deadline = Instant::now() + crate::testing::HANG_BACKSTOP;
        loop {
            match resources
                .store
                .release_revision(pin, session.writer_guard()?)
            {
                Err(error)
                    if error.to_string().starts_with("storage_busy")
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                other => return other,
            }
        }
    }

    #[cfg(feature = "test-support")]
    pub fn set_after_claim_hook_for_tests(&self, hook: impl FnOnce() + Send + 'static) {
        if let Ok(resources) = self.resources() {
            resources.store.set_after_claim_hook_for_tests(hook);
        }
    }

    /// Pause startup reconciliation after old-root settlement and before it
    /// captures the workspace. The worker runs it on its next attempt.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_startup_hook_for_tests(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        if let Ok(resources) = self.resources() {
            resources.scheduler.set_startup_hook_for_tests(hook);
        }
    }

    /// Pause mandatory call time; share this clock with the test transport.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn freeze_mcp_call_clock_for_tests(&self, origin: Instant) {
        *self.mcp_call_clock_origin.lock().unwrap() = Some(origin);
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn mcp_call_clock_now_for_tests(&self) -> Instant {
        self.mcp_clock_now()
    }

    /// Freeze only optional hint accounting. Mandatory call deadlines keep using real time.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    /// Freeze the optional-hint clock: it then reads the call's acceptance
    /// instant plus the advanced skew, so the hint budget and the call
    /// deadline share one origin however long acceptance took (#131).
    pub fn freeze_mcp_hint_clock_for_tests(&self) {
        *self.mcp_hint_clock_origin.lock().unwrap() = Some(Instant::now());
    }

    /// Hold this checkout's next MCP selected read until `hook` returns.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_mcp_read_hook_for_tests(&self, hook: impl FnOnce() + Send + 'static) {
        *self.mcp_read_hook.lock().unwrap() = Some(Box::new(hook));
    }

    /// Run `gate` at the start of every MCP selected read on this checkout.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_mcp_read_gate_for_tests(&self, gate: Arc<dyn Fn() + Send + Sync>) {
        *self.mcp_read_gate.lock().unwrap() = Some(gate);
    }

    /// Run `gate` after every MCP read admission on this checkout.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_mcp_admitted_gate_for_tests(&self, gate: Arc<dyn Fn() + Send + Sync>) {
        *self.mcp_admitted_gate.lock().unwrap() = Some(gate);
    }

    /// Run `hook` once, in the next evidence admission on this checkout, after it
    /// sampled `catchingUp` and before it opens the selected snapshot.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_evidence_sampled_hook_for_tests(&self, hook: impl FnOnce() + Send + 'static) {
        *self.evidence_sampled_hook.lock().unwrap() = Some(Box::new(hook));
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn run_mcp_admitted_gate_for_tests(&self) {
        let gate = self.mcp_admitted_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate();
        }
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn run_mcp_read_hook_for_tests(&self) {
        let gate = self.mcp_read_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate();
        }
        let hook = self.mcp_read_hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn count_search_hash_scan_for_tests(&self) {
        self.search_hash_scans
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Pending-head cache scans describe and CLI status started (#806).
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn search_coverage_scans_for_tests(&self) -> usize {
        self.search_coverage_scans
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Count status semantic projections a browser status read ran here.
    #[cfg(feature = "test-support")]
    pub(crate) fn count_browser_status_projections(&self, projections: usize) {
        self.browser_status_projections
            .fetch_add(projections, std::sync::atomic::Ordering::Relaxed);
    }

    /// How many status semantic projections the browser's `/status` and
    /// `/status/head` reads have run on this checkout (#666, #700).
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn browser_status_semantic_projections_for_tests(&self) -> usize {
        self.browser_status_projections
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Optional pending-head hash scans MCP search started on this checkout.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn search_hash_scans_for_tests(&self) -> usize {
        self.search_hash_scans
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Advance this checkout's MCP call clock without waiting real time.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn advance_mcp_clock_for_tests(&self, by: Duration) {
        let millis = u64::try_from(by.as_millis()).unwrap_or(u64::MAX);
        self.mcp_clock_skew_ms
            .fetch_add(millis, std::sync::atomic::Ordering::AcqRel);
    }

    /// Inspect this runtime's existing recovery flag in optional MCP expiry fixtures.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn mcp_recovery_pending_for_tests(&self) -> anyhow::Result<bool> {
        Ok(self.resources()?.store.recovery_pending_for_tests())
    }

    #[cfg(feature = "test-support")]
    pub fn mcp_busy_reads_for_tests(&self, count: usize) -> anyhow::Result<()> {
        self.resources()?.store.mcp_busy_reads_for_tests(count);
        Ok(())
    }

    /// One admitted read, one projection, and one final selected-read fence.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn metrics_memo_entries_for_tests(&self) -> anyhow::Result<usize> {
        Ok(self.resources()?.store.metrics_memo_entries_for_tests())
    }

    /// Wait for the head's core, whatever its outcome, once the head has settled.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn await_architecture_for_tests(&self) {
        let deadline = Instant::now() + crate::testing::HANG_BACKSTOP;
        while Instant::now() < deadline {
            let seen = self.state_version();
            let target = (|| {
                let resources = self.resources()?;
                let (response, catching_up) = self.evidence_response()?;
                let pin = response.selected_pin(None)?;
                let root = Path::new(resources.store.workspace_root());
                let hash = crate::rules::load(root)
                    .ok()
                    .flatten()
                    .map(|config| config.content_hash);
                response.finish((pin, hash, catching_up))
            })();
            let Ok((pin, hash, catching_up)) = target else {
                self.wait_state_change(seen, Duration::from_millis(50));
                continue;
            };
            let scheduler = self.browser_scheduler().expect("active runtime");
            let wait = deadline.saturating_duration_since(Instant::now());
            let outcome = scheduler.architecture_core(
                pin,
                crate::http::architecture::CoreRules::Exactly(hash.as_deref()),
                wait,
                &mut || Ok(()),
            );
            let building = outcome.as_ref().err().is_some_and(|error| {
                error
                    .downcast_ref::<crate::http::architecture::ArchitectureBuilding>()
                    .is_some()
            });
            if !building && !catching_up {
                return;
            }
            // The head moved past this pin, or startup is still going: sample again.
            self.wait_state_change(seen, Duration::from_millis(50));
        }
        panic!("the checkout did not settle");
    }
}
