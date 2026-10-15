//! Advisory, bounded filesystem signals for the verified leader's native work stream.
//! A signal never proves a snapshot: the leader must capture and verify before clearing it.
use notify::{
    Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
    event::{AccessKind, AccessMode, MetadataKind, ModifyKind},
};
use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    time::{Duration, Instant},
};

const MAX_DIRTY_PATHS: usize = 256;
const QUIET: Duration = Duration::from_millis(75);
const MAX_WAIT: Duration = Duration::from_millis(250);

/// Watcher health shared by every watcher one owner creates, read without a
/// lock. Each watcher reports under its own epoch, so a callback from a
/// replaced watcher can never mark the current one degraded (#138 item 22).
#[derive(Default)]
pub struct WatcherHealth {
    /// `epoch << 1 | degraded`: the epoch check and the health update are one
    /// compare-and-swap, so a report from a replaced watcher can never land
    /// after a new epoch began.
    state: AtomicU64,
    #[cfg(test)]
    after_load: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl std::fmt::Debug for WatcherHealth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WatcherHealth")
            .field("degraded", &self.degraded())
            .finish()
    }
}

impl WatcherHealth {
    pub fn degraded(&self) -> bool {
        self.state.load(Ordering::Acquire) & 1 == 1
    }

    /// Start a new watcher's epoch, healthy; reports from earlier watchers are
    /// ignored from here on.
    fn begin_epoch(&self) -> u64 {
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            let epoch = (current >> 1) + 1;
            match self.state.compare_exchange_weak(
                current,
                epoch << 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return epoch,
                Err(actual) => current = actual,
            }
        }
    }

    /// Report as of `epoch`; ignored once a later epoch has begun.
    fn report(&self, epoch: u64, degraded: bool) {
        let mut current = self.state.load(Ordering::Acquire);
        #[cfg(test)]
        {
            let hook = self.after_load.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        while current >> 1 == epoch {
            match self.state.compare_exchange_weak(
                current,
                epoch << 1 | u64::from(degraded),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    /// What setup found, published after the callback is installed. The epoch
    /// began healthy and setup only adds degradation, so an error the callback
    /// already published is never cleared.
    fn publish_setup(&self, epoch: u64, degraded: bool) {
        if degraded {
            self.report(epoch, true);
        }
    }

    /// The owner dropped its watcher: healthy until a new one reports.
    pub fn reset(&self) {
        self.begin_epoch();
    }
}

/// Wakes the checkout worker. Called after ingress records its generation, so a
/// wake can coalesce but never carries state the worker would not re-read.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// A generation can be cleared only after the corresponding capture was verified and published.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirtyBatch {
    pub generation: u64,
    pub full: bool,
    pub paths: BTreeSet<PathBuf>,
}

struct Signal {
    event: notify::Result<Event>,
    received_at: Instant,
}

// Both the real notify callback and deterministic injected events use this exact ingress.
#[allow(clippy::too_many_arguments)]
fn submit(
    sender: &SyncSender<Signal>,
    generation: &AtomicU64,
    overflow: &AtomicBool,
    waker: &std::sync::OnceLock<Waker>,
    root: &Path,
    selected: &BTreeSet<PathBuf>,
    executable: Option<&Path>,
    event: notify::Result<Event>,
) {
    if relevant(&event, root, selected, executable) {
        accept(sender, generation, overflow, waker, event);
    }
}

/// Whether an event can affect the checkout. This may read file metadata, so
/// a callback runs it before taking its liveness lock.
fn relevant(
    event: &notify::Result<Event>,
    root: &Path,
    selected: &BTreeSet<PathBuf>,
    executable: Option<&Path>,
) -> bool {
    !event.as_ref().is_ok_and(|event| {
        read_only(event.kind) || certainly_excluded(event, root, selected, executable)
    })
}

/// Accept an event: bump the generation, queue it, wake the worker. Only short,
/// non-blocking steps, so a callback may do this under its liveness lock.
fn accept(
    sender: &SyncSender<Signal>,
    generation: &AtomicU64,
    overflow: &AtomicBool,
    waker: &std::sync::OnceLock<Waker>,
    event: notify::Result<Event>,
) {
    generation.fetch_add(1, Ordering::SeqCst);
    if sender
        .try_send(Signal {
            event,
            received_at: Instant::now(),
        })
        .is_err()
    {
        overflow.store(true, Ordering::SeqCst);
    }
    if let Some(wake) = waker.get() {
        wake();
    }
}

/// Discard only paths inside a subtree capture always excludes. A disappeared
/// endpoint, symlink ancestor, selected input or running executable is uncertain
/// and must retain its full inventory intent. A mixed rename is never discarded.
fn certainly_excluded(
    event: &Event,
    root: &Path,
    selected: &BTreeSet<PathBuf>,
    executable: Option<&Path>,
) -> bool {
    if event.paths.is_empty() || matches!(event.kind, EventKind::Any | EventKind::Other) {
        return false;
    }
    let Some(executable) = executable else {
        return false;
    };
    let Ok(canonical_executable) = fs::canonicalize(executable) else {
        return false;
    };
    event.paths.iter().all(|path| {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            root.join(path)
        };
        if selected.contains(&absolute) || executable == absolute {
            return false;
        }
        // For an existing selected input, compare both literal and canonical
        // spellings; a failed canonicalization is uncertainty, not exclusion.
        let Ok(canonical) = fs::canonicalize(&absolute) else {
            return false;
        };
        if selected
            .iter()
            .any(|path| fs::canonicalize(path).ok().as_ref() == Some(&canonical))
            || canonical_executable == canonical
        {
            return false;
        }
        let Ok(relative) = absolute.strip_prefix(root) else {
            return false;
        };
        let mut current = root.to_path_buf();
        let mut parts = Vec::new();
        let mut excluded = false;
        for component in relative.components() {
            let Component::Normal(part) = component else {
                return false;
            };
            current.push(part);
            let Ok(meta) = fs::symlink_metadata(&current) else {
                return false;
            };
            if meta.file_type().is_symlink() {
                return false;
            }
            parts.push(part.to_string_lossy().into_owned());
            match part.to_str() {
                Some(".git" | "node_modules" | ".venv" | ".trellis") => excluded = true,
                Some("target" | "dist" | "build") => {
                    // Capture admits Java output subtrees below src/main,
                    // src/test and src/testFixtures/java. Any uncertain shape
                    // stays accepted rather than mirroring ignore internals.
                    let java_output = parts.windows(3).any(|window| {
                        window[0] == "src"
                            && matches!(window[1].as_str(), "main" | "test" | "testFixtures")
                            && window[2] == "java"
                    });
                    if !java_output {
                        excluded = true;
                    }
                }
                _ => {}
            }
        }
        excluded
    })
}

fn read_only(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime))
            | EventKind::Access(AccessKind::Read)
            | EventKind::Access(AccessKind::Open(
                AccessMode::Read | AccessMode::Any | AccessMode::Execute
            ))
            | EventKind::Access(AccessKind::Close(AccessMode::Read | AccessMode::Execute))
    )
}

