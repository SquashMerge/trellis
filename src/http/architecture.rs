//! The checkout's one Architecture owner (#556).
//!
//! The owner computes one [`ArchitectureCore`] for the published head and the reviewed rules
//! file, off every request path, and publishes it. Workers hint and reads submit demand;
//! neither computes, and any number of concurrent readers share one build. Reads recheck the
//! pin and the rules hash against what is published, so a new revision or a changed rules
//! file is never served from an older core. A page can stay on an older retained revision, so
//! the owner keeps the core it replaced and builds one such revision when a page asks.
//!
//! A read waits on the owner's signal, and its wait is bounded by what the caller can afford.
//! When that runs out it gets a typed [`ArchitectureBuilding`] with the progress so far.
use super::DaemonState;
use crate::architecture_core::{
    ArchitectureBuildPath, ArchitectureCore, ArchitectureProgress, ArchitectureReadA, CoreFailure,
    FullReason, ReferenceWorkMargin, finish_architecture, prepare_architecture_from,
};
#[cfg(feature = "test-support")]
use crate::architecture_core::{
    ArchitectureExecutionBranch, ArchitectureExecutionHook, ArchitectureExecutionObserver,
    ArchitectureExecutionScope, ArchitectureExecutionTrace,
};
use crate::model::IndexPin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

/// How long a browser read waits before it answers `building`; the page asks again.
#[cfg(not(feature = "test-support"))]
const BROWSER_WAIT: Duration = Duration::from_secs(1);
/// How long a revision metrics read waits for a running build instead of repeating its work.
/// With the read it may then do itself, it stays under the browser's 15 s request timeout.
pub(crate) const METRICS_WAIT: Duration = Duration::from_secs(5);
/// A waiter's cancellation and deadline have no signal here, so it looks at them this often.
const CHECK_WHILE_WAITING: Duration = Duration::from_millis(100);
/// External contention has no completion signal, so a failed build retries on this bound.
const CONTENTION_RETRY: Duration = Duration::from_secs(1);
/// Then contention waits for the next hint or read, and any other fault is reported.
const CONTENTION_RETRIES: u32 = 5;
/// A turn waits on a live publication's own end signal before it reads (#1245); this bound
/// is only a liveness backstop, for a publication that outlasts it.
const PUBLICATION_WAIT_BACKSTOP: Duration = Duration::from_secs(60);

/// How long a `building` answer asks its caller to wait before it reads again.
pub(crate) const RETRY_AFTER_MS: u64 = 250;

/// The read arrived while its core is still being computed. Retryable, never partial.
#[derive(Debug)]
pub struct ArchitectureBuilding {
    pub pin: IndexPin,
    pub(crate) progress: Option<ArchitectureProgress>,
    /// The rules file the read needs its core to have evaluated, so the read can wait for
    /// that core after its read has ended (#1252).
    pub(crate) rules: WantedRules,
}

/// An owned [`CoreRules`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WantedRules {
    Any,
    Exactly(Option<String>),
}
impl WantedRules {
    fn core_rules(&self) -> CoreRules<'_> {
        match self {
            Self::Any => CoreRules::Any,
            Self::Exactly(hash) => CoreRules::Exactly(hash.as_deref()),
        }
    }
}
impl From<CoreRules<'_>> for WantedRules {
    fn from(rules: CoreRules<'_>) -> Self {
        match rules {
            CoreRules::Any => Self::Any,
            CoreRules::Exactly(hash) => Self::Exactly(hash.map(str::to_owned)),
        }
    }
}
impl std::fmt::Display for ArchitectureBuilding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "deadline_exceeded: ")?;
        f.write_str(&self.message())
    }
}
impl std::error::Error for ArchitectureBuilding {}
impl ArchitectureBuilding {
    pub(crate) fn message(&self) -> String {
        match self.progress {
            Some(progress) if progress.total > 0 => format!(
                "Architecture is being computed for this revision ({} {} of {}); retry shortly",
                progress.phase, progress.completed, progress.total
            ),
            _ => "Architecture is being computed for this revision; retry shortly".into(),
        }
    }
    /// The summary's `building` answer.
    pub(crate) fn payload(&self) -> serde_json::Value {
        serde_json::json!({
            "state": "building",
            "indexGeneration": self.pin.index_generation.to_string(),
            "indexRevision": self.pin.index_revision,
            "progress": self.progress,
            "retryAfterMs": RETRY_AFTER_MS,
        })
    }
}

/// What a read needs from a core: its pin and, for a rule read, this rules file.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CoreKey {
    pin: IndexPin,
    rules_hash: Option<String>,
}

/// Which rules file the core a read takes must have evaluated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CoreRules<'a> {
    /// The read uses no rules part: a core of the pin under any rules file answers it.
    Any,
    /// Exactly this rules file, by content hash, or exactly none (#851): a core built with a
    /// rules file never answers a read of a checkout whose file was since removed, nor a core
    /// built without one a read of a file since added.
    Exactly(Option<&'a str>),
}
impl<'a> CoreRules<'a> {
    /// Whether a core built with the rules file `built` answers this read.
    pub(crate) fn admits(self, built: Option<&str>) -> bool {
        match self {
            Self::Any => true,
            Self::Exactly(wanted) => built == wanted,
        }
    }
    /// The rules file an older-revision request builds with.
    fn hash(self) -> Option<&'a str> {
        match self {
            Self::Any => None,
            Self::Exactly(hash) => hash,
        }
    }
}

struct Published {
    key: CoreKey,
    outcome: Result<Arc<ArchitectureCore>, CoreFailure>,
    /// An ordinary fault outlasted its retries: report it once, and let the next read retry.
    report_once: bool,
}
impl Published {
    fn answers(&self, pin: IndexPin, rules: CoreRules<'_>) -> bool {
        self.key.pin == pin && rules.admits(self.key.rules_hash.as_deref())
    }
}

struct Running {
    pin: IndexPin,
    rules_hash: Option<String>,
    progress: Option<ArchitectureProgress>,
    /// A borrowed base belongs to the episode at selection. Once invalidated, this
    /// operation stays cancelled even if the live hash returns to its original value.
    base_cancelled: Option<Arc<AtomicBool>>,
}

fn check_base_episode(cancelled: Option<&AtomicBool>) -> anyhow::Result<()> {
    anyhow::ensure!(
        !cancelled.is_some_and(|cancelled| cancelled.load(Ordering::Acquire)),
        "cancelled: architecture base episode ended"
    );
    Ok(())
}

#[derive(Default)]
struct OwnerState {
    /// Something may have changed; the owner compares head and rules with what it published.
    hinted: bool,
    /// A read asked for a core. Only a test's on-demand owner waits for this.
    #[cfg(feature = "test-support")]
    demanded: bool,
    shutdown: bool,
    thread_started: bool,
    /// The owner thread is in a turn: it may take a lease and build.
    stepping: bool,
    /// The head and rules file the last worker step saw, so an unchanged step does not wake
    /// the owner to read them under a lease.
    stepped: Option<(IndexPin, Option<String>)>,
    running: Option<Running>,
    /// Newest first: the head's core and the one before it, or one a page asked for.
    published: Vec<Published>,
    /// An older retained revision a page is showing and no published core answers.
    older: Option<CoreKey>,
    /// The head the owner last read.
    head: Option<IndexPin>,
    retry_at: Option<Instant>,
    retries: u32,
    /// The head whose build this owner skipped because a semantic catch-up of that same head
    /// was queued (#1313 T10b): the catch-up's publication supersedes it. The semantic
    /// maintenance owner clears it when a catch-up unit ends.
    deferred: Option<IndexPin>,
    /// Heads whose build is not skipped (again): a read asked for each, or a unit of its
    /// catch-up ended without publishing it. One entry per pin, so a read of an older pin
    /// never displaces the head's (#1313 T10b review); a turn drops pins older than its head.
    asked: Vec<IndexPin>,
    /// Counts catch-up units that ended, so a turn that sampled the queue before a unit ended
    /// does not skip on that sample.
    catchup_settles: u64,
    /// A hash returning after a different live hash starts a new eligibility episode.
    live_rules: Option<(uuid::Uuid, Option<String>)>,
    eligible: Vec<CoreKey>,
}
/// A checkout holds at most this many cores.
const PUBLISHED_CORES: usize = 2;

impl OwnerState {
    fn observe_rules(&mut self, key: &CoreKey) {
        let live = (key.pin.index_generation, key.rules_hash.clone());
        if self.live_rules.as_ref() != Some(&live) {
            self.eligible.clear();
            if let Some(cancelled) = self
                .running
                .as_ref()
                .and_then(|run| run.base_cancelled.as_ref())
            {
                cancelled.store(true, Ordering::Release);
            }
            self.live_rules = Some(live);
        }
    }

    /// The final admission and publication share the owner lock with observations.
    /// A transition after the last compute check therefore cannot publish an old base.
    fn finish_running(&mut self) -> anyhow::Result<()> {
        let running = self.running.take();
        check_base_episode(
            running
                .as_ref()
                .and_then(|run| run.base_cancelled.as_deref()),
        )
    }

    /// Record that `pin`'s own build was asked for (#1313 T10b).
    fn ask_for(&mut self, pin: IndexPin) {
        if !self.asked.contains(&pin) {
            self.asked.push(pin);
        }
    }

    /// Whether a turn skips building `head` for a queued semantic catch-up (#1313 T10b): the
    /// catch-up targets this same head, nothing asked for this head (a read, or a unit of
    /// its catch-up that ended without publishing it), and no unit ended since the turn
    /// sampled `settles`.
    fn defers_for_catchup(&self, head: IndexPin, catchup: Option<IndexPin>, settles: u64) -> bool {
        catchup == Some(head) && !self.asked.contains(&head) && self.catchup_settles == settles
    }

    /// Every retained core a build of `key` may start from, newest first (#1313).
    fn bases(&self, key: &CoreKey) -> Vec<Arc<ArchitectureCore>> {
        if self.head != Some(key.pin) {
            return Vec::new();
        }
        let mut bases: Vec<_> = self
            .published
            .iter()
            .filter(|published| {
                published.key.pin.index_generation == key.pin.index_generation
                    && published.key.pin.index_revision < key.pin.index_revision
                    && published.key.rules_hash == key.rules_hash
                    && self.eligible.contains(&published.key)
            })
            .filter_map(|published| published.outcome.as_ref().ok())
            .filter(|core| core.incremental_base_eligible())
            .cloned()
            .collect();
        bases.sort_by_key(|core| std::cmp::Reverse(core.pin.index_revision));
        bases
    }

