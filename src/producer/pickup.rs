//! R1 artifact-path pickup (D6): an owner-configured artifact is imported without running a
//! producer. This module reaches only `state`, `batch` and `import`, so pickup can never
//! start a producer.

use super::batch::{self, BatchSpec};
use super::pickup_watch::{Host, HostPhase, Intervals, PickupWatch};
use super::state::{LastImported, PickupConfig, PickupEntry, StateDir, now_millis};
use super::{INDEX_NOT_READY, INVALID_REQUEST, import};
use crate::{
    http::{DaemonState, RuntimePhase},
    semantic_batch::descriptor::GenerationOrigin,
    store::Store,
};
use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use uuid::Uuid;

/// Longest artifact path a pickup entry may name.
pub const MAX_ARTIFACT_PATH_BYTES: usize = 4096;
/// A transient failure for the same bytes is retried no sooner than this.
pub const TRANSIENT_RETRY_MILLIS: u64 = 60_000;
/// Failure classes that repeat for the same bytes against the same head.
const DETERMINISTIC_CLASSES: [&str; 4] = [
    "invalid_artifact",
    "invalid_request",
    "limit_exceeded",
    "producer_output_invalid",
];
/// Typed classes a failure before the import request keeps in its code.
const PRE_IMPORT_CLASSES: [&str; 4] = [
    "producer_output_invalid",
    "producer_output_missing",
    "limit_exceeded",
    "invalid_request",
];

/// Daemon operation payloads are closed and at most this large.
const MAX_PAYLOAD_BYTES: usize = 16 * 1024;

/// The `producer_pickup` payload.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PickupRequest {
    pub version: u32,
    pub producer: String,
    pub artifact: Option<String>,
    pub clear: bool,
}

impl PickupRequest {
    pub fn parse(payload: &Value) -> Result<Self> {
        ensure!(
            serde_json::to_vec(payload)?.len() <= MAX_PAYLOAD_BYTES,
            "{INVALID_REQUEST}: producer payload too large"
        );
        let request: Self = serde_json::from_value(payload.clone())
            .map_err(|_| anyhow::anyhow!("{INVALID_REQUEST}: producer options"))?;
        ensure!(request.version == 1, "{INVALID_REQUEST}: producer version");
        ensure!(
            request.artifact.is_some() != request.clear,
            "{INVALID_REQUEST}: pickup needs exactly one of artifact and clear"
        );
        Ok(request)
    }
}

/// What an automatic trigger does with the artifact it just hashed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Decision {
    Import,
    Skip,
}

/// How long an import waits for startup reconciliation, whose sweep removes pickup directories
/// no request names yet, before it creates one.
const READY_WAIT: Duration = Duration::from_secs(30);

/// The decision table of D6, a pure function of the last outcome for the same bytes.
pub fn decide(entry: &PickupEntry, sha256: &str, head_revision: &str, now: u64) -> Decision {
    decide_with_retry(entry, sha256, head_revision, now, TRANSIENT_RETRY_MILLIS)
}

/// `decide` with the transient retry interval in milliseconds.
pub fn decide_with_retry(
    entry: &PickupEntry,
    sha256: &str,
    head_revision: &str,
    now: u64,
    retry_ms: u64,
) -> Decision {
    let Some(last) = entry
        .last_imported
        .as_ref()
        .filter(|last| last.sha256 == sha256)
    else {
        return Decision::Import;
    };
    if last.state == "done" {
        return Decision::Skip;
    }
    let class = last
        .error_code
        .as_deref()
        .map(|code| code.split(':').next().unwrap_or(code));
    if class.is_some_and(|class| DETERMINISTIC_CLASSES.contains(&class)) {
        if last.source_revision == head_revision {
            Decision::Skip
        } else {
            Decision::Import
        }
    } else if now.saturating_sub(last.attempted_at) >= retry_ms {
        Decision::Import
    } else {
        Decision::Skip
    }
}

/// The checkout's pickup workflow: the only writer of `pickup.json`.
#[derive(Clone, Default)]
pub struct PickupOwner {
    workflow: Arc<std::sync::Mutex<()>>,
    watch: PickupWatch,
    /// Set by `shutdown`: a retired owner records nothing more.
    closed: Arc<std::sync::atomic::AtomicBool>,
}