/// One per active checkout, plus 4 for replacement and stuck teardown.
pub const MAX_NATIVE_WATCHERS: usize = crate::daemon::registry::MAX_ACTIVE_CHECKOUTS + 4;

/// The native-watcher budget (Decision 0009 §§2, 5, as amended 2026-10-15).
/// A slot is reserved before a native watcher is created and freed only when
/// its teardown returns, so a native stop that is lost (notify's macOS
/// FSEvents stop can never return) strands at most the watchers that hold
/// slots. At the limit no new native watcher is created: the checkout reports
/// degraded health and relies on periodic inventory. The daemon uses one
/// process-wide budget; a test uses its own.
pub struct NativeBudget {
    limit: usize,
    /// Native watchers holding a slot: active, or released and tearing down.
    held: AtomicUsize,
    /// Released watchers whose native teardown has not finished.
    stranded: AtomicUsize,
    /// Test-only: reapers wait on this before they drop, to stand in for a
    /// lost stop.
    #[cfg(test)]
    hold_reapers: std::sync::Mutex<Option<ReaperGate>>,
}

impl NativeBudget {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            held: AtomicUsize::new(0),
            stranded: AtomicUsize::new(0),
            #[cfg(test)]
            hold_reapers: std::sync::Mutex::new(None),
        }
    }

    /// The daemon's process-wide budget.
    fn global() -> Arc<Self> {
        static GLOBAL: std::sync::OnceLock<Arc<NativeBudget>> = std::sync::OnceLock::new();
        Arc::clone(GLOBAL.get_or_init(|| Arc::new(NativeBudget::new(MAX_NATIVE_WATCHERS))))
    }

    /// Reserve a slot, if the budget allows one.
    fn reserve(&self) -> bool {
        let mut held = self.held.load(Ordering::Acquire);
        while held < self.limit {
            match self.held.compare_exchange_weak(
                held,
                held + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(current) => held = current,
            }
        }
        false
    }

    fn free(&self) {
        self.held.fetch_sub(1, Ordering::AcqRel);
    }

    /// Native watchers holding a slot: active, or released and tearing down.
    pub fn held(&self) -> usize {
        self.held.load(Ordering::Acquire)
    }

    /// Released watchers whose native teardown has not finished.
    pub fn stranded(&self) -> usize {
        self.stranded.load(Ordering::Acquire)
    }
}

/// Test-only gate a reaper waits on before it drops its watcher.
#[cfg(test)]
type ReaperGate = Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>;

