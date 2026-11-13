//! The run owner (D8, D9, D11): one explicit, owner-requested producer run at a
//! time per checkout. `start_run` is the only caller of `spawn_producer`. The
//! owner alone writes `run.json` and alone signals the child; callers observe
//! the published record and the returned terminal reply.

use super::approval;
use super::batch::{self, ARTIFACT_PATH, BatchSpec};
use super::exec::{self, Ending, Hooks, ProducerSpec};
use super::import::{self, ImportOutcome};
use super::plan::{self, PlanRequest, Prepared};
use super::profiles::{self, Stub};
use super::state::{
    DaemonIdentity, RunOrigin, RunRecord, RunState, StateDir, SweepReport, now_millis,
};
use super::worktree;
use super::{
    Diagnostics, GroupStatus, NOT_CANCELLABLE, NOT_FOUND, PLAN_CHANGED, PRODUCER_BUSY,
    ProcessIdentity, ROOT_CHANGED,
};
use crate::{
    http::DaemonState,
    semantic_batch::{
        BatchCounters, StageInput, StageOwner, descriptor::GenerationOrigin, stage_batch,
    },
    store::Store,
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

/// How long a manual run waits for a preempted automatic run to release the slot (#660).
pub const PREEMPT_WAIT: Duration = Duration::from_secs(30);

/// Asks one automatic run to end because the head moved on (#660). The owner raises it before
/// it looks at the slot, so a run that installs later still finds it.
#[derive(Clone, Debug, Default)]
pub struct SupersedeToken(Arc<AtomicBool>);

impl SupersedeToken {
    pub fn request(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_requested(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub(super) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// What `supersede` found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutoCancel {
    /// The token's run was stopped, or will be when it installs.
    Requested,
    /// The token's run is past the point where it can be stopped.
    NotCancellable,
    /// No active run carries the token.
    NoAutoRun,
}

/// The `producer_run` payload: a plan request plus the hash the caller inspected.
#[derive(Clone, Debug)]
pub struct RunRequest {
    pub plan: PlanRequest,
    pub plan_sha256: String,
    pub origin: RunOrigin,
    pub supersede: Option<SupersedeToken>,
}

impl RunRequest {
    pub fn parse(payload: &Value) -> Result<Self> {
        let mut rest = payload.clone();
        let hash = rest
            .as_object_mut()
            .and_then(|object| object.remove("planSha256"))
            .and_then(|value| value.as_str().map(str::to_owned))
            .filter(|hash| {
                hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            })
            .ok_or_else(|| anyhow::anyhow!("invalid_request: plan digest"))?;
        Ok(Self {
            plan: PlanRequest::parse(&rest)?,
            plan_sha256: hash,
            origin: RunOrigin::Manual,
            supersede: None,
        })
    }

    /// An automatic re-run of an approved plan (#660). Only the checkout's owner builds one.
    pub fn automatic(plan: PlanRequest, plan_sha256: String, token: SupersedeToken) -> Self {
        Self {
            plan,
            plan_sha256,
            origin: RunOrigin::Auto,
            supersede: Some(token),
        }
    }
}

/// The `producer_cancel` payload.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CancelRequest {
    pub version: u32,
    pub run_id: String,
}

impl CancelRequest {
    pub fn parse(payload: &Value) -> Result<Self> {
        ensure!(
            serde_json::to_vec(payload)?.len() <= plan::MAX_PAYLOAD_BYTES,
            "invalid_request: producer payload too large"
        );
        let request: Self = serde_json::from_value(payload.clone())
            .map_err(|_| anyhow::anyhow!("invalid_request: producer options"))?;
        ensure!(request.version == 1, "invalid_request: producer version");
        Ok(request)
    }
}

/// What a run needs from its checkout.
pub struct RunContext<'a> {
    pub store: &'a Store,
    pub scheduler: &'a DaemonState,
    pub stub: Option<&'a Stub>,
}

/// Why the cancel flag was raised.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CancelReason {
    Requested,
    RootChanged,
    Superseded,
    Preempted,
}

struct Control {
    phase: RunState,
    /// The run's outcome is decided: a cancel now has nothing left to stop.
    finished: bool,
    reason: Option<CancelReason>,
    request_id: Option<String>,
    process: Option<ProcessIdentity>,
    /// What the ended producer phase established about the group.
    group_status: Option<GroupStatus>,
}

/// The one run of a checkout that is in flight.
struct ActiveRun {
    run_id: String,
    producer: String,
    trust_profile: super::TrustProfile,
    plan_sha256: String,
    source_revision: String,
    created_at: u64,
    origin: RunOrigin,
    supersede: Option<SupersedeToken>,
    /// Read by the worktree writer, the monitor loop and the snapshot preflight.
    cancel: Arc<AtomicBool>,
    control: Mutex<Control>,
}

impl ActiveRun {
    fn control(&self) -> MutexGuard<'_, Control> {
        self.control.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Cancel by phase. Once the import is enqueued the run can no longer be
    /// stopped. The phase and the flag change together under the lock, so a
    /// cancel can never land between the enqueue and its record.
    fn request_cancel(&self, reason: CancelReason) -> Result<Value> {
        let mut control = self.control();
        if control.finished {
            anyhow::bail!("{NOT_CANCELLABLE}: run already finished");
        }
        if control.phase == RunState::Importing {
            anyhow::bail!(
                "{NOT_CANCELLABLE}: import accepted as request {}",
                control.request_id.as_deref().unwrap_or("-")
            );
        }
        control.reason.get_or_insert(reason);
        self.cancel.store(true, Ordering::SeqCst);
        Ok(json!({"runId": self.run_id, "cancelRequested": true, "state": control.phase}))
    }
}

