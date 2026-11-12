//! The checkout's automatic producer re-runs (#660). One `AutoRunOwner` per active checkout
//! runtime watches the published head, and starts an approved, opted-in producer through the
//! daemon's own run path once the head has been quiet. It is not a writer and adds no
//! cross-process state: a run it starts enters the FIFO exactly as a manual run's import does.
//!
//! The owner decides from published state on every state-signal wake (`decide`, pure). It has at
//! most one launch in flight, learns of its end through a tagged outcome the run thread stores
//! before it wakes the owner, and never touches the producer slot, a lease or a process itself:
//! the registry implements `AutoRunRuntime::launch`, the only automatic caller of `start_run`.

use super::approval::{self, Approval, ApprovalRead};
use super::auto_config::{self, AutoConfig, ConfigState};
use super::auto_gate::{self, Gate, Probes};
use super::profiles::{Stub, Target};
use super::run::{AutoCancel, SupersedeToken, test_hook};
use super::state::{RunOrigin, RunRecord, StateDir, now_millis};
use crate::model::IndexPin;
use crate::store::{AutoRunHead, Store};
use anyhow::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// The owner's liveness backstop and the hold after a non-start: the one piece of polling. No
/// signal exists for `.trellis`, which is never captured, and a timeout is never proof.
const BACKSTOP: Duration = Duration::from_secs(60);

// --- Seams ---

/// The checkout runtime as the owner sees it. Implemented by `AutoHost` in
/// `src/daemon/registry.rs`, the only file that may call `start_run`.
pub(crate) trait AutoRunRuntime: Send + Sync + 'static {
    /// The runtime and its resources are still present.
    fn alive(&self) -> bool;
    /// The runtime is `Ready`.
    fn phase_ready(&self) -> bool;
    /// FIFO work or a watcher batch is pending.
    fn intent_pending(&self) -> bool;
    /// Who holds this runtime's producer slot.
    fn active_origin(&self) -> Option<RunOrigin>;
    /// Ask the automatic run carrying `token` to end because the head moved on.
    fn supersede(&self, token: &SupersedeToken) -> AutoCancel;
    fn state_version(&self) -> u64;
    /// Return once the state version differs from `seen`, or at `timeout`.
    fn wait_state_change(&self, seen: u64, timeout: Duration);
    /// Bump the state signal.
    fn wake(&self);
    /// Run `request` to its end. Blocking.
    fn launch(&self, request: LaunchRequest) -> LaunchOutcome;
}

/// What the owner reads: the published head, the configuration, the approvals and the run
/// records. Implemented by `StoreSources`; faked in the unit tests.
pub(crate) trait AutoRunSources: Send + Sync + 'static {
    fn pin(&self) -> Result<IndexPin>;
    /// The head and which of `producers` are due at it.
    fn head(&self, producers: &[&str]) -> Result<AutoRunHead>;
    fn config(&self) -> ConfigState;
    fn approval(&self, producer: &str) -> ApprovalRead;
    fn seed(&self) -> BTreeMap<String, Seed>;
}

pub(crate) trait Clock: Send + Sync {
    fn instant(&self) -> Instant;
    fn epoch_ms(&self) -> u64;
}

pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn instant(&self) -> Instant {
        Instant::now()
    }

    fn epoch_ms(&self) -> u64 {
        now_millis()
    }
}