/// A one-shot test hook.
#[cfg(test)]
type TestHook = Arc<std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>>;

/// Hand a native watcher to a reaper. It counts as stranded, and keeps its
/// slot in `budget`, until its drop returns; if no reaper thread can start it
/// is leaked and stays counted.
fn release_watcher(budget: &Arc<NativeBudget>, watcher: Option<RecommendedWatcher>) {
    let Some(watcher) = watcher else {
        return;
    };
    budget.stranded.fetch_add(1, Ordering::AcqRel);
    let slot = Arc::new(std::sync::Mutex::new(Some(watcher)));
    let reaped = Arc::clone(&slot);
    let reaper_budget = Arc::clone(budget);
    #[cfg(test)]
    let hold = budget.hold_reapers.lock().unwrap().clone();
    let spawned = std::thread::Builder::new()
        .name("notify-reaper".into())
        .spawn(move || {
            #[cfg(test)]
            if let Some(hold) = hold {
                let (released, changed) = &*hold;
                let mut released = released.lock().unwrap();
                while !*released {
                    released = changed.wait(released).unwrap();
                }
            }
            drop(reaped.lock().unwrap().take());
            reaper_budget.stranded.fetch_sub(1, Ordering::AcqRel);
            reaper_budget.free();
        });
    if spawned.is_err()
        && let Some(watcher) = slot.lock().unwrap().take()
    {
        std::mem::forget(watcher);
    }
}

impl Drop for WatchSignals {
    fn drop(&mut self) {
        self.release_native();
    }
}

/// Owned by the elected leader, never by a reader or a second durable work queue.
pub struct WatchSignals {
    root: PathBuf,
    captured_inputs: BTreeSet<PathBuf>,
    watcher: Option<RecommendedWatcher>,
    sender: SyncSender<Signal>,
    receiver: Receiver<Signal>,
    generation: Arc<AtomicU64>,
    overflow: Arc<AtomicBool>,
    pending: DirtyBatch,
    executable: Option<PathBuf>,
    acknowledged_generation: u64,
    first_signal: Option<Instant>,
    last_signal: Option<Instant>,
    urgent: bool,
    degraded: bool,
    /// Published copy of `degraded` for lock-free health reads, shared across
    /// every replacement watcher; this watcher reports under `epoch`.
    health: Arc<WatcherHealth>,
    epoch: u64,
    /// Set once by the owning worker; ingress wakes it after each signal.
    waker: Arc<std::sync::OnceLock<Waker>>,
    /// Cleared when the native watcher is released: its callbacks then do
    /// nothing (no health, no events, no wakes).
    live: Arc<std::sync::Mutex<bool>>,
    /// The budget this watcher's native slot belongs to.
    budget: Arc<NativeBudget>,
    /// The native callback, for tests to deliver events directly.
    #[cfg(test)]
    callback: Arc<dyn Fn(notify::Result<Event>) + Send + Sync>,
    /// Test-only: run once in this watcher's callback, under its lock.
    #[cfg(test)]
    pause_callback: TestHook,
    /// Test-only: run once in this watcher's callback, while classifying.
    #[cfg(test)]
    pause_classify: TestHook,
    /// Test-only: run once when a release finds a callback holding the lock.
    #[cfg(test)]
    release_waiting: TestHook,
}

impl WatchSignals {
    /// `root` must already be selected and identity-verified by the leader.
    /// Optional captured SCIP/manifest paths may be ignored by the walker or
    /// outside this root; their paths still demand a full input reconciliation.
    /// The adapter does not elect a leader or retry registration on its own.
    pub fn new(root: PathBuf, scip_path: Option<PathBuf>, manifest_path: Option<PathBuf>) -> Self {
        Self::publishing(
            root,
            scip_path,
            manifest_path,
            Arc::new(WatcherHealth::default()),
        )
    }

    /// A native watcher whose health is also published through `health`.
    pub fn publishing(
        root: PathBuf,
        scip_path: Option<PathBuf>,
        manifest_path: Option<PathBuf>,
        health: Arc<WatcherHealth>,
    ) -> Self {
        Self::construct(root, scip_path, manifest_path, true, health)
    }

    /// Fixture-only ingress with no asynchronous OS backend. Synthetic events
    /// still exercise the production channel, overflow, and acknowledgment path.
    #[doc(hidden)]
    pub fn synthetic_for_tests(
        root: PathBuf,
        scip_path: Option<PathBuf>,
        manifest_path: Option<PathBuf>,
    ) -> Self {
        Self::construct(
            root,
            scip_path,
            manifest_path,
            false,
            Arc::new(WatcherHealth::default()),
        )
    }

    fn construct(
        root: PathBuf,
        scip_path: Option<PathBuf>,
        manifest_path: Option<PathBuf>,
        native: bool,
        health: Arc<WatcherHealth>,
    ) -> Self {
        Self::construct_in(
            root,
            scip_path,
            manifest_path,
            native,
            health,
            NativeBudget::global(),
        )
    }