/// Owns the checkout's producer runs. Cloning shares the same slot.
#[derive(Clone, Default)]
pub struct ProducerOwner {
    slot: Arc<SlotCell>,
}

/// The slot and the signal that it was released.
#[derive(Default)]
struct SlotCell {
    slot: Mutex<Slot>,
    released: Condvar,
}

/// The run slot and the owner's closed mark, changed together under one lock so a
/// shutdown can never fall between a run's admission and its installation.
#[derive(Default)]
struct Slot {
    run: Option<Arc<ActiveRun>>,
    /// The checkout is closing or lost its root; a runtime is never reopened, so
    /// the mark is permanent.
    closed: bool,
    /// Manual runs waiting for a preempted automatic run to leave; automatic runs yield to them.
    manual_waiting: u32,
}

/// Clears the owner's slot on every exit, and only its own run's.
struct SlotGuard<'a> {
    owner: &'a ProducerOwner,
    run: Arc<ActiveRun>,
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        let mut slot = self.owner.slot();
        if slot
            .run
            .as_ref()
            .is_some_and(|run| Arc::ptr_eq(run, &self.run))
        {
            slot.run = None;
        }
        self.owner.slot.released.notify_all();
    }
}

/// Leaves a run directory stripped and its record terminal when the run ends by
/// any path that did not write its own terminal record (a panic, for example).
struct RunCleanup<'a> {
    state: &'a StateDir,
    run: &'a ActiveRun,
    armed: bool,
}

impl Drop for RunCleanup<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // The producer's group was stopped by the leader's owner in `spawn_producer`, the only
        // code that can prove it still holds the group ID; nothing here signals a recorded group.
        let enqueued = self.run.control().request_id.is_some();
        // The queued import still reads `out/` and `batch/`, and its record says so.
        if enqueued {
            return;
        }
        // Strip before the terminal record, as the sweep does, so a reader that sees the
        // terminal record sees the stripped directory. If the strip fails, the sweep strips
        // the terminal `abandoned` run later.
        let _ = self.state.strip_to_record(&self.run.run_id);
        if let Ok(Some(mut record)) = self.state.read_run_record(&self.run.run_id)
            && !record.state.is_terminal()
        {
            record.state = RunState::Failed;
            record.outcome = Some("abandoned".to_owned());
            record.error_code = Some("abandoned".to_owned());
            record.finished_at = Some(now_millis());
            let _ = self.state.write_run_record(&record);
        }
    }
}

/// How a run ended, before its record and reply are written.
struct Finish {
    state: RunState,
    outcome: &'static str,
    error_code: Option<String>,
    import: Option<ImportOutcome>,
    diagnostics: Option<Diagnostics>,
}

impl Finish {
    fn failed(outcome: &'static str, code: impl Into<String>) -> Self {
        Self {
            state: RunState::Failed,
            outcome,
            error_code: Some(code.into()),
            import: None,
            diagnostics: None,
        }
    }

    fn with(mut self, diagnostics: &Diagnostics) -> Self {
        self.diagnostics = Some(diagnostics.clone());
        self
    }
}

/// The typed `<class>[: <reason>]` head of an error message.
pub(super) fn typed_code(error: &anyhow::Error) -> (String, String) {
    let message = error.to_string();
    let line: String = message
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(120)
        .collect();
    let class = line.split(':').next().unwrap_or_default().to_owned();
    (class, line)
}

fn private_dir(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .with_context(|| format!("create {}", path.display()))
}

/// Test-support fault: fail the step named by `variable` once it is reached.
#[cfg(all(feature = "test-support", unix))]
fn test_fault(variable: &str) -> Result<()> {
    ensure!(
        std::env::var_os(variable).is_none(),
        "storage_busy: injected {variable}"
    );
    Ok(())
}

#[cfg(not(all(feature = "test-support", unix)))]
fn test_fault(_variable: &str) -> Result<()> {
    Ok(())
}

/// Test-support switch: whether `variable` is set.
#[cfg(all(feature = "test-support", unix))]
fn test_flag(variable: &str) -> bool {
    std::env::var_os(variable).is_some()
}

#[cfg(not(all(feature = "test-support", unix)))]
fn test_flag(_variable: &str) -> bool {
    false
}

/// Test-support hold point: bind the socket, announce it, and wait for the
/// test to connect. Once per path, like the semantic cutoff hook.
#[cfg(all(feature = "test-support", unix))]
pub(super) fn test_hook(variable: &str) {
    use std::os::unix::net::UnixListener;
    let Some(path) = std::env::var_os(variable) else {
        return;
    };
    let path = PathBuf::from(path);
    if path.exists() {
        return;
    }
    let Ok(listener) = UnixListener::bind(&path) else {
        return;
    };
    let _ = fs::write(path.with_extension("ready"), []);
    let _ = listener.accept();
}

#[cfg(not(all(feature = "test-support", unix)))]
pub(super) fn test_hook(_variable: &str) {}