pub(crate) struct LaunchRequest {
    pub approval: Approval,
    /// The native revision id the decision was made at; the run is refused if the head moved.
    pub expected_revision: String,
    pub token: SupersedeToken,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LaunchOutcome {
    /// The runtime could not take a run: not `Ready`, retiring or its root changed.
    NotReady,
    /// The head moved before the run could start.
    HeadMoved,
    /// Another run holds the slot.
    SlotBusy,
    /// The plan's template no longer matches the approval.
    NeedsApproval,
    Finished {
        run_id: Option<String>,
        outcome: AutoOutcome,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AutoOutcome {
    Imported,
    Superseded,
    Preempted,
    Failed { code: String, basis: String },
}

/// What the newest automatic `run.json` of a producer restores at activation (SD15).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Seed {
    pub run_id: String,
    pub created_at: u64,
    pub failure: Option<Failure>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Failure {
    pub code: String,
    /// The native revision id the failed run was bound to.
    pub basis: String,
}

// --- Intervals ---

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Intervals {
    /// Test-support replacement for every producer's quiet period.
    pub quiet: Option<Duration>,
    /// Test-support replacement for every producer's minimum interval.
    pub min_interval: Option<Duration>,
    /// How long a gated candidate stays deferred.
    pub gate_backstop: Duration,
    pub backstop: Duration,
}

impl Default for Intervals {
    fn default() -> Self {
        Self {
            quiet: None,
            min_interval: None,
            gate_backstop: BACKSTOP,
            backstop: BACKSTOP,
        }
    }
}

impl Intervals {
    /// `TRELLIS_TEST_AUTORUN_INTERVALS` (a test-support build only):
    /// `{"quietMs":..,"minIntervalMs":..,"gateBackstopMs":..,"backstopMs":..}`, every key required.
    pub fn current() -> Self {
        #[cfg(feature = "test-support")]
        if let Ok(raw) = std::env::var("TRELLIS_TEST_AUTORUN_INTERVALS") {
            return Self::from_json(&raw).unwrap_or_default();
        }
        Self::default()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn from_json(raw: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(raw).ok()?;
        let millis = |key: &str| value.get(key)?.as_u64().map(Duration::from_millis);
        Some(Self {
            quiet: Some(millis("quietMs")?),
            min_interval: Some(millis("minIntervalMs")?),
            gate_backstop: millis("gateBackstopMs")?,
            backstop: millis("backstopMs")?,
        })
    }
}

// --- Published status ---

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AutoState {
    Off,
    Idle,
    Scheduled,
    Deferred,
    Running,
    Failed,
    NeedsApproval,
    InvalidConfig,
}

impl AutoState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Idle => "idle",
            Self::Scheduled => "scheduled",
            Self::Deferred => "deferred",
            Self::Running => "running",
            Self::Failed => "failed",
            Self::NeedsApproval => "needsApproval",
            Self::InvalidConfig => "invalidConfig",
        }
    }
}

/// One producer's `autoRun` (§6.2). Times are epoch milliseconds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AutoRunStatus {
    pub enabled: bool,
    pub state: AutoState,
    pub reason: Option<String>,
    pub due_since: Option<u64>,
    pub next_earliest_start: Option<u64>,
    pub last_run_id: Option<String>,
}

impl AutoRunStatus {
    fn bare(enabled: bool, state: AutoState, reason: Option<String>) -> Self {
        Self {
            enabled,
            state,
            reason,
            due_since: None,
            next_earliest_start: None,
            last_run_id: None,
        }
    }
}

/// The statuses the last pass published.
#[derive(Clone, Debug)]
pub(crate) struct Published {
    pub statuses: BTreeMap<String, AutoRunStatus>,
    /// The status of a producer with no entry: `off`, or `invalidConfig` for a refused file.
    pub fallback: AutoRunStatus,
}

impl Default for Published {
    fn default() -> Self {
        Self {
            statuses: BTreeMap::new(),
            fallback: AutoRunStatus::bare(false, AutoState::Off, None),
        }
    }
}

// --- Decision state ---

#[derive(Clone, Debug)]
struct Head {
    native: String,
    /// When the owner first observed this native revision id (SD13).
    seen_at: Instant,
    due: BTreeMap<String, bool>,
}

#[derive(Clone, Debug)]
struct Active {
    producer: String,
    /// The native revision id the run was started for.
    basis: String,
    token: SupersedeToken,
    supersede_requested: bool,
    /// `last_start` before this start, restored if the run never started.
    prev_start: Option<u64>,
    /// `(templateSha256, approvedAt)` of the approval this launch carries. A template refusal
    /// holds back only this pair, never an approval loaded while the launch was in flight.
    approval: (String, u64),
}

/// The next attempt after a non-start waits for a newer signal or the backstop.
#[derive(Clone, Copy, Debug)]
struct Hold {
    version: u64,
    until: Instant,
}

/// A launch's end, tagged with the launch it belongs to.
struct Tagged {
    producer: String,
    token: SupersedeToken,
    outcome: LaunchOutcome,
}

pub(crate) struct OwnerState {
    intervals: Intervals,
    first: bool,
    config: ConfigState,
    approvals: BTreeMap<String, ApprovalRead>,
    /// `(templateSha256, approvedAt)` of an approval whose template no longer matched.
    template_refused: BTreeMap<String, (String, u64)>,
    pin: Option<IndexPin>,
    head: Option<Head>,
    head_dirty: bool,
    due_since: BTreeMap<String, u64>,
    last_start: BTreeMap<String, u64>,
    last_failure: BTreeMap<String, Failure>,
    last_run_id: BTreeMap<String, String>,
    active: Option<Active>,
    hold: Option<Hold>,
    /// A producer a gate held back, and until when (`now` plus the gate backstop).
    deferred: BTreeMap<String, (Gate, Instant)>,
}