    fn construct_in(
        root: PathBuf,
        scip_path: Option<PathBuf>,
        manifest_path: Option<PathBuf>,
        native: bool,
        health: Arc<WatcherHealth>,
        budget: Arc<NativeBudget>,
    ) -> Self {
        // Preserve short bursts beyond one bounded drain without treating a
        // repeated path as lost input. Actual channel overflow still forces full.
        let (sender, receiver) = mpsc::sync_channel(MAX_DIRTY_PATHS * 4);
        let generation = Arc::new(AtomicU64::new(1));
        let overflow = Arc::new(AtomicBool::new(false));
        let callback_sender = sender.clone();
        let callback_generation = Arc::clone(&generation);
        let callback_overflow = Arc::clone(&overflow);
        let callback_root = root.clone();
        let callback_selected = [scip_path.clone(), manifest_path.clone()]
            .into_iter()
            .flatten()
            .map(|path| {
                if path.is_absolute() {
                    path
                } else {
                    root.join(path)
                }
            })
            .collect::<BTreeSet<_>>();
        let callback_executable = std::env::current_exe().ok();
        let waker = Arc::new(std::sync::OnceLock::new());
        let callback_waker = Arc::clone(&waker);
        let epoch = health.begin_epoch();
        let callback_health = Arc::clone(&health);
        let live = Arc::new(std::sync::Mutex::new(true));
        let callback_live = Arc::clone(&live);
        #[cfg(test)]
        let pause_callback: TestHook = Default::default();
        #[cfg(test)]
        let callback_pause = Arc::clone(&pause_callback);
        #[cfg(test)]
        let pause_classify: TestHook = Default::default();
        #[cfg(test)]
        let classify_pause = Arc::clone(&pause_classify);
        let callback = Arc::new(move |event: notify::Result<Event>| {
            // Classification may read file metadata: never under the lock.
            #[cfg(test)]
            {
                let pause = classify_pause.lock().unwrap().take();
                if let Some(pause) = pause {
                    pause();
                }
            }
            let relevant = relevant(
                &event,
                &callback_root,
                &callback_selected,
                callback_executable.as_deref(),
            );
            // A released watcher has no authority over the checkout. The
            // lock is held only through the short effects below, so a
            // release waits for a callback already past this check.
            let live = callback_live
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !*live {
                return;
            }
            #[cfg(test)]
            {
                let pause = callback_pause.lock().unwrap().take();
                if let Some(pause) = pause {
                    pause();
                }
            }
            // A watcher error is published at once, not when the worker drains.
            if event.is_err() {
                callback_health.report(epoch, true);
            }
            if relevant {
                accept(
                    &callback_sender,
                    &callback_generation,
                    &callback_overflow,
                    &callback_waker,
                    event,
                );
            }
        });
        // A native watcher first reserves its slot; past the budget none is
        // created and the checkout runs degraded.
        let native_callback = Arc::clone(&callback);
        let watcher = (native && budget.reserve())
            .then(
                || match notify::recommended_watcher(move |event| native_callback(event)) {
                    Ok(mut watcher) => match watcher.watch(&root, RecursiveMode::Recursive) {
                        Ok(()) => Some(watcher),
                        Err(_) => {
                            // A failed setup is torn down off this thread too;
                            // it keeps its slot until that teardown returns.
                            *live
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
                            release_watcher(&budget, Some(watcher));
                            None
                        }
                    },
                    Err(_) => {
                        budget.free();
                        None
                    }
                },
            )
            .flatten();
        let degraded = native && watcher.is_none();
        health.publish_setup(epoch, degraded);
        let captured_inputs = [scip_path, manifest_path]
            .into_iter()
            .flatten()
            .map(|path| {
                if path.is_absolute() {
                    path
                } else {
                    root.join(path)
                }
            })
            .collect();
        Self {
            root,
            captured_inputs,
            watcher,
            sender,
            receiver,
            generation,
            overflow,
            executable: std::env::current_exe().ok(),
            acknowledged_generation: 0,
            pending: DirtyBatch {
                generation: 1,
                full: true, // Wake, restart and takeover require a full inventory.
                paths: BTreeSet::new(),
            },
            first_signal: None,
            last_signal: None,
            urgent: true,
            degraded,
            health,
            epoch,
            waker,
            live,
            budget,
            #[cfg(test)]
            callback,
            #[cfg(test)]
            pause_callback,
            #[cfg(test)]
            pause_classify,
            #[cfg(test)]
            release_waiting: Default::default(),
        }
    }

