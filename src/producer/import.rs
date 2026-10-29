//! Submitting a Trellis-written batch through the #61 queue and waiting for its terminal row.
//! This module is the only caller of `enqueue_generated_semantic_import`.

use super::batch::WrittenBatch;
use crate::{
    http::DaemonState,
    model::IndexPin,
    semantic_batch::descriptor::GenerationOrigin,
    store::{
        Store,
        requests::{GeneratedImportOptions, Request},
    },
};
use anyhow::{Result, ensure};
use std::time::{Duration, Instant};

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

fn read_request(store: &Store, id: &str) -> Result<Option<Request>> {
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

/// Enqueue `options` and return the accepted request's ID.
pub fn submit(scheduler: &DaemonState, options: &GeneratedImportOptions) -> Result<String> {
    Ok(scheduler.enqueue_generated_semantic_import(options)?.id)
}

/// Block until request `id`, accepted for `options`, is terminal. A failed import is an
/// `ImportOutcome` with its typed code; only an error in reading the queue is an `Err`.
pub fn wait(store: &Store, options: &GeneratedImportOptions, id: String) -> Result<ImportOutcome> {
    loop {
        // Recorded before the read: a terminal commit between the read and the wait is never
        // missed.
        let seen = store.terminal_generation();
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
            "queued" | "running" => store.wait_terminal_change(seen),
            _ => anyhow::bail!("store_unavailable: invalid request state"),
        }
    }
}

/// Enqueue `options` and block until the request is terminal. Only an error before acceptance
/// or in reading the queue is an `Err`.
pub fn submit_and_wait(
    store: &Store,
    scheduler: &DaemonState,
    options: &GeneratedImportOptions,
) -> Result<ImportOutcome> {
    let id = submit(scheduler, options)?;
    wait(store, options, id)
}
