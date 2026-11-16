//! The checkout runtime's CLI calls, except the producer run path, which stays in the
//! parent.
use super::CheckoutRuntime;
use crate::indexer::IndexOptions;
use std::time::{Duration, Instant};

/// A waiter does not change an accepted row on contention. Only direct,
/// transient lock failures are retried or converted to a typed expiry.
pub(super) fn cli_index_contention(error: anyhow::Error, deadline: Instant) -> anyhow::Result<()> {
    if !crate::store::transient_storage_contention(&error)
        || !crate::store::cli_read_retryable_contention(&error)
    {
        return Err(error);
    }
    if Instant::now() >= deadline {
        return Err(crate::store::topology::StorageBusy.into());
    }
    Ok(())
}

pub(super) fn cli_index_request(
    mut read: impl FnMut() -> anyhow::Result<Option<crate::store::requests::Request>>,
    deadline: Instant,
) -> anyhow::Result<Option<crate::store::requests::Request>> {
    loop {
        match read() {
            Ok(row) => return Ok(row),
            Err(error) => {
                cli_index_contention(error, deadline)?;
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// The CLI error for a failed request: its code first, then the recorded reason, if any.
fn failed_request_error(
    row: &crate::store::requests::Request,
    fallback: &str,
    suffix: &str,
) -> anyhow::Error {
    let (code, reason) =
        crate::store::requests::split_error(row.error_code.as_deref().unwrap_or(fallback));
    match reason {
        Some(reason) => anyhow::anyhow!("{code}: request {} failed: {reason}{suffix}", row.id),
        None => anyhow::anyhow!("{code}: request {} failed{suffix}", row.id),
    }
}

impl CheckoutRuntime {
    /// CLI calls share the activated checkout's Store and scheduler; they never
    /// reopen a second Store or independently elect a leader.
    pub fn cli_status(&self) -> anyhow::Result<serde_json::Value> {
        let resources = self.resources()?;
        resources.store.verify_root()?;
        if let Some(blocker) = resources.scheduler.recovery_blocker() {
            anyhow::bail!("{blocker}");
        }
        let (response, catching_up) = self.evidence_response()?;
        let mut status = response.status()?;
        crate::store::relabel_status(&mut status, &self.auto_run.labels());
        status.catching_up = catching_up;
        status.watcher = Some(self.resources()?.scheduler.checkout_watcher_witness());
        let search_head = self.search_head_coverage(&response, status.revision, &mut || Ok(()))?;
        let mut status = serde_json::to_value(response.finish(status)?)?;
        status["build"] = serde_json::to_value(crate::build_info::current())?;
        status["searchIndex"] =
            serde_json::to_value(self.search_availability().with_head(search_head))?;
        Ok(status)
    }

    /// The plan of one producer run. Reads and hashes files and the retained
    /// native revision only; it executes nothing and creates no state.
    pub fn cli_producer_plan(&self, args: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let request = crate::producer::plan::PlanRequest::parse(args)?;
        let resources = self.resources()?;
        let _lease = resources.scheduler.admission_lease()?;
        resources.store.verify_root()?;
        let stub = crate::producer::profiles::Stub::from_env();
        crate::producer::plan::plan(&resources.store, &request, stub.as_ref())
    }

    /// Producer detection, trust-profile availability and run state.
    pub fn cli_producer_status(
        &self,
        args: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let request = crate::producer::plan::StatusRequest::parse(args)?;
        let resources = self.resources()?;
        let _lease = resources.scheduler.admission_lease()?;
        resources.store.verify_root()?;
        let stub = crate::producer::profiles::Stub::from_env();
        let mut status = crate::producer::plan::status(&resources.store, &request, stub.as_ref())?;
        let status_run = self.producer.status();
        status["activeRun"] = status_run.clone();
        if let Some(producers) = status["producers"].as_array_mut() {
            for producer in producers {
                let name = producer["name"].as_str().unwrap_or_default().to_owned();
                producer["pickup"] = self.pickup.status(&resources.store, &name);
                producer["autoRun"] = self.auto_run_status(&name, &status_run);
            }
        }
        Ok(status)
    }

    /// `autoRun` of one producer in the status reply (#660). While the producer's automatic run
    /// is active, `lastRunId` is that run's.
    fn auto_run_status(&self, producer: &str, active_run: &serde_json::Value) -> serde_json::Value {
        use crate::producer::auto::AutoState;
        let status = self.auto_run.status(producer);
        let running = status.state == AutoState::Running
            && active_run["origin"] == "auto"
            && active_run["producer"] == producer;
        let last_run_id = if running {
            active_run["runId"].clone()
        } else {
            serde_json::json!(status.last_run_id)
        };
        serde_json::json!({
            "enabled": status.enabled,
            "state": status.state.as_str(),
            "reason": status.reason,
            "dueSince": status.due_since,
            "nextEarliestStart": status.next_earliest_start,
            "lastRunId": last_run_id,
        })
    }

    /// Cancel a run that has not yet enqueued its import.
    pub fn cli_producer_cancel(
        &self,
        args: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let request = crate::producer::run::CancelRequest::parse(args)?;
        let resources = self.resources()?;
        let _lease = resources.scheduler.admission_lease()?;
        resources.store.verify_root()?;
        self.producer.cancel(&resources.store, &request.run_id)
    }

    /// Configure or clear an owner artifact pickup. `--artifact` imports at once and replies
    /// with the typed import outcome; nothing here runs a producer.
    pub fn cli_producer_pickup(
        &self,
        args: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let request = crate::producer::pickup::PickupRequest::parse(args)?;
        let resources = self.resources()?;
        resources.store.verify_root()?;
        match &request.artifact {
            Some(artifact) => self.pickup.set(
                &resources.store,
                &*resources.scheduler,
                &request.producer,
                artifact,
            ),
            None => self.pickup.clear(&resources.store, &request.producer),
        }
    }

    pub fn cli_symbols(&self, search: &str, limit: usize) -> anyhow::Result<serde_json::Value> {
        anyhow::ensure!((1..=150).contains(&limit), "limit must be 1..150");
        let (response, catching_up) = self.evidence_response()?;
        // Corruption found by this read schedules the re-index, as on every other selected
        // read (docs/local-topology.md, #261); the caller still gets the typed refusal (#903).
        let (revision, items) = response
            .symbols_at(search, limit)
            .map_err(|error| response.report_index_failure(error))?;
        response.finish(())?;
        Ok(serde_json::json!({"catchingUp": catching_up,
            "data": {"revision": revision, "items": items}}))
    }

    pub fn cli_query(&self, query: &crate::model::ViewQuery) -> anyhow::Result<serde_json::Value> {
        query.validate()?;
        let (response, catching_up) = self.evidence_response()?;
        let view = response
            .query_view(query)
            .map_err(|error| response.report_index_failure(error))?
            .ok_or_else(|| anyhow::anyhow!("seed not found in current index"))?;
        Ok(serde_json::json!({"catchingUp": catching_up,
            "data": response.finish(view)?}))
    }

    pub fn cli_index(&self, options: &IndexOptions) -> anyhow::Result<serde_json::Value> {
        // Serialize acceptance with idle retirement. A caller may have cloned
        // this runtime before serve entered the registry gate: it must not
        // enqueue against resources the replacement has already retired.
        let resources = self.resources()?;
        // Durable acceptance is the single requests.db COMMIT. Only the
        // checkout worker may reconcile and claim this row. A CLI waiter
        // never creates a second writer or re-enqueues an uncertain job.
        let accepted = resources.scheduler.enqueue_index(options, None)?;
        let mut busy_deadline = Instant::now() + Duration::from_secs(30);
        loop {
            // Record the terminal-commit count before re-reading, so a COMMIT
            // between the read and the wait is never missed (Decision 0009 §8).
            let seen = resources.store.terminal_generation();
            let row = cli_index_request(
                || resources.store.request_by_id(&accepted.id),
                busy_deadline,
            )?;
            busy_deadline = Instant::now() + Duration::from_secs(30);
            let row =
                row.ok_or_else(|| anyhow::anyhow!("storage_busy: accepted request disappeared"))?;
            match row.state.as_str() {
                "done" => {
                    let revision = row.revision.ok_or_else(|| {
                        anyhow::anyhow!("store_unavailable: completed request missing pin")
                    })?;
                    let (response, catching_up) = self.evidence_response()?;
                    response.validate_pin(revision)?;
                    let mut status = response.status()?;
                    status.catching_up = catching_up;
                    let status = response.finish(status)?;
                    return Ok(
                        serde_json::json!({"publishedRevision": revision, "status": status}),
                    );
                }
                "failed" => {
                    return Err(failed_request_error(
                        &row,
                        "store_unavailable",
                        "; inspect job status",
                    ));
                }
                "queued" | "running" => {
                    crate::index_coordinator::diagnostic_marker(
                        "-",
                        "cli_index_wait",
                        &format!("id={} state={} seen={seen}", accepted.id, row.state),
                    );
                    resources.store.wait_terminal_change(seen);
                    crate::index_coordinator::diagnostic_marker(
                        "-",
                        "cli_index_woke",
                        &format!("id={}", accepted.id),
                    );
                }
                _ => anyhow::bail!("store_unavailable: invalid request state"),
            }
        }
    }

    pub fn cli_semantic_import(
        &self,
        options: &crate::store::requests::SemanticImportOptions,
    ) -> anyhow::Result<serde_json::Value> {
        let resources = self.resources()?;
        let accepted = resources.scheduler.enqueue_semantic_import(options)?;
        let mut busy_deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let seen = resources.store.terminal_generation();
            let row = cli_index_request(
                || resources.store.request_by_id(&accepted.id),
                busy_deadline,
            )?
            .ok_or_else(|| anyhow::anyhow!("storage_busy: accepted request disappeared"))?;
            busy_deadline = Instant::now() + Duration::from_secs(30);
            match row.state.as_str() {
                "done" => {
                    let pin = row.revision.ok_or_else(|| {
                        anyhow::anyhow!("store_unavailable: completed semantic request missing pin")
                    })?;
                    let report = resources
                        .store
                        .semantic_batch_report_at(pin, &options.descriptor_sha256)?
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "store_unavailable: completed semantic request missing report"
                            )
                        })?;
                    return Ok(
                        serde_json::json!({"requestId":accepted.id,"publishedRevision":pin,"report":report}),
                    );
                }
                "failed" => return Err(failed_request_error(&row, "index_failed", "")),
                "queued" | "running" => resources.store.wait_terminal_change(seen),
                _ => anyhow::bail!("store_unavailable: invalid request state"),
            }
        }
    }
}
