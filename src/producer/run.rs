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
    CompleteDiagnostics, Diagnostics, NOT_CANCELLABLE, NOT_FOUND, PLAN_CHANGED, PRODUCER_BUSY,
    ProcessIdentity, ROOT_CHANGED, TerminationStatus, UnknownDiagnostics,
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
    cancel: crate::model::CancelFlag,
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
    terminal_io: Arc<Mutex<()>>,
}

/// The run slot and the owner's closed mark, changed together under one lock so a
/// shutdown can never fall between a run's admission and its installation.
#[derive(Default)]
struct Slot {
    run: Option<Arc<ActiveRun>>,
    /// The checkout is closing or lost its root; a runtime is never reopened, so
    /// the mark is permanent.
    closed: bool,
    pending: Option<PendingTerminal>,
    version: u64,
    #[cfg(all(test, feature = "test-support"))]
    test_before_retry_write: Option<Box<dyn FnOnce() + Send>>,
}

/// A terminal result observed by this runtime but not yet durably recorded.
#[derive(Clone)]
struct PendingTerminal {
    state: StateDir,
    record: RunRecord,
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
            slot.version = slot.version.wrapping_add(1);
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
        let enqueued = {
            let mut control = self.run.control();
            control.process = None;
            control.request_id.is_some()
        };
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

    fn with(mut self, diagnostics: &CompleteDiagnostics) -> Self {
        self.diagnostics = Some(Diagnostics::Complete(diagnostics.clone()));
        self
    }