/// Recover producer state of this checkout: runs left by another daemon and
/// pickup directories no request names. Best effort; a failure leaves the
/// state for the next sweep. `keep_pickups` protects pickup directories while
/// this daemon may be importing one (a sweep before a run).
pub fn sweep_state(store: &Store, keep_pickups: bool) -> Option<SweepReport> {
    let (roots, identity) = store.state_topology();
    let state = StateDir::existing(roots, identity).ok()??;
    let mut keep = store.pending_generation_dirs().ok()?;
    if keep_pickups && let Ok(entries) = fs::read_dir(state.path().join("pickups")) {
        keep.extend(entries.flatten().map(|entry| entry.path()));
    }
    state.sweep(&DaemonIdentity::current(), &keep).ok()
}

struct Run<'a> {
    ctx: &'a RunContext<'a>,
    prepared: &'a Prepared,
    active: &'a Arc<ActiveRun>,
    dir: PathBuf,
    recorder: Recorder<'a>,
}

/// The in-memory copy of `run.json`, written whole at every transition.
struct Recorder<'a> {
    state: &'a StateDir,
    record: RunRecord,
}

impl Recorder<'_> {
    fn update(&mut self, change: impl FnOnce(&mut RunRecord)) -> Result<()> {
        change(&mut self.record);
        self.state.write_run_record(&self.record)
    }
}