    /// Release the native watcher: once this returns its callbacks do nothing
    /// (a callback already past its check finishes first), and its teardown
    /// runs on a reaper (Decision 0009 §§2, 5).
    fn release_native(&mut self) {
        let mut live = match self.live.try_lock() {
            Ok(live) => live,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                // A callback is applying its effects: wait for it.
                #[cfg(test)]
                {
                    let hook = self.release_waiting.lock().unwrap().take();
                    if let Some(hook) = hook {
                        hook();
                    }
                }
                self.live
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            }
        };
        *live = false;
        drop(live);
        release_watcher(&self.budget, self.watcher.take());
    }

    /// Wake `waker` after every accepted signal from now on.
    pub fn set_waker(&self, waker: Waker) {
        let _ = self.waker.set(waker);
    }

    /// The live ingress generation, incremented before any wake. A reader
    /// compares it with the acknowledged generation without taking a lock.
    pub fn generation_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.generation)
    }

    pub fn acknowledged_generation(&self) -> u64 {
        self.acknowledged_generation
    }

    /// Ingress intent other than an unacknowledged generation: the initial or
    /// forced full inventory, unless a verified capture already accounted for
    /// this degraded watcher's generation.
    pub fn full_intent(&self) -> bool {
        !self.degraded && (self.pending.full || self.urgent)
    }

    /// This is set synchronously at ingress, before debounce or a channel drain.
    /// The initial full inventory and replacement watcher remain unacknowledged.
    pub fn accepted_unacked(&self) -> bool {
        let unacknowledged = self.generation.load(Ordering::SeqCst) != self.acknowledged_generation;
        // Failure leaves full/urgent/degraded sticky for periodic fallback, but
        // a verified capture already accounted for this degraded generation.
        // Only genuinely new ingress (including require_full) has priority now.
        if self.degraded {
            return unacknowledged;
        }
        self.pending.full || self.urgent || unacknowledged
    }

    pub fn degraded(&self) -> bool {
        self.degraded
    }

    /// Fixture-only: suppress asynchronous OS delivery while preserving the
    /// exact bounded synthetic ingress, generation and acknowledgment logic.
    #[doc(hidden)]
    pub fn disable_native_watcher_for_tests(&mut self) {
        self.release_native();
    }

    pub fn watching(&self) -> bool {
        self.watcher.is_some() && !self.degraded
    }

    /// Wake/restart/failed publication must force a fresh full reconciliation.
    pub fn require_full(&mut self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.pending.full = true;
        self.urgent = true;
    }

    /// Feed the same bounded ingress used by notify's actual callback. A
    /// runtime error or a dropped signal forces a full inventory.
    pub fn submit_event(&self, event: notify::Result<Event>) {
        submit(
            &self.sender,
            &self.generation,
            &self.overflow,
            &self.waker,
            &self.root,
            &self.captured_inputs,
            self.executable.as_deref(),
            event,
        );
    }

    /// Nonblocking, bounded drain: never wait for a continuously active producer.
    /// The caller must drain again before acting on a batch or checking its cutoff.
    pub fn drain(&mut self) -> DirtyBatch {
        for _ in 0..MAX_DIRTY_PATHS {
            match self.receiver.try_recv() {
                Ok(Signal { event, received_at }) => match event {
                    Ok(event) => self.record(&event, received_at),
                    Err(_) => self.fail(),
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !self.degraded {
                        self.fail();
                    }
                    break;
                }
            }
        }
        // A bounded drain may leave queued events. Generation checks during
        // acknowledgment prevent clearing until they are incorporated.
        if self.overflow.swap(false, Ordering::SeqCst) {
            self.pending.full = true;
            self.urgent = true;
        }
        self.pending.generation = self.generation.load(Ordering::SeqCst);
        if self.degraded {
            self.pending.full = true;
            self.urgent = true;
        }
        self.pending.clone()
    }

    /// The single leader stream can use this deadline rather than sleeping in
    /// drain. Continuous edits never delay a batch beyond the first 250 ms.
    pub fn next_deadline(&self) -> Option<Instant> {
        if self.urgent || self.degraded {
            return Some(Instant::now());
        }
        match (self.first_signal, self.last_signal) {
            (Some(first), Some(last)) => Some((last + QUIET).min(first + MAX_WAIT)),
            _ => None,
        }
    }

    pub fn batch_ready_at(&self, now: Instant) -> bool {
        self.urgent || self.degraded || self.next_deadline().is_some_and(|deadline| now >= deadline)
    }

    /// Call only after a verified publication (or verified unchanged capture)
    /// accounts for the batch. A concurrent event leaves the dirty bit set.
    pub fn acknowledge(&mut self, batch: &DirtyBatch) -> bool {
        self.drain();
        if self.pending.generation != batch.generation
            || (self.pending.full && !batch.full)
            || self.generation.load(Ordering::SeqCst) != batch.generation
        {
            return false;
        }
        self.pending.paths.clear();
        self.pending.full = self.degraded;
        self.first_signal = None;
        self.last_signal = None;
        self.urgent = self.degraded;
        // An event racing the clear remains in the channel and invalidates the
        // acknowledged generation; force a full inventory if its timing is uncertain.
        if self.generation.load(Ordering::SeqCst) != batch.generation {
            self.pending.full = true;
            self.urgent = true;
            return false;
        }
        self.acknowledged_generation = batch.generation;
        true
    }

    fn fail(&mut self) {
        self.degraded = true;
        self.health.report(self.epoch, true);
        self.pending.full = true;
        self.urgent = true;
        self.release_native();
    }

    /// Direct feed for the leader's wider event loop. The normal notify callback
    /// goes through `submit`; both paths share event classification.
    pub fn observe_event(&mut self, event: &Event) {
        if read_only(event.kind)
            || certainly_excluded(
                event,
                &self.root,
                &self.captured_inputs,
                self.executable.as_deref(),
            )
        {
            return;
        }
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.record(event, Instant::now());
    }

    fn record(&mut self, event: &Event, received_at: Instant) {
        self.first_signal.get_or_insert(received_at);
        self.last_signal = Some(received_at);
        if event.paths.is_empty() || matches!(event.kind, EventKind::Any | EventKind::Other) {
            self.pending.full = true;
            self.urgent = true;
        }
        // A rename must include both endpoints; an incomplete backend event
        // needs a full walk to discover the missing endpoint.
        if matches!(event.kind, EventKind::Modify(ModifyKind::Name(_))) && event.paths.len() != 2 {
            self.pending.full = true;
        }
        if matches!(
            event.kind,
            EventKind::Create(_)
                | EventKind::Remove(_)
                | EventKind::Access(_)
                | EventKind::Modify(ModifyKind::Metadata(_))
        ) {
            self.pending.full = true;
        }
        for path in &event.paths {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                self.root.join(path)
            };
            if self.captured_inputs.contains(&absolute) {
                self.pending.full = true;
            }
            let Ok(relative) = absolute.strip_prefix(&self.root) else {
                self.pending.full = true;
                continue;
            };
            if relative
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
            {
                self.pending.full = true;
                continue;
            }
            if relevant_input(relative) || !source(relative) || !safe_regular(&self.root, relative)
            {
                self.pending.full = true;
            }
            self.pending.paths.insert(relative.to_path_buf());
            if self.pending.paths.len() > MAX_DIRTY_PATHS {
                self.pending.full = true;
                self.urgent = true;
                self.pending.paths.clear();
            }
        }
    }
}