impl OwnerState {
    pub(crate) fn seeded(seed: BTreeMap<String, Seed>, now_ms: u64, intervals: Intervals) -> Self {
        let mut state = Self {
            intervals,
            first: true,
            config: ConfigState::Missing,
            approvals: BTreeMap::new(),
            template_refused: BTreeMap::new(),
            pin: None,
            head: None,
            head_dirty: false,
            due_since: BTreeMap::new(),
            last_start: BTreeMap::new(),
            last_failure: BTreeMap::new(),
            last_run_id: BTreeMap::new(),
            active: None,
            hold: None,
            deferred: BTreeMap::new(),
        };
        for (producer, seed) in seed {
            // A clock stepped back must not push the next start into the future.
            state
                .last_start
                .insert(producer.clone(), seed.created_at.min(now_ms));
            state.last_run_id.insert(producer.clone(), seed.run_id);
            if let Some(failure) = seed.failure {
                state.last_failure.insert(producer, failure);
            }
        }
        state
    }

    /// `producer`'s table, with the test-support intervals applied.
    fn cfg(&self, producer: &str) -> Option<AutoConfig> {
        let ConfigState::Valid(tables) = &self.config else {
            return None;
        };
        let mut config = *tables.get(producer)?;
        if let Some(quiet) = self.intervals.quiet {
            config.quiet = quiet;
        }
        if let Some(min_interval) = self.intervals.min_interval {
            config.min_interval = min_interval;
        }
        Some(config)
    }

    fn enabled_set(&self) -> BTreeSet<String> {
        match &self.config {
            ConfigState::Valid(tables) => tables
                .iter()
                .filter(|(_, config)| config.enabled)
                .map(|(name, _)| name.clone())
                .collect(),
            _ => BTreeSet::new(),
        }
    }

    /// An index read is warranted: some producer is enabled, or a run is active.
    fn armed(&self) -> bool {
        self.active.is_some() || !self.enabled_set().is_empty()
    }

    /// The producers the due query covers: the enabled ones and the active run's.
    fn queried_producers(&self) -> Vec<String> {
        let mut names = self.enabled_set();
        names.extend(self.active.iter().map(|active| active.producer.clone()));
        names.into_iter().collect()
    }

    fn reload(&mut self, sources: &dyn AutoRunSources) {
        let before = self.enabled_set();
        self.config = sources.config();
        let enabled = self.enabled_set();
        self.approvals = enabled
            .iter()
            .map(|producer| (producer.clone(), sources.approval(producer)))
            .collect();
        if enabled != before {
            self.head_dirty = true;
        }
    }

    fn set_head(&mut self, head: AutoRunHead, now: Instant) {
        let seen_at = match &self.head {
            Some(known) if known.native == head.native_revision_id => known.seen_at,
            _ => now,
        };
        self.pin = Some(head.pin);
        self.head = Some(Head {
            native: head.native_revision_id,
            seen_at,
            due: head.due,
        });
    }

    /// Remember the producers the pass deferred, so they publish `deferred` until their time.
    fn apply_deferrals(&mut self, deferrals: &[(String, Gate, Instant)]) {
        for (producer, gate, until) in deferrals {
            self.deferred.insert(producer.clone(), (*gate, *until));
        }
    }

    /// Take a launch's end. Only the outcome of the launch in flight counts.
    fn apply(&mut self, done: Tagged, seen: u64, now: Instant) -> bool {
        let matches = self.active.as_ref().is_some_and(|active| {
            active.producer == done.producer && active.token.same(&done.token)
        });
        if !matches {
            return false;
        }
        let Some(active) = self.active.take() else {
            return false;
        };
        let producer = active.producer;
        let restore = |state: &mut Self| match active.prev_start {
            Some(previous) => {
                state.last_start.insert(producer.clone(), previous);
            }
            None => {
                state.last_start.remove(&producer);
            }
        };
        match done.outcome {
            LaunchOutcome::NotReady | LaunchOutcome::SlotBusy | LaunchOutcome::HeadMoved => {
                restore(self);
                self.hold = Some(Hold {
                    version: seen,
                    until: now + self.intervals.backstop,
                });
            }
            LaunchOutcome::NeedsApproval => {
                restore(self);
                self.template_refused
                    .insert(producer.clone(), active.approval);
            }
            LaunchOutcome::Finished { run_id, outcome } => {
                if let Some(run_id) = run_id {
                    self.last_run_id.insert(producer.clone(), run_id);
                }
                match outcome {
                    AutoOutcome::Imported => {
                        self.last_failure.remove(&producer);
                    }
                    AutoOutcome::Superseded | AutoOutcome::Preempted => {}
                    AutoOutcome::Failed { code, basis } => {
                        self.last_failure
                            .insert(producer.clone(), Failure { code, basis });
                    }
                }
            }
        }
        true
    }

