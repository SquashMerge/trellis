//! The triggers of an owner-configured pickup (D6): a non-recursive `notify` watch on the
//! artifact's parent directory, a debounce, a stability window, one check after activation
//! and a slow stat check. A trigger only asks the host to import; it never starts a producer,
//! so this module reaches nothing beyond `state` and `pickup`.
//!
//! Watcher events are hints. The stat check is the liveness backstop: it covers a watch that
//! could not be armed, a dropped event and a directory that appeared later.

use super::state::PickupConfig;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError, Sender, channel},
    },
    time::{Duration, Instant},
};

/// How often an idle watch thread looks at its runtime. The runtime's own state signal and
/// this thread's event channel are different primitives, so a closing runtime is observed by
/// this bounded poll; it only bounds how long the thread outlives its runtime.
const IDLE_TICK: Duration = Duration::from_millis(250);
/// The tick while any entry is debouncing or stabilizing.
const BUSY_TICK: Duration = Duration::from_millis(50);

/// The four trigger intervals. `TRELLIS_TEST_PICKUP_INTERVALS` (a JSON object with any of
/// `debounceMs`, `stableMs`, `checkMs` and `retryMs`) overrides them in a test-support build.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Intervals {
    pub debounce: Duration,
    pub stable: Duration,
    pub check: Duration,
    pub retry_ms: u64,
}

impl Default for Intervals {
    fn default() -> Self {
        Self {
            debounce: Duration::from_secs(2),
            stable: Duration::from_secs(1),
            check: Duration::from_secs(60),
            retry_ms: super::pickup::TRANSIENT_RETRY_MILLIS,
        }
    }
}

impl Intervals {
    pub fn current() -> Self {
        #[cfg(feature = "test-support")]
        if let Ok(raw) = std::env::var("TRELLIS_TEST_PICKUP_INTERVALS") {
            return Self::from_json(&raw).unwrap_or_default();
        }
        Self::default()
    }

    #[cfg(any(test, feature = "test-support"))]
    fn from_json(raw: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(raw).ok()?;
        let default = Self::default();
        let millis = |key: &str, fallback: u64| match value.get(key) {
            None => Some(fallback),
            Some(field) => field.as_u64(),
        };
        Some(Self {
            debounce: Duration::from_millis(millis(
                "debounceMs",
                default.debounce.as_millis() as u64,
            )?),
            stable: Duration::from_millis(millis("stableMs", default.stable.as_millis() as u64)?),
            check: Duration::from_millis(millis("checkMs", default.check.as_millis() as u64)?),
            retry_ms: millis("retryMs", default.retry_ms)?,
        })
    }
}

/// `(len, mtime, inode)` of a regular artifact file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stat {
    len: u64,
    mtime_ns: i128,
    ino: u64,
}