    fn artifact_metadata_error(diagnostics: &CompleteDiagnostics) -> Self {
        Self::failed("producer_failed", "producer_failed: artifact metadata").with(diagnostics)
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

fn artifact_presence(result: std::io::Result<bool>) -> std::io::Result<bool> {
    exec::metadata_or_missing(result).map(|present| present.unwrap_or(false))
}

#[cfg(all(test, feature = "test-support"))]
type SpawnTestHook = Box<dyn FnOnce(&ProducerSpec) -> Result<exec::ProcessOutcome>>;
#[cfg(all(test, feature = "test-support"))]
type EnqueuedTestHook = Box<dyn FnOnce(&Path, &str)>;
#[cfg(all(test, feature = "test-support"))]
type PendingTestHook = Box<dyn FnOnce(&ProducerOwner)>;
#[cfg(all(test, feature = "test-support"))]
thread_local! {
    static TEST_SPAWN: std::cell::RefCell<Option<SpawnTestHook>> = Default::default();
    static TEST_ENQUEUED: std::cell::RefCell<Option<EnqueuedTestHook>> = Default::default();
    static TEST_TERMINAL_PENDING: std::cell::RefCell<Option<PendingTestHook>> = Default::default();
    static TEST_FAIL_TRANSITION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static TEST_FAIL_SUBMIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static TEST_FAIL_TERMINAL_RECORD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static TEST_FAIL_ACCEPTED_WAIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static TEST_SWEEP_QUEUE_FAULT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
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
/// pickup directories no request names. A failure leaves the
/// state for the next sweep. `keep_pickups` protects pickup directories while
/// this daemon may be importing one (a sweep before a run).
pub fn sweep_state(store: &Store, keep_pickups: bool) -> Result<Option<SweepReport>> {
    sweep_state_inner(store, keep_pickups, false)
}

/// Called only after installing the checkout's exclusive run owner and before
/// creating its new run directory; no prior run of this owner can be active.
fn sweep_state_owned(store: &Store, keep_pickups: bool) -> Result<Option<SweepReport>> {
    sweep_state_inner(store, keep_pickups, true)
}

fn pending_dirs_with_retry<T>(
    mut read: impl FnMut() -> Result<T>,
    mut expired: impl FnMut() -> bool,
    mut wait: impl FnMut(),
) -> Result<T> {
    loop {
        match read() {
            Err(error) if crate::store::transient_storage_contention(&error) => {
                if [
                    "root_changed",
                    "incompatible_queue",
                    "store_unavailable",
                    "recovery_required",
                ]
                .contains(&error.to_string().split(':').next().unwrap_or(""))
                {
                    return Err(error);
                }
                if !crate::store::cli_read_retryable_contention(&error) {
                    return Err(recovery_operational_error(error));
                }
                if expired() {
                    return Err(error.context("storage_busy: producer recovery queue"));
                }
                wait();
            }
            result => return result,
        }
    }
}

fn recovery_operational_error(error: anyhow::Error) -> anyhow::Error {
    let message = error.to_string();
    if [
        "root_changed",
        "incompatible_queue",
        "invalid_request",
        "producer_state_corrupt",
        "recovery_required",
        "store_unavailable",
        "storage_busy",
    ]
    .contains(&message.split(':').next().unwrap_or(""))
    {
        return error;
    }
    if error
        .chain()
        .any(|cause| cause.is::<std::io::Error>() || cause.is::<rusqlite::Error>())
    {
        error.context("store_unavailable: producer recovery")
    } else {
        error
    }
}

fn sweep_state_inner(
    store: &Store,
    keep_pickups: bool,
    recover_current: bool,
) -> Result<Option<SweepReport>> {
    let (roots, identity) = store.state_topology();
    let Some(state) = StateDir::existing(roots, identity).map_err(recovery_operational_error)?
    else {
        return Ok(None);
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut keep = match pending_dirs_with_retry(
        || {
            #[cfg(all(test, feature = "test-support"))]
            if let Some(kind) = TEST_SWEEP_QUEUE_FAULT.with(|flag| flag.take()) {
                let error = anyhow::Error::new(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
                    None,
                ));
                return Err(if kind == "root" {
                    error.context("root_changed: injected guard")
                } else {
                    error
                });
            }
            store.pending_generation_dirs()
        },
        || std::time::Instant::now() >= deadline,
        || std::thread::sleep(std::time::Duration::from_millis(20)),
    ) {
        Ok(keep) => keep,
        Err(error) => {
            if error.to_string().starts_with("root_changed:") {
                return Err(error);
            }
            // Without a complete keep set no cleanup is safe, but readable
            // abandoned runs can still expose the unknown termination status.
            state
                .note_recovery_unknown_owned(&DaemonIdentity::current(), recover_current)
                .map_err(recovery_operational_error)?;
            return Err(recovery_operational_error(error));
        }
    };
    if keep_pickups {
        match fs::read_dir(state.path().join("pickups")) {
            Ok(entries) => {
                for entry in entries {
                    keep.insert(
                        entry
                            .map_err(anyhow::Error::from)
                            .map_err(recovery_operational_error)?
                            .path(),
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(recovery_operational_error(error.into())),
        }
    }
    Ok(Some(state.sweep_owned(
        &DaemonIdentity::current(),
        &keep,
        recover_current,
    )?))
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

    fn prepare_finish(&mut self, finish: &Finish) {
        {
            let record = &mut self.record;
            record.state = finish.state;
            record.outcome = Some(finish.outcome.to_owned());
            record.error_code = finish.error_code.clone();
            record.finished_at = Some(now_millis());
            record.process = None;
            if finish.diagnostics.is_some() {
                record.diagnostics = finish.diagnostics.clone();
            }
            if let Some(imported) = &finish.import {
                record.request_id = Some(imported.request_id.clone());
            }
        }
    }

    fn write_terminal(&self) -> Result<()> {
        #[cfg(all(test, feature = "test-support"))]
        if TEST_FAIL_TERMINAL_RECORD.with(|flag| flag.replace(false)) {
            return Err(std::io::Error::other("injected terminal record failure").into());
        }
        self.state.write_run_record(&self.record)
    }

    #[cfg(all(test, feature = "test-support"))]
    fn finish(&mut self, finish: &Finish) -> Result<()> {
        self.prepare_finish(finish);
        self.write_terminal()
    }
}

fn run_reply(run_id: &str, plan_sha256: &str, active: &ActiveRun, finish: &Finish) -> Value {
    json!({
        "runId": run_id,
        "state": finish.state,
        "outcome": finish.outcome,
        "errorCode": finish.error_code,
        "planSha256": plan_sha256,
        "producer": active.producer,
        "trustProfile": active.trust_profile,
        "sourceRevision": active.source_revision,
        "import": finish.import.as_ref().map(|imported| json!({
            "requestId": imported.request_id,
            "publishedRevision": imported.published_revision,
            "report": imported.report,
        })),
        "diagnostics": finish.diagnostics,
    })
}

impl Run<'_> {
    fn process_error(active: &ActiveRun, error: &anyhow::Error) -> Finish {
        let Some(failure) = error.downcast_ref::<exec::ProcessFailure>() else {
            return Finish::failed("producer_failed", "producer_failed");
        };
        let mut finish = match failure.ending {
            Some(Ending::Cancelled)
                if active.control().reason == Some(CancelReason::RootChanged) =>
            {
                Finish::failed("root_changed", ROOT_CHANGED)
            }
            Some(Ending::Cancelled) => Finish {
                state: RunState::Cancelled,
                outcome: "cancelled",
                error_code: Some("cancelled".into()),
                import: None,
                diagnostics: None,
            },
            Some(Ending::TimedOut) => Finish::failed("timed_out", "timed_out"),
            Some(Ending::LimitExceeded(kind)) => Finish::failed(
                "limit_exceeded",
                format!("limit_exceeded: {}", kind.reason()),
            ),
            _ => Finish::failed("producer_failed", "producer_failed"),
        };
        finish.diagnostics = Some(Diagnostics::Unknown(UnknownDiagnostics {
            termination: TerminationStatus::Unknown,
            failure: failure.kind,
            group_survived: None,
        }));
        finish
    }

    fn cancel_finish(&self, diagnostics: Option<&CompleteDiagnostics>) -> Finish {
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
        diagnostics: Option<&CompleteDiagnostics>,
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
            | "invalid_request"
            | "store_unavailable"
            | "incompatible_index"
            | "recovery_required"
            | "producer_failed" => Finish::failed("producer_failed", line),
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
        #[cfg(all(test, feature = "test-support"))]
        if phase == RunState::WritingBatch && TEST_FAIL_TRANSITION.with(|flag| flag.replace(false))
        {
            anyhow::bail!("store_unavailable: injected transition failure");
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
            #[cfg(all(test, feature = "test-support"))]
            let injected = TEST_SPAWN.with(|hook| hook.borrow_mut().take());
            #[cfg(all(test, feature = "test-support"))]
            if let Some(injected) = injected {
                injected(&spec)
            } else {
                exec::spawn_producer(&spec, &limits, &active.cancel, &mut hooks)
            }
            #[cfg(not(all(test, feature = "test-support")))]
            exec::spawn_producer(&spec, &limits, &active.cancel, &mut hooks)
        };
        self.active.control().process = None;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = writeln_stderr(&format!("producer process failed: {error:#}"));
                return Ok(Self::process_error(self.active, &error));
            }
        };
        // The producer phase is over. Group survival, including an unknown
        // result, and the log tails are recorded in the diagnostics.
        let _ = self.recorder.state.end_producer_phase(&run_id);
        test_hook("TRELLIS_TEST_PRODUCER_EXITED_SOCKET");
        let mut diagnostics = outcome.diagnostics;
        let _ = self.recorder.update(|record| {
            record.process = None;
            record.diagnostics = Some(Diagnostics::Complete(diagnostics.clone()));
        });
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
            Ending::ArtifactMetadataUnavailable => {
                if self.cancelled() {
                    return Ok(self.cancel_finish(Some(&diagnostics)));
                }
                return Ok(Finish::artifact_metadata_error(&diagnostics));
            }
            Ending::Exited => {}
        }
        if diagnostics.exit.code != Some(0) {
            return Ok(Finish::failed("producer_failed", "producer_failed").with(&diagnostics));
        }
        let artifact_present = match artifact_presence(
            fs::symlink_metadata(self.dir.join(ARTIFACT_PATH)).map(|meta| meta.is_file()),
        ) {
            Ok(present) => present,
            Err(_) => {
                diagnostics.artifact_bytes = None;
                if self.cancelled() {
                    return Ok(self.cancel_finish(Some(&diagnostics)));
                }
                return Ok(Finish::artifact_metadata_error(&diagnostics));
            }
        };
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
        let spec = BatchSpec {
            pin: &prepared.pin,
            language: row.target.language(),
            workspace: Path::new(store.workspace_root()),
            dir: &self.dir,
            generation,
        };
        let written = if diagnostics.requires_validated_snapshot() {
            (|| {
                let limits = crate::semantic_batch::BatchLimits::effective()
                    .map_err(|error| anyhow::anyhow!("invalid_request: {error}"))?;
                let mut counters = crate::semantic_batch::BatchCounters::default();
                let written = batch::write_frozen_batch_at_revision(
                    store,
                    &spec,
                    &limits,
                    &mut counters,
                    &self.active.cancel,
                    &revision,
                )?;
                ensure!(
                    written.source_revision == revision,
                    "stale_basis: head revision changed"
                );
                store.with_native_reader(store.index_baseline()?, |reader| {
                    ensure!(
                        reader.header()?.revision.id == revision,
                        "stale_basis: head revision changed"
                    );
                    Ok(())
                })?;
                Ok(written)
            })()
        } else {
            store.with_native_reader(store.index_baseline()?, |reader| {
                // The head must still be the revision the worktree was written from.
                ensure!(
                    reader.header()?.revision.id == revision,
                    "stale_basis: head revision changed"
                );
                batch::write_batch(reader, &spec)
            })
        };
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
            #[cfg(all(test, feature = "test-support"))]
            if TEST_FAIL_SUBMIT.with(|flag| flag.replace(false)) {
                anyhow::bail!("storage_busy: injected pre-acceptance failure");
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
        #[cfg(all(test, feature = "test-support"))]
        TEST_ENQUEUED.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook(&self.dir, &id);
            }
        });
        #[cfg(all(test, feature = "test-support"))]
        if TEST_FAIL_ACCEPTED_WAIT.with(|flag| flag.replace(false)) {
            anyhow::bail!("storage_busy: injected accepted wait failure");
        }
        test_fault("TRELLIS_TEST_PRODUCER_FAIL_WAIT")?;
        let imported = import::wait(store, self.ctx.scheduler, &options, id)?;
        let finish = if imported.is_done() {
            Finish {
                state: RunState::Done,
                outcome: "imported",
                error_code: None,
                import: Some(imported),
                diagnostics: Some(Diagnostics::Complete(diagnostics)),
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

    /// An operation-scoped retry. The dedicated I/O mutex serializes writes;
    /// shutdown never waits for it or for filesystem I/O.
    fn reconcile_terminal(&self) -> Result<()> {
        let _io = self
            .terminal_io
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let pending = {
            let slot = self.slot();
            if slot.closed {
                return Ok(());
            }
            slot.pending.clone()
        };
        let Some(pending) = pending else {
            return Ok(());
        };
        #[cfg(all(test, feature = "test-support"))]
        {
            let hook = self.slot().test_before_retry_write.take();
            if let Some(hook) = hook {
                hook();
            }
        }
        {
            let slot = self.slot();
            if slot.closed
                || !slot
                    .pending
                    .as_ref()
                    .is_some_and(|current| current.record.run_id == pending.record.run_id)
            {
                return Ok(());
            }
        }
        match pending.state.write_run_record(&pending.record) {
            Ok(()) => {
                let mut slot = self.slot();
                if slot
                    .pending
                    .as_ref()
                    .is_some_and(|current| current.record.run_id == pending.record.run_id)
                {
                    slot.pending = None;
                    slot.version = slot.version.wrapping_add(1);
                }
                Ok(())
            }
            Err(error) => Err(error.context("store_unavailable: producer terminal record")),
        }
    }

    /// Report the owner's observed terminal result while an unsuccessful write
    /// is pending; never present it as a confirmed durable record.
    pub fn producer_status(
        &self,
        store: &Store,
        request: &plan::StatusRequest,
        stub: Option<&Stub>,
    ) -> Result<Value> {
        let _ = self.reconcile_terminal();
        // The owner version is a published transition signal. Retry only if an
        // actual owner transition raced the disk read, with a bounded refusal.
        for _ in 0..3 {
            let (version, pending, active_id, active) = {
                let slot = self.slot();
                let active_id = slot
                    .run
                    .as_ref()
                    .filter(|run| !run.control().finished)
                    .map(|run| run.run_id.clone());
                (
                    slot.version,
                    slot.pending.as_ref().map(|pending| pending.record.clone()),
                    active_id,
                    Self::active_status(&slot),
                )
            };
            let mut status = plan::status_with_pending(
                store,
                request,
                stub,
                pending.as_ref(),
                Some(active_id.as_deref()),
            )?;
            if self.slot().version == version {
                status["activeRun"] = active;
                return Ok(status);
            }
        }
        anyhow::bail!("storage_busy: producer status changed")
    }

    /// `activeRun` of the status reply.
    pub fn status(&self) -> Value {
        Self::active_status(&self.slot())
    }

    fn active_status(slot: &Slot) -> Value {
        let Some(run) = slot.run.as_ref() else {
            return Value::Null;
        };
        let control = run.control();
        if control.finished {
            return Value::Null;
        }
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
        // No retry outlives this owner. A later owner infers uncertainty from
        // its own published active-run identity and recovers the old record.
        slot.pending = None;
        slot.version = slot.version.wrapping_add(1);
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
        ensure!(
            slot.pending.is_none(),
            "storage_busy: producer terminal record pending"
        );
        slot.run = Some(active.clone());
        slot.version = slot.version.wrapping_add(1);
        if slot.closed {
            let _ = active.request_cancel(CancelReason::RootChanged);
        }
        Ok(())
    }

    /// Run one producer to a terminal record. Refuses with `plan_changed`
    /// unless the recomputed plan hashes to the caller's `planSha256`, and with
    /// `producer_busy` while another run of this checkout is active.
    pub fn start_run(&self, ctx: &RunContext<'_>, request: &RunRequest) -> Result<Value> {
        self.reconcile_terminal()?;
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
            cancel: Arc::new(AtomicBool::new(false)),
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
        sweep_state_owned(store, true)?;
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
            Err(error) => {
                let mut finish = run.finish_from_error(&error, None);
                if finish.diagnostics.is_none() {
                    finish.diagnostics = run.recorder.record.diagnostics.clone();
                }
                finish
            }
        };
        let io = self
            .terminal_io
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        {
            let mut slot = self.slot();
            active.control().finished = true;
            run.recorder.prepare_finish(&finish);
            if !slot.closed {
                slot.pending = Some(PendingTerminal {
                    state: state.clone(),
                    record: run.recorder.record.clone(),
                });
            }
            slot.version = slot.version.wrapping_add(1);
        }
        let written = run
            .recorder
            .write_terminal()
            .map_err(|error| error.context("store_unavailable: producer terminal record"));
        if written.is_ok() {
            let mut slot = self.slot();
            if slot
                .pending
                .as_ref()
                .is_some_and(|pending| pending.record.run_id == run_id)
            {
                slot.pending = None;
            }
            slot.version = slot.version.wrapping_add(1);
        }
        drop(io);
        if written.is_err() {
            #[cfg(all(test, feature = "test-support"))]
            TEST_TERMINAL_PENDING.with(|hook| {
                if let Some(hook) = hook.borrow_mut().take() {
                    hook(self);
                }
            });
        }
        cleanup.armed = false;
        // The import request is terminal or never existed, so nothing but the record is needed.
        let _ = state.strip_to_record(&run_id);
        // Retention never deletes a directory another accepted request still reads; when
        // the pending set cannot be read, pruning waits for the next sweep.
        if written.is_ok()
            && let Ok(keep) = store.pending_generation_dirs()
        {
            let _ = state.prune_runs(&keep);
        }
        written?;
        Ok(run_reply(&run_id, &request.plan_sha256, &active, &finish))
    }
}

#[cfg(all(test, feature = "test-support"))]
mod lifecycle_tests {
    include!("run_lifecycle_tests.rs");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "test-support")]
    pub(in crate::producer::run) mod frozen_run {
        use super::*;
        use protobuf::{EnumOrUnknown, Message};
        use std::os::unix::fs::MetadataExt;