/// The nearest existing ancestor of `path`, canonicalized, with the missing tail re-appended.
fn resolved(path: &Path) -> Result<PathBuf> {
    let mut tail = Vec::new();
    let mut base = path;
    loop {
        match std::fs::canonicalize(base) {
            Ok(real) => {
                return Ok(tail.iter().rev().fold(real, |acc, part| acc.join(part)));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let (Some(parent), Some(name)) = (base.parent(), base.file_name()) else {
                    return Err(error.into());
                };
                tail.push(name.to_owned());
                base = parent;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// The path rules of D6: absolute, bounded, an existing parent directory, and outside the
/// cache and data directories.
fn check_artifact_path(store: &Store, artifact: &str) -> Result<PathBuf> {
    let path = Path::new(artifact);
    ensure!(
        path.is_absolute()
            && !artifact.is_empty()
            && artifact.len() <= MAX_ARTIFACT_PATH_BYTES
            && path.file_name().is_some(),
        "{INVALID_REQUEST}: artifact path"
    );
    let parent = path.parent().map(std::fs::canonicalize);
    let Some(Ok(parent)) = parent else {
        anyhow::bail!("{INVALID_REQUEST}: artifact parent directory");
    };
    ensure!(
        parent.is_dir(),
        "{INVALID_REQUEST}: artifact parent directory"
    );
    let (roots, _) = store.state_topology();
    for protected in [&roots.cache, &roots.data] {
        ensure!(
            !parent.starts_with(resolved(protected)?),
            "{INVALID_REQUEST}: artifact path inside Trellis state"
        );
    }
    let resolved_path = parent.join(path.file_name().expect("checked above"));
    // The file may not exist yet, but whatever is there must be a regular file, not a link.
    let regular_or_absent = match std::fs::symlink_metadata(&resolved_path) {
        Ok(meta) => meta.is_file(),
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    };
    ensure!(
        !artifact.ends_with('/') && regular_or_absent,
        "{INVALID_REQUEST}: artifact path"
    );
    Ok(resolved_path)
}

/// A failure before the import request that keeps its typed code.
fn pre_import_code(error: &anyhow::Error) -> Option<String> {
    let message = error.to_string();
    let line = message.lines().next()?;
    let class = line.split(':').next()?;
    PRE_IMPORT_CLASSES
        .contains(&class)
        .then(|| line.chars().take(120).collect())
}

fn entry_json(entry: &PickupEntry) -> Value {
    json!({"artifact": entry.artifact, "lastImported": entry.last_imported})
}

impl PickupOwner {
    /// `{artifact, lastImported, watch}` of the producer's entry, or `null` when none is
    /// configured. `watch` is `active` or `degraded`. A configuration that cannot be read
    /// disables pickup and says so, with its typed or I/O error.
    pub fn status(&self, store: &Store, producer: &str) -> Value {
        let (roots, identity) = store.state_topology();
        let config = match StateDir::existing(roots, identity)
            .and_then(|state| state.map(|s| s.read_pickup_config()).transpose())
        {
            Ok(config) => config.flatten(),
            Err(error) => {
                return json!({
                    "artifact": null,
                    "lastImported": null,
                    "watch": "degraded",
                    "error": error.to_string(),
                });
            }
        };
        config
            .and_then(|config| config.entries.into_iter().find(|e| e.producer == producer))
            .map_or(Value::Null, |entry| {
                let mut value = entry_json(&entry);
                value["watch"] = json!(self.watch.health(producer).unwrap_or("degraded"));
                value
            })
    }

    /// Arm the configured watches and start the trigger thread. It imports only once startup
    /// reconciliation has finished, and ends when the runtime closes.
    pub fn start_watch(&self, store: &Store, scheduler: &Arc<DaemonState>) {
        let (roots, identity) = store.state_topology();
        if let Ok(Some(state)) = StateDir::existing(roots, identity) {
            self.watch
                .sync(state.read_pickup_config().ok().flatten().as_ref());
        }
        self.watch.spawn(Arc::new(WatchHost {
            owner: self.clone(),
            store: store.clone(),
            scheduler: scheduler.clone(),
        }));
    }

    /// Stop the watch and wait out an import in flight, so a retired owner can never record
    /// into `pickup.json` after a successor runtime has taken over. The runtime is already
    /// closing, so an in-flight import ends without waiting on this caller.
    pub fn shutdown(&self) {
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        self.watch.shutdown();
        drop(self.workflow.lock().unwrap());
    }

    #[cfg(test)]
    pub(super) fn hold_workflow(&self) -> std::sync::MutexGuard<'_, ()> {
        self.workflow.lock().unwrap()
    }

    #[cfg(test)]
    pub(super) fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Re-read the configuration and match the watches to it.
    fn resync(&self, store: &Store) {
        let _workflow = self.workflow.lock().unwrap();
        let (roots, identity) = store.state_topology();
        if let Ok(Some(state)) = StateDir::existing(roots, identity) {
            self.watch
                .sync(state.read_pickup_config().ok().flatten().as_ref());
        }
    }

    /// Block until startup reconciliation has finished, so its sweep cannot remove the pickup
    /// directory this import is about to create.
    fn wait_ready(scheduler: &DaemonState) -> Result<()> {
        let deadline = Instant::now() + READY_WAIT;
        loop {
            let seen = scheduler.state_version();
            match scheduler.runtime_phase() {
                RuntimePhase::Closing | RuntimePhase::Retired => {
                    anyhow::bail!("{INDEX_NOT_READY}: checkout is closing")
                }
                RuntimePhase::Ready | RuntimePhase::RetireRequested => return Ok(()),
                RuntimePhase::Activating | RuntimePhase::Reconciling { .. } => {}
            }
            let now = Instant::now();
            ensure!(
                now < deadline,
                "{INDEX_NOT_READY}: checkout is still starting"
            );
            scheduler.wait_state_change(seen, deadline - now);
        }
    }

    /// Whether the producer's last failed outcome is due again, judged from the stored digest
    /// without reading the artifact.
    fn retry_due(&self, store: &Store, producer: &str) -> bool {
        let _workflow = self.workflow.lock().unwrap();
        let (roots, identity) = store.state_topology();
        let Ok(Some(state)) = StateDir::existing(roots, identity) else {
            return false;
        };
        let Ok(Some(config)) = state.read_pickup_config() else {
            return false;
        };
        let Some(entry) = config.entries.iter().find(|e| e.producer == producer) else {
            return false;
        };
        let Some(last) = &entry.last_imported else {
            return false;
        };
        let Ok(revision) = head_revision(store) else {
            return false;
        };
        decide_with_retry(
            entry,
            &last.sha256,
            &revision,
            now_millis(),
            Intervals::current().retry_ms,
        ) == Decision::Import
    }

    /// Configure the producer's artifact and import it now, as an explicit owner action.
    pub fn set(
        &self,
        store: &Store,
        scheduler: &DaemonState,
        producer: &str,
        artifact: &str,
    ) -> Result<Value> {
        let _workflow = self.workflow.lock().unwrap();
        ensure!(
            batch::pickup_target(producer).is_some(),
            "{INVALID_REQUEST}: pickup producer"
        );
        let artifact = check_artifact_path(store, artifact)?;
        let (roots, identity) = store.state_topology();
        let state = StateDir::open(roots, identity)?;
        let mut config = state.read_pickup_config()?.unwrap_or(PickupConfig {
            version: 1,
            entries: Vec::new(),
        });
        let artifact = artifact
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("{INVALID_REQUEST}: artifact path"))?
            .to_owned();
        match config.entries.iter_mut().find(|e| e.producer == producer) {
            Some(entry) => {
                if entry.artifact != artifact {
                    entry.artifact = artifact;
                    entry.last_imported = None;
                }
            }
            None => config.entries.push(PickupEntry {
                producer: producer.to_owned(),
                artifact,
                last_imported: None,
            }),
        }
        state.write_pickup_config(&config)?;
        self.watch.sync(Some(&config));
        self.import_locked(store, scheduler, &state, producer, true)
    }

    /// Remove the producer's pickup entry. Reports whether one existed.
    pub fn clear(&self, store: &Store, producer: &str) -> Result<Value> {
        let _workflow = self.workflow.lock().unwrap();
        let (roots, identity) = store.state_topology();
        let Some(state) = StateDir::existing(roots, identity)? else {
            return Ok(json!({"producer": producer, "cleared": false}));
        };
        let Some(mut config) = state.read_pickup_config()? else {
            return Ok(json!({"producer": producer, "cleared": false}));
        };
        let before = config.entries.len();
        config.entries.retain(|e| e.producer != producer);
        let cleared = config.entries.len() != before;
        if config.entries.is_empty() {
            state.clear_pickup_config()?;
        } else if cleared {
            state.write_pickup_config(&config)?;
        }
        self.watch.sync(Some(&config));
        Ok(json!({"producer": producer, "cleared": cleared}))
    }

    /// Import the configured artifact. `force` skips the decision table, as the set command
    /// does; an automatic trigger passes `false`. A skipped import replies `state: skipped`.
    pub fn import(
        &self,
        store: &Store,
        scheduler: &DaemonState,
        producer: &str,
        force: bool,
    ) -> Result<Value> {
        let _workflow = self.workflow.lock().unwrap();
        let (roots, identity) = store.state_topology();
        let state = StateDir::open(roots, identity)?;
        self.import_locked(store, scheduler, &state, producer, force)
    }

    fn import_locked(
        &self,
        store: &Store,
        scheduler: &DaemonState,
        state: &StateDir,
        producer: &str,
        force: bool,
    ) -> Result<Value> {
        let mut config = state
            .read_pickup_config()?
            .ok_or_else(|| anyhow::anyhow!("{}: no pickup configured", super::NOT_FOUND))?;
        let entry = config
            .entries
            .iter()
            .find(|e| e.producer == producer)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{}: no pickup configured", super::NOT_FOUND))?;
        let (pin, language) = batch::pickup_target(producer)
            .ok_or_else(|| anyhow::anyhow!("{INVALID_REQUEST}: pickup producer"))?;
        Self::wait_ready(scheduler)?;
        let generation_id = Uuid::new_v4().simple().to_string();
        let dir = state.create_pickup_dir(&generation_id)?;
        // Set once the queue accepts the import: the request then owns the directory's batch.
        let mut accepted: Option<LastImported> = None;
        // The directory goes on every exit before acceptance, whatever the outcome.
        let result = (|| -> Result<(Value, Option<LastImported>)> {
            let copied = batch::copy_artifact(Path::new(&entry.artifact), &dir);
            let (sha256, _) = match copied {
                Ok(copied) => copied,
                Err(error) => match pre_import_code(&error) {
                    Some(code) => return Ok((failure(producer, &entry, None, &code), None)),
                    None => return Err(error),
                },
            };
            let head = store.index_baseline()?;
            let retry_ms = Intervals::current().retry_ms;
            // The decision comes before the batch is written: a skip costs no batch.
            let (revision, written) = store.with_native_reader(head, |reader| {
                let revision = reader.header()?.revision.id;
                if !force
                    && decide_with_retry(&entry, &sha256, &revision, now_millis(), retry_ms)
                        == Decision::Skip
                {
                    return Ok((revision, None));
                }
                let dir = std::fs::canonicalize(&dir)?;
                let written = batch::write_batch(
                    reader,
                    &BatchSpec {
                        pin: &pin,
                        language,
                        workspace: Path::new(store.workspace_root()),
                        dir: &dir,
                        generation: batch::pickup_generation(&generation_id),
                    },
                );
                Ok((revision, Some(written)))
            })?;
            let Some(written) = written else {
                return Ok((
                    json!({"producer": producer, "artifact": entry.artifact, "state": "skipped",
                        "sha256": sha256, "sourceRevision": revision}),
                    None,
                ));
            };
            let attempted_at = now_millis();
            let written = match written {
                Ok(written) => written,
                Err(error) => {
                    let code = pre_import_code(&error).ok_or(error)?;
                    let last = LastImported {
                        sha256: sha256.clone(),
                        source_revision: revision.clone(),
                        request_id: None,
                        state: "failed".into(),
                        error_code: Some(code.clone()),
                        attempted_at,
                    };
                    return Ok((
                        failure(producer, &entry, Some((&sha256, &revision)), &code),
                        Some(last),
                    ));
                }
            };
            let options =
                import::options_for(&written, GenerationOrigin::OwnerPickup, &generation_id)?;
            let request_id = import::submit(scheduler, &options)?;
            accepted = Some(LastImported {
                sha256: sha256.clone(),
                source_revision: revision.clone(),
                request_id: Some(request_id.clone()),
                state: "failed".into(),
                error_code: None,
                attempted_at,
            });
            import::test_wait_fault()?;
            let outcome = import::wait(store, scheduler, &options, request_id)?;
            let last = LastImported {
                sha256: sha256.clone(),
                source_revision: revision.clone(),
                request_id: Some(outcome.request_id.clone()),
                state: outcome.state.into(),
                error_code: outcome.error_code.clone(),
                attempted_at,
            };
            let reply = json!({
                "producer": producer,
                "artifact": entry.artifact,
                "state": outcome.state,
                "errorCode": outcome.error_code,
                "sha256": sha256,
                "sourceRevision": revision,
                "requestId": outcome.request_id,
                "publishedRevision": outcome.published_revision,
                "report": outcome.report,
            });
            Ok((reply, Some(last)))
        })();
        let result = match (result, accepted) {
            (Err(error), Some(mut last)) => {
                // The wait ended before the accepted request settled: the request still
                // reads the batch, so the directory stays and the sweep reclaims it once
                // the request is terminal. The unknown outcome counts as a transient
                // failure, so a later trigger retries after the retry interval.
                let message = format!("{error:#}");
                last.error_code = Some(message.split(':').next().unwrap_or(&message).to_owned());
                self.record_last(state, &mut config, producer, last)?;
                return Err(error);
            }
            (result, _) => {
                let _ = state.remove_pickup_dir(&generation_id);
                result
            }
        };
        let (reply, last) = result?;
        if let Some(last) = last {
            self.record_last(state, &mut config, producer, last)?;
        }
        Ok(reply)
    }

    /// Record `last` for `producer` unless this owner is retired.
    fn record_last(
        &self,
        state: &StateDir,
        config: &mut PickupConfig,
        producer: &str,
        last: LastImported,
    ) -> Result<()> {
        if !self.closed.load(std::sync::atomic::Ordering::Acquire)
            && let Some(entry) = config.entries.iter_mut().find(|e| e.producer == producer)
        {
            entry.last_imported = Some(last);
            state.write_pickup_config(config)?;
        }
        Ok(())
    }
}

fn failure(producer: &str, entry: &PickupEntry, bound: Option<(&str, &str)>, code: &str) -> Value {
    json!({
        "producer": producer,
        "artifact": entry.artifact,
        "state": "failed",
        "errorCode": code,
        "sha256": bound.map(|(sha, _)| sha),
        "sourceRevision": bound.map(|(_, revision)| revision),
        "requestId": null,
        "publishedRevision": null,
        "report": null,
    })
}

/// The revision ID of the retained native head.
fn head_revision(store: &Store) -> Result<String> {
    let head = store.index_baseline()?;
    store.with_native_reader(head, |reader| Ok(reader.header()?.revision.id))
}

/// What the watch thread asks of its checkout.
struct WatchHost {
    owner: PickupOwner,
    store: Store,
    scheduler: Arc<DaemonState>,
}

impl Host for WatchHost {
    fn phase(&self) -> HostPhase {
        match self.scheduler.runtime_phase() {
            RuntimePhase::Ready => HostPhase::Ready,
            RuntimePhase::Closing | RuntimePhase::Retired => HostPhase::Stopped,
            _ => HostPhase::Starting,
        }
    }

    fn import(&self, producer: &str) -> Result<()> {
        // A failed import is already recorded in `lastImported`; only an error before a
        // verdict makes the trigger look again.
        self.owner
            .import(&self.store, &self.scheduler, producer, false)
            .map(drop)
    }

    fn retry_due(&self, producer: &str) -> bool {
        self.owner.retry_due(&self.store, producer)
    }

    fn resync(&self) {
        self.owner.resync(&self.store);
    }
}