// Never use extension alone to classify symlinks, disappeared paths or a
// symlinked ancestor as an ordinary source edit. Capture still walks no-follow.
fn safe_regular(root: &Path, relative: &Path) -> bool {
    let mut path = root.to_path_buf();
    for part in relative.components() {
        path.push(part);
        let Ok(meta) = fs::symlink_metadata(&path) else {
            return false;
        };
        if path == root.join(relative) {
            return meta.file_type().is_file();
        }
        if !meta.file_type().is_dir() {
            return false;
        }
    }
    false
}

fn source(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("js" | "mjs" | "cjs" | "rs" | "java" | "py")
    )
}

fn relevant_input(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|e| e.to_str()),
        Some(".gitignore" | ".ignore")
    ) || path.components().count() == 1
        && crate::capture::ROOT_INPUTS
            .iter()
            .any(|name| path == Path::new(name))
}
#[cfg(test)]
mod ingress_tests {
    use super::*;
    #[test]
    fn running_executable_inside_excluded_target_forces_reconciliation() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        let executable = target.join("trellis");
        let other = target.join("scratch.js");
        fs::write(&executable, "binary").unwrap();
        fs::write(&other, "noise").unwrap();
        let event = |path: PathBuf| {
            Event::new(EventKind::Modify(ModifyKind::Data(
                notify::event::DataChange::Content,
            )))
            .add_path(path)
        };
        assert!(certainly_excluded(
            &event(other),
            root.path(),
            &BTreeSet::new(),
            Some(&executable)
        ));
        assert!(!certainly_excluded(
            &event(executable.clone()),
            root.path(),
            &BTreeSet::new(),
            Some(&executable)
        ));
        let alias = target.join("alias");
        std::os::unix::fs::symlink(&executable, &alias).unwrap();
        assert!(!certainly_excluded(
            &event(alias),
            root.path(),
            &BTreeSet::new(),
            Some(&executable)
        ));
    }
}

#[cfg(test)]
mod health_epoch_tests {
    use super::*;