impl Run<'_> {
    fn cancel_finish(&self, diagnostics: Option<&Diagnostics>) -> Finish {
        let reason = self.active.control().reason;
        let finish = match reason {
            Some(CancelReason::RootChanged) => Finish::failed("root_changed", ROOT_CHANGED),
            Some(reason @ (CancelReason::Superseded | CancelReason::Preempted)) => Finish {
                state: RunState::Cancelled,
                outcome: "cancelled",
                error_code: Some(
                    match reason {
                        CancelReason::Superseded => "cancelled: superseded",
                        _ => "cancelled: preempted",
                    }
                    .to_owned(),
                ),
                import: None,
                diagnostics: None,
            },
            _ => Finish {
                state: RunState::Cancelled,
                outcome: "cancelled",
                error_code: Some("cancelled".to_owned()),
                import: None,
                diagnostics: None,
            },
        };
        match diagnostics {
            Some(diagnostics) => finish.with(diagnostics),
            None => finish,
        }
    }

    fn cancelled(&self) -> bool {
        self.active.cancel.load(Ordering::SeqCst)
    }

    fn finish_from_error(
        &self,
        error: &anyhow::Error,
        diagnostics: Option<&Diagnostics>,
    ) -> Finish {
        if self.cancelled() {
            return self.cancel_finish(diagnostics);
        }
        let (class, line) = typed_code(error);
        let finish = match class.as_str() {
            "producer_output_invalid" | "producer_output_missing" => {
                let outcome = if class == "producer_output_missing" {
                    "producer_output_missing"
                } else {
                    "producer_output_invalid"
                };
                Finish::failed(outcome, line)
            }
            "limit_exceeded" => Finish::failed("limit_exceeded", line),
            "stale_basis" => Finish::failed("stale_basis", line),
            ROOT_CHANGED => Finish::failed("root_changed", line),
            // The typed operation codes of the checkout's own state and queue.
            "capture_unavailable"
            | "index_not_ready"
            | "storage_busy"
            | "producer_state_corrupt"
            | "invalid_request" => Finish::failed("producer_failed", line),
            _ => {
                use std::io::Write;
                let _ = writeln!(std::io::stderr(), "producer run failed: {error:#}");
                Finish::failed("producer_failed", "producer_failed: run setup")
            }
        };
        match diagnostics {
            Some(diagnostics) => finish.with(diagnostics),
            None => finish,
        }
    }

    fn transition(&mut self, phase: RunState) -> Result<bool> {
        {
            let mut control = self.active.control();
            if self.active.cancel.load(Ordering::SeqCst) {
                return Ok(false);
            }
            control.phase = phase;
        }
        self.recorder.update(|record| record.state = phase)?;
        Ok(true)
    }

    /// Admits the frozen batch in full, as the import will: the descriptor, every source and
    /// coverage page (a validated disposition for every requested document), the snapshot's
    /// digest, its full decode and the closure that every covered document was decoded. The
    /// stage is discarded; nothing is published.
    fn preflight(&self, written: &batch::WrittenBatch) -> Result<()> {
        test_hook("TRELLIS_TEST_PRODUCER_PREFLIGHT_SOCKET");
        let store = self.ctx.store;
        let input = StageInput::generated(
            Path::new(store.workspace_root()),
            GenerationOrigin::TrellisRun,
            &self.active.run_id,
            &self.dir,
        )?;
        let limits = store
            .batch_limits()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        stage_batch(
            store,
            &input,
            StageOwner {
                request_id: &self.active.run_id,
                incarnation: "preflight",
            },
            &written.descriptor,
            &written.descriptor_sha256,
            &limits,
            &mut BatchCounters::default(),
            &self.active.cancel,
        )
        .map(drop)
    }

    /// A snapshot that fails its preflight is the producer's output, refused with the code
    /// the import would have reported.
    fn preflight_finish(&self, error: &anyhow::Error, diagnostics: &Diagnostics) -> Finish {
        use crate::scip_raw::ScipCaptureError;
        let finish = match error.downcast_ref::<ScipCaptureError>() {
            _ if self.cancelled() => return self.cancel_finish(Some(diagnostics)),
            Some(ScipCaptureError::LimitExceeded(detail)) => {
                Finish::failed("limit_exceeded", format!("limit_exceeded: {detail}"))
            }
            Some(ScipCaptureError::InvalidArtifact(detail)) => Finish::failed(
                "producer_output_invalid",
                format!("invalid_artifact: {detail}"),
            ),
            Some(ScipCaptureError::InvalidAdmission(detail)) => Finish::failed(
                "producer_output_invalid",
                format!("invalid_request: {detail}"),
            ),
            // Claim proofs the artifact's decoded facts could not establish.
            Some(
                failure @ (ScipCaptureError::MissingProof
                | ScipCaptureError::NonUniqueProof
                | ScipCaptureError::DigestCollision),
            ) => Finish::failed(
                "producer_output_invalid",
                format!("invalid_artifact: {}", failure.detail()),
            ),
            _ => return self.finish_from_error(error, Some(diagnostics)),
        };
        finish.with(diagnostics)
    }

    fn execute(&mut self) -> Result<Finish> {
        let store = self.ctx.store;
        let prepared = self.prepared;
        let row = prepared.row;
        let run_id = self.active.run_id.clone();
        let revision = self.active.source_revision.clone();
        let dir_text = self
            .dir
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("invalid_request: non-UTF8 run directory"))?
            .to_owned();
        let command = profiles::command(
            &row,
            &prepared.resolution,
            &prepared.declared,
            self.ctx.stub,
            &prepared.pin,
            &prepared.rules,
            &dir_text,
            prepared.limits.memory_mib,
        );
        let rules = &prepared.rules;

        if self.cancelled() {
            return Ok(self.cancel_finish(None));
        }
        let baseline = store.index_baseline()?;
        let materialized = store.with_native_reader(baseline, |reader| {
            ensure!(
                reader.header()?.revision.id == revision,
                "stale_basis: head revision changed"
            );
            worktree::materialize(reader, rules, &self.dir, &self.active.cancel)
        });
        if let Err(error) = materialized {
            return Ok(self.finish_from_error(&error, None));
        }
        for name in ["home", "tmp", "out", "logs"] {
            private_dir(&self.dir.join(name))?;
        }
        if !command.bin_links.is_empty() {
            worktree::write_bin_links(&self.dir, &command.bin_links)?;
        }

        if !self.transition(RunState::Running)? {
            return Ok(self.cancel_finish(None));
        }
        let spec = ProducerSpec {
            argv: command.argv.iter().map(OsString::from).collect(),
            cwd: self.dir.join("worktree"),
            env: command.env.clone(),
            artifact: self.dir.join(ARTIFACT_PATH),
            log_dir: Some(self.dir.join("logs")),
        };
        let limits = plan::effective_limits(prepared.limits);
        let outcome = {
            let active = self.active;
            let recorder = &mut self.recorder;
            let mut on_spawn = |process: &ProcessIdentity| {
                active.control().process = Some(*process);
                let _ = recorder.update(|record| record.process = Some(*process));
                test_hook("TRELLIS_TEST_PRODUCER_SPAWNED_SOCKET");
            };
            let mut hooks = Hooks {
                spawned: Some(&mut on_spawn),
                ..Hooks::default()
            };
            exec::spawn_producer(&spec, &limits, &active.cancel, &mut hooks)
        };
        let started = self.active.control().process.take().is_some();
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                // A started producer whose leader could no longer be observed was not signalled.
                if started {
                    self.active.control().group_status = Some(GroupStatus::Unknown);
                    let _ = self
                        .recorder
                        .update(|record| record.group_status = Some(GroupStatus::Unknown));
                }
                let _ = writeln_stderr(&format!("producer did not start: {error:#}"));
                return Ok(Finish::failed("producer_failed", "producer_failed"));
            }
        };
        let mut diagnostics = outcome.diagnostics;
        if test_flag("TRELLIS_TEST_PRODUCER_GROUP_UNKNOWN") {
            diagnostics.group_status = GroupStatus::Unknown;
        }
        if test_flag("TRELLIS_TEST_PRODUCER_GROUP_EMPTY") {
            diagnostics.group_status = GroupStatus::Empty;
        }
        let group_status = diagnostics.group_status;
        self.active.control().group_status = Some(group_status);
        let _ = self
            .recorder
            .update(|record| record.group_status = Some(group_status));
        // The producer phase is over, so what only it needed goes now. The log tails are in
        // the diagnostics.
        let _ = self.recorder.state.end_producer_phase(&run_id);
        test_hook("TRELLIS_TEST_PRODUCER_EXITED_SOCKET");
        match outcome.ending {
            Ending::Cancelled => return Ok(self.cancel_finish(Some(&diagnostics))),
            Ending::TimedOut => {
                return Ok(Finish::failed("timed_out", "timed_out").with(&diagnostics));
            }
            Ending::LimitExceeded(kind) => {
                return Ok(Finish::failed(
                    "limit_exceeded",
                    format!("limit_exceeded: {}", kind.reason()),
                )
                .with(&diagnostics));
            }
            Ending::Exited => {}
        }
        if diagnostics.exit.code != Some(0) {
            return Ok(Finish::failed("producer_failed", "producer_failed").with(&diagnostics));
        }
        let artifact_present =
            fs::symlink_metadata(self.dir.join(ARTIFACT_PATH)).is_ok_and(|meta| meta.is_file());
        if !artifact_present {
            return Ok(
                Finish::failed("producer_output_missing", "producer_output_missing")
                    .with(&diagnostics),
            );
        }

        if !self.transition(RunState::WritingBatch)? {
            return Ok(self.cancel_finish(Some(&diagnostics)));
        }
        // A group not proven empty may still hold a producer process that rewrites
        // `out/index.scip`: the batch then names a snapshot only Trellis writes, admitted in
        // full before the queue accepts it (owner ruling 2026-11-02, #624).
        let frozen = group_status != GroupStatus::Empty;
        let generation = batch::run_generation(
            &profiles::run_generation(&row, &prepared.resolution),
            GenerationOrigin::TrellisRun,
            &run_id,
        );
        let written = store.with_native_reader(store.index_baseline()?, |reader| {
            // The head must still be the revision the worktree was written from.
            ensure!(
                reader.header()?.revision.id == revision,
                "stale_basis: head revision changed"
            );
            batch::write_batch(
                reader,
                &BatchSpec {
                    pin: &prepared.pin,
                    languages: row.target.languages(),
                    workspace: Path::new(store.workspace_root()),
                    dir: &self.dir,
                    generation,
                    snapshot: frozen,
                },
            )
        });
        let written = match written {
            Ok(written) => written,
            Err(error) => return Ok(self.finish_from_error(&error, Some(&diagnostics))),
        };
        if frozen && let Err(error) = self.preflight(&written) {
            return Ok(self.preflight_finish(&error, &diagnostics));
        }
        let options = import::options_for(&written, GenerationOrigin::TrellisRun, &run_id)?;

        // Enqueue under the control lock: a cancel either lands before it and
        // wins, or finds the request ID and is refused. The ID is chosen first, so an
        // enqueue error is settled against it before the lock is released (#629).
        let (id, unobservable) = {
            let mut control = self.active.control();
            if self.active.cancel.load(Ordering::SeqCst) {
                drop(control);
                return Ok(self.cancel_finish(Some(&diagnostics)));
            }
            let id = import::new_request_id();
            let (id, unobservable) = match import::submit(store, self.ctx.scheduler, &options, id) {
                Ok(id) => (id, None),
                Err(import::SubmitError::Rejected(error)) => return Err(error),
                // Possibly accepted: the run keeps the ID as an accepted request it can no
                // longer observe, so its inputs stay until the queue no longer names them.
                Err(import::SubmitError::Unknown(error)) => (id.to_string(), Some(error)),
            };
            control.phase = RunState::Importing;
            control.request_id = Some(id.clone());
            (id, unobservable)
        };
        // The request is accepted: from here the run only observes it. A record that
        // cannot be written leaves `writingBatch` behind, and the final record corrects it.
        let _ = test_fault("TRELLIS_TEST_PRODUCER_FAIL_IMPORTING_RECORD").and_then(|()| {
            self.recorder.update(|record| {
                record.state = RunState::Importing;
                record.request_id = Some(id.clone());
            })
        });
        if let Some(error) = unobservable {
            return Err(error);
        }
        test_hook("TRELLIS_TEST_PRODUCER_ENQUEUED_SOCKET");
        test_fault("TRELLIS_TEST_PRODUCER_FAIL_WAIT")?;
        let imported = import::wait(store, self.ctx.scheduler, &options, id)?;
        let finish = if imported.is_done() {
            Finish {
                state: RunState::Done,
                outcome: "imported",
                error_code: None,
                import: Some(imported),
                diagnostics: Some(diagnostics),
            }
        } else {
            let code = imported
                .error_code
                .clone()
                .unwrap_or_else(|| "index_failed".to_owned());
            let outcome = match imported.error_class() {
                Some("stale_basis") => "stale_basis",
                Some(ROOT_CHANGED) => "root_changed",
                _ => "import_failed",
            };
            Finish {
                import: Some(imported),
                ..Finish::failed(outcome, code).with(&diagnostics)
            }
        };
        Ok(finish)
    }
}