    /// Record the dates the pass just published: set on entry, and cleared with the schedule,
    /// including for a producer the pass no longer evaluates (its table or the whole file went).
    fn sync_due_since(&mut self, statuses: &BTreeMap<String, AutoRunStatus>) {
        let kept = |producer: &String| {
            statuses
                .get(producer)
                .is_some_and(|status| status.due_since.is_some())
        };
        self.deferred.retain(|producer, _| kept(producer));
        self.due_since.retain(|producer, _| {
            statuses
                .get(producer)
                .is_some_and(|status| status.due_since.is_some())
        });
        for (producer, status) in statuses {
            if let Some(since) = status.due_since {
                self.due_since.entry(producer.clone()).or_insert(since);
            }
        }
    }
}

/// The live inputs of one pass, sampled from the runtime.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Live {
    pub phase_ready: bool,
    pub intent_pending: bool,
    pub slot: Option<RunOrigin>,
    pub seen: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Action {
    None,
    Start(String),
    Supersede,
}

pub(crate) struct Decision {
    pub statuses: BTreeMap<String, AutoRunStatus>,
    pub fallback: AutoRunStatus,
    pub action: Action,
    /// Gate deferrals made in this pass: (producer, gate, deferred_until).
    pub deferrals: Vec<(String, Gate, Instant)>,
    /// The soonest future, time-gated instant that changes the decision.
    pub next_deadline: Option<Instant>,
}

/// A producer waiting out its quiet period and minimum interval.
struct Waiting {
    due_since: u64,
    producer: String,
    quiet_at: Instant,
    interval_at_ms: u64,
}

/// `instant_of(ms)`: the instant epoch millisecond `ms` falls on, or `now` once it has passed.
fn instant_of(ms: u64, now: Instant, now_ms: u64) -> Instant {
    now + Duration::from_millis(ms.saturating_sub(now_ms))
}

/// The epoch millisecond of the future instant `at`.
fn epoch_of(at: Instant, now: Instant, now_ms: u64) -> u64 {
    now_ms.saturating_add(at.saturating_duration_since(now).as_millis() as u64)
}

/// What the owner should do now, from published state only. Runs on every wake.
///
/// `gate` names the gate that holds a producer back, if any. It is asked only of a candidate
/// that could start now, in `(dueSince, producer)` order.
pub(crate) fn decide(
    s: &OwnerState,
    live: &Live,
    now: Instant,
    now_ms: u64,
    gate: &mut dyn FnMut(&str) -> Option<Gate>,
) -> Decision {
    let fallback = match &s.config {
        ConfigState::Invalid(reason) => {
            AutoRunStatus::bare(false, AutoState::InvalidConfig, Some((*reason).to_owned()))
        }
        _ => AutoRunStatus::bare(false, AutoState::Off, None),
    };
    let mut names: BTreeSet<&str> = BTreeSet::new();
    if let ConfigState::Valid(tables) = &s.config {
        names.extend(tables.keys().map(String::as_str));
    }
    names.extend(s.active.iter().map(|active| active.producer.as_str()));

    let mut statuses: BTreeMap<String, AutoRunStatus> = BTreeMap::new();
    let mut waiting = Vec::new();
    for name in names {
        let status = evaluate(s, name, live, now, now_ms, &mut waiting);
        statuses.insert(name.to_owned(), status);
    }

    let hold_active = s
        .hold
        .is_some_and(|hold| hold.version == live.seen && now < hold.until);
    let mut next_deadline: Option<Instant> = None;
    let mut soonest = |at: Instant| {
        if at > now {
            next_deadline = Some(next_deadline.map_or(at, |known| known.min(at)));
        }
    };
    waiting.sort_by(|a, b| (a.due_since, &a.producer).cmp(&(b.due_since, &b.producer)));
    let mut ready = Vec::new();
    for entry in &waiting {
        let passed = now >= entry.quiet_at && now_ms >= entry.interval_at_ms;
        if passed {
            ready.push(entry.producer.clone());
        } else {
            soonest(
                entry
                    .quiet_at
                    .max(instant_of(entry.interval_at_ms, now, now_ms)),
            );
        }
    }
    if let Some(hold) = s.hold
        && hold_active
    {
        soonest(hold.until);
    }

    let can_start = s.active.is_none()
        && live.phase_ready
        && live.slot.is_none()
        && !live.intent_pending
        && !hold_active;
    let mut deferrals = Vec::new();
    let mut start = None;
    if can_start {
        for producer in ready {
            match gate(&producer) {
                Some(gated) => {
                    deferrals.push((producer, gated, now + s.intervals.gate_backstop));
                }
                None => {
                    start = Some(producer);
                    break;
                }
            }
        }
    }
    for (producer, gated, until) in &deferrals {
        if let Some(status) = statuses.get_mut(producer) {
            status.state = AutoState::Deferred;
            status.reason = Some(gated.reason().to_owned());
            status.next_earliest_start = Some(epoch_of(*until, now, now_ms));
        }
        soonest(*until);
    }
    for (producer, (_, until)) in &s.deferred {
        if statuses
            .get(producer)
            .is_some_and(|status| status.state == AutoState::Deferred)
        {
            soonest(*until);
        }
    }
    let action = if let Some(first) = start {
        Action::Start(first)
    } else if let (Some(active), Some(head)) = (&s.active, &s.head)
        && !active.supersede_requested
        && head.native != active.basis
    {
        Action::Supersede
    } else {
        Action::None
    };
    Decision {
        statuses,
        fallback,
        action,
        deferrals,
        next_deadline,
    }
}

