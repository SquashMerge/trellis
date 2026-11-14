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
use crate::architecture_core::{ArchitectureCore, ArchitectureProgress, CoreFailure};
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

/// How long a `building` answer asks its caller to wait before it reads again.
pub(crate) const RETRY_AFTER_MS: u64 = 250;

/// The read arrived while its core is still being computed. Retryable, never partial.
#[derive(Debug)]
pub struct ArchitectureBuilding {
    pub pin: IndexPin,
    pub(crate) progress: Option<ArchitectureProgress>,
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
}
/// A checkout holds at most this many cores.
const PUBLISHED_CORES: usize = 2;

impl OwnerState {
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
        self.published.retain(|p| p.key.pin != published.key.pin);
        self.published.insert(0, published);
        if self.published.len() > PUBLISHED_CORES {
            let head = self.head;
            let evicted = self.published.iter().rposition(|p| Some(p.key.pin) != head);
            self.published.remove(evicted.unwrap_or(PUBLISHED_CORES));
        }
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

pub(crate) struct ArchitectureOwner {
    state: Mutex<OwnerState>,
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
}

impl ArchitectureOwner {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
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
        let rules = match crate::rules::load(std::path::Path::new(self.store.workspace_root())) {
            Ok(config) => config.map(|config| config.content_hash),
            Err(_) => Some("unreadable".into()),
        };
        let seen = Some((head, rules));
        // The head is recorded and its turn submitted under one lock, so the settled wait
        // never sees the head stepped with no turn due.
        let mut state = self.architecture.0.state.lock().unwrap();
        if state.stepped == seen {
            return;
        }
        state.stepped = seen;
        self.hint_locked(&mut state);
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
                state.ask();
                self.start_architecture_owner(&mut state);
                owner.changed.notify_all();
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            // The head moved past this pin, so no build is coming: the caller reads again.
            if remaining.is_zero() || state.passed(pin) {
                return Err(ArchitectureBuilding { pin, progress }.into());
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
        let Some(lease) = self.background_lease() else {
            return;
        };
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
        // A rules file that does not load answers its own reads, and references and metrics
        // do not depend on it: a core of this revision is enough.
        let loaded = crate::rules::load(std::path::Path::new(self.store.workspace_root()));
        let unreadable = loaded.is_err();
        let config = loaded.ok().flatten();
        let head_key = CoreKey {
            pin: head,
            rules_hash: config.as_ref().map(|config| config.content_hash.clone()),
        };
        let key = {
            let mut state = owner.state.lock().unwrap();
            state.head = Some(head);
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
            let key = if !head_done && owner.wanted(&state) {
                head_key
            } else if let Some(older) = state.older.clone() {
                older
            } else {
                return;
            };
            if state.shutdown {
                return;
            }
            owner.stop.store(false, Ordering::Release);
            owner.head_may_have_moved.store(false, Ordering::Release);
            state.running = Some(Running {
                pin: key.pin,
                rules_hash: key.rules_hash.clone(),
                progress: None,
            });
            key
        };
        let pin = key.pin;
        owner.changed.notify_all();
        owner.hook("architecture_build_started");
        #[cfg(feature = "test-support")]
        owner.builds.fetch_add(1, Ordering::AcqRel);
        let mut check = || {
            anyhow::ensure!(
                !owner.stop.load(Ordering::Acquire),
                "cancelled: architecture build stopped"
            );
            if owner.head_may_have_moved.swap(false, Ordering::AcqRel) && self.head_moved(head) {
                owner.hook("architecture_build_superseded");
                anyhow::bail!("cancelled: architecture build superseded");
            }
            Ok(())
        };
        let progress = |progress: ArchitectureProgress| {
            if let Some(running) = owner.state.lock().unwrap().running.as_mut() {
                running.progress = Some(progress);
            }
            owner.hook(progress.phase);
        };
        // The transaction and lease end with the read: a publication and a retirement
        // wait for reads, and nothing after this one uses SQLite.
        let input = response
            .architecture_input(pin, &mut check, &progress)
            .and_then(|input| response.finish(input));
        drop(response);
        drop(lease);
        let built = input.and_then(|input| {
            ArchitectureCore::compute(
                &self.store,
                input,
                config.as_ref(),
                true,
                &mut check,
                &progress,
            )
        });
        owner.hook("architecture_build_finished");
        let mut state = owner.state.lock().unwrap();
        state.running = None;
        match built {
            Ok(core) => {
                state.retries = 0;
                state.publish(Published {
                    key,
                    outcome: Ok(Arc::new(core)),
                    report_once: false,
                });
            }
            Err(error) if error.to_string().starts_with("cancelled") => {}
            Err(error) if contention(&error) => {
                drop(state);
                owner.retry_later();
                return;
            }
            Err(error) if CoreFailure::repeats(&error) => {
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

    /// Whether a head newer than `pin` is published. Retirement answers no.
    fn head_moved(&self, pin: IndexPin) -> bool {
        self.background_lease().is_some_and(|_lease| {
            self.store
                .evidence_response_daemon_local()
                .and_then(|fresh| fresh.selected_pin(None))
                .is_ok_and(|head| head != pin)
        })
    }

    #[cfg(feature = "test-support")]
    pub fn set_architecture_hook_for_tests(&self, hook: TestHook) {
        *self.architecture.0.test_hook.lock().unwrap() = Some(hook);
    }

    /// Admission leases held now.
    #[cfg(feature = "test-support")]
    pub fn leases_for_tests(&self) -> usize {
        self.leases()
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
    #[cfg(test)]
    pub(crate) fn await_architecture_idle_for_tests(&self) {
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

    fn revisions(state: &OwnerState) -> Vec<u64> {
        state
            .published
            .iter()
            .map(|p| p.key.pin.index_revision)
            .collect()
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
