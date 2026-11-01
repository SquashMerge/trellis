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
use crate::model::IndexPin;
use crate::store::{ArchitectureCore, ArchitectureProgress, CoreFailure};
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
            "retryAfterMs": 250,
        })
    }
}

/// What a read needs from a core: its pin and, for a rule read, this rules file.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CoreKey {
    pin: IndexPin,
    rules_hash: Option<String>,
}

struct Published {
    key: CoreKey,
    outcome: Result<Arc<ArchitectureCore>, CoreFailure>,
    /// An ordinary fault outlasted its retries: report it once, and let the next read retry.
    report_once: bool,
}
impl Published {
    fn answers(&self, pin: IndexPin, rules_hash: Option<&str>) -> bool {
        self.key.pin == pin
            && rules_hash.is_none_or(|hash| self.key.rules_hash.as_deref() == Some(hash))
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
    /// A read asked for a core, so the owner builds even without a rules file.
    demanded: bool,
    shutdown: bool,
    thread_started: bool,
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
    fn answer(&self, pin: IndexPin, rules_hash: Option<&str>) -> Option<&Published> {
        self.published.iter().find(|p| p.answers(pin, rules_hash))
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
        })
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
    /// A coalescing hint after a worker step or a rules edit: no SQLite and no wait.
    pub fn architecture_hint(self: &Arc<Self>) {
        let owner = &self.architecture.0;
        let mut state = owner.state.lock().unwrap();
        if !state.demanded && !state.thread_started && !self.rules_file_present() {
            return;
        }
        owner.head_may_have_moved.store(true, Ordering::Release);
        state.ask();
        self.start_architecture_owner(&mut state);
        drop(state);
        owner.changed.notify_all();
    }

    fn rules_file_present(&self) -> bool {
        std::path::Path::new(self.store.workspace_root())
            .join(crate::rules::RULES_PATH)
            .exists()
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
        rules_hash: Option<&str>,
        wait: Duration,
        check: &mut impl FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<Arc<ArchitectureCore>> {
        self.architecture_outcome(pin, rules_hash, wait, check, true)
    }

    /// Wait for what `architecture_core` would answer, leaving any failure to that read.
    pub(crate) fn architecture_wait(
        self: &Arc<Self>,
        pin: IndexPin,
        rules_hash: Option<&str>,
        wait: Duration,
        check: &mut impl FnMut() -> anyhow::Result<()>,
    ) {
        let _ = self.architecture_outcome(pin, rules_hash, wait, check, false);
    }

    fn architecture_outcome(
        self: &Arc<Self>,
        pin: IndexPin,
        rules_hash: Option<&str>,
        wait: Duration,
        check: &mut impl FnMut() -> anyhow::Result<()>,
        answering: bool,
    ) -> anyhow::Result<Arc<ArchitectureCore>> {
        let owner = &self.architecture.0;
        let deadline = Instant::now() + wait;
        let mut state = owner.state.lock().unwrap();
        state.demanded = true;
        let mut submitted = false;
        let mut waiting = false;
        loop {
            if let Some(published) = state.answer(pin, rules_hash) {
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
                running.pin == pin
                    && rules_hash.is_none_or(|hash| running.rules_hash.as_deref() == Some(hash))
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
        rules_hash: Option<&str>,
    ) {
        let mut state = self.architecture.0.state.lock().unwrap();
        state.older = Some(CoreKey {
            pin,
            rules_hash: rules_hash.map(str::to_owned),
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
            }) = state.answer(pin, None)
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
            let wanted = state.demanded || config.is_some();
            let key = if !head_done && wanted {
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

    /// Only a build's read holds a lease: retirement goes ahead while it computes, and the
    /// owner's next turn never cancels a requested retirement.
    #[test]
    fn retirement_does_not_wait_for_a_build_to_compute() {
        let (_tmp, _store, state, _roots, identity) =
            crate::http::exceptional_recovery_tests::fixture();
        state.worker_step().unwrap();
        std::fs::create_dir_all(identity.root.join(".trellis")).unwrap();
        std::fs::write(
            identity.root.join(crate::rules::RULES_PATH),
            "version=1\n[layers.app]\norder=1\npaths=['**/*.js']",
        )
        .unwrap();
        let (entered, reached) = mpsc::channel();
        let (release, held) = mpsc::channel::<()>();
        let held = Mutex::new(held);
        state.set_architecture_hook_for_tests(Arc::new(move |point| {
            if point == "resolving" {
                let _ = entered.send(());
                let _ = held
                    .lock()
                    .unwrap()
                    .recv_timeout(crate::testing::HANG_BACKSTOP);
            }
        }));
        state.architecture_hint();
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