fn evaluate(
    s: &OwnerState,
    producer: &str,
    live: &Live,
    now: Instant,
    now_ms: u64,
    waiting: &mut Vec<Waiting>,
) -> AutoRunStatus {
    let config = s.cfg(producer);
    let enabled = config.is_some_and(|config| config.enabled);
    let kept = s.due_since.get(producer).copied();
    let last_run_id = s.last_run_id.get(producer).cloned();
    let status = |state, reason: Option<String>| AutoRunStatus {
        last_run_id: last_run_id.clone(),
        ..AutoRunStatus::bare(enabled, state, reason)
    };

    if s.active
        .as_ref()
        .is_some_and(|active| active.producer == producer)
    {
        return AutoRunStatus {
            due_since: kept,
            ..status(AutoState::Running, None)
        };
    }
    let Some(config) = config.filter(|config| config.enabled) else {
        return status(AutoState::Off, None);
    };
    let approval = s.approvals.get(producer);
    let refused = match approval {
        Some(ApprovalRead::Valid(approval)) => {
            s.template_refused.get(producer).is_some_and(|(sha, at)| {
                *sha == approval.template_sha256 && *at == approval.approved_at
            })
        }
        _ => false,
    };
    if refused {
        return status(
            AutoState::NeedsApproval,
            Some("approval template changed".to_owned()),
        );
    }
    match approval {
        Some(ApprovalRead::Valid(_)) => {}
        Some(other) => {
            return status(AutoState::NeedsApproval, other.reason().map(str::to_owned));
        }
        None => {
            return status(
                AutoState::NeedsApproval,
                ApprovalRead::Absent.reason().map(str::to_owned),
            );
        }
    }
    if !live.phase_ready {
        return status(AutoState::Idle, Some("runtime not ready".to_owned()));
    }
    let Some(head) = s
        .head
        .as_ref()
        .filter(|head| head.due.get(producer) == Some(&true))
    else {
        return status(AutoState::Idle, None);
    };
    let since = kept.unwrap_or(now_ms);
    let due_since = Some(since);
    if let Some(failure) = s.last_failure.get(producer)
        && failure.basis == head.native
    {
        return AutoRunStatus {
            due_since,
            ..status(
                AutoState::Failed,
                Some(format!("automatic re-run failed: {}", failure.code)),
            )
        };
    }
    if let Some((gate, until)) = s.deferred.get(producer)
        && *until > now
    {
        return AutoRunStatus {
            due_since,
            next_earliest_start: Some(epoch_of(*until, now, now_ms)),
            ..status(AutoState::Deferred, Some(gate.reason().to_owned()))
        };
    }
    let quiet_at = head.seen_at + config.quiet;
    let interval_at_ms = s.last_start.get(producer).map_or(0, |start| {
        start.saturating_add(config.min_interval.as_millis() as u64)
    });
    let quiet_ms =
        now_ms.saturating_add(quiet_at.saturating_duration_since(now).as_millis() as u64);
    waiting.push(Waiting {
        due_since: since,
        producer: producer.to_owned(),
        quiet_at,
        interval_at_ms,
    });
    AutoRunStatus {
        due_since,
        next_earliest_start: Some(quiet_ms.max(interval_at_ms)),
        ..status(
            AutoState::Scheduled,
            Some(crate::store::RERUN_SCHEDULED.to_owned()),
        )
    }
}