        #[derive(Clone, Copy)]
        pub(in crate::producer::run) enum Scenario {
            Normal,
            ChangedHead,
            TransitionFailure,
            SubmitFailure,
            RecoveryFailure,
            TerminationFailure(Ending, bool),
            TerminalWriteFailure,
            TerminalWriteShutdown,
            TerminalAfterRename,
            TerminalRenameLost,
            AcceptedWaiterExit,
            AcceptedWaiterFailure,
            ActiveStatus,
        }

        pub(in crate::producer::run) fn artifact(paths: &[&str], invalid: bool) -> Vec<u8> {
            let pin = profiles::Target::Stub.pin().unwrap();
            let mut index = scip::types::Index::new();
            let mut metadata = scip::types::Metadata::new();
            metadata.text_document_encoding = EnumOrUnknown::from_i32(1);
            let mut tool = scip::types::ToolInfo::new();
            tool.name = pin.name;
            tool.version = pin.version;
            metadata.tool_info = Some(tool).into();
            index.metadata = Some(metadata).into();
            for (n, path) in paths.iter().enumerate() {
                let mut doc = scip::types::Document::new();
                doc.relative_path = (*path).into();
                doc.language = "javascript".into();
                let mut occurrence = scip::types::Occurrence::new();
                occurrence.range = vec![0, 0, 1];
                occurrence.symbol = format!("pkg d{n}.");
                occurrence.symbol_roles = 1;
                if invalid && n + 1 == paths.len() {
                    occurrence.syntax_kind = EnumOrUnknown::from_i32(999_999);
                }
                doc.occurrences.push(occurrence);
                index.documents.push(doc);
            }
            index.write_to_bytes().unwrap()
        }

