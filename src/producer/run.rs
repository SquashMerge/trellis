//! The run owner (D8, D9, D11): one explicit, owner-requested producer run at a
//! time per checkout. `start_run` is the only caller of `spawn_producer`. The
//! owner alone writes `run.json` and alone signals the child; callers observe
//! the published record and the returned terminal reply.

use super::batch::{self, ARTIFACT_PATH, BatchSpec};
use super::exec::{self, Ending, Hooks, ProducerSpec};
use super::import::{self, ImportOutcome};
use super::plan::{self, PlanRequest, Prepared};
use super::profiles::{self, Stub};
use super::state::{DaemonIdentity, RunRecord, RunState, StateDir, SweepReport, now_millis};
use super::worktree;
use super::{
    Diagnostics, NOT_CANCELLABLE, NOT_FOUND, PLAN_CHANGED, PRODUCER_BUSY, ProcessIdentity,
    ROOT_CHANGED,
};
use crate::{http::DaemonState, semantic_batch::descriptor::GenerationOrigin, store::Store};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
};
use uuid::Uuid;

/// The `producer_run` payload: a plan request plus the hash the caller inspected.
#[derive(Clone, Debug)]
pub struct RunRequest {
    pub plan: PlanRequest,
    pub plan_sha256: String,
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
        })
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
}

struct Control {
    phase: RunState,
    /// The run's outcome is decided: a cancel now has nothing left to stop.
    finished: bool,
    reason: Option<CancelReason>,
    request_id: Option<String>,
    process: Option<ProcessIdentity>,
}

/// The one run of a checkout that is in flight.
struct ActiveRun {
    run_id: String,
    producer: String,
    trust_profile: super::TrustProfile,
    plan_sha256: String,
    source_revision: String,
    created_at: u64,
    /// Read by the worktree writer and the monitor loop.
    cancel: AtomicBool,
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
    slot: Arc<Mutex<Slot>>,
}

/// The run slot and the owner's closed mark, changed together under one lock so a
/// shutdown can never fall between a run's admission and its installation.
#[derive(Default)]
struct Slot {
    run: Option<Arc<ActiveRun>>,
    /// The checkout is closing or lost its root; a runtime is never reopened, so
    /// the mark is permanent.
    closed: bool,
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
        let (process, enqueued) = {
            let mut control = self.run.control();
            (control.process.take(), control.request_id.is_some())
        };
        if let Some(process) = process {
            unsafe { libc::killpg(process.pgid, libc::SIGKILL) };
        }
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
fn typed_code(error: &anyhow::Error) -> (String, String) {
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

/// Test-support hold point: bind the socket, announce it, and wait for the
/// test to connect. Once per path, like the semantic cutoff hook.
#[cfg(all(feature = "test-support", unix))]
fn test_hook(variable: &str) {
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
fn test_hook(_variable: &str) {}

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
            &dir_text,
        );
        let rules = profiles::rules(&row);

        if self.cancelled() {
            return Ok(self.cancel_finish(None));
        }
        let baseline = store.index_baseline()?;
        let materialized = store.with_native_reader(baseline, |reader| {
            ensure!(
                reader.header()?.revision.id == revision,
                "stale_basis: head revision changed"
            );
            worktree::materialize(reader, &rules, &self.dir, &self.active.cancel)
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
            };
            exec::spawn_producer(&spec, &limits, &active.cancel, &mut hooks)
        };
        self.active.control().process = None;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = writeln_stderr(&format!("producer did not start: {error:#}"));
                return Ok(Finish::failed("producer_failed", "producer_failed"));
            }
        };
        // The producer phase is over: the group is empty, so what only it
        // needed goes now. The log tails are in the diagnostics.
        let _ = self.recorder.state.end_producer_phase(&run_id);
        test_hook("TRELLIS_TEST_PRODUCER_EXITED_SOCKET");
        let diagnostics = outcome.diagnostics;
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
                    language: row.target.language(),
                    workspace: Path::new(store.workspace_root()),
                    dir: &self.dir,
                    generation,
                },
            )
        });
        let written = match written {
            Ok(written) => written,
            Err(error) => return Ok(self.finish_from_error(&error, Some(&diagnostics))),
        };
        let options = import::options_for(&written, GenerationOrigin::TrellisRun, &run_id)?;

        // Enqueue under the control lock: a cancel either lands before it and
        // wins, or finds the request ID and is refused.
        let id = {
            let mut control = self.active.control();
            if self.active.cancel.load(Ordering::SeqCst) {
                drop(control);
                return Ok(self.cancel_finish(Some(&diagnostics)));
            }
            let id = import::submit(self.ctx.scheduler, &options)?;
            control.phase = RunState::Importing;
            control.request_id = Some(id.clone());
            id
        };
        // The request is accepted: from here the run only observes it. A record that
        // cannot be written leaves `writingBatch` behind, and the final record corrects it.
        let _ = test_fault("TRELLIS_TEST_PRODUCER_FAIL_IMPORTING_RECORD").and_then(|()| {
            self.recorder.update(|record| {
                record.state = RunState::Importing;
                record.request_id = Some(id.clone());
            })
        });
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
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
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
            "requestId": control.request_id,
            "process": control.process,
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
    fn install(&self, active: &Arc<ActiveRun>) -> Result<()> {
        let mut slot = self.slot();
        if let Some(current) = &slot.run {
            anyhow::bail!("{PRODUCER_BUSY}: run {} is active", current.run_id);
        }
        slot.run = Some(active.clone());
        if slot.closed {
            let _ = active.request_cancel(CancelReason::RootChanged);
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
            cancel: AtomicBool::new(false),
            control: Mutex::new(Control {
                phase: RunState::Materializing,
                finished: false,
                reason: None,
                request_id: None,
                process: None,
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
        Arc::new(ActiveRun {
            run_id: run_id.to_owned(),
            producer: "stub".to_owned(),
            trust_profile: super::super::TrustProfile::Restricted,
            plan_sha256: "0".repeat(64),
            source_revision: "revision".to_owned(),
            created_at: 1,
            cancel: AtomicBool::new(false),
            control: Mutex::new(Control {
                phase: RunState::Materializing,
                finished: false,
                reason: None,
                request_id: None,
                process: None,
            }),
        })
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
}