fn writeln_stderr(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    writeln!(std::io::stderr(), "{text}")
}

impl ProducerOwner {
    fn slot(&self) -> MutexGuard<'_, Slot> {
        self.slot
            .slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Who started the active run, if one is active.
    pub fn active_origin(&self) -> Option<RunOrigin> {
        self.slot().run.as_ref().map(|run| run.origin)
    }

    /// Stop the automatic run carrying `token`, because the head moved on. The token is raised
    /// before the slot is read, so a run that installs after this call is stopped when it does.
    pub fn supersede(&self, token: &SupersedeToken) -> AutoCancel {
        token.request();
        let slot = self.slot();
        match &slot.run {
            Some(run) if run.supersede.as_ref().is_some_and(|own| own.same(token)) => {
                match run.request_cancel(CancelReason::Superseded) {
                    Ok(_) => AutoCancel::Requested,
                    Err(_) => AutoCancel::NotCancellable,
                }
            }
            _ => AutoCancel::NoAutoRun,
        }
    }

    /// `activeRun` of the status reply.
    pub fn status(&self) -> Value {
        let Some(run) = self.slot().run.clone() else {
            return Value::Null;
        };
        let control = run.control();
        json!({
            "runId": run.run_id,
            "producer": run.producer,
            "trustProfile": run.trust_profile,
            "state": control.phase,
            "planSha256": run.plan_sha256,
            "sourceRevision": run.source_revision,
            "createdAt": run.created_at,
            "origin": run.origin,
            "requestId": control.request_id,
            "process": control.process,
            "groupStatus": control.group_status,
        })
    }