    fn answer(&self, pin: IndexPin, rules: CoreRules<'_>) -> Option<&Published> {
        self.published.iter().find(|p| p.answers(pin, rules))
    }
    /// The owner has seen a newer head than `pin`, and no page asked it for `pin`.
    fn passed(&self, pin: IndexPin) -> bool {
        self.older.as_ref().is_none_or(|older| older.pin != pin)
            && self.head.is_some_and(|head| {
                head.index_generation != pin.index_generation
                    || head.index_revision > pin.index_revision
            })
    }
    /// The head's core stays; a page's older revision takes the other place.
    fn publish(&mut self, published: Published) {
        if self.older.as_ref() == Some(&published.key) {
            self.older = None;
        }
        self.eligible.retain(|key| key.pin != published.key.pin);
        if published
            .outcome
            .as_ref()
            .is_ok_and(|core| core.incremental_base_eligible())
        {
            self.eligible.push(published.key.clone());
        }
        self.published.retain(|p| p.key.pin != published.key.pin);
        self.published.insert(0, published);
        if self.published.len() > PUBLISHED_CORES {
            let head = self.head;
            let evicted = self.published.iter().rposition(|p| Some(p.key.pin) != head);
            self.published.remove(evicted.unwrap_or(PUBLISHED_CORES));
        }
        self.eligible
            .retain(|key| self.published.iter().any(|p| p.key == *key));
    }
    /// Ask the owner to look again. A retry that is due keeps its bound, so a stream of hints
    /// and reads cannot turn it into a hot loop or stop a fault from being reported; a retry
    /// budget that ran out starts over.
    fn ask(&mut self) {
        if self.retry_at.is_some() {
            return;
        }
        self.hinted = true;
        if self.running.is_none() && self.retries >= CONTENTION_RETRIES {
            self.retries = 0;
        }
    }
}

#[cfg(feature = "test-support")]
type TestHook = Arc<dyn Fn(&'static str) + Send + Sync>;

/// One owner operation, including attempts that stopped without a publication.
#[cfg(feature = "test-support")]
#[derive(Clone, Debug)]
pub struct ArchitectureAttempt {
    pub pin: IndexPin,
    pub base: Option<IndexPin>,
    pub path: ArchitectureBuildPath,
    pub semantic_sources: Vec<crate::native_records::DocumentKey>,
    /// Fresh commits of Retry when present, otherwise Candidate; never worker progress.
    pub measured_sources: Vec<crate::native_records::DocumentKey>,
    /// The same compute branch. Its accounting is complete only when that branch's
    /// execution trace has `references: Some(...)`; invocation keys are always actual.
    pub references: crate::references::ReferenceReplayTrace,
    /// Actual invocation/commit observations, including incomplete branches.
    pub executions:
        std::collections::BTreeMap<ArchitectureExecutionBranch, ArchitectureExecutionTrace>,
    pub progress: Vec<ArchitectureProgress>,
    /// The documents the candidate's Read A read rather than took from kept documents.
    pub native_reads: Vec<crate::native_records::DocumentKey>,
    pub full_retries: usize,
    pub compared: bool,
    pub published: bool,
}

pub(crate) struct ArchitectureOwner {
    state: Mutex<OwnerState>,
    /// The native documents of the last build's Read A, for the next one (#1315). Only the
    /// owner's own builds use it; an oracle or a full retry reads every document.
    kept: crate::architecture_core::KeptSlot,
    changed: Condvar,
    /// The runtime closed: stop the running build.
    stop: AtomicBool,
    /// A worker step finished during the build, so the head may have moved.
    head_may_have_moved: AtomicBool,
    #[cfg(feature = "test-support")]
    test_hook: Mutex<Option<TestHook>>,
    #[cfg(feature = "test-support")]
    builds: std::sync::atomic::AtomicUsize,
    #[cfg(feature = "test-support")]
    browser_wait: Mutex<Option<Duration>>,
    /// Build only once a read asks, as before #798.
    #[cfg(feature = "test-support")]
    on_demand: AtomicBool,
    /// Verify mode (#1301): on unless a test turns it off.
    #[cfg(feature = "test-support")]
    verify: AtomicBool,
    /// The path each build that published took, oldest first (#1301).
    #[cfg(feature = "test-support")]
    paths: Mutex<Vec<(IndexPin, crate::architecture_core::ArchitectureBuildPath)>>,
    /// Builds verify mode compared with a full build.
    #[cfg(feature = "test-support")]
    verified: std::sync::atomic::AtomicUsize,
    /// The next later admission of a build fails with this integrity fault.
    #[cfg(feature = "test-support")]
    admission_fault: Mutex<Option<String>>,
    #[cfg(feature = "test-support")]
    attempts: Mutex<Vec<ArchitectureAttempt>>,
    #[cfg(feature = "test-support")]
    fault: Mutex<Option<(&'static str, String)>>,
    #[cfg(feature = "test-support")]
    mismatch: Mutex<Option<String>>,
    #[cfg(feature = "test-support")]
    differences: Mutex<Vec<String>>,
    #[cfg(feature = "test-support")]
    work_limit: Mutex<Option<crate::references::ReferenceWorkLimitForTests>>,
    #[cfg(feature = "test-support")]
    execution_hook: Mutex<Option<ArchitectureExecutionHook>>,
}

impl ArchitectureOwner {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
            kept: Arc::default(),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
            head_may_have_moved: AtomicBool::new(false),
            #[cfg(feature = "test-support")]
            test_hook: Mutex::new(None),
            #[cfg(feature = "test-support")]
            builds: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(feature = "test-support")]
            browser_wait: Mutex::new(None),
            #[cfg(feature = "test-support")]
            on_demand: AtomicBool::new(false),
            #[cfg(feature = "test-support")]
            verify: AtomicBool::new(true),
            #[cfg(feature = "test-support")]
            paths: Mutex::default(),
            #[cfg(feature = "test-support")]
            verified: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(feature = "test-support")]
            admission_fault: Mutex::default(),
            #[cfg(feature = "test-support")]
            attempts: Mutex::default(),
            #[cfg(feature = "test-support")]
            fault: Mutex::default(),
            #[cfg(feature = "test-support")]
            mismatch: Mutex::default(),
            #[cfg(feature = "test-support")]
            differences: Mutex::default(),
            #[cfg(feature = "test-support")]
            work_limit: Mutex::default(),
            #[cfg(feature = "test-support")]
            execution_hook: Mutex::default(),
        })
    }

    /// Whether the owner builds `state`'s head now. In production it always does (#798).
    fn wanted(&self, _state: &OwnerState) -> bool {
        #[cfg(feature = "test-support")]
        if self.on_demand.load(Ordering::Acquire) {
            return _state.demanded;
        }
        true
    }

    pub(super) fn shut_down(&self) {
        let mut state = self.state.lock().unwrap();
        state.shutdown = true;
        self.stop.store(true, Ordering::Release);
        drop(state);
        self.changed.notify_all();
    }

    fn hook(&self, _point: &'static str) {
        #[cfg(feature = "test-support")]
        {
            let hook = self.test_hook.lock().unwrap().clone();
            if let Some(hook) = hook {
                hook(_point);
            }
        }
    }

    fn run(self: Arc<Self>, daemon: Weak<DaemonState>) {
        loop {
            {
                let mut state = self.state.lock().unwrap();
                loop {
                    if state.shutdown {
                        return;
                    }
                    if state.hinted {
                        break;
                    }
                    let retry = state.retry_at.filter(|at| *at <= Instant::now());
                    if retry.is_some() {
                        break;
                    }
                    state = match state.retry_at {
                        Some(at) => {
                            let wait = at.saturating_duration_since(Instant::now());
                            self.changed.wait_timeout(state, wait).unwrap().0
                        }
                        None => self.changed.wait(state).unwrap(),
                    };
                }
                state.hinted = false;
                state.retry_at = None;
                state.stepping = true;
            }
            let Some(daemon) = daemon.upgrade() else {
                return;
            };
            let step = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                daemon.architecture_step(&self);
            }));
            if step.is_err() {
                // Report it once; the next read starts over.
                let mut state = self.state.lock().unwrap();
                if let Some(running) = state.running.take() {
                    state.publish(Published {
                        key: CoreKey {
                            pin: running.pin,
                            rules_hash: running.rules_hash,
                        },
                        outcome: Err(CoreFailure::panicked()),
                        report_once: true,
                    });
                }
                drop(state);
                self.changed.notify_all();
            }
            self.state.lock().unwrap().stepping = false;
            self.changed.notify_all();
            // The turn ended: a settled wait in a test samples the owner again.
            daemon.note_state_change();
        }
    }

    /// Test support records the path of each build the owner publishes (#1301).
    fn published_build(&self, _pin: IndexPin, _path: ArchitectureBuildPath) {
        #[cfg(feature = "test-support")]
        self.paths.lock().unwrap().push((_pin, _path));
    }

    fn verifying(&self) -> bool {
        #[cfg(feature = "test-support")]
        if self.verify.load(Ordering::Acquire) {
            return true;
        }
        verify_value(std::env::var_os("TRELLIS_ARCHITECTURE_VERIFY").as_deref())
    }

    /// A failed build retries on a bound; a later hint or read starts over.
    fn retry_later(&self) {
        let mut state = self.state.lock().unwrap();
        if state.retries < CONTENTION_RETRIES {
            state.retries += 1;
            state.retry_at = Some(Instant::now() + CONTENTION_RETRY);
        }
        self.changed.notify_all();
    }
}

/// Dropping the runtime ends its owner thread, which only holds a weak reference to it.
pub(super) struct ArchitectureHandle(pub(super) Arc<ArchitectureOwner>);
impl Drop for ArchitectureHandle {
    fn drop(&mut self) {
        self.0.shut_down();
    }
}

/// A stopped build: given way, superseded, retired or not admitted. Nothing of it is
/// published, whatever cause it carries.
fn cancelled(error: &anyhow::Error) -> bool {
    error.to_string().starts_with("cancelled")
}

/// A failure the owner keeps as its revision's answer, with the precedence of a turn's own
/// outcome: never a stop or contention, even over a cause that would repeat.
fn kept(error: &anyhow::Error) -> bool {
    !cancelled(error) && !contention(error) && CoreFailure::repeats(error)
}

