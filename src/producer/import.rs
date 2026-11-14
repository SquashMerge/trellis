//! Submitting a Trellis-written batch through the #61 queue and waiting for its terminal row.
//! This module is the only caller of `enqueue_generated_semantic_import`.

use super::batch::WrittenBatch;
use crate::{
    model::IndexPin,
    semantic_batch::descriptor::GenerationOrigin,
    store::{
        Store,
        requests::{GeneratedImportOptions, Request},
    },
};
use anyhow::{Result, ensure};
use std::time::{Duration, Instant};

/// How far the checkout runtime that schedules imports has come.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulerPhase {
    /// Startup reconciliation has not finished.
    Starting,
    Ready,
    /// Still serving, with a retirement pending.
    RetireRequested,
    /// The runtime is closing or gone.
    Stopped,
}

/// What producer code asks of the checkout runtime that owns the request queue: its phase,
/// a wait for the next phase change, and the enqueue of a generated import. The daemon
/// implements it; this layer only names it.
pub trait ImportScheduler: Send + Sync {
    fn phase(&self) -> SchedulerPhase;
    /// A counter that changes whenever the runtime state does; pass it to `wait_state_change`.
    fn state_version(&self) -> u64;
    /// Wait until the runtime state changes after `seen`, or `timeout` passes.
    fn wait_state_change(&self, seen: u64, timeout: Duration);
    /// Enqueue `options` as request `id`. Only `submit` calls this.
    fn enqueue_generated_semantic_import(
        &self,
        options: &GeneratedImportOptions,
        id: uuid::Uuid,
    ) -> Result<Request>;
}

/// How long a transiently busy queue read is retried before it is reported.
const BUSY_WAIT: Duration = Duration::from_secs(30);

/// The terminal state of one generated import request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportOutcome {
    pub request_id: String,
    /// `done` or `failed`.
    pub state: &'static str,
    pub published_revision: Option<IndexPin>,
    pub report: Option<serde_json::Value>,
    /// The typed `<class>[: <reason>]` code of a failed import.
    pub error_code: Option<String>,
}

impl ImportOutcome {
    pub fn is_done(&self) -> bool {
        self.state == "done"
    }

    /// The request's `class` of a failed import: the code up to its first colon.
    pub fn error_class(&self) -> Option<&str> {
        self.error_code
            .as_deref()
            .map(|code| code.split(':').next().unwrap_or(code))
    }
}

/// The queued options of `batch`, which lives under the generation directory of `origin`.
pub fn options_for(
    batch: &WrittenBatch,
    origin: GenerationOrigin,
    generation_id: &str,
) -> Result<GeneratedImportOptions> {
    Ok(GeneratedImportOptions {
        version: 2,
        descriptor: batch
            .descriptor
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("invalid_request: non-UTF8 descriptor path"))?
            .to_owned(),
        descriptor_sha256: batch.descriptor_sha256.clone(),
        origin,
        generation_id: generation_id.to_owned(),
    })
}