        pub(in crate::producer::run) fn exercise(
            bytes: Vec<u8>,
            expected: &str,
            partial: bool,
            missing_disposition: bool,
            cancel_preflight: bool,
            scenario: Scenario,
        ) {
            let temp = tempfile::tempdir().unwrap();
            let workspace = temp.path().join("workspace");
            fs::create_dir(&workspace).unwrap();
            for name in ["a.js", "b.js"] {
                fs::write(workspace.join(name), "function example() {}\n").unwrap();
            }
            let store = Store::open_for_tests(&temp.path().join("state"), &workspace).unwrap();
            let options = crate::indexer::IndexOptions::new(workspace.clone());
            let cancel = Arc::new(AtomicBool::new(false));
            let (graph, native, capture) =
                crate::indexer::index_workspace_bundle(&options, store.root_id(), &cancel, |_| {})
                    .unwrap();
            let writer = store.publication_for_tests().unwrap();
            store
                .publish_native(
                    &graph,
                    &capture,
                    &native,
                    &writer,
                    store.index_baseline().unwrap(),
                    &cancel,
                )
                .unwrap();
            drop(writer);
            let scheduler = crate::http::new(
                store.clone(),
                options,
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                "127.0.0.1:1".parse().unwrap(),
            )
            .unwrap();
            scheduler.force_root_transition_tick_for_tests().unwrap();
            let stub = Stub {
                path: std::env::current_exe().unwrap(),
                args: vec![],
            };
            let plan = PlanRequest::parse(&json!({
                "version": 1, "producer": profiles::STUB_NAME, "trustProfile": "restricted",
                "limits": {"timeoutSecs": 10, "memoryMiB": 128}, "executable": null,
                "environment": {"PATH": "/usr/bin:/bin", "HOME": "/tmp"}
            }))
            .unwrap();
            let hash = plan::plan(&store, &plan, Some(&stub)).unwrap()["planSha256"]
                .as_str()
                .unwrap()
                .to_owned();
            let original = bytes.clone();
            let owner = ProducerOwner::default();
            if matches!(scenario, Scenario::TransitionFailure) {
                TEST_FAIL_TRANSITION.with(|flag| flag.set(true));
            }
            if matches!(scenario, Scenario::SubmitFailure) {
                TEST_FAIL_SUBMIT.with(|flag| flag.set(true));
            }
            if matches!(
                scenario,
                Scenario::TerminalWriteFailure | Scenario::TerminalWriteShutdown
            ) {
                TEST_FAIL_TERMINAL_RECORD.with(|flag| flag.set(true));
            }
            if matches!(
                scenario,
                Scenario::TerminalAfterRename | Scenario::TerminalRenameLost
            ) {
                crate::producer::state::fail_terminal_after_rename();
            }
            if matches!(scenario, Scenario::TerminalWriteFailure) {
                let store = store.clone();
                let stub = stub.clone();
                TEST_TERMINAL_PENDING.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move |owner| {
                        let (state, record) = {
                            let slot = owner.slot();
                            assert!(slot.run.is_some(), "the original slot guard is still active");
                            let pending = slot.pending.as_ref().unwrap();
                            (pending.state.clone(), pending.record.clone())
                        };
                        let fault = state.run_dir(&record.run_id).unwrap().join(format!(".run.json.tmp-{}", std::process::id()));
                        fs::create_dir(&fault).unwrap();
                        let request = plan::StatusRequest::parse(&json!({"version": 1, "runId": record.run_id, "environment": {"PATH": "/usr/bin", "HOME": "/tmp"}})).unwrap();
                        let status = owner.producer_status(&store, &request, Some(&stub)).unwrap();
                        assert!(status["activeRun"].is_null());
                        assert_eq!(status["run"]["state"], "failed");
                        assert_eq!(status["run"]["recordedState"], Value::Null);
                        assert_eq!(status["run"]["terminalRecordPending"], true);
                        assert_eq!(status["run"]["diagnostics"]["termination"], "unknown");
                        fs::remove_dir(fault).unwrap();
                    }));
                });
            }
            let during_spawn_store = store.clone();
            let during_spawn_workspace = workspace.clone();
            let during_spawn_owner = owner.clone();
            let during_spawn_stub = stub.clone();
            let before_rename = Arc::new(Mutex::new(None::<Vec<u8>>));
            let before_rename_spawn = before_rename.clone();
            let accepted = Arc::new(Mutex::new(None::<(PathBuf, String)>));
            if matches!(
                scenario,
                Scenario::AcceptedWaiterExit | Scenario::AcceptedWaiterFailure
            ) {
                TEST_FAIL_ACCEPTED_WAIT.with(|flag| flag.set(true));
                let accepted = accepted.clone();
                TEST_ENQUEUED.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move |dir, id| {
                        *accepted.lock().unwrap() = Some((dir.to_owned(), id.to_owned()));
                    }))
                });
            }
            TEST_SPAWN.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move |spec| {
                    fs::write(&spec.artifact, bytes).unwrap();
                    if matches!(scenario, Scenario::TerminalRenameLost) {
                        let record = spec.artifact.parent().unwrap().parent().unwrap().join("run.json");
                        *before_rename_spawn.lock().unwrap() = Some(fs::read(record).unwrap());
                    }
                    if matches!(scenario, Scenario::ActiveStatus) {
                        let id = during_spawn_owner.slot().run.as_ref().unwrap().run_id.clone();
                        let request = plan::StatusRequest::parse(&json!({"version":1,"runId":id,"environment":{"PATH":"/usr/bin","HOME":"/tmp"}})).unwrap();
                        let status = during_spawn_owner.producer_status(&during_spawn_store, &request, Some(&during_spawn_stub)).unwrap();
                        assert_eq!(status["run"]["state"], "running");
                        assert_eq!(status["run"]["recoveryTermination"], Value::Null);
                        assert_eq!(status["activeRun"]["runId"], id);
                        return Ok(exec::ProcessOutcome { ending: Ending::TimedOut, process: ProcessIdentity { pgid: 1, leader_start_time: 0 }, diagnostics: CompleteDiagnostics::default() });
                    }
                    if matches!(scenario, Scenario::ChangedHead) {
                        fs::remove_file(during_spawn_workspace.join("b.js")).unwrap();
                        let cancel = Arc::new(AtomicBool::new(false));
                        let (graph, native, capture) = crate::indexer::index_workspace_bundle(
                            &crate::indexer::IndexOptions::new(during_spawn_workspace),
                            during_spawn_store.root_id(),
                            &cancel,
                            |_| {},
                        )
                        .unwrap();
                        let writer = during_spawn_store.publication_for_tests().unwrap();
                        during_spawn_store
                            .publish_native(
                                &graph,
                                &capture,
                                &native,
                                &writer,
                                during_spawn_store.index_baseline().unwrap(),
                                &cancel,
                            )
                            .unwrap();
                    }
                    if let Scenario::TerminationFailure(ending, root_changed) = scenario {
                        if root_changed {
                            during_spawn_owner.shutdown();
                        }
                        return Err(exec::ProcessFailure {
                            kind: super::super::super::TerminationFailure::Signal,
                            detail: "injected".into(),
                            ending: Some(ending),
                        }
                        .into());
                    }
                    if matches!(
                        scenario,
                        Scenario::TerminalWriteFailure | Scenario::TerminalWriteShutdown | Scenario::TerminalAfterRename | Scenario::TerminalRenameLost
                    ) {
                        return Err(exec::ProcessFailure {
                            kind: super::super::super::TerminationFailure::Signal,
                            detail: "injected process termination failure".into(),
                            ending: None,
                        }
                        .into());
                    }
                    Ok(exec::ProcessOutcome {
                        ending: Ending::Exited,
                        process: ProcessIdentity {
                            pgid: 1,
                            leader_start_time: 0,
                        },
                        diagnostics: CompleteDiagnostics {
                            exit: super::super::super::ExitReport {
                                code: Some(0),
                                ..Default::default()
                            },
                            termination: TerminationStatus::Unknown,
                            group_survived: None,
                            ..Default::default()
                        },
                    })
                }))
            });
            if missing_disposition {
                batch::BEFORE_FROZEN_VALIDATE.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(|written| {
                        use crate::semantic_batch::descriptor::{Descriptor, PageKind};
                        let root = written.descriptor.parent().unwrap().parent().unwrap();
                        let mut descriptor =
                            Descriptor::parse(&fs::read(&written.descriptor).unwrap()).unwrap();
                        let entry = descriptor
                            .pages
                            .iter_mut()
                            .find(|page| page.kind == PageKind::Coverage)
                            .unwrap();
                        let page_path = root.join(&entry.relative_path);
                        let mut page: Value =
                            serde_json::from_slice(&fs::read(&page_path).unwrap()).unwrap();
                        page["rows"].as_array_mut().unwrap().pop().unwrap();
                        let bytes = serde_json::to_vec(&page).unwrap();
                        descriptor.totals.page_bytes -= entry.byte_length;
                        entry.byte_length = bytes.len() as u64;
                        entry.row_count -= 1;
                        entry.sha256 = crate::scip_raw::sha(&bytes);
                        descriptor.totals.page_bytes += entry.byte_length;
                        descriptor.totals.covered_documents -= 1;
                        fs::write(page_path, bytes).unwrap();
                        let bytes = serde_json::to_vec(&descriptor).unwrap();
                        fs::write(&written.descriptor, &bytes).unwrap();
                        written.descriptor_sha256 = crate::scip_raw::sha(&bytes);
                    }));
                });
            }
            if cancel_preflight {
                let owner = owner.clone();
                let store = store.clone();
                batch::BEFORE_FROZEN_VALIDATE.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move |written| {
                        let active = owner.slot().run.clone().unwrap();
                        assert_eq!(active.control().phase, RunState::WritingBatch);
                        assert!(owner.install(&super::active("second")).is_err());
                        sweep_state(&store, true).unwrap();
                        assert!(written.descriptor.exists());
                        active.request_cancel(CancelReason::Requested).unwrap();
                    }));
                });
            }
            if expected == "done" {
                let worker = scheduler.clone();
                let store_for_hook = store.clone();
                TEST_ENQUEUED.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move |dir, id| {
                        let request = store_for_hook.request_by_id(id).unwrap().unwrap();
                        assert_eq!(request.state, "queued");
                        let job = request.semantic_job().unwrap();
                        assert_eq!(
                            Path::new(job.descriptor()),
                            dir.join("batch/descriptor.json")
                        );
                        assert_eq!(
                            job.descriptor_sha256(),
                            crate::scip_raw::sha(&fs::read(job.descriptor()).unwrap())
                        );
                        let (roots, identity) = store_for_hook.state_topology();
                        let state = StateDir::existing(roots, identity).unwrap().unwrap();
                        let record = state
                            .read_run_record(dir.file_name().unwrap().to_str().unwrap())
                            .unwrap()
                            .unwrap();
                        assert_eq!(
                            serde_json::to_value(record.diagnostics).unwrap()["termination"],
                            "unknown"
                        );
                        let descriptor = crate::semantic_batch::descriptor::Descriptor::parse(
                            &fs::read(dir.join("batch/descriptor.json")).unwrap(),
                        )
                        .unwrap();
                        assert_eq!(descriptor.artifacts[0].relative_path, "batch/artifact.scip");
                        assert_eq!(
                            descriptor.totals.covered_documents,
                            if partial { 1 } else { 2 }
                        );
                        assert_eq!(descriptor.totals.excluded_documents, u64::from(partial));
                        if partial {
                            assert_eq!(descriptor.completion, crate::scip_raw::Completion::Partial);
                        }
                        let old = dir.join(ARTIFACT_PATH);
                        let frozen = dir.join(&descriptor.artifacts[0].relative_path);
                        assert_ne!(
                            fs::metadata(&old).unwrap().ino(),
                            fs::metadata(&frozen).unwrap().ino()
                        );
                        fs::write(old, b"changed after freeze").unwrap();
                        assert_eq!(fs::read(&frozen).unwrap(), original);
                        // Simulate the next daemon's recovery while the accepted row is pending.
                        let keep = store_for_hook.pending_generation_dirs().unwrap();
                        state
                            .sweep(
                                &DaemonIdentity {
                                    pid: -1,
                                    start_time: 0,
                                },
                                &keep,
                            )
                            .unwrap();
                        let recovered = state
                            .read_run_record(dir.file_name().unwrap().to_str().unwrap())
                            .unwrap()
                            .unwrap();
                        assert_eq!(
                            recovered.error_code.as_deref(),
                            Some("termination_unestablished")
                        );
                        assert_eq!(fs::read(&frozen).unwrap(), original);
                        worker.force_root_transition_tick_for_tests().unwrap();
                        assert_eq!(
                            store_for_hook.request_by_id(id).unwrap().unwrap().state,
                            "done"
                        );
                    }))
                });
            }
            let baseline = store.index_baseline().unwrap();
            let recovery_fixture = if matches!(scenario, Scenario::RecoveryFailure) {
                let (roots, identity) = store.state_topology();
                let state = StateDir::open(roots, identity).unwrap();
                let old_id = "1".repeat(32);
                let dir = state.create_run_dir(&old_id).unwrap();
                let record = RunRecord {
                    version: 1,
                    run_id: old_id,
                    producer: profiles::STUB_NAME.into(),
                    trust_profile: super::super::super::TrustProfile::Restricted,
                    plan_sha256: "0".repeat(64),
                    source_revision: "revision".into(),
                    state: RunState::Running,
                    outcome: None,
                    error_code: None,
                    daemon: DaemonIdentity {
                        pid: -1,
                        start_time: 0,
                    },
                    process: None,
                    request_id: None,
                    created_at: 1,
                    finished_at: None,
                    diagnostics: None,
                };
                state.write_run_record(&record).unwrap();
                fs::create_dir(dir.join("batch")).unwrap();
                let fault = dir.join(format!(".run.json.tmp-{}", std::process::id()));
                fs::create_dir(&fault).unwrap();
                Some((dir, fault))
            } else {
                None
            };
            let result = owner.start_run(
                &RunContext {
                    store: &store,
                    scheduler: &scheduler,
                    stub: Some(&stub),
                },
                &RunRequest {
                    plan: plan.clone(),
                    plan_sha256: hash.clone(),
                },
            );
            if let Some((dir, fault)) = recovery_fixture {
                let error = result.unwrap_err();
                assert!(
                    error.to_string().starts_with("store_unavailable:"),
                    "{error:#}"
                );
                assert!(dir.join("batch").exists());
                assert!(owner.slot().run.is_none());
                assert_eq!(store.index_baseline().unwrap(), baseline);
                fs::remove_dir(fault).unwrap();
                TEST_SPAWN.with(|hook| *hook.borrow_mut() = None);
                return;
            }
            if matches!(
                scenario,
                Scenario::AcceptedWaiterExit | Scenario::AcceptedWaiterFailure
            ) {
                assert!(result.unwrap_err().to_string().starts_with("storage_busy:"));
                let (dir, id) = accepted.lock().unwrap().take().unwrap();
                let run_id = dir.file_name().unwrap().to_str().unwrap().to_owned();
                let (roots, identity) = store.state_topology();
                let state = StateDir::existing(roots, identity).unwrap().unwrap();
                let initial = state.read_run_record(&run_id).unwrap().unwrap();
                assert_eq!(initial.state, RunState::Importing);
                assert_eq!(initial.request_id.as_deref(), Some(id.as_str()));
                let state_for_spawn = state.clone();
                let store_for_spawn = store.clone();
                let dir_for_spawn = dir.clone();
                let id_for_spawn = id.clone();
                let run_for_spawn = run_id.clone();
                TEST_SPAWN.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move |_| {
                        let record = state_for_spawn
                            .read_run_record(&run_for_spawn)
                            .unwrap()
                            .unwrap();
                        assert_eq!(record.state, RunState::Importing);
                        assert_eq!(record.request_id.as_deref(), Some(id_for_spawn.as_str()));
                        assert_eq!(
                            store_for_spawn
                                .request_by_id(&id_for_spawn)
                                .unwrap()
                                .unwrap()
                                .state,
                            "queued"
                        );
                        assert!(dir_for_spawn.join("batch/artifact.scip").exists());
                        Ok(exec::ProcessOutcome {
                            ending: Ending::TimedOut,
                            process: ProcessIdentity {
                                pgid: 1,
                                leader_start_time: 0,
                            },
                            diagnostics: CompleteDiagnostics::default(),
                        })
                    }))
                });
                let next = owner
                    .start_run(
                        &RunContext {
                            store: &store,
                            scheduler: &scheduler,
                            stub: Some(&stub),
                        },
                        &RunRequest {
                            plan: plan.clone(),
                            plan_sha256: hash.clone(),
                        },
                    )
                    .unwrap();
                assert_eq!(next["outcome"], "timed_out");
                if matches!(scenario, Scenario::AcceptedWaiterFailure) {
                    fs::write(dir.join("batch/artifact.scip"), b"corrupt after acceptance")
                        .unwrap();
                }
                scheduler.force_root_transition_tick_for_tests().unwrap();
                let expected_queue = if matches!(scenario, Scenario::AcceptedWaiterFailure) {
                    "failed"
                } else {
                    "done"
                };
                assert_eq!(
                    store.request_by_id(&id).unwrap().unwrap().state,
                    expected_queue
                );
                let settled = state.read_run_record(&run_id).unwrap().unwrap();
                assert_eq!(settled.state, RunState::Importing);
                assert_eq!(settled.request_id.as_deref(), Some(id.as_str()));
                assert!(dir.join("batch/artifact.scip").exists());
                let next_hash = plan::plan(&store, &plan, Some(&stub)).unwrap()["planSha256"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                TEST_SPAWN.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(|_| {
                        Ok(exec::ProcessOutcome {
                            ending: Ending::TimedOut,
                            process: ProcessIdentity {
                                pgid: 1,
                                leader_start_time: 0,
                            },
                            diagnostics: CompleteDiagnostics::default(),
                        })
                    }))
                });
                let third = owner
                    .start_run(
                        &RunContext {
                            store: &store,
                            scheduler: &scheduler,
                            stub: Some(&stub),
                        },
                        &RunRequest {
                            plan,
                            plan_sha256: next_hash,
                        },
                    )
                    .unwrap();
                assert_eq!(third["outcome"], "timed_out");
                let after = state.read_run_record(&run_id).unwrap().unwrap();
                assert_eq!(after.state, RunState::Importing);
                assert_eq!(after.request_id.as_deref(), Some(id.as_str()));
                assert_eq!(
                    store.request_by_id(&id).unwrap().unwrap().state,
                    expected_queue
                );
                assert!(dir.join("batch/artifact.scip").exists());
                let status_request = plan::StatusRequest::parse(&json!({"version":1,"runId":run_id,"environment":{"PATH":"/usr/bin","HOME":"/tmp"}})).unwrap();
                let status = owner
                    .producer_status(&store, &status_request, Some(&stub))
                    .unwrap();
                assert_eq!(status["run"]["state"], "importing");
                assert_eq!(status["run"]["requestId"], id);
                return;
            }
            if matches!(
                scenario,
                Scenario::TerminalAfterRename | Scenario::TerminalRenameLost
            ) {
                let error = result.unwrap_err();
                assert!(error.to_string().starts_with("store_unavailable:"));
                let old = owner.slot().pending.as_ref().unwrap().record.clone();
                let (roots, identity) = store.state_topology();
                let state = StateDir::existing(roots, identity).unwrap().unwrap();
                assert_eq!(
                    state.read_run_record(&old.run_id).unwrap().unwrap().state,
                    RunState::Failed,
                    "rename occurred before injected fault"
                );
                owner.shutdown();
                drop(owner);
                let request = plan::StatusRequest::parse(&json!({"version":1,"runId":old.run_id,"environment":{"PATH":"/usr/bin","HOME":"/tmp"}})).unwrap();
                if matches!(scenario, Scenario::TerminalAfterRename) {
                    let mut historical = old.clone();
                    historical.run_id = "f".repeat(32);
                    historical.producer = "scip-typescript".into();
                    historical.created_at = 1;
                    state.create_run_dir(&historical.run_id).unwrap();
                    state.write_run_record(&historical).unwrap();
                }
                crate::producer::state::fail_state_sync(true);
                // A fresh owner models loss of all in-memory state at restart.
                let restarted = ProducerOwner::default();
                if matches!(scenario, Scenario::TerminalRenameLost) {
                    // Model a crash in which the unsynced rename did not survive.
                    fs::write(
                        state.run_dir(&old.run_id).unwrap().join("run.json"),
                        before_rename.lock().unwrap().take().unwrap(),
                    )
                    .unwrap();
                    let status = restarted
                        .producer_status(&store, &request, Some(&stub))
                        .unwrap();
                    assert_eq!(status["run"]["state"], "unknown");
                    assert_eq!(status["run"]["recoveryTermination"], "unknown");
                    crate::producer::state::fail_state_sync(false);
                    sweep_state_owned(&store, true).unwrap().unwrap();
                    let status = restarted
                        .producer_status(&store, &request, Some(&stub))
                        .unwrap();
                    assert_eq!(status["run"]["state"], "failed");
                    assert_eq!(status["run"]["errorCode"], "termination_unestablished");
                    return;
                }
                let status = restarted
                    .producer_status(&store, &request, Some(&stub))
                    .unwrap();
                assert_eq!(status["run"]["state"], "failed");
                assert_eq!(status["run"]["recordedState"], "failed");
                assert_eq!(status["run"]["diagnostics"]["termination"], "unknown");
                assert_eq!(status["run"]["terminalRecordPending"], Value::Null);
                let ordinary = plan::StatusRequest::parse(&json!({"version":1,"runId":null,"environment":{"PATH":"/usr/bin","HOME":"/tmp"}})).unwrap();
                let status = restarted
                    .producer_status(&store, &ordinary, Some(&stub))
                    .unwrap();
                let historical = status["producers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|producer| producer["name"] == "scip-typescript")
                    .unwrap();
                assert_eq!(historical["lastRun"]["recordedState"], "failed");
                assert_eq!(historical["lastRun"]["terminalRecordPending"], Value::Null);
                assert!(sweep_state_owned(&store, true).is_ok());
                assert!(
                    state.write_run_record(&old).is_err(),
                    "the fsync fault remained armed"
                );
                crate::producer::state::fail_state_sync(false);
                return;
            }
            if matches!(scenario, Scenario::TerminalWriteShutdown) {
                let error = result.unwrap_err();
                assert!(error.to_string().starts_with("store_unavailable:"));
                assert!(error.downcast_ref::<std::io::Error>().is_some());
                let (state, old) = {
                    let slot = owner.slot();
                    let pending = slot.pending.as_ref().unwrap();
                    (pending.state.clone(), pending.record.clone())
                };
                let dir = state.run_dir(&old.run_id).unwrap();
                let fault = dir.join(format!(".run.json.tmp-{}", std::process::id()));
                fs::create_dir(&fault).unwrap();
                let moved = workspace.with_extension("moved");
                fs::rename(&workspace, &moved).unwrap();
                assert!(store.verify_root().is_err());
                let (entered_tx, entered_rx) = std::sync::mpsc::channel();
                let (release_tx, release_rx) = std::sync::mpsc::channel();
                owner.slot().test_before_retry_write = Some(Box::new(move || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                }));
                let retry_owner = owner.clone();
                let retry = std::thread::spawn(move || retry_owner.reconcile_terminal());
                entered_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
                let shutdown_owner = owner.clone();
                let (stopped_tx, stopped_rx) = std::sync::mpsc::channel();
                let shutdown = std::thread::spawn(move || {
                    shutdown_owner.shutdown();
                    stopped_tx.send(()).unwrap();
                });
                let stopped = stopped_rx.recv_timeout(std::time::Duration::from_secs(10));
                release_tx.send(()).unwrap();
                shutdown.join().unwrap();
                retry.join().unwrap().unwrap();
                stopped.expect("shutdown must not wait for the paused retry");
                assert!(owner.slot().pending.is_none());
                assert_eq!(scheduler.leases(), 0);
                fs::rename(&moved, &workspace).unwrap();
                let replacement = ProducerOwner::default();
                let request = plan::StatusRequest::parse(&json!({"version":1,"runId":old.run_id,"environment":{"PATH":"/usr/bin","HOME":"/tmp"}})).unwrap();
                let status = replacement
                    .producer_status(&store, &request, Some(&stub))
                    .unwrap();
                assert!(status["activeRun"].is_null());
                assert_eq!(status["run"]["state"], "unknown");
                assert_eq!(status["run"]["recordedState"], "running");
                assert_eq!(status["run"]["recoveryTermination"], "unknown");
                assert_eq!(status["run"]["terminalRecordPending"], Value::Null);
                let refused = replacement
                    .start_run(
                        &RunContext {
                            store: &store,
                            scheduler: &scheduler,
                            stub: Some(&stub),
                        },
                        &RunRequest {
                            plan: plan.clone(),
                            plan_sha256: hash.clone(),
                        },
                    )
                    .unwrap_err();
                assert!(refused.to_string().starts_with("store_unavailable:"));
                assert!(replacement.slot().run.is_none());
                fs::remove_dir(fault).unwrap();
                TEST_SPAWN.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(|_| {
                        Ok(exec::ProcessOutcome {
                            ending: Ending::TimedOut,
                            process: ProcessIdentity {
                                pgid: 1,
                                leader_start_time: 0,
                            },
                            diagnostics: CompleteDiagnostics::default(),
                        })
                    }));
                });
                let next = replacement
                    .start_run(
                        &RunContext {
                            store: &store,
                            scheduler: &scheduler,
                            stub: Some(&stub),
                        },
                        &RunRequest {
                            plan,
                            plan_sha256: hash,
                        },
                    )
                    .unwrap();
                assert_eq!(next["outcome"], "timed_out");
                let recovered = state.read_run_record(&old.run_id).unwrap().unwrap();
                assert_eq!(recovered.state, RunState::Failed);
                assert_eq!(
                    recovered.error_code.as_deref(),
                    Some("termination_unestablished")
                );
                assert!(owner.slot().pending.is_none());
                return;
            }
            if matches!(scenario, Scenario::TerminalWriteFailure) {
                let error = result.unwrap_err();
                assert!(error.to_string().starts_with("store_unavailable:"));
                assert!(error.downcast_ref::<std::io::Error>().is_some());
                let (state, observed) = {
                    let slot = owner.slot();
                    let pending = slot.pending.as_ref().unwrap();
                    (pending.state.clone(), pending.record.clone())
                };
                assert!(owner.slot().run.is_none());
                assert!(owner.install(&super::active("successor")).is_err());
                let stored = state.read_run_record(&observed.run_id).unwrap().unwrap();
                assert_eq!(stored.state, RunState::Running);
                let dir = state.run_dir(&observed.run_id).unwrap();
                let fault = dir.join(format!(".run.json.tmp-{}", std::process::id()));
                fs::create_dir(&fault).unwrap();
                let error = owner.reconcile_terminal().unwrap_err();
                assert!(error.to_string().starts_with("store_unavailable:"));
                assert!(error.downcast_ref::<std::io::Error>().is_some());
                assert_eq!(scheduler.leases(), 0);
                let request = plan::StatusRequest::parse(&json!({"version": 1, "runId": observed.run_id, "environment": {"PATH": "/usr/bin", "HOME": "/tmp"}})).unwrap();
                let status = owner
                    .producer_status(&store, &request, Some(&stub))
                    .unwrap();
                assert!(status["activeRun"].is_null());
                assert_eq!(status["run"]["state"], "failed");
                assert_eq!(status["run"]["recordedState"], Value::Null);
                assert_eq!(status["run"]["diagnostics"]["termination"], "unknown");
                assert_eq!(
                    status["run"]["diagnostics"].get("groupSurvived"),
                    Some(&Value::Null)
                );
                assert_eq!(status["run"]["terminalRecordPending"], true);
                let last = status["producers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|p| p["name"] == profiles::STUB_NAME)
                    .unwrap();
                assert_eq!(last["lastRun"]["state"], "failed");
                assert_eq!(last["lastRun"]["recordedState"], Value::Null);
                assert_eq!(last["lastRun"]["terminalRecordPending"], true);
                assert_eq!(last["lastRun"]["diagnostics"]["termination"], "unknown");
                assert_eq!(
                    state
                        .read_run_record(&observed.run_id)
                        .unwrap()
                        .unwrap()
                        .state,
                    RunState::Running
                );
                fs::remove_dir(fault).unwrap();
                let status = owner
                    .producer_status(&store, &request, Some(&stub))
                    .unwrap();
                assert_eq!(status["run"]["terminalRecordPending"], Value::Null);
                assert_eq!(status["run"]["recordedState"], "failed");
                assert_eq!(
                    state.read_run_record(&observed.run_id).unwrap().unwrap(),
                    observed
                );
                assert!(owner.slot().pending.is_none());
                let successor = super::active("successor");
                owner.install(&successor).unwrap();
                {
                    let old = SlotGuard {
                        owner: &owner,
                        run: super::active("old"),
                    };
                    drop(old);
                }
                assert!(Arc::ptr_eq(owner.slot().run.as_ref().unwrap(), &successor));
                owner.slot().run = None;
                return;
            }
            let reply = result.unwrap();
            assert_eq!(reply["state"], expected, "{reply}");
            assert_eq!(reply["diagnostics"]["termination"], "unknown");
            assert!(reply["diagnostics"]["groupSurvived"].is_null());
            let status = plan::status(&store, &plan::StatusRequest::parse(&json!({"version": 1, "runId": reply["runId"], "environment": {"PATH": "/usr/bin", "HOME": "/tmp"}})).unwrap(), Some(&stub)).unwrap();
            assert_eq!(status["run"]["diagnostics"]["termination"], "unknown");
            assert_eq!(status["run"]["diagnostics"], reply["diagnostics"]);
            let last = status["producers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["name"] == profiles::STUB_NAME)
                .unwrap();
            assert_eq!(last["lastRun"]["diagnostics"]["termination"], "unknown");
            assert_eq!(
                last["lastRun"]["diagnostics"].get("groupSurvived"),
                Some(&Value::Null)
            );
            if matches!(scenario, Scenario::TerminationFailure(..)) {
                assert_eq!(reply["diagnostics"]["failure"], "signal");
            }
            if expected != "done" {
                assert_eq!(
                    reply["outcome"],
                    if let Scenario::TerminationFailure(ending, root_changed) = scenario {
                        match ending {
                            Ending::Cancelled if root_changed => "root_changed",
                            Ending::Cancelled => "cancelled",
                            Ending::TimedOut => "timed_out",
                            Ending::LimitExceeded(_) => "limit_exceeded",
                            _ => "producer_failed",
                        }
                    } else if matches!(scenario, Scenario::ActiveStatus) {
                        "timed_out"
                    } else if matches!(scenario, Scenario::ChangedHead) {
                        "stale_basis"
                    } else if matches!(
                        scenario,
                        Scenario::TransitionFailure | Scenario::SubmitFailure
                    ) {
                        "producer_failed"
                    } else if cancel_preflight {
                        "cancelled"
                    } else {
                        "producer_output_invalid"
                    }
                );
                if !matches!(scenario, Scenario::ChangedHead) {
                    assert_eq!(store.index_baseline().unwrap(), baseline);
                }
                assert!(store.pending_generation_ids().unwrap().is_empty());
                let (roots, identity) = store.state_topology();
                let state = StateDir::existing(roots, identity).unwrap().unwrap();
                let dir = state.run_dir(reply["runId"].as_str().unwrap()).unwrap();
                assert!(!dir.join("batch").exists());
                assert!(owner.slot().run.is_none());
            }
        }

        #[test]
        fn unknown_group_imports_frozen_bytes_and_preserves_partial_dispositions() {
            exercise(
                artifact(&["a.js", "b.js"], false),
                "done",
                false,
                false,
                false,
                Scenario::Normal,
            );
            exercise(
                artifact(&["a.js"], false),
                "done",
                true,
                false,
                false,
                Scenario::Normal,
            );
        }

        #[test]
        fn unknown_group_rejects_late_invalid_enum_before_acceptance() {
            exercise(
                artifact(&["a.js", "b.js"], true),
                "failed",
                false,
                false,
                false,
                Scenario::Normal,
            );
        }

        #[test]
        fn unknown_group_rejects_missing_coverage_disposition_before_acceptance() {
            exercise(
                artifact(&["a.js", "b.js"], false),
                "failed",
                false,
                true,
                false,
                Scenario::Normal,
            );
        }

        #[test]
        fn cancellation_during_preflight_retains_owner_until_cleanup() {
            exercise(
                artifact(&["a.js", "b.js"], false),
                "cancelled",
                false,
                false,
                true,
                Scenario::Normal,
            );
        }

        #[test]
        fn changed_head_wins_over_invalid_deleted_artifact_document() {
            exercise(
                artifact(&["a.js", "b.js"], true),
                "failed",
                false,
                false,
                false,
                Scenario::ChangedHead,
            );
        }

        #[test]
        fn observed_diagnostics_survive_transition_and_submit_errors() {
            for scenario in [Scenario::TransitionFailure, Scenario::SubmitFailure] {
                exercise(
                    artifact(&["a.js", "b.js"], false),
                    "failed",
                    false,
                    false,
                    false,
                    scenario,
                );
            }
        }

        #[test]
        fn run_start_reports_recovery_record_write_failure() {
            exercise(
                artifact(&["a.js", "b.js"], false),
                "failed",
                false,
                false,
                false,
                Scenario::RecoveryFailure,
            );
        }

        #[test]
        fn observed_bound_and_cancellation_survive_termination_failure() {
            for (ending, root, state) in [
                (Ending::TimedOut, false, "failed"),
                (
                    Ending::LimitExceeded(exec::LimitKind::Memory),
                    false,
                    "failed",
                ),
                (
                    Ending::LimitExceeded(exec::LimitKind::ArtifactBytes),
                    false,
                    "failed",
                ),
                (Ending::Cancelled, false, "cancelled"),
                (Ending::Cancelled, true, "failed"),
            ] {
                exercise(
                    artifact(&["a.js", "b.js"], false),
                    state,
                    false,
                    false,
                    false,
                    Scenario::TerminationFailure(ending, root),
                );
            }
        }
    }

    fn active(run_id: &str) -> Arc<ActiveRun> {
        Arc::new(ActiveRun {
            run_id: run_id.to_owned(),
            producer: "stub".to_owned(),
            trust_profile: super::super::TrustProfile::Restricted,
            plan_sha256: "0".repeat(64),
            source_revision: "revision".to_owned(),
            created_at: 1,
            cancel: Arc::new(AtomicBool::new(false)),
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

    #[test]
    fn artifact_presence_preserves_missing_and_reports_other_io_errors() {
        assert!(
            !artifact_presence(Err(std::io::Error::from(std::io::ErrorKind::NotFound))).unwrap()
        );
        assert_eq!(
            artifact_presence(Err(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied
            )))
            .unwrap_err()
            .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(!artifact_presence(Ok(false)).unwrap());
        assert!(artifact_presence(Ok(true)).unwrap());
        let dir = tempfile::tempdir().unwrap();
        assert!(
            !artifact_presence(
                fs::symlink_metadata(dir.path().join("missing.scip")).map(|meta| meta.is_file())
            )
            .unwrap()
        );
    }

    #[test]
    fn artifact_metadata_error_cannot_report_success_or_a_known_size() {
        let diagnostics = CompleteDiagnostics {
            artifact_bytes: None,
            termination: TerminationStatus::Unknown,
            group_survived: None,
            ..Default::default()
        };
        let finish = Finish::artifact_metadata_error(&diagnostics);
        assert_eq!(finish.state, RunState::Failed);
        assert_eq!(finish.outcome, "producer_failed");
        assert_eq!(
            finish.error_code.as_deref(),
            Some("producer_failed: artifact metadata")
        );
        let diagnostics = serde_json::to_value(finish.diagnostics).unwrap();
        assert_eq!(diagnostics["artifactBytes"], Value::Null);
        assert_eq!(diagnostics["termination"], "unknown");
        assert_eq!(diagnostics["groupSurvived"], Value::Null);
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn termination_failure_is_in_reply_and_durable_record_without_guessed_metrics() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        fs::write(workspace.join("source.js"), "function example() {}\n").unwrap();
        let store = Store::open_for_tests(&temp.path().join("state"), &workspace).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let (graph, native, capture) = crate::indexer::index_workspace_bundle(
            &crate::indexer::IndexOptions::new(workspace.clone()),
            store.root_id(),
            &cancel,
            |_| {},
        )
        .unwrap();
        let leader = store.publication_for_tests().unwrap();
        store
            .publish_native(
                &graph,
                &capture,
                &native,
                &leader,
                store.index_baseline().unwrap(),
                &cancel,
            )
            .unwrap();
        drop(leader);
        let (roots, identity) = store.state_topology();
        let state = StateDir::open(roots, identity).unwrap();
        let run_id = "0".repeat(32);
        state.create_run_dir(&run_id).unwrap();
        let active = active(&run_id);
        let record = RunRecord {
            version: 1,
            run_id: run_id.clone(),
            producer: active.producer.clone(),
            trust_profile: active.trust_profile,
            plan_sha256: active.plan_sha256.clone(),
            source_revision: active.source_revision.clone(),
            state: RunState::Running,
            outcome: None,
            error_code: None,
            daemon: DaemonIdentity::current(),
            process: None,
            request_id: None,
            created_at: active.created_at,
            finished_at: None,
            diagnostics: None,
        };
        let mut recorder = Recorder {
            state: &state,
            record,
        };
        let request = plan::StatusRequest::parse(&json!({
            "version": 1, "runId": run_id,
            "environment": {"PATH": "/usr/bin", "HOME": "/tmp"}
        }))
        .unwrap();
        for (kind, reason) in [
            (super::super::TerminationFailure::Observation, "observation"),
            (super::super::TerminationFailure::Signal, "signal"),
            (super::super::TerminationFailure::Reap, "reap"),
            (super::super::TerminationFailure::Timeout, "timeout"),
            (super::super::TerminationFailure::Unwind, "unwind"),
        ] {
            let error: anyhow::Error = exec::ProcessFailure {
                kind,
                detail: "injected".into(),
                ending: None,
            }
            .into();
            let finish = Run::process_error(&active, &error);
            recorder.finish(&finish).unwrap();
            let reply = run_reply(&run_id, &active.plan_sha256, &active, &finish);
            let record =
                serde_json::to_value(state.read_run_record(&run_id).unwrap().unwrap()).unwrap();
            let status = plan::status(&store, &request, None).unwrap();
            for value in [&reply, &record, &status["run"]] {
                assert_eq!(value["state"], "failed");
                assert_eq!(value["diagnostics"]["termination"], "unknown");
                assert_eq!(value["diagnostics"]["failure"], reason);
                assert_eq!(value["diagnostics"]["groupSurvived"], Value::Null);
                assert!(value["diagnostics"].get("artifactBytes").is_none());
                assert!(value["diagnostics"].get("peakRssBytes").is_none());
            }
        }
        let diagnostics = CompleteDiagnostics {
            exit: super::super::ExitReport {
                code: Some(0),
                ..Default::default()
            },
            group_survived: None,
            termination: TerminationStatus::Unknown,
            stdout_bytes: 3,
            stdout_complete: Some(false),
            stderr_complete: Some(true),
            ..Default::default()
        };
        let finish = Finish {
            state: RunState::Done,
            outcome: "imported",
            error_code: None,
            import: None,
            diagnostics: None,
        }
        .with(&diagnostics);
        recorder.finish(&finish).unwrap();
        let reply = run_reply(&run_id, &active.plan_sha256, &active, &finish);
        let status = plan::status(&store, &request, None).unwrap();
        for value in [&reply, &status["run"]] {
            assert_eq!(value["state"], "done");
            assert_eq!(value["diagnostics"]["termination"], "unknown");
            assert_eq!(value["diagnostics"]["groupSurvived"], Value::Null);
            assert_eq!(value["diagnostics"]["exit"]["code"], 0);
            assert_eq!(value["diagnostics"]["artifactBytes"], Value::Null);
            assert_eq!(value["diagnostics"]["stdoutBytes"], 3);
            assert_eq!(value["diagnostics"]["stdoutComplete"], false);
            assert_eq!(value["diagnostics"]["stderrComplete"], true);
        }
    }
}