    /// Cancel `run_id` while it can still be stopped.
    pub fn cancel(&self, store: &Store, run_id: &str) -> Result<Value> {
        let active = self.slot().run.clone();
        if let Some(run) = active
            && run.run_id == run_id
        {
            return run.request_cancel(CancelReason::Requested);
        }
        let (roots, identity) = store.state_topology();
        let record = match StateDir::existing(roots, identity)? {
            Some(state) => state.read_run_record(run_id)?,
            None => None,
        };
        match record {
            Some(_) => anyhow::bail!("{NOT_CANCELLABLE}: run already finished"),
            None => anyhow::bail!("{NOT_FOUND}: run"),
        }
    }

    /// The checkout is retiring or lost its root: stop the active run, which
    /// then finishes as `root_changed`. A run past its enqueue is left to its
    /// import request, which the checkout settles itself. The owner is marked
    /// closed under the slot lock, so a run still preparing to take its slot
    /// finds the mark when it does and is cancelled too.
    pub fn shutdown(&self) {
        let mut slot = self.slot();
        slot.closed = true;
        if let Some(run) = &slot.run {
            let _ = run.request_cancel(CancelReason::RootChanged);
        }
    }

    /// Put `active` in the slot. An owner closed before this moment installs it already
    /// cancelled, so the run ends as `root_changed` without starting anything.
    ///
    /// An automatic run never waits: it is busy while the slot is taken or a manual run is
    /// waiting for it. A manual run preempts an automatic run that is not importing and waits
    /// for it to leave, at most `PREEMPT_WAIT`.
    fn install(&self, active: &Arc<ActiveRun>) -> Result<()> {
        self.install_within(active, PREEMPT_WAIT)
    }