/// A boundary read cannot turn an already stopped operation into a contention retry.
fn recheck_build(
    built: anyhow::Result<ArchitectureCore>,
    recheck: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<ArchitectureCore> {
    if built
        .as_ref()
        .is_err_and(|error| cancelled(error) || contention(error))
    {
        built
    } else {
        recheck().and(built)
    }
}

fn verify_value(value: Option<&std::ffi::OsStr>) -> bool {
    value == Some(std::ffi::OsStr::new("1"))
}

struct BuildRecord {
    path: ArchitectureBuildPath,
    /// The retained core the plan chose to build on (#1313).
    base: Option<IndexPin>,
    semantic_sources: Vec<crate::native_records::DocumentKey>,
    native_reads: Vec<crate::native_records::DocumentKey>,
    references: Mutex<crate::references::ReferenceReplayTrace>,
    full_retries: usize,
    compared: bool,
    #[cfg(feature = "test-support")]
    fault: Mutex<Option<(&'static str, String)>>,
    #[cfg(feature = "test-support")]
    mismatch: Mutex<Option<String>>,
    #[cfg(feature = "test-support")]
    execution_hook: Option<ArchitectureExecutionHook>,
    #[cfg(feature = "test-support")]
    executions:
        std::collections::BTreeMap<ArchitectureExecutionBranch, Arc<ArchitectureExecutionObserver>>,
}
impl Default for BuildRecord {
    fn default() -> Self {
        Self {
            path: ArchitectureBuildPath::Full(FullReason::NoBase),
            base: None,
            semantic_sources: Vec::new(),
            native_reads: Vec::new(),
            references: Mutex::default(),
            full_retries: 0,
            compared: false,
            #[cfg(feature = "test-support")]
            fault: Mutex::default(),
            #[cfg(feature = "test-support")]
            mismatch: Mutex::default(),
            #[cfg(feature = "test-support")]
            execution_hook: None,
            #[cfg(feature = "test-support")]
            executions: Default::default(),
        }
    }
}

impl BuildRecord {
    #[cfg(feature = "test-support")]
    fn execution_scope(
        &mut self,
        branch: ArchitectureExecutionBranch,
    ) -> ArchitectureExecutionScope {
        let observer = ArchitectureExecutionObserver::new(branch, self.execution_hook.clone());
        let scope = observer.enter();
        self.executions.insert(branch, observer);
        scope
    }

    fn fault(&self, _point: &'static str) -> anyhow::Result<()> {
        #[cfg(feature = "test-support")]
        {
            let mut fault = self.fault.lock().unwrap();
            if fault.as_ref().is_some_and(|(point, _)| *point == _point) {
                let (_, message) = fault.take().unwrap();
                if message.starts_with("storage_busy:") {
                    return Err(crate::store::topology::StorageBusy.into());
                }
                anyhow::bail!("{message}");
            }
        }
        Ok(())
    }
}

fn contention(error: &anyhow::Error) -> bool {
    crate::store::cli_read_retryable_contention(error)
        || error.chain().any(|cause| {
            cause
                .downcast_ref::<crate::store::RebuildInProgress>()
                .is_some()
                || cause.to_string().starts_with("index_not_ready")
        })
}

impl DaemonState {
    /// A coalescing hint after a worker step or a rules edit: no SQLite and no wait. Every
    /// active runtime's owner builds each new head, rules file or not, so the first read after
    /// an activation or a publication usually finds its core published (#798).
    pub fn architecture_hint(self: &Arc<Self>) {
        let mut state = self.architecture.0.state.lock().unwrap();
        self.hint_locked(&mut state);
        drop(state);
        self.architecture.0.changed.notify_all();
    }

    fn hint_locked(self: &Arc<Self>, state: &mut OwnerState) {
        let owner = &self.architecture.0;
        if !state.thread_started && !owner.wanted(state) {
            return;
        }
        owner.head_may_have_moved.store(true, Ordering::Release);
        state.ask();
        self.start_architecture_owner(state);
    }

    /// After a worker step: hint the owner only when the published head or the rules file
    /// differs from what the last step saw. Building every new head (#798) then adds no read,
    /// and no lease a retirement would wait for, to a step that changed nothing.
    /// A runtime that is retiring, closing or closed, or has no readable head, has nothing to
    /// build, and this reads nothing from it.
    pub(crate) fn architecture_hint_after_step(self: &Arc<Self>) {
        use super::RuntimePhase;
        if matches!(
            self.runtime_phase(),
            RuntimePhase::RetireRequested | RuntimePhase::Closing | RuntimePhase::Retired
        ) {
            return;
        }
        let Ok(head) = self.store.index_baseline() else {
            return;
        };
        let loaded = crate::rules::load(std::path::Path::new(self.store.workspace_root()));
        let observed_rules = loaded
            .as_ref()
            .ok()
            .and_then(|config| config.as_ref())
            .map(|config| config.content_hash.clone());
        let rules = match loaded {
            Ok(config) => config.map(|config| config.content_hash),
            Err(_) => Some("unreadable".into()),
        };
        let seen = Some((head, rules));
        // The head is recorded and its turn submitted under one lock, so the settled wait
        // never sees the head stepped with no turn due.
        let mut state = self.architecture.0.state.lock().unwrap();
        state.observe_rules(&CoreKey {
            pin: head,
            rules_hash: observed_rules,
        });
        if state.stepped == seen {
            return;
        }
        state.stepped = seen;
        self.hint_locked(&mut state);
        drop(state);
        self.architecture.0.changed.notify_all();
    }

    /// A semantic catch-up unit ended (#1313 T10b). When the owner skipped the head's build
    /// for that catch-up, it looks again now: a published catch-up moved the head, and one
    /// that did not publish leaves this head to build.
    pub(crate) fn architecture_catchup_unit_ended(self: &Arc<Self>, published: bool) {
        let mut state = self.architecture.0.state.lock().unwrap();
        state.catchup_settles = state.catchup_settles.wrapping_add(1);
        let Some(head) = state.deferred.take() else {
            return;
        };
        if !published {
            state.ask_for(head);
        }
        self.hint_locked(&mut state);
        drop(state);
        self.architecture.0.changed.notify_all();
    }

    /// Build `pin` even while a semantic catch-up of it is queued, as when a unit of that
    /// catch-up ended without publishing it: for fixtures that drive catch-up themselves and
    /// ask for the native head's own core.
    #[cfg(feature = "test-support")]
    pub fn architecture_decline_catchup_for_tests(self: &Arc<Self>, pin: IndexPin) {
        let mut state = self.architecture.0.state.lock().unwrap();
        state.ask_for(pin);
        if state.deferred.take().is_some() {
            self.hint_locked(&mut state);
        }
        drop(state);
        self.architecture.0.changed.notify_all();
    }

    fn start_architecture_owner(self: &Arc<Self>, state: &mut OwnerState) {
        if state.thread_started {
            return;
        }
        let owner = self.architecture.0.clone();
        let daemon = Arc::downgrade(self);
        if std::thread::Builder::new()
            .name("trellis-architecture".into())
            .spawn(move || owner.run(daemon))
            .is_ok()
        {
            state.thread_started = true;
        }
    }

    /// Release `state` for one bounded wait on the owner, then look at the waiter's `check`.
    /// Waiting holds no lease or read transaction: callers wait before they admit their read.
    fn wait_for_owner<'a>(
        &'a self,
        state: std::sync::MutexGuard<'a, OwnerState>,
        remaining: Duration,
        check: &mut impl FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<std::sync::MutexGuard<'a, OwnerState>> {
        let owner = &self.architecture.0;
        let wait = remaining.min(CHECK_WHILE_WAITING);
        drop(owner.changed.wait_timeout(state, wait).unwrap().0);
        check()?;
        Ok(owner.state.lock().unwrap())
    }

    /// The core for `pin`, waiting at most `wait` for the owner. `rules_hash` is the file a
    /// rule read needs. `Err` is the published failure, the caller's `check` failure or
    /// `ArchitectureBuilding`.
    pub(crate) fn architecture_core(
        self: &Arc<Self>,
        pin: IndexPin,
        rules: CoreRules<'_>,
        wait: Duration,
        check: &mut impl FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<Arc<ArchitectureCore>> {
        self.architecture_outcome(pin, rules, wait, check, true)
    }

    /// Wait for what `architecture_core` would answer, leaving any failure to that read.
    pub(crate) fn architecture_wait(
        self: &Arc<Self>,
        pin: IndexPin,
        rules: CoreRules<'_>,
        wait: Duration,
        check: &mut impl FnMut() -> anyhow::Result<()>,
    ) {
        let _ = self.architecture_outcome(pin, rules, wait, check, false);
    }

    /// After an admitted read found `building`'s core still being computed and ended (#1252):
    /// wait at most `wait` on the owner, holding no lease or read. When the owner has passed
    /// the pin (the head moved), the answer is the core of the head it moved to, with the
    /// same rules, whatever was published for the passed pin meanwhile: the caller waits for
    /// that core in turn instead of admitting a read that would only find it building too
    /// (#1329). Otherwise `None` when a new read should be admitted, because the core or its
    /// kept failure is published or `check` failed, and else the core still building, with
    /// its progress now.
    pub(crate) fn architecture_still_building(
        self: &Arc<Self>,
        building: &ArchitectureBuilding,
        wait: Duration,
        check: &mut impl FnMut() -> anyhow::Result<()>,
    ) -> Option<ArchitectureBuilding> {
        let pin = building.pin;
        let rules = building.rules.core_rules();
        let outcome = self.architecture_outcome(pin, rules, wait, check, false);
        // The passed pin first: its own outcome, published before the head moved, answers
        // no read the caller could be admitted for now.
        let state = self.architecture.0.state.lock().unwrap();
        if let Some(head) = state.head.filter(|_| state.passed(pin)) {
            return Some(ArchitectureBuilding {
                pin: head,
                progress: state
                    .running
                    .as_ref()
                    .filter(|running| {
                        running.pin == head && rules.admits(running.rules_hash.as_deref())
                    })
                    .and_then(|running| running.progress),
                rules: building.rules.clone(),
            });
        }
        drop(state);
        outcome.err()?.downcast::<ArchitectureBuilding>().ok()
    }

    fn architecture_outcome(
        self: &Arc<Self>,
        pin: IndexPin,
        rules: CoreRules<'_>,
        wait: Duration,
        check: &mut impl FnMut() -> anyhow::Result<()>,
        answering: bool,
    ) -> anyhow::Result<Arc<ArchitectureCore>> {
        let owner = &self.architecture.0;
        let deadline = Instant::now() + wait;
        let mut state = owner.state.lock().unwrap();
        #[cfg(feature = "test-support")]
        {
            state.demanded = true;
        }
        let mut submitted = false;
        let mut waiting = false;
        loop {
            if let Some(published) = state.answer(pin, rules) {
                let result = match &published.outcome {
                    Ok(core) => Ok(core.clone()),
                    Err(failure) => Err(failure.error()),
                };
                if published.report_once && answering {
                    state.published.retain(|p| p.key.pin != pin);
                }
                return result;
            }
            let running = state.running.as_ref().filter(|running| {
                running.pin == pin && rules.admits(running.rules_hash.as_deref())
            });
            let progress = running.and_then(|running| running.progress);
            if running.is_none() && !submitted {
                submitted = true;
                // A read of a head whose catch-up is queued wants that head's own core: the
                // owner builds it rather than wait for the catch-up (#1313 T10b, #934).
                state.ask_for(pin);
                if state.deferred == Some(pin) {
                    state.deferred = None;
                }
                state.ask();
                self.start_architecture_owner(&mut state);
                owner.changed.notify_all();
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            // The head moved past this pin, so no build is coming: the caller reads again.
            if remaining.is_zero() || state.passed(pin) {
                return Err(ArchitectureBuilding {
                    pin,
                    progress,
                    rules: rules.into(),
                }
                .into());
            }
            if state.shutdown {
                anyhow::bail!("index_not_ready: checkout released");
            }
            if !waiting {
                waiting = true;
                owner.hook("architecture_read_waiting");
            }
            state = self.wait_for_owner(state, remaining, check)?;
        }
    }

    /// Ask the owner for an older retained revision a page is showing, after the head.
    pub(crate) fn architecture_request_older(
        self: &Arc<Self>,
        pin: IndexPin,
        rules: CoreRules<'_>,
    ) {
        let mut state = self.architecture.0.state.lock().unwrap();
        state.older = Some(CoreKey {
            pin,
            rules_hash: rules.hash().map(str::to_owned),
        });
    }

    /// A published core for `pin`, or one that a running build finishes within `wait`.
    /// Metrics can compute without a core, so this never asks for one.
    pub(crate) fn architecture_core_if_available(
        &self,
        pin: IndexPin,
        wait: Duration,
        check: &mut impl FnMut() -> anyhow::Result<()>,
    ) -> Option<Arc<ArchitectureCore>> {
        let owner = &self.architecture.0;
        let deadline = Instant::now() + wait;
        let mut state = owner.state.lock().unwrap();
        loop {
            if let Some(Published {
                outcome: Ok(core), ..
            }) = state.answer(pin, CoreRules::Any)
            {
                return Some(core.clone());
            }
            let building = state.running.as_ref().is_some_and(|run| run.pin == pin);
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !building || remaining.is_zero() || state.shutdown {
                return None;
            }
            state = self.wait_for_owner(state, remaining, check).ok()?;
        }
    }

    /// How long a browser read waits for the owner.
    pub(crate) fn architecture_browser_wait(&self) -> Duration {
        #[cfg(feature = "test-support")]
        {
            // Functional tests read a result; a test of `building` sets its own wait.
            *self
                .architecture
                .0
                .browser_wait
                .lock()
                .unwrap()
                .get_or_insert(Duration::from_secs(60))
        }
        #[cfg(not(feature = "test-support"))]
        BROWSER_WAIT
    }

    /// One owner turn: compare head and rules with what is published and build what differs.
    fn architecture_step(self: &Arc<Self>, owner: &ArchitectureOwner) {
        // Before the turn admits its read: a test orders the owner's read against other work.
        owner.hook("architecture_step_started");
        // A live publication's COMMIT waits out every reader's SHARED lock, so the read waits
        // for the publication instead (#1245). It waits on the publication's own signal and
        // holds no lease, lock or snapshot meanwhile; the bound is only a liveness backstop,
        // after which the read's own checks give way again.
        if self.store.publication_writing() {
            owner.hook("architecture_awaiting_publication");
            self.store
                .await_publications_written(PUBLICATION_WAIT_BACKSTOP);
        }
        let Some(lease) = self.background_lease() else {
            return;
        };
        // #1313 T10b: sampled before this turn's read is admitted. A catch-up unit that ends
        // after it bumps the count, and this turn then builds rather than skip on a stale read
        // of the queue.
        let settles = owner.state.lock().unwrap().catchup_settles;
        let admitted = self
            .store
            .evidence_response_daemon_local()
            .and_then(|response| {
                let pin = response.selected_pin(None)?;
                Ok((response, pin))
            });
        let (response, head) = match admitted {
            Ok(admitted) => admitted,
            Err(error) => {
                if contention(&error) {
                    owner.retry_later();
                }
                return;
            }
        };
        // With this turn's read admitted: a test commits a write against it here.
        owner.hook("architecture_step_admitted");
        // A rules file that does not load answers its own reads, and references and metrics
        // do not depend on it: a core of this revision is enough.
        let loaded = crate::rules::load(std::path::Path::new(self.store.workspace_root()));
        let unreadable = loaded.is_err();
        let config = loaded.ok().flatten();
        let head_key = CoreKey {
            pin: head,
            rules_hash: config.as_ref().map(|config| config.content_hash.clone()),
        };
        // #1313 T10b: whether a semantic catch-up of this same head is queued, from this
        // turn's own admitted snapshot: no second snapshot opens while it is held. Its
        // publication is the next head, so an eager build of this one would be superseded.
        let catchup = response.pending_semantic_target_pin().ok().flatten();
        let (key, bases, base_cancelled, defer) = {
            let mut state = owner.state.lock().unwrap();
            state.head = Some(head);
            state.observe_rules(&head_key);
            let head_done = state
                .published
                .iter()
                .any(|p| p.key == head_key || (unreadable && p.key.pin == head));
            // The rules decide what an older revision shows, so that request is for this file.
            let older = state
                .older
                .take()
                .filter(|older| older.rules_hash == head_key.rules_hash)
                .filter(|older| older.pin != head)
                .filter(|older| !state.published.iter().any(|p| p.key == *older));
            state.older = older;
            state.asked.retain(|pin| {
                pin.index_generation == head.index_generation
                    && pin.index_revision >= head.index_revision
            });
            let building_head = !head_done && owner.wanted(&state);
            // Only the eager build of the head is skipped: a queued older revision is still
            // built this turn. The catch-up's own owner wakes this one when a unit ends; its
            // publication moves the head and hints the next turn.
            let defer = building_head
                && !state.shutdown
                && state.defers_for_catchup(head, catchup, settles);
            if defer {
                state.deferred = Some(head);
            }
            let key = if building_head && !defer {
                head_key
            } else if let Some(older) = state.older.clone() {
                older
            } else {
                drop(state);
                if defer {
                    owner.hook("architecture_deferred_to_catchup");
                }
                return;
            };
            if state.shutdown {
                return;
            }
            owner.stop.store(false, Ordering::Release);
            owner.head_may_have_moved.store(false, Ordering::Release);
            let bases = state.bases(&key);
            let base_cancelled = (!bases.is_empty()).then(|| Arc::new(AtomicBool::new(false)));
            state.running = Some(Running {
                pin: key.pin,
                rules_hash: key.rules_hash.clone(),
                progress: None,
                base_cancelled: base_cancelled.clone(),
            });
            (key, bases, base_cancelled, defer)
        };
        if defer {
            owner.hook("architecture_deferred_to_catchup");
        }
        let pin = key.pin;
        #[cfg(feature = "test-support")]
        let _work_limit = owner
            .work_limit
            .lock()
            .unwrap()
            .take()
            .map(|limit| limit.apply());
        owner.changed.notify_all();
        owner.hook("architecture_build_started");
        #[cfg(feature = "test-support")]
        owner.builds.fetch_add(1, Ordering::AcqRel);
        let mut check = || {
            if self.store.publication_writing() {
                owner.hook("architecture_read_yielded");
                anyhow::bail!("cancelled: architecture build yielded to a publication");
            }
            anyhow::ensure!(
                !owner.stop.load(Ordering::Acquire),
                "cancelled: architecture build stopped"
            );
            check_base_episode(base_cancelled.as_deref())?;
            if owner.head_may_have_moved.swap(false, Ordering::AcqRel) && self.head_moved(head) {
                owner.hook("architecture_build_superseded");
                anyhow::bail!("cancelled: architecture build superseded");
            }
            // The head check can itself observe a new generation.
            check_base_episode(base_cancelled.as_deref())?;
            Ok(())
        };
        #[cfg(feature = "test-support")]
        let progress_trace = Mutex::new(Vec::new());
        let progress = |progress: ArchitectureProgress| {
            #[cfg(feature = "test-support")]
            progress_trace.lock().unwrap().push(progress);
            if let Some(running) = owner.state.lock().unwrap().running.as_mut() {
                running.progress = Some(progress);
            }
            owner.hook(progress.phase);
        };
        // The read gives its snapshot up at its next check once a live publication raises its
        // write intent (#1245): that publication supersedes this head, or, if it fails, the
        // next turn reads the same head again. Nothing of a yielded read is published.
        let give_way = || {
            if self.store.publication_writing() {
                owner.hook("architecture_read_yielded");
                anyhow::bail!("cancelled: architecture read yielded to a publication");
            }
            Ok(())
        };
        let mut record = BuildRecord::default();
        #[cfg(feature = "test-support")]
        {
            *record.fault.lock().unwrap() = owner.fault.lock().unwrap().take();
            *record.mismatch.lock().unwrap() = owner.mismatch.lock().unwrap().take();
            record.execution_hook = owner.execution_hook.lock().unwrap().take();
        }
        #[cfg(feature = "test-support")]
        let newest_base = bases.first().map(|core| core.pin);
        let built = self.architecture_build(
            (lease, response),
            (pin, config.as_ref(), bases),
            &mut check,
            (&give_way, &|point| owner.hook(point)),
            (&progress, &mut record),
        );
        let built = recheck_build(check().and(built), || self.architecture_current_rules(&key));
        let built =
            self.architecture_verified((pin, config.as_ref()), built, &mut check, &mut record);
        owner.hook("architecture_build_finished");
        // Hooks can order a publication or retirement here too. Nothing checked before
        // the hook proves this operation is still current afterwards.
        let built = recheck_build(check().and(built), || self.architecture_current_rules(&key));
        let mut state = owner.state.lock().unwrap();
        let finished = state.finish_running();
        let built = if built.as_ref().is_err_and(cancelled) {
            built
        } else {
            // Episode cancellation outranks a carried integrity or contention error,
            // without replacing an earlier give-way/stop.
            finished.and(built)
        };
        #[cfg(feature = "test-support")]
        let executions: std::collections::BTreeMap<_, _> = record
            .executions
            .iter()
            .map(|(branch, observer)| (*branch, observer.snapshot()))
            .collect();
        #[cfg(feature = "test-support")]
        let execution = executions
            .get(&ArchitectureExecutionBranch::Retry)
            .or_else(|| executions.get(&ArchitectureExecutionBranch::Candidate))
            .cloned()
            .unwrap_or_default();
        #[cfg(feature = "test-support")]
        let references = {
            let mut references = execution.references.clone().unwrap_or_default();
            references.file_references = execution.file_references;
            references
        };
        #[cfg(feature = "test-support")]
        owner.attempts.lock().unwrap().push(ArchitectureAttempt {
            pin,
            base: record.base.or(newest_base),
            path: record.path.clone(),
            semantic_sources: record.semantic_sources.clone(),
            measured_sources: execution.measured_sources,
            native_reads: record.native_reads.clone(),
            references,
            executions,
            progress: progress_trace.into_inner().unwrap(),
            full_retries: record.full_retries,
            compared: record.compared,
            published: built.is_ok()
                || built.as_ref().is_err_and(|error| {
                    kept(error)
                        || (!cancelled(error)
                            && !contention(error)
                            && state.retries >= CONTENTION_RETRIES)
                }),
        });
        // The precedence `kept` follows: a stop, then contention, then a repeating failure.
        match built {
            Ok(core) => {
                state.retries = 0;
                owner.published_build(pin, record.path);
                state.publish(Published {
                    key,
                    outcome: Ok(Arc::new(core)),
                    report_once: false,
                });
            }
            Err(error) if cancelled(&error) => {}
            Err(error) if contention(&error) => {
                drop(state);
                owner.retry_later();
                return;
            }
            Err(error) if CoreFailure::repeats(&error) => {
                owner.published_build(pin, record.path);
                state.publish(Published {
                    key,
                    outcome: Err(CoreFailure::of(&error)),
                    report_once: false,
                });
            }
            Err(_) if state.retries < CONTENTION_RETRIES => {
                drop(state);
                owner.retry_later();
                return;
            }
            Err(error) => {
                owner.published_build(pin, record.path);
                state.publish(Published {
                    key,
                    outcome: Err(CoreFailure::of(&error)),
                    report_once: true,
                });
            }
        }
        // Look again: the head may have moved, and an older revision may be waiting its turn.
        state.hinted = true;
        drop(state);
        owner.changed.notify_all();
    }

    /// One build of `pin` (#1301): read A in the admitted `response`, read B in a second
    /// admitted transaction at the same pin, then the computation, which holds no transaction.
    /// Each read's transaction and lease end with it: a retirement waits for reads. Each read
    /// checks `give_way`, then `check`; the computation checks `check`. `hook` marks the
    /// point before read B's admission for tests.
    fn architecture_build(
        &self,
        (lease, response): (super::AdmissionLease, crate::store::EvidenceResponse),
        (pin, config, bases): (
            IndexPin,
            Option<&crate::rules::Config>,
            Vec<Arc<ArchitectureCore>>,
        ),
        check: &mut (impl FnMut() -> anyhow::Result<()> + Send),
        (give_way, hook): (
            &(impl Fn() -> anyhow::Result<()> + Sync),
            &dyn Fn(&'static str),
        ),
        (progress, record): (&(dyn Fn(ArchitectureProgress) + Sync), &mut BuildRecord),
    ) -> anyhow::Result<ArchitectureCore> {
        #[cfg(feature = "test-support")]
        let _execution = record.execution_scope(ArchitectureExecutionBranch::Candidate);
        let incremental = !bases.is_empty();
        let candidate = (|| {
            let mut read_check = || {
                give_way()?;
                check()
            };
            hook("architecture_read_a");
            let input = response
                .architecture_native_input_kept(
                    pin,
                    Some(&self.architecture.0.kept),
                    &mut read_check,
                    progress,
                )
                .and_then(|input| response.finish(input));
            drop(response);
            drop(lease);
            let input = input?;
            record.native_reads = input.read_documents();
            read_check()?;
            record.fault("prepare")?;
            let mut plan = prepare_architecture_from(
                ArchitectureReadA {
                    input,
                    store: &self.store,
                    config,
                    measure: true,
                    progress,
                    trace: &record.references,
                    #[cfg(feature = "test-support")]
                    work_limit: self.architecture_work_limit(),
                },
                bases,
                &mut read_check,
            )?;
            record.base = plan.base_pin();
            record.path = plan.path.clone();
            record.semantic_sources = plan.semantic_keys();
            read_check()?;
            hook("architecture_read_b");
            let (lease, response) = self.architecture_admission()?;
            hook("architecture_read_b_admitted");
            let read = record.fault("read_b").and_then(|()| {
                response
                    .architecture_plan_read_b(&mut plan, &mut read_check)
                    .and_then(|read| response.finish(read))
            });
            drop(response);
            drop(lease);
            // A stop outranks even an integrity or contention error carried by B.
            read_check()?;
            let read = read?;
            record.fault("finish")?;
            finish_architecture(plan, read, &mut read_check)
        })();
        give_way()?;
        check()?;
        match candidate {
            Err(error)
                if incremental
                    && !cancelled(&error)
                    && !contention(&error)
                    && (matches!(record.path, ArchitectureBuildPath::Incremental { .. })
                        || record.path == ArchitectureBuildPath::Full(FullReason::NoBase)) =>
            {
                record.path = ArchitectureBuildPath::Full(
                    if error.downcast_ref::<ReferenceWorkMargin>().is_some() {
                        FullReason::ReferenceWorkMargin
                    } else {
                        FullReason::IncrementalFailure
                    },
                );
                hook("architecture_full_retry");
                give_way()?;
                check()?;
                self.architecture_current_rules(&CoreKey {
                    pin,
                    rules_hash: config.map(|config| config.content_hash.clone()),
                })?;
                check()?;
                *record.references.lock().unwrap() = Default::default();
                record.full_retries += 1;
                #[cfg(feature = "test-support")]
                let _execution = record.execution_scope(ArchitectureExecutionBranch::Retry);
                self.architecture_full(pin, config, check, progress, record)
            }
            result => result,
        }
    }

    fn architecture_current_rules(&self, key: &CoreKey) -> anyhow::Result<()> {
        use anyhow::Context;
        self.architecture.0.hook("architecture_before_recheck");
        let head = self.architecture.0.state.lock().unwrap().head;
        let lease = self
            .background_lease()
            .context("cancelled: architecture read retired")?;
        let selected = self
            .store
            .evidence_response_daemon_local()
            .and_then(|response| response.selected_pin(None));
        drop(lease);
        let selected = selected.map_err(|error| {
            if contention(&error) {
                error
            } else {
                error.context("cancelled: architecture head not admitted")
            }
        })?;
        let loaded = crate::rules::load(std::path::Path::new(self.store.workspace_root()));
        let current = loaded.ok().flatten().map(|config| config.content_hash);
        // An observation ends the episode even when this operation cannot publish.
        // In particular, a later return to A must not resurrect its pre-B base.
        self.architecture
            .0
            .state
            .lock()
            .unwrap()
            .observe_rules(&CoreKey {
                pin: selected,
                rules_hash: current.clone(),
            });
        anyhow::ensure!(
            head == Some(selected),
            "cancelled: architecture head superseded"
        );
        anyhow::ensure!(
            current == key.rules_hash,
            "cancelled: architecture rules superseded"
        );
        Ok(())
    }

    /// Independent original full computation: fresh A and all-source B at the same pin.
    fn architecture_full(
        &self,
        pin: IndexPin,
        config: Option<&crate::rules::Config>,
        check: &mut (impl FnMut() -> anyhow::Result<()> + Send),
        progress: &(dyn Fn(ArchitectureProgress) + Sync),
        record: &mut BuildRecord,
    ) -> anyhow::Result<ArchitectureCore> {
        #[cfg(feature = "test-support")]
        let _work_limit = self.architecture_work_limit().apply();
        check()?;
        let (lease, response) = self.architecture_admission()?;
        let input = response
            .architecture_native_input(pin, check, progress)
            .and_then(|input| response.finish(input));
        drop(response);
        drop(lease);
        let mut input = input?;
        record.semantic_sources = input.document_keys();
        progress(ArchitectureProgress {
            phase: "resolving",
            completed: 0,
            total: record.semantic_sources.len(),
        });
        check()?;
        let (lease, response) = self.architecture_admission()?;
        let read = response
            .architecture_semantic_input(&mut input, check)
            .and_then(|()| response.finish(()));
        drop(response);
        drop(lease);
        check()?;
        read?;
        self.architecture.0.hook("architecture_full_compute");
        record.fault("full_compute")?;
        check()?;
        let limit = input.reference_limit(check)?;
        let core = ArchitectureCore::compute(&self.store, input, config, true, check, progress)?;
        record.semantic_sources = core.document_keys();
        *record.references.lock().unwrap() = core.full_reference_trace(limit);
        Ok(core)
    }

    #[cfg(feature = "test-support")]
    fn architecture_work_limit(&self) -> crate::references::ReferenceWorkLimitForTests {
        crate::references::ReferenceWorkLimitForTests::capture()
    }

    /// A lease and an admitted read for a build's later read. Without a lease the runtime is
    /// retiring, and a read that cannot be admitted is retried as a turn's first read is: by
    /// contention, or else by the next hint. Either stops this build as cancelled.
    fn architecture_admission(
        &self,
    ) -> anyhow::Result<(super::AdmissionLease, crate::store::EvidenceResponse)> {
        use anyhow::Context;
        let lease = self
            .background_lease()
            .context("cancelled: architecture read retired")?;
        let admitted = self.store.evidence_response_daemon_local();
        // A test makes the next admission fail with an integrity fault, as a corrupt read would.
        #[cfg(feature = "test-support")]
        let admitted = match self.architecture.0.admission_fault.lock().unwrap().take() {
            Some(fault) => Err(crate::store::SelectedIntegrity(fault).into()),
            None => admitted,
        };
        let response = admitted.map_err(|error| {
            if contention(&error) {
                error
            } else {
                error.context("cancelled: architecture read not admitted")
            }
        })?;
        Ok((lease, response))
    }

    /// Compare with independently admitted full inputs. Equality retains the candidate's
    /// shared parts; a mismatch serves the full result without a second publication.
    fn architecture_verified(
        &self,
        (pin, config): (IndexPin, Option<&crate::rules::Config>),
        built: anyhow::Result<ArchitectureCore>,
        check: &mut (impl FnMut() -> anyhow::Result<()> + Send),
        record: &mut BuildRecord,
    ) -> anyhow::Result<ArchitectureCore> {
        check()?;
        let owner = &self.architecture.0;
        if !owner.verifying() || built.as_ref().is_err_and(|error| !kept(error)) {
            return built;
        }
        owner.hook("architecture_verify_started");
        check()?;
        let mut oracle = BuildRecord::default();
        #[cfg(feature = "test-support")]
        let _execution = record.execution_scope(ArchitectureExecutionBranch::Oracle);
        #[cfg(feature = "test-support")]
        {
            *oracle.fault.lock().unwrap() = record.fault.lock().unwrap().take();
        }
        let full = oracle
            .fault("oracle")
            .and_then(|()| self.architecture_full(pin, config, check, &|_| {}, &mut oracle));
        check()?;
        if full.as_ref().is_err_and(|error| !kept(error)) {
            return full;
        }
        self.architecture_current_rules(&CoreKey {
            pin,
            rules_hash: config.map(|config| config.content_hash.clone()),
        })?;
        check()?;
        let difference = match (&built, &full) {
            (Ok(candidate), Ok(oracle)) => {
                let candidate = candidate.canonical()?;
                #[cfg(feature = "test-support")]
                let candidate = {
                    let mut candidate = candidate;
                    if let Some(pointer) = record.mismatch.lock().unwrap().take() {
                        *candidate
                            .pointer_mut(&pointer)
                            .ok_or_else(|| anyhow::anyhow!("invalid mismatch injection path"))? =
                            serde_json::json!("injected mismatch");
                    }
                    candidate
                };
                crate::architecture_core::first_difference(&candidate, &oracle.canonical()?)
            }
            (Err(candidate), Err(oracle)) => {
                (format!("{candidate:#}") != format!("{oracle:#}")).then(|| "/failure".into())
            }
            _ => Some("/outcome".into()),
        };
        check()?;
        self.architecture_current_rules(&CoreKey {
            pin,
            rules_hash: config.map(|config| config.content_hash.clone()),
        })?;
        check()?;
        #[cfg(feature = "test-support")]
        owner.verified.fetch_add(1, Ordering::AcqRel);
        record.compared = true;
        if let Some(at) = difference {
            use std::io::Write;
            let _ = writeln!(
                std::io::stderr().lock(),
                "architecture verify: pin={pin:?} attempted={:?} first_difference={at}",
                record.path
            );
            #[cfg(feature = "test-support")]
            owner.differences.lock().unwrap().push(at);
            full
        } else {
            built
        }
    }

    /// Whether a head newer than `pin` is published. Retirement answers no.
    fn head_moved(&self, pin: IndexPin) -> bool {
        self.background_lease().is_some_and(|_lease| {
            self.store
                .evidence_response_daemon_local()
                .and_then(|fresh| fresh.selected_pin(None))
                .is_ok_and(|head| {
                    if head.index_generation != pin.index_generation {
                        let rules =
                            crate::rules::load(std::path::Path::new(self.store.workspace_root()))
                                .ok()
                                .flatten()
                                .map(|config| config.content_hash);
                        self.architecture
                            .0
                            .state
                            .lock()
                            .unwrap()
                            .observe_rules(&CoreKey {
                                pin: head,
                                rules_hash: rules,
                            });
                    }
                    head != pin
                })
        })
    }

    #[cfg(feature = "test-support")]
    pub fn set_architecture_hook_for_tests(&self, hook: TestHook) {
        *self.architecture.0.test_hook.lock().unwrap() = Some(hook);
    }

    /// Install one operation's scoped execution hook; retries and its oracle share
    /// the callback but record their observations independently.
    #[cfg(feature = "test-support")]
    pub fn set_architecture_execution_hook_for_tests(&self, hook: ArchitectureExecutionHook) {
        *self.architecture.0.execution_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(feature = "test-support")]
    pub fn architecture_core_for_tests(
        &self,
        pin: IndexPin,
    ) -> Option<crate::architecture_core::ArchitectureCoreHandle> {
        self.architecture
            .0
            .state
            .lock()
            .unwrap()
            .published
            .iter()
            .find(|published| published.key.pin == pin)
            .and_then(|published| published.outcome.as_ref().ok())
            .map(|core| ArchitectureCore::test_handle(core.clone()))
    }

    #[cfg(feature = "test-support")]
    pub fn architecture_attempts_for_tests(&self) -> Vec<ArchitectureAttempt> {
        self.architecture.0.attempts.lock().unwrap().clone()
    }

    #[cfg(feature = "test-support")]
    pub fn architecture_differences_for_tests(&self) -> Vec<String> {
        self.architecture.0.differences.lock().unwrap().clone()
    }

    /// Inject one operation-local ordinary, stopped, or contended stage failure.
    #[cfg(feature = "test-support")]
    pub fn set_architecture_fault_for_tests(&self, stage: &'static str, error: String) {
        *self.architecture.0.fault.lock().unwrap() = Some((stage, error));
    }

    #[cfg(feature = "test-support")]
    pub fn set_architecture_admission_fault_for_tests(&self, error: String) {
        *self.architecture.0.admission_fault.lock().unwrap() = Some(error);
    }

    #[cfg(feature = "test-support")]
    pub fn set_architecture_mismatch_for_tests(&self, pointer: String) {
        *self.architecture.0.mismatch.lock().unwrap() = Some(pointer);
    }

    /// Capture the requesting thread's RAII override before the owner starts its work.
    #[cfg(feature = "test-support")]
    pub fn capture_architecture_work_limit_for_tests(&self) {
        *self.architecture.0.work_limit.lock().unwrap() =
            Some(crate::references::ReferenceWorkLimitForTests::capture());
    }

    /// Admission leases held now.
    #[cfg(feature = "test-support")]
    pub fn leases_for_tests(&self) -> usize {
        self.leases()
    }

    /// Whether the owner has a core published for `pin` built with a rules file.
    #[cfg(feature = "test-support")]
    pub fn architecture_ruled_core_published_for_tests(&self, pin: IndexPin) -> bool {
        let state = self.architecture.0.state.lock().unwrap();
        state
            .published
            .iter()
            .any(|p| p.key.pin == pin && p.key.rules_hash.is_some())
    }

    /// Builds this runtime has started.
    #[cfg(feature = "test-support")]
    pub fn architecture_builds_for_tests(&self) -> usize {
        self.architecture.0.builds.load(Ordering::Acquire)
    }

    /// Whether the owner still owes work for the published head `pin`, for the tests' settled
    /// wait: a turn is running or due, or no worker step has submitted `pin` yet. A head that
    /// semantic catch-up or maintenance published is submitted by the worker's next step, so
    /// until then the owner owes its build (#798).
    pub(crate) fn architecture_owes(&self, pin: IndexPin) -> bool {
        let state = self.architecture.0.state.lock().unwrap();
        state.hinted
            || state.stepping
            || state.retry_at.is_some()
            || state.stepped.as_ref().is_none_or(|(head, _)| *head != pin)
    }

    /// Wait until the owner has no turn running or due.
    #[cfg(any(test, feature = "test-support"))]
    pub fn await_architecture_idle_for_tests(&self) {
        let owner = &self.architecture.0;
        let deadline = Instant::now() + crate::testing::HANG_BACKSTOP;
        let mut state = owner.state.lock().unwrap();
        while state.hinted || state.stepping || state.retry_at.is_some() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "the architecture owner did not settle"
            );
            state = owner.changed.wait_timeout(state, remaining).unwrap().0;
        }
    }

    #[cfg(feature = "test-support")]
    pub fn set_architecture_on_demand_for_tests(&self) {
        self.architecture.0.on_demand.store(true, Ordering::Release);
    }

    /// Turn verify mode (#1301) on or off for this runtime's owner. It is on by default.
    #[cfg(feature = "test-support")]
    pub fn set_architecture_verify_incremental_for_tests(&self, on: bool) {
        self.architecture.0.verify.store(on, Ordering::Release);
    }

    /// Builds verify mode compared with a full build of the same revision.
    #[cfg(feature = "test-support")]
    pub fn architecture_verified_for_tests(&self) -> usize {
        self.architecture.0.verified.load(Ordering::Acquire)
    }

    /// The path of each build that published a core or a kept failure, oldest first.
    #[cfg(feature = "test-support")]
    pub fn architecture_build_paths_for_tests(
        &self,
    ) -> Vec<(IndexPin, crate::architecture_core::ArchitectureBuildPath)> {
        self.architecture.0.paths.lock().unwrap().clone()
    }

    #[cfg(feature = "test-support")]
    pub fn set_architecture_browser_wait_for_tests(&self, wait: Duration) {
        *self.architecture.0.browser_wait.lock().unwrap() = Some(wait);
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use crate::http::RetireOutcome;
    use std::sync::mpsc;

    #[test]
    fn architecture_verify_candidate_policy_exact_environment_value() {
        use std::ffi::OsStr;
        assert!(verify_value(Some(OsStr::new("1"))));
        for value in [
            None,
            Some(""),
            Some("0"),
            Some("true"),
            Some("01"),
            Some("1 "),
        ] {
            assert!(!verify_value(value.map(OsStr::new)), "{value:?}");
        }
    }

    /// Holds the owner's build at the `nth` time it reaches `point` until the sender drops.
    fn hold_at(
        state: &Arc<DaemonState>,
        point: &'static str,
        nth: usize,
    ) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered, reached) = mpsc::channel();
        let (release, held) = mpsc::channel::<()>();
        let held = Mutex::new(held);
        let seen = std::sync::atomic::AtomicUsize::new(0);
        state.set_architecture_hook_for_tests(Arc::new(move |at| {
            if at == point && seen.fetch_add(1, Ordering::AcqRel) == nth {
                let _ = entered.send(());
                let _ = held
                    .lock()
                    .unwrap()
                    .recv_timeout(crate::testing::HANG_BACKSTOP);
            }
        }));
        (reached, release)
    }

    fn head(state: &DaemonState) -> IndexPin {
        state
            .store
            .evidence_response_daemon_local()
            .and_then(|response| response.selected_pin(None))
            .unwrap()
    }

    /// Only a build's read holds a lease: retirement goes ahead while it computes, and the
    /// owner's next turn never cancels a requested retirement.
    #[test]
    fn retirement_does_not_wait_for_a_build_to_compute() {
        let (_tmp, _store, state, _roots, _identity) =
            crate::http::exceptional_recovery_tests::fixture();
        let (reached, release) = hold_at(&state, "resolving", 0);
        // The publication's own hint starts the build (#798).
        state.worker_step().unwrap();
        reached.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
        assert_eq!(state.leases(), 0);
        assert!(matches!(
            state.request_retire(|_| true),
            RetireOutcome::Requested
        ));
        assert!(state.background_lease().is_none());
        state.architecture_hint();
        assert!(matches!(
            state.request_retire(|_| true),
            RetireOutcome::Requested
        ));
        drop(release);
        assert_eq!(state.architecture_builds_for_tests(), 1);
    }

    /// #798: a retirement asked for while the head's eager build is reading it declines, as for
    /// any admitted read, and is requested once that read has ended. Retirement is retried, so
    /// the warm read delays it and never starves it.
    #[test]
    fn a_retirement_during_the_build_read_declines_then_is_requested() {
        let (_tmp, _store, state, _roots, _identity) =
            crate::http::exceptional_recovery_tests::fixture();
        let (reached, release) = hold_at(&state, "reading", 0);
        state.worker_step().unwrap();
        reached.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
        assert_eq!(state.leases(), 1);
        assert!(matches!(
            state.request_retire(|_| true),
            RetireOutcome::Declined
        ));
        drop(release);
        state.await_architecture_idle_for_tests();
        assert_eq!(state.leases(), 0);
        assert!(matches!(
            state.request_retire(|_| true),
            RetireOutcome::Requested
        ));
    }

    /// #798: a publication builds the new head with no rules file and no read asking, so the
    /// first read finds its core published.
    #[test]
    fn a_publication_builds_the_head_without_rules_or_a_read() {
        let (_tmp, _store, state, _roots, identity) =
            crate::http::exceptional_recovery_tests::fixture();
        assert!(!identity.root.join(crate::rules::RULES_PATH).exists());
        let (reached, release) = hold_at(&state, "architecture_build_finished", 0);
        state.worker_step().unwrap();
        reached
            .recv_timeout(crate::testing::HANG_BACKSTOP)
            .expect("the publication started a build");
        drop(release);
        // Waits only while that build publishes; this never asks for a build.
        let core = state.architecture_core_if_available(
            head(&state),
            crate::testing::HANG_BACKSTOP,
            &mut || Ok(()),
        );
        assert!(core.is_some());
        assert!(
            state
                .architecture_core(head(&state), CoreRules::Any, Duration::ZERO, &mut || Ok(()))
                .is_ok()
        );
        assert_eq!(state.architecture_builds_for_tests(), 1);
    }

    /// What the owner has published, the paths it recorded and the comparisons it made.
    fn published_state(state: &DaemonState) -> (usize, usize, usize) {
        let owner = &state.architecture.0;
        (
            owner.state.lock().unwrap().published.len(),
            owner.paths.lock().unwrap().len(),
            owner.verified.load(Ordering::Acquire),
        )
    }

    #[test]
    fn architecture_incremental_worker_rules_observations_cancel_borrowed_base() {
        let (_tmp, store, state, _roots, _identity) =
            crate::http::exceptional_recovery_tests::fixture();
        let root = std::path::Path::new(store.workspace_root());
        std::fs::remove_file(root.join("a.js")).unwrap();
        for (path, text) in [
            ("main.rs", "fn main() { let value = 1; }\n"),
            ("one.rs", "pub fn one() {}\n"),
            ("two.rs", "pub fn two() {}\n"),
            ("three.rs", "pub fn three() {}\n"),
        ] {
            std::fs::write(root.join(path), text).unwrap();
        }
        let rules_path = root.join(".trellis/rules.toml");
        std::fs::create_dir_all(rules_path.parent().unwrap()).unwrap();
        let rules_a = "version=1\n[layers.code]\norder=1\npaths=['**/*.rs']\n";
        let rules_b = "version=1\n[layers.other]\norder=1\npaths=['**/*.rs']\n";
        std::fs::write(&rules_path, rules_a).unwrap();
        state.worker_step().unwrap();
        state.await_architecture_idle_for_tests();
        let original = head(&state);
        let before_paths = state.architecture_build_paths_for_tests().len();
        let before_compared = state.architecture_verified_for_tests();
        assert_eq!(before_paths, 1);
        assert_eq!(before_compared, 1);
        assert!(state.architecture_differences_for_tests().is_empty());

        let (reached, release) = hold_at(&state, "architecture_read_b", 0);
        std::fs::write(root.join("main.rs"), "fn main() { let value = 2; }\n").unwrap();
        store
            .enqueue_request(&crate::indexer::IndexOptions::new(root.to_owned()), None)
            .unwrap();
        state.worker_step().unwrap();
        reached.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
        let pin = head(&state);
        assert_ne!(pin, original);
        assert_eq!(state.leases(), 0, "read A ended before the owner was held");

        // Drive the real post-step observer at the unchanged selected head, while
        // the owner already holds a base Arc but no read can hide the rules edits.
        for rules in [rules_b, rules_a] {
            std::fs::write(&rules_path, rules).unwrap();
            state.worker_step().unwrap();
            assert_eq!(head(&state), pin);
            let live = crate::rules::load(root).unwrap().unwrap().content_hash;
            assert_eq!(
                state.architecture.0.state.lock().unwrap().live_rules,
                Some((pin.index_generation, Some(live)))
            );
        }
        drop(release);
        state.await_architecture_idle_for_tests();

        let attempts: Vec<_> = state
            .architecture_attempts_for_tests()
            .into_iter()
            .filter(|attempt| attempt.pin == pin)
            .collect();
        let stopped = &attempts[0];
        assert_eq!(stopped.base, Some(original));
        assert!(
            !stopped.compared && !stopped.published,
            "an observed rules episode cancels the borrowed base: {stopped:?}"
        );
        assert_eq!(stopped.full_retries, 0);
        assert!(matches!(
            stopped.path,
            ArchitectureBuildPath::Incremental { .. }
        ));
        assert_eq!(attempts.len(), 2, "{attempts:?}");
        let next = &attempts[1];
        assert_eq!(next.base, None);
        assert_eq!(next.path, ArchitectureBuildPath::Full(FullReason::NoBase));
        assert!(next.compared && next.published);
        assert_eq!(
            state.architecture_build_paths_for_tests().len(),
            before_paths + 1
        );
        assert_eq!(state.architecture_verified_for_tests(), before_compared + 1);
        assert!(state.architecture_differences_for_tests().is_empty());
    }

    /// #1301: a stop keeps its precedence over a cause that would repeat, as in a turn's own
    /// outcome handling.
    #[test]
    fn a_stopped_build_is_never_kept_whatever_its_cause() {
        let stopped = anyhow::Error::from(crate::store::SelectedIntegrity("corrupt".into()))
            .context("cancelled: architecture read not admitted");
        assert!(CoreFailure::repeats(&stopped));
        assert!(cancelled(&stopped));
        assert!(!kept(&stopped));
        let repeats = anyhow::Error::from(crate::store::SelectedIntegrity("corrupt".into()));
        assert!(kept(&repeats));
        assert!(!kept(&anyhow::anyhow!("storage_busy: index read")));
    }

    #[test]
    fn a_boundary_read_cannot_reclassify_a_stopped_build() {
        for error in [
            anyhow::anyhow!("cancelled: stopped"),
            anyhow::Error::from(crate::store::SelectedIntegrity("corrupt".into()))
                .context("cancelled: read not admitted"),
            anyhow::Error::from(crate::store::topology::StorageBusy),
        ] {
            let expected = format!("{error:#}");
            let result = recheck_build(Err(error), || {
                panic!("a stopped build admits no boundary read")
            });
            assert_eq!(format!("{:#}", result.err().unwrap()), expected);
        }
    }

    /// #1301: the runtime retires between read A and read B. Read B gets no lease, so the
    /// build stops: nothing is published, no path is recorded, and verify mode compares
    /// nothing.
    #[test]
    fn a_retirement_before_read_b_stops_the_build_and_publishes_nothing() {
        let (_tmp, _store, state, _roots, _identity) =
            crate::http::exceptional_recovery_tests::fixture();
        let (reached, release) = hold_at(&state, "architecture_read_b", 0);
        state.worker_step().unwrap();
        reached.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
        // Read A ended, so nothing holds a lease and the retirement goes ahead.
        assert_eq!(state.leases(), 0);
        assert!(matches!(
            state.request_retire(|_| true),
            RetireOutcome::Requested
        ));
        drop(release);
        state.await_architecture_idle_for_tests();
        assert_eq!(state.architecture_builds_for_tests(), 1);
        assert_eq!(published_state(&state), (0, 0, 0));
    }

    /// #1301: read B's admission fails with an integrity cause, which the build reports as
    /// stopped. Nothing is published or recorded for it and verify mode compares nothing; the
    /// next turn builds the head again and publishes that.
    #[test]
    fn a_read_b_admission_failure_stops_the_build_and_publishes_nothing() {
        let (_tmp, _store, state, _roots, _identity) =
            crate::http::exceptional_recovery_tests::fixture();
        let (reached, release) = mpsc::channel::<()>();
        let (go, held) = mpsc::channel::<()>();
        let held = Mutex::new(held);
        let faulted = Arc::downgrade(&state);
        let turns = std::sync::atomic::AtomicUsize::new(0);
        state.set_architecture_hook_for_tests(Arc::new(move |at| match at {
            "architecture_read_b" => {
                if let Some(state) = faulted.upgrade() {
                    *state.architecture.0.admission_fault.lock().unwrap() =
                        Some("corrupt admitted read".into());
                }
            }
            // The second turn: the first one's outcome has been handled.
            "architecture_step_started" if turns.fetch_add(1, Ordering::AcqRel) == 1 => {
                let _ = reached.send(());
                let _ = held
                    .lock()
                    .unwrap()
                    .recv_timeout(crate::testing::HANG_BACKSTOP);
            }
            _ => {}
        }));
        state.worker_step().unwrap();
        release.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
        assert_eq!(state.architecture_builds_for_tests(), 1);
        assert_eq!(published_state(&state), (0, 0, 0));
        state.set_architecture_hook_for_tests(Arc::new(|_| {}));
        drop(go);
        state.await_architecture_idle_for_tests();
        assert_eq!(state.architecture_builds_for_tests(), 2);
        assert_eq!(published_state(&state), (1, 1, 1));
    }

    /// A stopped oracle is not a completed full success. The operation publishes nothing;
    /// the next turn independently builds and verifies the same head.
    #[test]
    fn a_stopped_verify_build_compares_nothing_and_the_next_turn_builds() {
        let (_tmp, _store, state, _roots, _identity) =
            crate::http::exceptional_recovery_tests::fixture();
        let faulted = Arc::downgrade(&state);
        let injected = AtomicBool::new(false);
        state.set_architecture_hook_for_tests(Arc::new(move |at| {
            if at == "architecture_verify_started"
                && !injected.swap(true, Ordering::AcqRel)
                && let Some(state) = faulted.upgrade()
            {
                *state.architecture.0.admission_fault.lock().unwrap() =
                    Some("corrupt admitted read".into());
            }
        }));
        state.worker_step().unwrap();
        state.await_architecture_idle_for_tests();
        assert_eq!(state.architecture_builds_for_tests(), 2);
        assert_eq!(published_state(&state), (1, 1, 1));
        let attempts = state.architecture_attempts_for_tests();
        assert!(!attempts[0].compared);
        assert!(!attempts[0].published);
        assert_eq!(attempts[0].full_retries, 0);
        let head = head(&state);
        let owner = state.architecture.0.state.lock().unwrap();
        assert!(matches!(
            owner.published.as_slice(),
            [Published {
                key: CoreKey { pin, .. },
                outcome: Ok(_),
                ..
            }] if *pin == head
        ));
    }

    /// #798: measuring reports each measured document, so a read during it sees the count
    /// move, and its `building` answer carries that progress.
    #[test]
    fn measuring_reports_each_measured_document() {
        let (_tmp, _store, state, _roots, identity) =
            crate::http::exceptional_recovery_tests::fixture();
        std::fs::write(identity.root.join("b.js"), "function b() {}\n").unwrap();
        std::fs::write(identity.root.join("c.js"), "function c() {}\n").unwrap();
        // The first report is the phase's start; the second is the first measured document.
        let (reached, release) = hold_at(&state, "measuring", 1);
        state.worker_step().unwrap();
        reached.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
        let pin = head(&state);
        let error = state
            .architecture_core(pin, CoreRules::Any, Duration::ZERO, &mut || Ok(()))
            .err()
            .expect("the core is still building");
        let building = error.downcast_ref::<ArchitectureBuilding>().unwrap();
        assert_eq!(
            building.progress,
            Some(ArchitectureProgress {
                phase: "measuring",
                completed: 1,
                total: 3,
            })
        );
        assert_eq!(
            building.message(),
            "Architecture is being computed for this revision (measuring 1 of 3); retry shortly"
        );
        drop(release);
        assert!(
            state
                .architecture_core(
                    pin,
                    CoreRules::Any,
                    crate::testing::HANG_BACKSTOP,
                    &mut || Ok(())
                )
                .is_ok()
        );
        assert_eq!(state.architecture_builds_for_tests(), 1);
    }
}

#[cfg(test)]
mod owner_state_tests {
    use super::*;

    fn pin(revision: u64) -> IndexPin {
        IndexPin {
            index_generation: uuid::Uuid::nil(),
            index_revision: revision,
        }
    }
    fn published(revision: u64) -> Published {
        Published {
            key: CoreKey {
                pin: pin(revision),
                rules_hash: None,
            },
            outcome: Err(CoreFailure::panicked()),
            report_once: false,
        }
    }
    /// #851 review: a core answers a rule read only under exactly its own rules file. A core
    /// built with a file never answers a read of the checkout once the file is removed, nor a
    /// core built without one a read under a file; a read that uses no rules takes either.
    #[test]
    fn a_core_answers_only_reads_of_its_own_rules_state() {
        let mut ruled = published(1);
        ruled.key.rules_hash = Some("h".into());
        let unruled = published(1);
        for (core, built) in [(&ruled, Some("h")), (&unruled, None)] {
            assert!(core.answers(pin(1), CoreRules::Any));
            assert!(core.answers(pin(1), CoreRules::Exactly(built)));
            assert!(!core.answers(pin(2), CoreRules::Exactly(built)));
        }
        assert!(!ruled.answers(pin(1), CoreRules::Exactly(None)));
        assert!(!unruled.answers(pin(1), CoreRules::Exactly(Some("h"))));
        assert!(!ruled.answers(pin(1), CoreRules::Exactly(Some("other"))));
        let mut state = OwnerState::default();
        state.published.push(ruled);
        assert!(state.answer(pin(1), CoreRules::Exactly(None)).is_none());
        assert!(state.answer(pin(1), CoreRules::Any).is_some());
    }

    /// #1313 T10b: a turn skips the head's build only for a catch-up of that same head, never
    /// for a catch-up of another revision or none, never again once a unit of it ended
    /// without publishing that head, and never on a queue read older than a unit's end.
    #[test]
    fn the_head_build_defers_only_to_a_catch_up_of_that_same_head() {
        let mut state = OwnerState::default();
        assert!(state.defers_for_catchup(pin(5), Some(pin(5)), 0));
        assert!(!state.defers_for_catchup(pin(5), Some(pin(4)), 0));
        assert!(!state.defers_for_catchup(pin(5), None, 0));
        // A catch-up unit ended after this turn read the queue.
        state.catchup_settles = 1;
        assert!(!state.defers_for_catchup(pin(5), Some(pin(5)), 0));
        assert!(state.defers_for_catchup(pin(5), Some(pin(5)), 1));
        // A unit ended without publishing head 5: 5 builds, a later head may defer again.
        state.ask_for(pin(5));
        assert!(!state.defers_for_catchup(pin(5), Some(pin(5)), 1));
        assert!(state.defers_for_catchup(pin(6), Some(pin(6)), 1));
    }

    fn revisions(state: &OwnerState) -> Vec<u64> {
        state
            .published
            .iter()
            .map(|p| p.key.pin.index_revision)
            .collect()
    }

    #[test]
    fn borrowed_base_transition_after_a_check_is_rejected_at_final_admission() {
        let original = CoreKey {
            pin: pin(2),
            rules_hash: Some("A".into()),
        };
        let mut other_rules = original.clone();
        other_rules.rules_hash = Some("B".into());
        let mut other_generation = original.clone();
        other_generation.pin.index_generation = uuid::Uuid::from_u128(1);
        for changed in [other_rules, other_generation] {
            let mut state = OwnerState::default();
            state.observe_rules(&original);
            let base_cancelled = Arc::new(AtomicBool::new(false));
            state.running = Some(Running {
                pin: original.pin,
                rules_hash: original.rules_hash.clone(),
                progress: None,
                base_cancelled: Some(base_cancelled.clone()),
            });
            let mut same_episode = original.clone();
            same_episode.pin.index_revision += 1;
            state.observe_rules(&same_episode);
            check_base_episode(Some(&base_cancelled)).unwrap();

            // The last sampled compute check passed. Both transitions now take
            // place before the lock-owned final admission, without another check.
            state.observe_rules(&changed);
            state.observe_rules(&original);
            let error = state.finish_running().unwrap_err();
            assert_eq!(
                error.to_string(),
                "cancelled: architecture base episode ended"
            );
            assert!(state.running.is_none());
            assert!(base_cancelled.load(Ordering::Acquire));

            // Ending that operation does not cancel a successor in the new episode.
            let next = Arc::new(AtomicBool::new(false));
            state.running = Some(Running {
                pin: original.pin,
                rules_hash: original.rules_hash.clone(),
                progress: None,
                base_cancelled: Some(next.clone()),
            });
            state.observe_rules(&original);
            state.finish_running().unwrap();
            assert!(!next.load(Ordering::Acquire));
        }
    }

    #[test]
    fn publishing_keeps_the_head_and_one_other() {
        let mut state = OwnerState {
            head: Some(pin(5)),
            ..OwnerState::default()
        };
        state.publish(published(4));
        state.publish(published(5));
        state.publish(published(2));
        assert_eq!(revisions(&state), [2, 5]);
        state.publish(published(3));
        assert_eq!(revisions(&state), [3, 5]);
        state.head = Some(pin(6));
        state.publish(published(6));
        assert_eq!(revisions(&state), [6, 3]);
    }

    #[test]
    fn a_pin_behind_the_owners_head_is_passed_unless_a_page_asked_for_it() {
        let mut state = OwnerState {
            head: Some(pin(5)),
            ..OwnerState::default()
        };
        assert!(state.passed(pin(4)));
        assert!(!state.passed(pin(5)));
        // A reader can see a head before the owner does.
        assert!(!state.passed(pin(6)));
        state.older = Some(published(4).key);
        assert!(!state.passed(pin(4)));
    }

    #[test]
    fn publishing_an_older_request_ends_it() {
        let mut state = OwnerState {
            head: Some(pin(5)),
            older: Some(published(2).key),
            ..OwnerState::default()
        };
        state.publish(published(5));
        assert!(state.older.is_some());
        state.publish(published(2));
        assert!(state.older.is_none());
    }

    #[test]
    fn asking_keeps_a_due_retry_and_only_restarts_a_spent_budget() {
        let mut state = OwnerState {
            retries: 2,
            retry_at: Some(Instant::now()),
            ..OwnerState::default()
        };
        state.ask();
        assert!(!state.hinted);
        assert_eq!(state.retries, 2);
        state.retry_at = None;
        state.ask();
        assert!(state.hinted);
        assert_eq!(state.retries, 2);
        state.retries = CONTENTION_RETRIES;
        state.ask();
        assert_eq!(state.retries, 0);
    }
}