impl Stat {
    pub fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::symlink_metadata(path).ok()?;
        meta.is_file().then(|| Self {
            len: meta.len(),
            mtime_ns: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
            ino: meta.ino(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    Idle,
    /// An event or the activation check wants a look at `due`.
    Debouncing {
        due: Instant,
    },
    /// The stat must stay the same until `since + stable`.
    Stabilizing {
        stat: Option<Stat>,
        since: Instant,
    },
}

/// What the owner of a tracker should do next.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Step {
    Wait,
    /// The bytes may have changed: import (the decision table skips unchanged bytes).
    Import,
    /// The stat is the one already examined: re-import only if a failed outcome is due again,
    /// without hashing.
    Retry,
}

/// The debounce, stability and stat-check state of one artifact. Pure: the caller supplies
/// the clock and the stat.
#[derive(Debug)]
pub struct Tracker {
    examined: Option<Stat>,
    stage: Stage,
    next_check: Instant,
}

impl Tracker {
    pub fn new(now: Instant, intervals: &Intervals) -> Self {
        Self {
            examined: None,
            stage: Stage::Idle,
            next_check: now + intervals.check,
        }
    }

    /// A watcher event (a restart of the debounce).
    pub fn event(&mut self, now: Instant, intervals: &Intervals) {
        self.stage = Stage::Debouncing {
            due: now + intervals.debounce,
        };
    }

    /// The check after startup reconciliation: no debounce, the stability window still holds.
    pub fn activation(&mut self, now: Instant) {
        self.examined = None;
        self.stage = Stage::Debouncing { due: now };
    }

    /// An import attempt failed before it could judge the bytes: look again at the next check.
    pub fn forget(&mut self) {
        self.examined = None;
    }

    pub fn is_idle(&self) -> bool {
        self.stage == Stage::Idle
    }

    pub fn step(
        &mut self,
        now: Instant,
        intervals: &Intervals,
        stat: impl FnOnce() -> Option<Stat>,
    ) -> Step {
        match self.stage {
            Stage::Debouncing { due } => {
                if now >= due {
                    self.stage = Stage::Stabilizing {
                        stat: stat(),
                        since: now,
                    };
                }
                Step::Wait
            }
            Stage::Stabilizing { stat: seen, since } => {
                let current = stat();
                if current.is_none() {
                    self.stage = Stage::Idle;
                } else if current != seen {
                    self.stage = Stage::Stabilizing {
                        stat: current,
                        since: now,
                    };
                } else if now.duration_since(since) >= intervals.stable {
                    self.stage = Stage::Idle;
                    self.next_check = now + intervals.check;
                    if current == self.examined {
                        return Step::Retry;
                    }
                    self.examined = current;
                    return Step::Import;
                }
                Step::Wait
            }
            Stage::Idle => {
                if now < self.next_check {
                    return Step::Wait;
                }
                self.next_check = now + intervals.check;
                let current = stat();
                if current.is_none() {
                    Step::Wait
                } else if current == self.examined {
                    Step::Retry
                } else {
                    self.stage = Stage::Stabilizing {
                        stat: current,
                        since: now,
                    };
                    Step::Wait
                }
            }
        }
    }
}

/// How alive the host runtime is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostPhase {
    /// Startup reconciliation has not finished (or a retirement is pending): do nothing yet.
    Starting,
    Ready,
    /// The runtime is closing or gone.
    Stopped,
}

/// What a watch thread asks of its checkout.
pub trait Host: Send + Sync {
    fn phase(&self) -> HostPhase;
    /// Import the producer's configured artifact under the decision table. `Ok` when the
    /// attempt reached a verdict (imported, failed typed or skipped).
    fn import(&self, producer: &str) -> anyhow::Result<()>;
    /// Whether a failed outcome for the producer's already-hashed bytes is due again.
    fn retry_due(&self, producer: &str) -> bool;
    /// Re-read the pickup configuration and `sync` the watch to it (a read that failed at
    /// startup is retried at each stat check).
    fn resync(&self) {}
}

enum Msg {
    Event(String),
}

struct Armed {
    artifact: PathBuf,
    watcher: Option<RecommendedWatcher>,
    tracker: Tracker,
    /// A degraded watch is armed again at this time, once per stat-check interval.
    rearm_at: Instant,
}

impl Armed {
    fn healthy(&self) -> bool {
        self.watcher.is_some()
    }
}

struct Shared {
    tx: Sender<Msg>,
    rx: Mutex<Option<Receiver<Msg>>>,
    entries: Mutex<BTreeMap<String, Armed>>,
    closed: AtomicBool,
}

/// The watches of one checkout, one per configured producer.
#[derive(Clone)]
pub struct PickupWatch {
    shared: Arc<Shared>,
}

impl Default for PickupWatch {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self {
            shared: Arc::new(Shared {
                tx,
                rx: Mutex::new(Some(rx)),
                entries: Mutex::new(BTreeMap::new()),
                closed: AtomicBool::new(false),
            }),
        }
    }
}

fn arm(shared: &Shared, producer: &str, artifact: &Path) -> Option<RecommendedWatcher> {
    let parent = artifact.parent()?;
    let name: OsString = artifact.file_name()?.to_owned();
    let tx = shared.tx.clone();
    let producer = producer.to_owned();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
        let relevant = match &event {
            // A failed watch is a hint too: look at the file.
            Err(_) => true,
            Ok(event) => {
                !matches!(event.kind, EventKind::Access(_))
                    && event
                        .paths
                        .iter()
                        .any(|path| path.file_name() == Some(name.as_os_str()))
            }
        };
        if relevant {
            let _ = tx.send(Msg::Event(producer.clone()));
        }
    })
    .ok()?;
    watcher.watch(parent, RecursiveMode::NonRecursive).ok()?;
    Some(watcher)
}