    /// `install` with the preemption wait chosen by the caller, so a test need not wait 30 s.
    fn install_within(&self, active: &Arc<ActiveRun>, preempt_wait: Duration) -> Result<()> {
        let mut slot = self.slot();
        let busy = |slot: &Slot| {
            let run_id = slot.run.as_ref().map_or("-", |run| run.run_id.as_str());
            anyhow::anyhow!("{PRODUCER_BUSY}: run {run_id} is active")
        };
        match active.origin {
            RunOrigin::Auto if slot.run.is_some() || slot.manual_waiting > 0 => {
                return Err(busy(&slot));
            }
            RunOrigin::Auto => {}
            // A second manual run never joins the first one's wait.
            RunOrigin::Manual if slot.manual_waiting > 0 => return Err(busy(&slot)),
            RunOrigin::Manual => {
                if let Some(current) = slot.run.clone() {
                    let preempted = current.origin == RunOrigin::Auto
                        && current.request_cancel(CancelReason::Preempted).is_ok();
                    if !preempted {
                        return Err(busy(&slot));
                    }
                    slot.manual_waiting += 1;
                    let deadline = Instant::now() + preempt_wait;
                    while slot.run.is_some() {
                        let left = deadline.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            slot.manual_waiting -= 1;
                            return Err(busy(&slot));
                        }
                        slot = self
                            .slot
                            .released
                            .wait_timeout(slot, left)
                            .unwrap_or_else(PoisonError::into_inner)
                            .0;
                    }
                    slot.manual_waiting -= 1;
                }
            }
        }
        slot.run = Some(active.clone());
        if slot.closed {
            let _ = active.request_cancel(CancelReason::RootChanged);
        } else if active
            .supersede
            .as_ref()
            .is_some_and(SupersedeToken::is_requested)
        {
            let _ = active.request_cancel(CancelReason::Superseded);
        }
        Ok(())
    }

    /// Run one producer to a terminal record. Refuses with `plan_changed`
    /// unless the recomputed plan hashes to the caller's `planSha256`, and with
    /// `producer_busy` while another run of this checkout is active.
    pub fn start_run(&self, ctx: &RunContext<'_>, request: &RunRequest) -> Result<Value> {
        let store = ctx.store;
        let prepared = plan::prepare(store, &request.plan, ctx.stub)?;
        let template = plan::run_dir_template(store);
        let planned = plan::with_hash(prepared.build(store, ctx.stub, &template));
        ensure!(
            planned["planSha256"].as_str() == Some(request.plan_sha256.as_str()),
            "{PLAN_CHANGED}: the plan no longer matches the one inspected"
        );

        test_hook("TRELLIS_TEST_PRODUCER_PRESLOT_SOCKET");
        let run_id = Uuid::new_v4().simple().to_string();
        let active = Arc::new(ActiveRun {
            run_id: run_id.clone(),
            producer: prepared.row.target.name().to_owned(),
            trust_profile: prepared.row.trust,
            plan_sha256: request.plan_sha256.clone(),
            source_revision: prepared.basis.source_revision.clone(),
            created_at: now_millis(),
            origin: request.origin,
            supersede: request.supersede.clone(),
            cancel: Arc::new(AtomicBool::new(false)),
            control: Mutex::new(Control {
                phase: RunState::Materializing,
                finished: false,
                reason: None,
                request_id: None,
                process: None,
                group_status: None,
            }),
        });
        self.install(&active)?;
        let _slot = SlotGuard {
            owner: self,
            run: active.clone(),
        };

        let (roots, identity) = store.state_topology();
        let state = StateDir::open(roots, identity)?;
        sweep_state(store, true);
        let dir = fs::canonicalize(state.create_run_dir(&run_id)?)?;
        let mut cleanup = RunCleanup {
            state: &state,
            run: &active,
            armed: true,
        };
        let record = RunRecord {
            version: 1,
            run_id: run_id.clone(),
            producer: active.producer.clone(),
            trust_profile: active.trust_profile,
            plan_sha256: active.plan_sha256.clone(),
            source_revision: active.source_revision.clone(),
            state: RunState::Materializing,
            outcome: None,
            error_code: None,
            daemon: DaemonIdentity::current(),
            process: None,
            request_id: None,
            created_at: active.created_at,
            finished_at: None,
            diagnostics: None,
            group_status: None,
            origin: active.origin,
        };
        state.write_run_record(&record)?;
        let mut run = Run {
            ctx,
            prepared: &prepared,
            active: &active,
            dir,
            recorder: Recorder {
                state: &state,
                record,
            },
        };
        let finish = match run.execute() {
            Ok(finish) => finish,
            Err(error) if active.control().request_id.is_some() => {
                // The import is accepted but its outcome is unknown (the queue stayed busy
                // or the runtime closed). The request still reads `out/` and `batch/`, and
                // the record stays `importing`; the sweep abandons it and strips those
                // directories once the request is no longer pending.
                cleanup.armed = false;
                return Err(error);
            }
            Err(error) => run.finish_from_error(&error, None),
        };
        active.control().finished = true;
        let written = run.recorder.update(|record| {
            record.state = finish.state;
            record.outcome = Some(finish.outcome.to_owned());
            record.error_code = finish.error_code.clone();
            record.finished_at = Some(now_millis());
            record.process = None;
            record.diagnostics = finish.diagnostics.clone();
            if let Some(imported) = &finish.import {
                record.request_id = Some(imported.request_id.clone());
            }
        });
        cleanup.armed = false;
        // The import request is terminal or never existed, so nothing but the record is needed.
        let _ = state.strip_to_record(&run_id);
        // Retention never deletes a directory another accepted request still reads; when
        // the pending set cannot be read, pruning waits for the next sweep.
        if let Ok(keep) = store.pending_generation_dirs() {
            let _ = state.prune_runs(&keep);
        }
        // Only a manual import approves later automatic runs, and only the operator's request
        // does: the record is the one they inspected, never a run Trellis started itself.
        if active.origin == RunOrigin::Manual && finish.outcome == "imported" {
            let approved = approval::record(&request.plan, &prepared, identity);
            if let Err(error) = approval::write(&state, &approved) {
                let _ = writeln_stderr(&format!("producer approval not written: {error:#}"));
            }
        }
        written?;
        Ok(json!({
            "runId": run_id,
            "state": finish.state,
            "outcome": finish.outcome,
            "errorCode": finish.error_code,
            "planSha256": request.plan_sha256,
            "producer": active.producer,
            "trustProfile": active.trust_profile,
            "sourceRevision": active.source_revision,
            "origin": active.origin,
            "import": finish.import.as_ref().map(|imported| json!({
                "requestId": imported.request_id,
                "publishedRevision": imported.published_revision,
                "report": imported.report,
            })),
            "diagnostics": finish.diagnostics,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active(run_id: &str) -> Arc<ActiveRun> {
        active_as(run_id, RunOrigin::Manual, None)
    }

    fn auto(run_id: &str, token: &SupersedeToken) -> Arc<ActiveRun> {
        active_as(run_id, RunOrigin::Auto, Some(token.clone()))
    }

    fn active_as(
        run_id: &str,
        origin: RunOrigin,
        supersede: Option<SupersedeToken>,
    ) -> Arc<ActiveRun> {
        Arc::new(ActiveRun {
            run_id: run_id.to_owned(),
            producer: "stub".to_owned(),
            trust_profile: super::super::TrustProfile::Restricted,
            plan_sha256: "0".repeat(64),
            source_revision: "revision".to_owned(),
            created_at: 1,
            origin,
            supersede,
            cancel: Arc::new(AtomicBool::new(false)),
            control: Mutex::new(Control {
                phase: RunState::Materializing,
                finished: false,
                reason: None,
                request_id: None,
                process: None,
                group_status: None,
            }),
        })
    }

    fn is_busy(result: Result<()>) -> bool {
        result.is_err_and(|error| error.to_string().starts_with(PRODUCER_BUSY))
    }

    /// Shutdown before the run takes its slot is not lost: the run installs cancelled
    /// with the root-change reason.
    #[test]
    fn shutdown_before_install_cancels_the_run_that_installs_after() {
        let owner = ProducerOwner::default();
        owner.shutdown();
        let run = active("a");
        owner.install(&run).unwrap();
        assert!(run.cancel.load(Ordering::SeqCst));
        assert_eq!(run.control().reason, Some(CancelReason::RootChanged));
    }

    #[test]
    fn shutdown_after_install_cancels_the_active_run_and_a_second_is_busy() {
        let owner = ProducerOwner::default();
        let run = active("a");
        owner.install(&run).unwrap();
        assert!(!run.cancel.load(Ordering::SeqCst));
        assert!(owner.install(&active("b")).is_err());
        owner.shutdown();
        assert!(run.cancel.load(Ordering::SeqCst));
    }

    #[test]
    fn cancel_of_a_finished_run_is_refused() {
        let run = active("a");
        run.control().finished = true;
        let error = run.request_cancel(CancelReason::Requested).unwrap_err();
        assert!(error.to_string().starts_with(NOT_CANCELLABLE));
        assert!(!run.cancel.load(Ordering::SeqCst));
    }

    #[test]
    fn install_honours_supersede_token() {
        let owner = ProducerOwner::default();
        let token = SupersedeToken::default();
        token.request();
        let run = auto("a", &token);
        owner.install(&run).unwrap();
        assert!(run.cancel.load(Ordering::SeqCst));
        assert_eq!(run.control().reason, Some(CancelReason::Superseded));

        // A run that was never superseded installs untouched.
        let owner = ProducerOwner::default();
        let run = auto("b", &SupersedeToken::default());
        owner.install(&run).unwrap();
        assert!(!run.cancel.load(Ordering::SeqCst));
    }

    #[test]
    fn supersede_sets_token_when_no_run() {
        let owner = ProducerOwner::default();
        let token = SupersedeToken::default();
        assert!(!token.is_requested());
        assert_eq!(owner.supersede(&token), AutoCancel::NoAutoRun);
        assert!(token.is_requested());
        assert_eq!(owner.active_origin(), None);
    }

    #[test]
    fn supersede_ignores_other_runs() {
        let owner = ProducerOwner::default();
        let (own, other) = (SupersedeToken::default(), SupersedeToken::default());
        let manual = active("m");
        owner.install(&manual).unwrap();
        assert_eq!(owner.supersede(&other), AutoCancel::NoAutoRun);
        assert!(!manual.cancel.load(Ordering::SeqCst));
        drop(SlotGuard {
            owner: &owner,
            run: manual,
        });

        let run = auto("a", &own);
        owner.install(&run).unwrap();
        assert_eq!(owner.active_origin(), Some(RunOrigin::Auto));
        assert_eq!(owner.supersede(&other), AutoCancel::NoAutoRun);
        assert!(other.is_requested() && !run.cancel.load(Ordering::SeqCst));
        assert_eq!(owner.supersede(&own), AutoCancel::Requested);
        assert!(run.cancel.load(Ordering::SeqCst));
        assert_eq!(run.control().reason, Some(CancelReason::Superseded));
    }

    #[test]
    fn supersede_of_an_importing_run_is_not_cancellable() {
        let owner = ProducerOwner::default();
        let token = SupersedeToken::default();
        let run = auto("a", &token);
        run.control().phase = RunState::Importing;
        owner.install(&run).unwrap();
        assert_eq!(owner.supersede(&token), AutoCancel::NotCancellable);
        assert!(!run.cancel.load(Ordering::SeqCst));
    }

    /// A manual run cancels a running automatic run and takes its slot once it leaves. The
    /// waiter is released by the slot guard's drop, not by a timer.
    #[test]
    fn preemption_waits_for_release() {
        let owner = ProducerOwner::default();
        let running = auto("a", &SupersedeToken::default());
        owner.install(&running).unwrap();
        let manual = active("m");
        let waiter = {
            let (owner, manual) = (owner.clone(), manual.clone());
            std::thread::spawn(move || owner.install(&manual))
        };
        while !running.cancel.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        assert_eq!(running.control().reason, Some(CancelReason::Preempted));
        assert_eq!(owner.slot().manual_waiting, 1);
        assert_eq!(owner.active_origin(), Some(RunOrigin::Auto));

        drop(SlotGuard {
            owner: &owner,
            run: running,
        });
        waiter.join().unwrap().unwrap();
        assert_eq!(owner.active_origin(), Some(RunOrigin::Manual));
        assert_eq!(owner.slot().manual_waiting, 0);
    }

    /// A preempted run that never leaves does not hold the manual run forever: the wait ends
    /// with `producer_busy` and the waiter count returns to zero.
    #[test]
    fn preemption_wait_expires_as_busy() {
        let owner = ProducerOwner::default();
        let running = auto("a", &SupersedeToken::default());
        owner.install(&running).unwrap();
        let result = owner.install_within(&active("m"), Duration::from_millis(50));
        assert!(is_busy(result));
        assert_eq!(running.control().reason, Some(CancelReason::Preempted));
        assert_eq!(owner.slot().manual_waiting, 0);
        assert_eq!(owner.active_origin(), Some(RunOrigin::Auto));
    }

    #[test]
    fn manual_against_manual_or_an_importing_run_is_busy() {
        let owner = ProducerOwner::default();
        owner.install(&active("a")).unwrap();
        assert!(is_busy(owner.install(&active("b"))));

        let owner = ProducerOwner::default();
        let importing = auto("a", &SupersedeToken::default());
        importing.control().phase = RunState::Importing;
        owner.install(&importing).unwrap();
        assert!(is_busy(owner.install(&active("b"))));
        assert!(!importing.cancel.load(Ordering::SeqCst));
        assert_eq!(owner.slot().manual_waiting, 0);

        let owner = ProducerOwner::default();
        let finished = auto("a", &SupersedeToken::default());
        finished.control().finished = true;
        owner.install(&finished).unwrap();
        assert!(is_busy(owner.install(&active("b"))));
    }

    #[test]
    fn auto_against_a_run_or_a_waiting_manual_run_is_busy() {
        let owner = ProducerOwner::default();
        owner.install(&active("a")).unwrap();
        assert!(is_busy(
            owner.install(&auto("b", &SupersedeToken::default()))
        ));

        let owner = ProducerOwner::default();
        owner.slot().manual_waiting = 1;
        assert!(is_busy(
            owner.install(&auto("b", &SupersedeToken::default()))
        ));
        // A second manual run does not join the first one's wait.
        assert!(is_busy(owner.install(&active("m"))));
        owner.slot().manual_waiting = 0;
        owner
            .install(&auto("c", &SupersedeToken::default()))
            .unwrap();
    }
}