    /// #138 item 22: a report from a replaced watcher never changes the
    /// current watcher's published health.
    #[test]
    fn a_replaced_watcher_cannot_mark_the_current_one_degraded() {
        let health = WatcherHealth::default();
        let replaced = health.begin_epoch();
        let current = health.begin_epoch();
        health.report(replaced, true);
        assert!(!health.degraded());
        health.report(current, true);
        assert!(health.degraded());
        health.reset();
        assert!(!health.degraded());
        health.report(current, true);
        assert!(!health.degraded(), "a reset ends the current epoch too");
    }

    /// A stale report paused after reading the epoch, resumed after the
    /// replacement began and reported healthy, changes nothing.
    #[test]
    fn a_stale_report_paused_across_replacement_is_dropped() {
        let health = Arc::new(WatcherHealth::default());
        let old = health.begin_epoch();
        let (paused_tx, paused_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel::<()>();
        *health.after_load.lock().unwrap() = Some(Box::new(move || {
            paused_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
        }));
        let stale = {
            let health = Arc::clone(&health);
            std::thread::spawn(move || health.report(old, true))
        };
        paused_rx.recv().unwrap();
        let current = health.begin_epoch();
        health.report(current, false);
        resume_tx.send(()).unwrap();
        stale.join().unwrap();
        assert!(!health.degraded(), "the replaced watcher's error leaked");
    }

    /// An error the callback publishes during setup survives setup.
    #[test]
    fn a_setup_error_from_the_callback_is_not_cleared() {
        let health = WatcherHealth::default();
        let epoch = health.begin_epoch();
        health.report(epoch, true); // the OS callback, mid-setup
        health.publish_setup(epoch, false);
        assert!(health.degraded());
    }
}

#[cfg(test)]
mod reaper_tests {
    use super::*;

    /// A test's own native-watcher budget, with its reapers held until
    /// `open`; nothing it counts is shared with other tests.
    struct Budget(Arc<NativeBudget>);
    impl Budget {
        fn held(limit: usize) -> Self {
            let budget = Arc::new(NativeBudget::new(limit));
            *budget.hold_reapers.lock().unwrap() = Some(Arc::new((
                std::sync::Mutex::new(false),
                std::sync::Condvar::new(),
            )));
            Self(budget)
        }
        fn open(&self) {
            if let Some(gate) = self.0.hold_reapers.lock().unwrap().take() {
                *gate.0.lock().unwrap() = true;
                gate.1.notify_all();
            }
        }
        /// Wait until no native watcher holds a slot in this budget.
        fn drained(&self) {
            let deadline = Instant::now() + crate::testing::HANG_BACKSTOP;
            while self.0.held() != 0 {
                assert!(Instant::now() < deadline, "reapers never finished");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        fn signals(&self, root: &Path, health: &Arc<WatcherHealth>) -> WatchSignals {
            WatchSignals::construct_in(
                root.to_path_buf(),
                None,
                None,
                true,
                Arc::clone(health),
                Arc::clone(&self.0),
            )
        }
    }
    impl Drop for Budget {
        fn drop(&mut self) {
            self.open();
        }
    }

    fn hook(run: impl FnOnce() + Send + 'static) -> Option<Box<dyn FnOnce() + Send>> {
        Some(Box::new(run))
    }

    /// Decision 0009 §§2, 5 (amended): a native watcher holds its slot from
    /// creation until its teardown returns, so stuck teardowns can never
    /// exceed the budget, even across a replacement that creates the new
    /// watcher before releasing the old. At the budget a checkout degrades.
    #[test]
    fn the_native_budget_counts_active_and_stranded_watchers() {
        let temp = tempfile::tempdir().unwrap();
        let budget = Budget::held(3);
        let health = || Arc::new(WatcherHealth::default());
        let first = budget.signals(temp.path(), &health());
        let second = budget.signals(temp.path(), &health());
        assert!(first.watcher.is_some() && second.watcher.is_some());
        // A replacement: the new watcher exists before the old is released.
        let replacement = budget.signals(temp.path(), &health());
        assert!(replacement.watcher.is_some());
        drop(first);
        assert_eq!((budget.0.held(), budget.0.stranded()), (3, 1));
        // The stuck teardown still holds its slot: no fourth native watcher.
        let limited_health = health();
        let limited = budget.signals(temp.path(), &limited_health);
        assert!(
            limited.watcher.is_none(),
            "a native watcher past the budget"
        );
        assert!(limited_health.degraded());
        drop(limited);
        drop(second);
        drop(replacement);
        assert!(budget.0.stranded() <= 3, "more stranded than the budget");
        budget.open();
        budget.drained();
        let resumed = budget.signals(temp.path(), &health());
        assert!(resumed.watcher.is_some());
        drop(resumed);
        budget.drained();
    }

    /// Decision 0009 §5 (amended): a release waits for a callback that is
    /// applying its effects, and afterwards callbacks do nothing. The release
    /// is shown blocked on that callback (its waiting hook runs only when the
    /// lock is held), not inferred from timing.
    #[test]
    fn a_release_waits_for_an_in_flight_callback_and_silences_later_ones() {
        let temp = tempfile::tempdir().unwrap();
        let health = Arc::new(WatcherHealth::default());
        let watched = WatchSignals::construct(
            temp.path().to_path_buf(),
            None,
            None,
            false,
            Arc::clone(&health),
        );
        let wakes = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&wakes);
        watched.set_waker(Arc::new(move || {
            counted.fetch_add(1, Ordering::AcqRel);
        }));
        let callback = Arc::clone(&watched.callback);
        let event = || {
            Ok(
                Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
                    .add_path(temp.path().join("a.js")),
            )
        };
        let (paused_tx, paused) = std::sync::mpsc::sync_channel(1);
        let (resume_tx, resume) = std::sync::mpsc::sync_channel::<()>(1);
        *watched.pause_callback.lock().unwrap() = hook(move || {
            paused_tx.send(()).unwrap();
            let _ = resume.recv();
        });
        let (waiting_tx, waiting) = std::sync::mpsc::sync_channel(1);
        *watched.release_waiting.lock().unwrap() = hook(move || {
            waiting_tx.send(()).unwrap();
        });
        let in_flight = {
            let callback = Arc::clone(&callback);
            let event = event();
            std::thread::spawn(move || callback(event))
        };
        paused
            .recv_timeout(crate::testing::HANG_BACKSTOP)
            .expect("the callback never reached its effects");
        let releasing = std::thread::spawn(move || drop(watched));
        waiting
            .recv_timeout(crate::testing::HANG_BACKSTOP)
            .expect("the release did not wait for the in-flight callback");
        resume_tx.send(()).unwrap();
        in_flight.join().unwrap();
        releasing.join().unwrap();
        assert_eq!(wakes.load(Ordering::Acquire), 1, "the in-flight wake");
        callback(event());
        assert_eq!(
            wakes.load(Ordering::Acquire),
            1,
            "a released watcher's callback woke the worker"
        );
    }