// --- Failure classification ---

/// SD8: the code of `automatic re-run failed: <code>`: the text up to its first `:`, when that
/// is 1 to 64 bytes of `[a-z_]`; otherwise `internal_error`.
pub(crate) fn failure_code(text: &str) -> String {
    let code = text.split(':').next().unwrap_or_default();
    if (1..=64).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
        code.to_owned()
    } else {
        "internal_error".to_owned()
    }
}

/// SD6: whether a terminal run that ended `outcome` / `error_code` backs off the next attempt.
/// Imports, supersession, preemption, root loss and abandonment do not.
pub(crate) fn is_failure(outcome: Option<&str>, error_code: Option<&str>) -> bool {
    let code = error_code.or(outcome).unwrap_or_default();
    let class = code.split(':').next().unwrap_or_default();
    !(outcome == Some("imported")
        || matches!(code, "cancelled: superseded" | "cancelled: preempted")
        || matches!(class, "abandoned" | "root_changed")
        || matches!(outcome, Some("abandoned" | "root_changed")))
}

/// The seed of each producer: its newest automatic run record (§6.6). `runs` is newest first.
pub(crate) fn seed_from(runs: &[RunRecord]) -> BTreeMap<String, Seed> {
    let mut seeds = BTreeMap::new();
    for record in runs {
        if record.origin != RunOrigin::Auto || seeds.contains_key(&record.producer) {
            continue;
        }
        let failure = (record.state.is_terminal()
            && is_failure(record.outcome.as_deref(), record.error_code.as_deref()))
        .then(|| Failure {
            code: failure_code(
                record
                    .error_code
                    .as_deref()
                    .or(record.outcome.as_deref())
                    .unwrap_or_default(),
            ),
            basis: record.source_revision.clone(),
        });
        seeds.insert(
            record.producer.clone(),
            Seed {
                run_id: record.run_id.clone(),
                created_at: record.created_at,
                failure,
            },
        );
    }
    seeds
}

// --- Production sources ---

pub(crate) struct StoreSources {
    store: Store,
    stub: Option<Stub>,
}

impl StoreSources {
    pub(crate) fn new(store: Store, stub: Option<Stub>) -> Self {
        Self { store, stub }
    }

    fn target(&self, producer: &str) -> Option<Target> {
        Target::parse(producer, self.stub.is_some())
    }
}

impl AutoRunSources for StoreSources {
    fn pin(&self) -> Result<IndexPin> {
        self.store.index_baseline()
    }

    fn head(&self, producers: &[&str]) -> Result<AutoRunHead> {
        let named: Vec<(&str, &[&str])> = producers
            .iter()
            .filter_map(|name| Some((*name, self.target(name)?.languages())))
            .collect();
        self.store.auto_run_due(&named)
    }

    fn config(&self) -> ConfigState {
        auto_config::load(std::path::Path::new(self.store.workspace_root()), |name| {
            self.target(name).is_some()
        })
    }

    fn approval(&self, producer: &str) -> ApprovalRead {
        let (roots, identity) = self.store.state_topology();
        match StateDir::existing(roots, identity) {
            Ok(Some(state)) => approval::read(&state, producer, identity),
            Ok(None) => ApprovalRead::Absent,
            Err(_) => ApprovalRead::Unreadable,
        }
    }

    fn seed(&self) -> BTreeMap<String, Seed> {
        let (roots, identity) = self.store.state_topology();
        match StateDir::existing(roots, identity).map(|state| state.map(|state| state.runs())) {
            Ok(Some(Ok(runs))) => seed_from(&runs),
            _ => BTreeMap::new(),
        }
    }
}

// --- The owner ---

/// Starts one thread to run `job`; a failure to start is reported, not panicked on.
pub(crate) type Spawner =
    Arc<dyn Fn(Box<dyn FnOnce() + Send>) -> std::io::Result<()> + Send + Sync>;