pub fn read_request(store: &Store, id: &str) -> Result<Option<Request>> {
    let deadline = Instant::now() + BUSY_WAIT;
    loop {
        match store.request_by_id(id) {
            Ok(row) => return Ok(row),
            Err(error)
                if crate::store::transient_storage_contention(&error)
                    && crate::store::cli_read_retryable_contention(&error) =>
            {
                ensure!(
                    Instant::now() < deadline,
                    crate::store::topology::StorageBusy
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Test-support fault: fail the wait for an accepted request, as a lost queue read does.
#[cfg(all(feature = "test-support", unix))]
pub fn test_wait_fault() -> Result<()> {
    const VARIABLE: &str = "TRELLIS_TEST_PRODUCER_FAIL_WAIT";
    ensure!(
        std::env::var_os(VARIABLE).is_none(),
        "storage_busy: injected {VARIABLE}"
    );
    Ok(())
}

#[cfg(not(all(feature = "test-support", unix)))]
pub fn test_wait_fault() -> Result<()> {
    Ok(())
}

/// Test-support fault: fail the re-read that settles an enqueue error, as a queue that stays
/// busy past the bounded wait does.
#[cfg(all(feature = "test-support", unix))]
fn test_acceptance_read_fault() -> Result<()> {
    const VARIABLE: &str = "TRELLIS_TEST_PRODUCER_FAIL_ACCEPTANCE_READ";
    ensure!(
        std::env::var_os(VARIABLE).is_none(),
        "storage_busy: injected {VARIABLE}"
    );
    Ok(())
}

#[cfg(not(all(feature = "test-support", unix)))]
fn test_acceptance_read_fault() -> Result<()> {
    Ok(())
}

/// Why `submit` did not return an accepted request.
#[derive(Debug)]
pub enum SubmitError {
    /// The queue holds no request with the submitted ID: nothing was accepted.
    Rejected(anyhow::Error),
    /// The enqueue failed and its ID could not be re-read, so the request may have been
    /// accepted. The caller treats it as an accepted request it can no longer observe.
    Unknown(anyhow::Error),
}

impl From<SubmitError> for anyhow::Error {
    fn from(error: SubmitError) -> Self {
        match error {
            SubmitError::Rejected(error) | SubmitError::Unknown(error) => error,
        }
    }
}

/// A fresh request ID, chosen before the enqueue so its outcome can be re-read.
pub fn new_request_id() -> uuid::Uuid {
    uuid::Uuid::new_v4()
}

/// Enqueue `options` as request `id`. An enqueue that reports an error may still have
/// committed the row (#629), so the error is settled by re-reading `id`: a row is the accepted
/// request, no row is a rejection, and an unreadable queue leaves acceptance unknown.
pub fn submit(
    store: &Store,
    scheduler: &dyn ImportScheduler,
    options: &GeneratedImportOptions,
    id: uuid::Uuid,
) -> std::result::Result<String, SubmitError> {
    let error = match scheduler.enqueue_generated_semantic_import(options, id) {
        Ok(row) => return Ok(row.id),
        Err(error) => error,
    };
    let id = id.to_string();
    match test_acceptance_read_fault().and_then(|()| read_request(store, &id)) {
        Ok(Some(_)) => Ok(id),
        Ok(None) => Err(SubmitError::Rejected(error)),
        Err(read_error) => Err(SubmitError::Unknown(read_error)),
    }
}

/// How often a waiting import re-reads its row. The terminal-commit condvar has no timed wait
/// and the runtime closing signals through another one, so a closing runtime is only
/// observable by polling; the interval bounds how long a wait outlives its runtime.
const POLL: Duration = Duration::from_millis(100);

/// Block until request `id`, accepted for `options`, is terminal. A failed import is an
/// `ImportOutcome` with its typed code. Transient queue contention is retried for a bounded
/// time and then reported as `storage_busy`; a runtime that closes before the row settles
/// ends the wait as `root_changed`, because only a replacement runtime would settle it.
pub fn wait(
    store: &Store,
    scheduler: &dyn ImportScheduler,
    options: &GeneratedImportOptions,
    id: String,
) -> Result<ImportOutcome> {
    loop {
        let row = read_request(store, &id)?
            .ok_or_else(|| anyhow::anyhow!("storage_busy: accepted request disappeared"))?;
        match row.state.as_str() {
            "done" => {
                let pin = row.revision.ok_or_else(|| {
                    anyhow::anyhow!("store_unavailable: completed semantic request missing pin")
                })?;
                let report = store
                    .semantic_batch_report_at(pin, &options.descriptor_sha256)?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "store_unavailable: completed semantic request missing report"
                        )
                    })?;
                return Ok(ImportOutcome {
                    request_id: id,
                    state: "done",
                    published_revision: Some(pin),
                    report: Some(report),
                    error_code: None,
                });
            }
            "failed" => {
                return Ok(ImportOutcome {
                    request_id: id,
                    state: "failed",
                    published_revision: None,
                    report: None,
                    error_code: Some(row.error_code.unwrap_or_else(|| "index_failed".into())),
                });
            }
            "queued" | "running" => {
                // A running request is the closing worker's to finish; a queued one is
                // never started by a closing runtime.
                ensure!(
                    row.state == "running" || scheduler.phase() != SchedulerPhase::Stopped,
                    "{}: captured root lost",
                    super::ROOT_CHANGED
                );
                std::thread::sleep(POLL);
            }
            _ => anyhow::bail!("store_unavailable: invalid request state"),
        }
    }
}