impl PickupWatch {
    /// Make the armed watches match `config`: arm new entries and moved artifacts, drop the
    /// rest. A watch that cannot be armed leaves the entry degraded to the stat check.
    pub fn sync(&self, config: Option<&PickupConfig>) {
        let mut entries = self.shared.entries.lock().unwrap();
        let wanted: BTreeMap<&str, &str> = config
            .into_iter()
            .flat_map(|config| &config.entries)
            .map(|entry| (entry.producer.as_str(), entry.artifact.as_str()))
            .collect();
        entries.retain(|producer, armed| {
            wanted
                .get(producer.as_str())
                .is_some_and(|artifact| Path::new(artifact) == armed.artifact)
        });
        if self.shared.closed.load(Ordering::Acquire) {
            entries.clear();
            return;
        }
        let intervals = Intervals::current();
        for (producer, artifact) in wanted {
            entries.entry(producer.to_owned()).or_insert_with(|| {
                let artifact = PathBuf::from(artifact);
                Armed {
                    watcher: arm(&self.shared, producer, &artifact),
                    tracker: Tracker::new(Instant::now(), &intervals),
                    rearm_at: Instant::now() + intervals.check,
                    artifact,
                }
            });
        }
    }

    /// `active` while a watcher is armed on the artifact's directory, `degraded` while only
    /// the stat check covers it, `None` when the producer has no pickup.
    pub fn health(&self, producer: &str) -> Option<&'static str> {
        let entries = self.shared.entries.lock().unwrap();
        entries.get(producer).map(|armed| {
            if armed.healthy() {
                "active"
            } else {
                "degraded"
            }
        })
    }

    /// Stop the watch: it drops every watcher and its thread ends.
    pub fn shutdown(&self) {
        self.shared.closed.store(true, Ordering::Release);
        self.shared.entries.lock().unwrap().clear();
    }

    /// Start the one thread that turns events and checks into import requests. At most one
    /// thread runs per watch; a second call does nothing.
    pub fn spawn(&self, host: Arc<dyn Host>) {
        let Some(rx) = self.shared.rx.lock().unwrap().take() else {
            return;
        };
        let shared = self.shared.clone();
        let spawned = std::thread::Builder::new()
            .name("pickup-watch".into())
            .spawn(move || run(&shared, &rx, host.as_ref()));
        if spawned.is_err() {
            self.shutdown();
        }
    }
}

fn run(shared: &Shared, rx: &Receiver<Msg>, host: &dyn Host) {
    let mut activated = false;
    let mut resync_at = Instant::now() + Intervals::current().check;
    loop {
        let busy = shared
            .entries
            .lock()
            .unwrap()
            .values()
            .any(|armed| !armed.tracker.is_idle());
        let message = match rx.recv_timeout(if busy { BUSY_TICK } else { IDLE_TICK }) {
            Ok(message) => Some(message),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let phase = host.phase();
        if shared.closed.load(Ordering::Acquire) || phase == HostPhase::Stopped {
            shared.entries.lock().unwrap().clear();
            return;
        }
        let intervals = Intervals::current();
        let now = Instant::now();
        {
            let mut entries = shared.entries.lock().unwrap();
            if let Some(Msg::Event(producer)) = message
                && let Some(armed) = entries.get_mut(&producer)
            {
                armed.tracker.event(now, &intervals);
            }
            if phase == HostPhase::Ready && !activated {
                activated = true;
                for armed in entries.values_mut() {
                    armed.tracker.activation(now);
                }
            }
        }
        if phase != HostPhase::Ready {
            continue;
        }
        if now >= resync_at {
            resync_at = now + intervals.check;
            host.resync();
        }
        step_all(shared, host, &intervals, now);
    }
}

fn step_all(shared: &Shared, host: &dyn Host, intervals: &Intervals, now: Instant) {
    let due: Vec<(String, Step)> = {
        let mut entries = shared.entries.lock().unwrap();
        entries
            .iter_mut()
            .filter_map(|(producer, armed)| {
                // A watch that failed or errored is retried at each stat check.
                if !armed.healthy() && armed.tracker.is_idle() && now >= armed.rearm_at {
                    armed.rearm_at = now + intervals.check;
                    armed.watcher = arm(shared, producer, &armed.artifact);
                }
                let artifact = armed.artifact.clone();
                match armed.tracker.step(now, intervals, || Stat::of(&artifact)) {
                    Step::Wait => None,
                    step => Some((producer.clone(), step)),
                }
            })
            .collect()
    };
    for (producer, step) in due {
        let attempted = match step {
            Step::Import => host.import(&producer).is_ok(),
            Step::Retry => !host.retry_due(&producer) || host.import(&producer).is_ok(),
            Step::Wait => continue,
        };
        if !attempted && let Some(armed) = shared.entries.lock().unwrap().get_mut(&producer) {
            armed.tracker.forget();
        }
    }
}

#[cfg(test)]
pub(super) fn parse_intervals(raw: &str) -> Option<Intervals> {
    Intervals::from_json(raw)
}