pub(crate) fn thread_spawner() -> Spawner {
    Arc::new(|job| {
        std::thread::Builder::new()
            .name("trellis-autorun-run".to_owned())
            .spawn(job)
            .map(drop)
    })
}

pub(crate) struct Deps {
    pub runtime: Arc<dyn AutoRunRuntime>,
    pub sources: Arc<dyn AutoRunSources>,
    pub clock: Arc<dyn Clock>,
    pub probes: Arc<dyn Probes>,
    pub intervals: Intervals,
    pub spawn: Spawner,
    pub closed: Arc<AtomicBool>,
    pub published: Arc<Mutex<Published>>,
}

/// The one automatic re-run owner of a checkout runtime.
#[derive(Default)]
pub struct AutoRunOwner {
    closed: Arc<AtomicBool>,
    published: Arc<Mutex<Published>>,
    runtime: Mutex<Option<Arc<dyn AutoRunRuntime>>>,
}

impl AutoRunOwner {
    /// Start the decider thread. Called once, when the runtime is activated.
    pub(crate) fn start(&self, runtime: Arc<dyn AutoRunRuntime>, sources: Arc<dyn AutoRunSources>) {
        *self.runtime.lock().unwrap_or_else(PoisonError::into_inner) = Some(runtime.clone());
        let deps = Deps {
            runtime,
            sources,
            clock: Arc::new(SystemClock),
            probes: auto_gate::current(),
            intervals: Intervals::current(),
            spawn: thread_spawner(),
            closed: self.closed.clone(),
            published: self.published.clone(),
        };
        let started = std::thread::Builder::new()
            .name("trellis-autorun".to_owned())
            .spawn(move || run_loop(deps));
        if started.is_err() {
            let _ = writeln!(std::io::stderr(), "trellis: automatic re-runs unavailable");
        }
    }

    /// Stop the decider. Never blocks or joins: it exits at its next wake, which this causes.
    pub fn shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        let runtime = self
            .runtime
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(runtime) = runtime {
            runtime.wake();
        }
    }

    /// The runtime labels for the read path (#660), from the same publication cell as `status`:
    /// the languages of every producer scheduled or running, and of every one that failed.
    pub(crate) fn labels(&self) -> crate::store::AutoRunLabels {
        let published = self
            .published
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let stub = crate::producer::profiles::Stub::from_env().is_some();
        let mut labels = crate::store::AutoRunLabels::default();
        for (producer, status) in &published.statuses {
            let Some(target) = Target::parse(producer, stub) else {
                continue;
            };
            for language in target.languages() {
                match (status.state, &status.reason) {
                    (AutoState::Scheduled | AutoState::Running, _) => {
                        labels.catching_up.insert((*language).to_owned());
                    }
                    (AutoState::Failed, Some(reason)) => {
                        labels.failed.insert((*language).to_owned(), reason.clone());
                    }
                    _ => {}
                }
            }
        }
        labels
    }

    pub(crate) fn status(&self, producer: &str) -> AutoRunStatus {
        let published = self
            .published
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        published
            .statuses
            .get(producer)
            .unwrap_or(&published.fallback)
            .clone()
    }
}

/// The launch's end, handed from the run thread to the decider.
#[derive(Default)]
struct RunCell(Mutex<Option<Tagged>>);

impl RunCell {
    fn put(&self, done: Tagged) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(done);
    }

    fn take(&self) -> Option<Tagged> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

fn publish(deps: &Deps, s: &mut OwnerState, decision: &Decision) {
    s.sync_due_since(&decision.statuses);
    let mut published = deps
        .published
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if published.statuses != decision.statuses || published.fallback != decision.fallback {
        published.statuses = decision.statuses.clone();
        published.fallback = decision.fallback.clone();
    }
}

/// `decide` with the host's gates, and the deferrals it made remembered.
fn decide_gated(
    s: &mut OwnerState,
    probes: &Arc<dyn Probes>,
    live: &Live,
    now: Instant,
    now_ms: u64,
) -> Decision {
    let decision = decide(s, live, now, now_ms, &mut |producer| {
        auto_gate::check(&s.cfg(producer)?, &**probes)
    });
    s.apply_deferrals(&decision.deferrals);
    decision
}