    /// Classification (which may read file metadata) runs outside the lock:
    /// a release is not held up by a callback still classifying, and that
    /// callback then does nothing.
    #[test]
    fn a_release_is_not_held_by_a_callback_still_classifying() {
        let temp = tempfile::tempdir().unwrap();
        let watched = WatchSignals::construct(
            temp.path().to_path_buf(),
            None,
            None,
            false,
            Arc::new(WatcherHealth::default()),
        );
        let wakes = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&wakes);
        watched.set_waker(Arc::new(move || {
            counted.fetch_add(1, Ordering::AcqRel);
        }));
        let callback = Arc::clone(&watched.callback);
        let (paused_tx, paused) = std::sync::mpsc::sync_channel(1);
        let (resume_tx, resume) = std::sync::mpsc::sync_channel::<()>(1);
        *watched.pause_classify.lock().unwrap() = hook(move || {
            paused_tx.send(()).unwrap();
            let _ = resume.recv();
        });
        // Either the release waits on the callback, or it finishes.
        let (outcome_tx, outcome) = std::sync::mpsc::channel();
        let waiting_tx = outcome_tx.clone();
        *watched.release_waiting.lock().unwrap() = hook(move || {
            let _ = waiting_tx.send("waited");
        });
        let classifying = {
            let callback = Arc::clone(&callback);
            let event = Ok(
                Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
                    .add_path(temp.path().join("a.js")),
            );
            std::thread::spawn(move || callback(event))
        };
        paused
            .recv_timeout(crate::testing::HANG_BACKSTOP)
            .expect("the callback never classified");
        let releasing = std::thread::spawn(move || {
            drop(watched);
            let _ = outcome_tx.send("released");
        });
        let first = outcome
            .recv_timeout(crate::testing::HANG_BACKSTOP)
            .expect("the release neither waited nor finished");
        resume_tx.send(()).unwrap();
        classifying.join().unwrap();
        releasing.join().unwrap();
        assert_eq!(first, "released", "the release waited on classification");
        assert_eq!(
            wakes.load(Ordering::Acquire),
            0,
            "a callback classified before release acted after it"
        );
    }

    /// A watcher whose setup fails is torn down by a reaper, never on the
    /// constructing (checkout worker) thread.
    #[test]
    fn a_failed_watcher_setup_is_torn_down_by_a_reaper() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        let budget = Budget::held(MAX_NATIVE_WATCHERS);
        let health = Arc::new(WatcherHealth::default());
        let failed = budget.signals(&missing, &health);
        assert!(failed.watcher.is_none());
        assert!(health.degraded());
        assert_eq!(
            (budget.0.stranded(), budget.0.held()),
            (1, 1),
            "the failed watcher was dropped on the constructing thread"
        );
        drop(failed);
        budget.open();
        budget.drained();
    }
}
