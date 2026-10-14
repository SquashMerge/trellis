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
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    time::{Duration, Instant},
};

const MAX_DIRTY_PATHS: usize = 256;
const QUIET: Duration = Duration::from_millis(75);
const MAX_WAIT: Duration = Duration::from_millis(250);

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
fn submit(
    sender: &SyncSender<Signal>,
    generation: &AtomicU64,
    overflow: &AtomicBool,
    root: &Path,
    selected: &BTreeSet<PathBuf>,
    executable: Option<&Path>,
    event: notify::Result<Event>,
) {
    if event.as_ref().is_ok_and(|event| {
        read_only(event.kind) || certainly_excluded(event, root, selected, executable)
    }) {
        return;
    }
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
    /// Published copy of `degraded` for lock-free health reads. A watcher
    /// owner shares one flag across every replacement watcher it creates.
    health: Arc<AtomicBool>,
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
            Arc::new(AtomicBool::new(false)),
        )
    }

    /// A native watcher whose health is also published through `health`.
    pub fn publishing(
        root: PathBuf,
        scip_path: Option<PathBuf>,
        manifest_path: Option<PathBuf>,
        health: Arc<AtomicBool>,
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
            Arc::new(AtomicBool::new(false)),
        )
    }

    fn construct(
        root: PathBuf,
        scip_path: Option<PathBuf>,
        manifest_path: Option<PathBuf>,
        native: bool,
        health: Arc<AtomicBool>,
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
        let callback = move |event| {
            submit(
                &callback_sender,
                &callback_generation,
                &callback_overflow,
                &callback_root,
                &callback_selected,
                callback_executable.as_deref(),
                event,
            );
        };
        let watcher = native
            .then(|| {
                notify::recommended_watcher(callback).and_then(|mut watcher| {
                    watcher.watch(&root, RecursiveMode::Recursive)?;
                    Ok(watcher)
                })
            })
            .and_then(Result::ok);
        let degraded = native && watcher.is_none();
        health.store(degraded, Ordering::Release);
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
        }
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
        self.watcher = None;
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
        self.health.store(true, Ordering::Release);
        self.pending.full = true;
        self.urgent = true;
        self.watcher = None;
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