/// The decider (§6.5). No lock is held across a runtime or source call.
pub(crate) fn run_loop(deps: Deps) {
    let Deps {
        runtime,
        sources,
        clock,
        probes,
        intervals,
        ..
    } = &deps;
    let cell = Arc::new(RunCell::default());
    let mut s = OwnerState::seeded(sources.seed(), clock.epoch_ms(), *intervals);
    let mut backstop_at = clock.instant();
    let mut superseded_once = false;
    loop {
        // Read before the closed check: a shutdown between the two bumps a newer version.
        let seen = runtime.state_version();
        if deps.closed.load(Ordering::Acquire) || !runtime.alive() {
            return;
        }
        let (now, now_ms) = (clock.instant(), clock.epoch_ms());
        let live = Live {
            phase_ready: runtime.phase_ready(),
            intent_pending: runtime.intent_pending(),
            slot: runtime.active_origin(),
            seen,
        };
        let taken = cell.take().is_some_and(|done| s.apply(done, seen, now));

        let armed = s.armed();
        let pin_changed = armed && matches!(sources.pin(), Ok(pin) if Some(pin) != s.pin);
        if s.first || taken || pin_changed || now >= backstop_at {
            s.reload(&**sources);
            if now >= backstop_at {
                backstop_at = now + intervals.backstop;
            }
            s.first = false;
        }
        if s.armed() && (pin_changed || s.head_dirty) {
            match sources.head(
                &s.queried_producers()
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            ) {
                Ok(head) => {
                    s.set_head(head, now);
                    s.head_dirty = false;
                }
                Err(_) => s.head_dirty = true,
            }
        }

        let mut decision = decide_gated(&mut s, probes, &live, now, now_ms);
        publish(&deps, &mut s, &decision);
        match decision.action.clone() {
            Action::Start(producer) => {
                // The owning transition: config and approval are re-read, and what that decides
                // replaces the first decision.
                s.reload(&**sources);
                let recheck = decide_gated(&mut s, probes, &live, now, now_ms);
                publish(&deps, &mut s, &recheck);
                if recheck.action != Action::Start(producer.clone()) {
                    continue;
                }
                if !spawn_run(&deps, &mut s, &cell, &producer, now_ms) {
                    continue;
                }
                decision = decide_gated(&mut s, probes, &live, now, now_ms);
                publish(&deps, &mut s, &decision);
            }
            Action::Supersede => {
                if let Some(active) = s.active.as_mut() {
                    let result = runtime.supersede(&active.token);
                    if matches!(result, AutoCancel::Requested | AutoCancel::NotCancellable) {
                        active.supersede_requested = true;
                    }
                    if !superseded_once {
                        superseded_once = true;
                        test_hook("TRELLIS_TEST_AUTORUN_SUPERSEDE_SOCKET");
                    }
                }
            }
            Action::None => {}
        }

        let until = decision
            .next_deadline
            .map_or(backstop_at, |at| at.min(backstop_at));
        let timeout = until.saturating_duration_since(clock.instant().max(now));
        if !timeout.is_zero() {
            runtime.wait_state_change(seen, timeout);
        }
    }
}

/// Start the run thread for `producer`. `false`: it could not start, and the pass re-runs at
/// once instead of waiting on a deadline computed before the spawn.
fn spawn_run(
    deps: &Deps,
    s: &mut OwnerState,
    cell: &Arc<RunCell>,
    producer: &str,
    now_ms: u64,
) -> bool {
    let (Some(head), Some(ApprovalRead::Valid(approval))) =
        (s.head.as_ref(), s.approvals.get(producer))
    else {
        return false;
    };
    let basis = head.native.clone();
    let token = SupersedeToken::default();
    let request = LaunchRequest {
        approval: (**approval).clone(),
        expected_revision: basis.clone(),
        token: token.clone(),
    };
    let prev_start = s.last_start.insert(producer.to_owned(), now_ms);
    s.deferred.remove(producer);
    s.active = Some(Active {
        producer: producer.to_owned(),
        basis: basis.clone(),
        token: token.clone(),
        supersede_requested: false,
        prev_start,
        approval: (approval.template_sha256.clone(), approval.approved_at),
    });
    let job: Box<dyn FnOnce() + Send> = {
        let runtime = deps.runtime.clone();
        let cell = cell.clone();
        let producer = producer.to_owned();
        Box::new(move || {
            let outcome = runtime.launch(request);
            cell.put(Tagged {
                producer,
                token,
                outcome,
            });
            runtime.wake();
        })
    };
    if (deps.spawn)(job).is_err() {
        s.active = None;
        s.last_failure.insert(
            producer.to_owned(),
            Failure {
                code: "internal_error".to_owned(),
                basis,
            },
        );
        return false;
    }
    true
}

#[cfg(test)]
#[path = "auto_tests.rs"]
mod tests;
