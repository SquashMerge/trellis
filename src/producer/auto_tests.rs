use super::*;
use crate::producer::TrustProfile;
use crate::producer::approval::ApprovalRequest;
use crate::producer::detect::Environment;
use crate::producer::plan::LimitsRequest;
use crate::producer::state::{DaemonIdentity, RunState};
use std::{collections::VecDeque, sync::Condvar};

const NOW_MS: u64 = 1_800_000_000_000;
const ALPHA: &str = "alpha";
const BETA: &str = "beta";

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn pin(revision: u64) -> IndexPin {
    IndexPin {
        index_generation: uuid::Uuid::from_u128(1),
        index_revision: revision,
    }
}

fn approval(producer: &str) -> Approval {
    Approval {
        producer: producer.to_owned(),
        trust_profile: TrustProfile::Trusted,
        request: ApprovalRequest {
            limits: LimitsRequest {
                timeout_secs: 1800,
                memory_mib: 8192,
            },
            executable: None,
            environment: Environment::default(),
        },
        template_sha256: "a".repeat(64),
        root_record_id: "record".into(),
        root_inode: 1,
        approved_at: 7,
    }
}

fn valid(producer: &str) -> ApprovalRead {
    ApprovalRead::Valid(Box::new(approval(producer)))
}

fn config(names: &[&str], quiet: u64, min_interval: u64) -> ConfigState {
    ConfigState::Valid(
        names
            .iter()
            .map(|name| {
                (
                    (*name).to_owned(),
                    AutoConfig {
                        enabled: true,
                        quiet: secs(quiet),
                        min_interval: secs(min_interval),
                        battery_gate: true,
                        load_gate: true,
                    },
                )
            })
            .collect(),
    )
}

fn head(native: &str, revision: u64, names: &[&str]) -> AutoRunHead {
    AutoRunHead {
        pin: pin(revision),
        native_revision_id: native.to_owned(),
        due: names
            .iter()
            .map(|name| ((*name).to_owned(), true))
            .collect(),
    }
}

/// A state with `names` enabled (quiet 10 s, minimum interval 60 s), approved, and due at
/// head `h1`, first seen at `base`.
fn ready(names: &[&str], base: Instant) -> OwnerState {
    let mut state = OwnerState::seeded(BTreeMap::new(), NOW_MS, Intervals::default());
    state.first = false;
    state.config = config(names, 10, 60);
    state.approvals = names
        .iter()
        .map(|name| ((*name).to_owned(), valid(name)))
        .collect();
    state.set_head(head("h1", 1, names), base);
    state
}

fn live() -> Live {
    Live {
        phase_ready: true,
        intent_pending: false,
        slot: None,
        seen: 1,
    }
}

/// Decide `after` seconds past `base`, publishing like the loop does.
fn at(s: &mut OwnerState, live: &Live, base: Instant, after: u64) -> Decision {
    at_gated(s, live, base, after, &mut |_| None)
}

/// Like `at`, with `gate` deciding which producers a gate holds back.
fn at_gated(
    s: &mut OwnerState,
    live: &Live,
    base: Instant,
    after: u64,
    gate: &mut dyn FnMut(&str) -> Option<Gate>,
) -> Decision {
    let decision = decide(s, live, base + secs(after), NOW_MS + after * 1000, gate);
    s.apply_deferrals(&decision.deferrals);
    s.sync_due_since(&decision.statuses);
    decision
}

/// Probes that read nothing: no gate ever holds a run back.
struct NoProbes;

impl Probes for NoProbes {
    fn on_battery(&self) -> Option<bool> {
        None
    }

    fn load_saturated(&self) -> Option<bool> {
        None
    }
}

/// Make `producer` the launch in flight, as `spawn_run` does.
fn activate(s: &mut OwnerState, producer: &str, now_ms: u64) -> SupersedeToken {
    let token = SupersedeToken::default();
    let basis = s.head.as_ref().unwrap().native.clone();
    let prev_start = s.last_start.insert(producer.to_owned(), now_ms);
    s.active = Some(Active {
        producer: producer.to_owned(),
        basis,
        token: token.clone(),
        supersede_requested: false,
        prev_start,
    });
    token
}

fn tagged(producer: &str, token: &SupersedeToken, outcome: LaunchOutcome) -> Tagged {
    Tagged {
        producer: producer.to_owned(),
        token: token.clone(),
        outcome,
    }
}

fn finished(outcome: AutoOutcome) -> LaunchOutcome {
    LaunchOutcome::Finished {
        run_id: Some("run".into()),
        outcome,
    }
}

fn failed(code: &str, basis: &str) -> AutoOutcome {
    AutoOutcome::Failed {
        code: code.into(),
        basis: basis.into(),
    }
}

fn future_or_none(deadline: Option<Instant>, now: Instant) {
    assert!(
        deadline.is_none_or(|at| at > now),
        "{deadline:?} vs {now:?}"
    );
}

// --- decide() ---

#[test]
fn starts_only_after_quiet_period() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA], base);
    let early = at(&mut s, &live(), base, 9);
    assert_eq!(early.action, Action::None);
    assert_eq!(early.statuses[ALPHA].state, AutoState::Scheduled);
    assert_eq!(early.next_deadline, Some(base + secs(10)));
    assert_eq!(
        early.statuses[ALPHA].next_earliest_start,
        Some(NOW_MS + 10_000)
    );
    assert_eq!(
        at(&mut s, &live(), base, 10).action,
        Action::Start(ALPHA.into())
    );
}

#[test]
fn pending_intent_defers_start() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA], base);
    let pending = Live {
        intent_pending: true,
        ..live()
    };
    assert_eq!(at(&mut s, &pending, base, 20).action, Action::None);
    assert_eq!(
        at(&mut s, &live(), base, 20).action,
        Action::Start(ALPHA.into())
    );
}

#[test]
fn min_interval_since_last_auto_start() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA], base);
    s.last_start.insert(ALPHA.into(), NOW_MS - 30_000);
    let early = at(&mut s, &live(), base, 20);
    assert_eq!(early.action, Action::None);
    assert_eq!(
        early.statuses[ALPHA].next_earliest_start,
        Some(NOW_MS + 30_000)
    );
    assert_eq!(early.next_deadline, Some(base + secs(30)));
    assert_eq!(
        at(&mut s, &live(), base, 30).action,
        Action::Start(ALPHA.into())
    );
}

#[test]
fn burst_of_heads_gives_one_start() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA], base);
    for (second, native) in [(2, "h2"), (4, "h3"), (6, "h4")] {
        s.set_head(head(native, second, &[ALPHA]), base + secs(second));
        assert_eq!(at(&mut s, &live(), base, second).action, Action::None);
    }
    // Quiet runs from the last head, not the first.
    assert_eq!(at(&mut s, &live(), base, 15).action, Action::None);
    assert_eq!(
        at(&mut s, &live(), base, 16).action,
        Action::Start(ALPHA.into())
    );
}

#[test]
fn oldest_due_first_one_at_a_time() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA, BETA], base);
    s.due_since.insert(ALPHA.into(), 200);
    s.due_since.insert(BETA.into(), 100);
    assert_eq!(
        at(&mut s, &live(), base, 20).action,
        Action::Start(BETA.into())
    );
    activate(&mut s, BETA, NOW_MS + 20_000);
    let next = at(&mut s, &live(), base, 20);
    assert_eq!(next.action, Action::None);
    assert_eq!(next.statuses[BETA].state, AutoState::Running);
    assert_eq!(next.statuses[ALPHA].state, AutoState::Scheduled);
}

#[test]
fn gated_candidate_defers_and_rechecks_at_backstop() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA], base);
    let mut asked = 0;
    let held = at_gated(&mut s, &live(), base, 20, &mut |_| {
        asked += 1;
        Some(Gate::Load)
    });
    assert_eq!(held.action, Action::None);
    assert_eq!(asked, 1);
    // Inside the backstop the gate is not asked again, and the producer stays deferred.
    let waiting = at_gated(&mut s, &live(), base, 50, &mut |_| {
        asked += 1;
        None
    });
    assert_eq!(waiting.action, Action::None);
    assert_eq!(waiting.statuses[ALPHA].state, AutoState::Deferred);
    assert_eq!(asked, 1);
    // At the backstop it is a candidate again, and a gate that still holds defers it afresh.
    let again = at_gated(&mut s, &live(), base, 80, &mut |_| {
        asked += 1;
        Some(Gate::Battery)
    });
    assert_eq!(asked, 2);
    assert_eq!(again.statuses[ALPHA].state, AutoState::Deferred);
    assert_eq!(
        again.statuses[ALPHA].reason.as_deref(),
        Some("deferred: battery")
    );
    let cleared = at_gated(&mut s, &live(), base, 140, &mut |_| None);
    assert_eq!(cleared.action, Action::Start(ALPHA.into()));
}

#[test]
fn ungated_second_candidate_starts() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA, BETA], base);
    s.due_since.insert(ALPHA.into(), 100);
    s.due_since.insert(BETA.into(), 200);
    let mut asked = Vec::new();
    let decision = at_gated(&mut s, &live(), base, 20, &mut |producer| {
        asked.push(producer.to_owned());
        (producer == ALPHA).then_some(Gate::Battery)
    });
    assert_eq!(asked, [ALPHA, BETA]);
    assert_eq!(decision.action, Action::Start(BETA.into()));
    assert_eq!(decision.deferrals.len(), 1);
    assert_eq!(decision.statuses[ALPHA].state, AutoState::Deferred);
    assert_eq!(decision.statuses[BETA].state, AutoState::Scheduled);
}

#[test]
fn new_deferral_publishes_deferred_with_deadline_same_pass() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA], base);
    let decision = at_gated(&mut s, &live(), base, 20, &mut |_| Some(Gate::Battery));
    let status = &decision.statuses[ALPHA];
    assert_eq!(status.state, AutoState::Deferred);
    assert_eq!(status.reason.as_deref(), Some("deferred: battery"));
    assert_eq!(status.next_earliest_start, Some(NOW_MS + 80_000));
    assert!(status.due_since.is_some());
    assert_eq!(decision.next_deadline, Some(base + secs(80)));
    assert_eq!(
        decision.deferrals,
        [(ALPHA.to_owned(), Gate::Battery, base + secs(80))]
    );
}

#[test]
fn a_blocked_start_asks_no_gate() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA], base);
    let manual = Live {
        slot: Some(RunOrigin::Manual),
        ..live()
    };
    let decision = at_gated(&mut s, &manual, base, 20, &mut |_| {
        panic!("a producer that cannot start is not gated")
    });
    assert!(decision.deferrals.is_empty());
}

#[test]
fn manual_run_active_defers() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA], base);
    let manual = Live {
        slot: Some(RunOrigin::Manual),
        ..live()
    };
    assert_eq!(at(&mut s, &manual, base, 20).action, Action::None);
}

#[test]
fn manual_active_has_no_past_deadline() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA], base);
    let manual = Live {
        slot: Some(RunOrigin::Manual),
        ..live()
    };
    let decision = at(&mut s, &manual, base, 20);
    assert_eq!(decision.next_deadline, None);
}

#[test]
fn blocked_by_intent_has_no_past_deadline() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA], base);
    s.last_start.insert(ALPHA.into(), NOW_MS - 120_000);
    let pending = Live {
        intent_pending: true,
        ..live()
    };
    let decision = at(&mut s, &pending, base, 20);
    assert_eq!(decision.action, Action::None);
    assert_eq!(decision.next_deadline, None);
}

#[test]
fn intent_drain_starts_without_deadline() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA], base);
    let decision = at(&mut s, &live(), base, 20);
    assert_eq!(decision.action, Action::Start(ALPHA.into()));
    assert_eq!(decision.next_deadline, None);
}

#[test]
fn own_run_pre_install_blocks_second_start() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA, BETA], base);
    activate(&mut s, ALPHA, NOW_MS + 20_000);
    // The slot is still empty: the run has not installed, or has just released it.
    let decision = at(&mut s, &live(), base, 20);
    assert_eq!(decision.action, Action::None);
    future_or_none(decision.next_deadline, base + secs(20));
}

#[test]
fn own_run_with_second_due_has_no_past_deadline() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA, BETA], base);
    s.last_start.insert(BETA.into(), NOW_MS - 1000);
    activate(&mut s, ALPHA, NOW_MS + 20_000);
    let decision = at(&mut s, &live(), base, 20);
    future_or_none(decision.next_deadline, base + secs(20));
}

#[test]
fn outcome_for_other_producer_or_token_is_ignored() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA, BETA], base);
    let token = activate(&mut s, ALPHA, NOW_MS + 20_000);
    let outcome = || finished(AutoOutcome::Imported);
    assert!(!s.apply(tagged(BETA, &token, outcome()), 1, base));
    assert!(!s.apply(
        tagged(ALPHA, &SupersedeToken::default(), outcome()),
        1,
        base
    ));
    assert!(s.active.is_some());
    assert!(s.apply(tagged(ALPHA, &token, outcome()), 1, base));
    assert!(s.active.is_none());
    // Nothing is in flight now: a late outcome changes nothing.
    assert!(!s.apply(tagged(ALPHA, &token, outcome()), 1, base));
}

#[test]
fn head_moved_and_slot_busy_do_not_count_as_starts() {
    let base = Instant::now();
    for outcome in [
        LaunchOutcome::HeadMoved,
        LaunchOutcome::SlotBusy,
        LaunchOutcome::NotReady,
    ] {
        let mut s = ready(&[ALPHA], base);
        let token = activate(&mut s, ALPHA, NOW_MS + 20_000);
        assert!(s.apply(tagged(ALPHA, &token, outcome), 3, base));
        assert_eq!(s.last_start.get(ALPHA), None);
        assert!(s.hold.is_some());
    }
    let mut s = ready(&[ALPHA], base);
    s.last_start.insert(ALPHA.into(), NOW_MS - 90_000);
    let token = activate(&mut s, ALPHA, NOW_MS + 20_000);
    assert!(s.apply(tagged(ALPHA, &token, LaunchOutcome::SlotBusy), 3, base));
    assert_eq!(s.last_start[ALPHA], NOW_MS - 90_000);
}

#[test]
fn not_ready_has_no_past_deadline() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA], base);
    let token = activate(&mut s, ALPHA, NOW_MS + 20_000);
    s.apply(
        tagged(ALPHA, &token, LaunchOutcome::NotReady),
        1,
        base + secs(20),
    );
    let held = at(&mut s, &live(), base, 20);
    assert_eq!(held.action, Action::None);
    assert_eq!(held.next_deadline, Some(base + secs(20) + BACKSTOP));
    let not_ready = Live {
        phase_ready: false,
        seen: 2,
        ..live()
    };
    assert_eq!(at(&mut s, &not_ready, base, 21).next_deadline, None);
}

#[test]
fn hold_after_slot_busy_lifts_on_next_signal_or_backstop() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA], base);
    let token = activate(&mut s, ALPHA, NOW_MS + 20_000);
    s.apply(
        tagged(ALPHA, &token, LaunchOutcome::SlotBusy),
        5,
        base + secs(20),
    );
    let same_signal = Live { seen: 5, ..live() };
    assert_eq!(at(&mut s, &same_signal, base, 21).action, Action::None);
    let newer = Live { seen: 6, ..live() };
    assert_eq!(
        at(&mut s, &newer, base, 21).action,
        Action::Start(ALPHA.into())
    );
    let after = 20 + BACKSTOP.as_secs();
    assert_eq!(
        at(&mut s, &same_signal, base, after).action,
        Action::Start(ALPHA.into())
    );
}

#[test]
fn semantic_only_pin_move_is_not_a_head_change() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA], base);
    let seen_at = s.head.as_ref().unwrap().seen_at;
    let token = activate(&mut s, ALPHA, NOW_MS + 20_000);
    s.set_head(head("h1", 2, &[ALPHA]), base + secs(5));
    assert_eq!(s.head.as_ref().unwrap().seen_at, seen_at);
    assert_eq!(s.pin, Some(pin(2)));
    assert_eq!(at(&mut s, &live(), base, 6).action, Action::None);
    s.set_head(head("h2", 3, &[ALPHA]), base + secs(7));
    assert_eq!(at(&mut s, &live(), base, 7).action, Action::Supersede);
    drop(token);
}

#[test]
fn not_ready_drops_schedule() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA], base);
    assert_eq!(
        at(&mut s, &live(), base, 1).statuses[ALPHA].due_since,
        Some(NOW_MS + 1000)
    );
    let not_ready = Live {
        phase_ready: false,
        ..live()
    };
    let idle = at(&mut s, &not_ready, base, 2);
    let status = &idle.statuses[ALPHA];
    assert_eq!(status.state, AutoState::Idle);
    assert_eq!(status.reason.as_deref(), Some("runtime not ready"));
    assert_eq!(status.due_since, None);
    assert_eq!(idle.action, Action::None);
    assert!(!s.due_since.contains_key(ALPHA));
    // Ready again: a new schedule, with a new dueSince.
    assert_eq!(
        at(&mut s, &live(), base, 3).statuses[ALPHA].due_since,
        Some(NOW_MS + 3000)
    );
}

#[test]
fn status_rows_off_invalid_and_needs_approval() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA], base);
    s.approvals.insert(ALPHA.into(), ApprovalRead::Absent);
    let status = at(&mut s, &live(), base, 1).statuses[ALPHA].clone();
    assert_eq!(status.state, AutoState::NeedsApproval);
    assert_eq!(status.reason.as_deref(), Some("no approval record"));
    assert_eq!(at(&mut s, &live(), base, 1).action, Action::None);

    s.approvals.insert(ALPHA.into(), valid(ALPHA));
    s.template_refused.insert(ALPHA.into(), ("a".repeat(64), 7));
    let status = at(&mut s, &live(), base, 1).statuses[ALPHA].clone();
    assert_eq!(status.reason.as_deref(), Some("approval template changed"));
    // A newer manual run rewrote the record: the refusal no longer applies.
    let mut renewed = approval(ALPHA);
    renewed.approved_at = 8;
    s.approvals
        .insert(ALPHA.into(), ApprovalRead::Valid(Box::new(renewed)));
    assert_eq!(
        at(&mut s, &live(), base, 1).statuses[ALPHA].state,
        AutoState::Scheduled
    );

    s.config = ConfigState::Invalid("producers.toml syntax error");
    let decision = at(&mut s, &live(), base, 1);
    assert!(decision.statuses.is_empty());
    assert_eq!(decision.fallback.state, AutoState::InvalidConfig);
    assert_eq!(
        decision.fallback.reason.as_deref(),
        Some("producers.toml syntax error")
    );
    s.config = ConfigState::Missing;
    assert_eq!(at(&mut s, &live(), base, 1).fallback.state, AutoState::Off);
}

/// While a run is active no start is possible, so no gate is consulted for a due producer.
#[test]
fn an_active_run_asks_no_gate() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA, BETA], base);
    activate(&mut s, ALPHA, NOW_MS);
    let decision = at_gated(&mut s, &live(), base, 20, &mut |_| {
        panic!("a gate is never consulted during a run")
    });
    assert!(decision.deferrals.is_empty());
    assert_eq!(decision.action, Action::None);
}

#[test]
fn newer_head_while_running_supersedes_until_requested() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA], base);
    activate(&mut s, ALPHA, NOW_MS);
    assert_eq!(at(&mut s, &live(), base, 1).action, Action::None);
    s.set_head(head("h2", 2, &[ALPHA]), base + secs(2));
    assert_eq!(at(&mut s, &live(), base, 2).action, Action::Supersede);
    s.active.as_mut().unwrap().supersede_requested = true;
    assert_eq!(at(&mut s, &live(), base, 3).action, Action::None);
}

/// A producer that left the evaluated set loses its schedule date: when it returns it is a new
/// entry, not the old one (and not first in line by an old date).
#[test]
fn due_since_is_fresh_after_the_producer_returns() {
    let base = Instant::now();
    type Drop = fn(&mut OwnerState);
    let drops: [(&str, Drop); 3] = [
        ("invalid", |s| {
            s.config = ConfigState::Invalid("producers.toml syntax error")
        }),
        ("missing", |s| s.config = ConfigState::Missing),
        ("table removed", |s| s.config = config(&[BETA], 10, 60)),
    ];
    for (name, drop_producer) in drops {
        let mut s = ready(&[ALPHA, BETA], base);
        let first = at(&mut s, &live(), base, 1);
        assert_eq!(
            first.statuses[ALPHA].due_since,
            Some(NOW_MS + 1000),
            "{name}"
        );
        drop_producer(&mut s);
        let gone = at(&mut s, &live(), base, 2);
        assert!(
            gone.statuses
                .get(ALPHA)
                .is_none_or(|status| status.due_since.is_none()),
            "{name}"
        );
        assert!(!s.due_since.contains_key(ALPHA), "{name}");
        s.config = config(&[ALPHA, BETA], 10, 60);
        let back = at(&mut s, &live(), base, 5);
        assert_eq!(
            back.statuses[ALPHA].due_since,
            Some(NOW_MS + 5000),
            "{name}"
        );
    }
}

// --- failure handling and seeding ---

#[test]
fn failure_blocks_until_head_moves_and_interval_passes() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA], base);
    s.last_start.insert(ALPHA.into(), NOW_MS - 1000);
    s.last_failure.insert(
        ALPHA.into(),
        Failure {
            code: "producer_failed".into(),
            basis: "h1".into(),
        },
    );
    // Same head: failed, however long it has been.
    let stuck = at(&mut s, &live(), base, 3600);
    assert_eq!(stuck.action, Action::None);
    assert_eq!(stuck.statuses[ALPHA].state, AutoState::Failed);
    assert_eq!(
        stuck.statuses[ALPHA].reason.as_deref(),
        Some("automatic re-run failed: producer_failed")
    );
    assert_eq!(stuck.next_deadline, None);
    // A new head ends the failure, but the minimum interval still holds.
    s.set_head(head("h2", 2, &[ALPHA]), base + secs(5));
    assert_eq!(at(&mut s, &live(), base, 30).action, Action::None);
    assert_eq!(at(&mut s, &live(), base, 58).action, Action::None);
    assert_eq!(
        at(&mut s, &live(), base, 59).action,
        Action::Start(ALPHA.into())
    );
}

#[test]
fn superseded_and_preempted_are_not_failures() {
    let base = Instant::now();
    for outcome in [AutoOutcome::Superseded, AutoOutcome::Preempted] {
        let mut s = ready(&[ALPHA], base);
        let token = activate(&mut s, ALPHA, NOW_MS);
        assert!(s.apply(tagged(ALPHA, &token, finished(outcome)), 1, base));
        assert!(s.last_failure.is_empty());
        assert_eq!(s.last_run_id[ALPHA], "run");
        assert_eq!(s.last_start[ALPHA], NOW_MS);
    }
}

#[test]
fn failure_is_cleared_only_by_an_import() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA], base);
    let token = activate(&mut s, ALPHA, NOW_MS);
    s.apply(
        tagged(ALPHA, &token, finished(failed("timed_out", "h1"))),
        1,
        base,
    );
    assert_eq!(s.last_failure[ALPHA].code, "timed_out");
    let token = activate(&mut s, ALPHA, NOW_MS);
    s.apply(
        tagged(ALPHA, &token, finished(AutoOutcome::Superseded)),
        1,
        base,
    );
    assert_eq!(s.last_failure[ALPHA].code, "timed_out");
    let token = activate(&mut s, ALPHA, NOW_MS);
    s.apply(
        tagged(ALPHA, &token, finished(AutoOutcome::Imported)),
        1,
        base,
    );
    assert!(s.last_failure.is_empty());
}

#[test]
fn needs_approval_outcome_records_the_refused_template() {
    let base = Instant::now();
    let mut s = ready(&[ALPHA], base);
    let token = activate(&mut s, ALPHA, NOW_MS);
    s.apply(tagged(ALPHA, &token, LaunchOutcome::NeedsApproval), 1, base);
    assert_eq!(s.template_refused[ALPHA], ("a".repeat(64), 7));
    assert_eq!(s.last_start.get(ALPHA), None);
    let decision = at(&mut s, &live(), base, 30);
    assert_eq!(decision.statuses[ALPHA].state, AutoState::NeedsApproval);
}

#[test]
fn sd8_code_extraction() {
    assert_eq!(failure_code("producer_failed: exit 1"), "producer_failed");
    assert_eq!(failure_code("timed_out"), "timed_out");
    assert_eq!(failure_code("cancelled"), "cancelled");
    for bad in [
        "",
        ": no code",
        "Producer-Failed: x",
        "has space: x",
        "a1: x",
    ] {
        assert_eq!(failure_code(bad), "internal_error", "{bad:?}");
    }
    assert_eq!(failure_code(&"a".repeat(64)), "a".repeat(64));
    assert_eq!(failure_code(&"a".repeat(65)), "internal_error");
}

#[test]
fn failure_outcomes_follow_sd6() {
    for (outcome, code, failure) in [
        ("imported", None, false),
        ("cancelled", Some("cancelled: superseded"), false),
        ("cancelled", Some("cancelled: preempted"), false),
        (
            "root_changed",
            Some("root_changed: captured root lost"),
            false,
        ),
        ("abandoned", Some("abandoned"), false),
        (
            "abandoned",
            Some("abandoned: producer termination unestablished"),
            false,
        ),
        ("cancelled", Some("cancelled"), true),
        ("producer_failed", Some("producer_failed: exit 1"), true),
        ("timed_out", None, true),
        (
            "stale_basis",
            Some("stale_basis: head revision changed"),
            true,
        ),
        ("import_failed", Some("storage_busy: x"), true),
    ] {
        assert_eq!(
            is_failure(Some(outcome), code),
            failure,
            "{outcome} {code:?}"
        );
    }
}

fn record(
    run_id: &str,
    producer: &str,
    origin: RunOrigin,
    created_at: u64,
    state: RunState,
    outcome: Option<&str>,
    error_code: Option<&str>,
) -> RunRecord {
    RunRecord {
        version: 1,
        run_id: run_id.to_owned(),
        producer: producer.to_owned(),
        trust_profile: TrustProfile::Trusted,
        plan_sha256: "0".repeat(64),
        source_revision: format!("rev-{run_id}"),
        state,
        outcome: outcome.map(str::to_owned),
        error_code: error_code.map(str::to_owned),
        daemon: DaemonIdentity {
            pid: 1,
            start_time: 0,
        },
        process: None,
        request_id: None,
        created_at,
        finished_at: None,
        diagnostics: None,
        group_status: None,
        origin,
    }
}

#[test]
fn seed_restores_last_start_failure_and_run_id() {
    let runs = [
        // Newest first. A manual run is never a seed.
        record(
            "m",
            ALPHA,
            RunOrigin::Manual,
            900,
            RunState::Done,
            Some("imported"),
            None,
        ),
        record(
            "a2",
            ALPHA,
            RunOrigin::Auto,
            800,
            RunState::Failed,
            Some("producer_failed"),
            Some("producer_failed: exit 1"),
        ),
        record(
            "a1",
            ALPHA,
            RunOrigin::Auto,
            700,
            RunState::Done,
            Some("imported"),
            None,
        ),
        record(
            "b1",
            BETA,
            RunOrigin::Auto,
            600,
            RunState::Done,
            Some("imported"),
            None,
        ),
    ];
    let seeds = seed_from(&runs);
    assert_eq!(
        seeds[ALPHA],
        Seed {
            run_id: "a2".into(),
            created_at: 800,
            failure: Some(Failure {
                code: "producer_failed".into(),
                basis: "rev-a2".into()
            }),
        }
    );
    assert_eq!(seeds[BETA].failure, None);
    assert_eq!(seeds[BETA].run_id, "b1");

    let state = OwnerState::seeded(seeds, NOW_MS, Intervals::default());
    assert_eq!(state.last_start[ALPHA], 800);
    assert_eq!(state.last_run_id[ALPHA], "a2");
    assert_eq!(state.last_failure[ALPHA].basis, "rev-a2");
    assert!(!state.last_failure.contains_key(BETA));
}

#[test]
fn seed_treats_abandoned_and_superseded_as_not_failures() {
    for (state, outcome, code) in [
        (RunState::Failed, Some("abandoned"), Some("abandoned")),
        (
            RunState::Cancelled,
            Some("cancelled"),
            Some("cancelled: superseded"),
        ),
        (
            RunState::Cancelled,
            Some("cancelled"),
            Some("cancelled: preempted"),
        ),
        (
            RunState::Failed,
            Some("root_changed"),
            Some("root_changed: captured root lost"),
        ),
        (RunState::Done, Some("imported"), None),
        // Still running when the daemon died: not terminal, so not a failure.
        (RunState::Running, None, None),
    ] {
        let runs = [record(
            "a",
            ALPHA,
            RunOrigin::Auto,
            800,
            state,
            outcome,
            code,
        )];
        let seed = &seed_from(&runs)[ALPHA];
        assert_eq!(seed.failure, None, "{state:?} {outcome:?} {code:?}");
        // Every one of them still counts as the last start.
        assert_eq!(seed.created_at, 800);
    }
    // A user's cancel is a failure.
    let runs = [record(
        "a",
        ALPHA,
        RunOrigin::Auto,
        800,
        RunState::Cancelled,
        Some("cancelled"),
        Some("cancelled"),
    )];
    assert_eq!(
        seed_from(&runs)[ALPHA].failure.as_ref().unwrap().code,
        "cancelled"
    );
}

#[test]
fn user_cancel_backs_off_like_failure() {
    let base = Instant::now();
    let runs = [record(
        "a",
        ALPHA,
        RunOrigin::Auto,
        NOW_MS - 1000,
        RunState::Cancelled,
        Some("cancelled"),
        Some("cancelled"),
    )];
    let mut s = OwnerState::seeded(seed_from(&runs), NOW_MS, Intervals::default());
    s.first = false;
    s.config = config(&[ALPHA], 10, 60);
    s.approvals.insert(ALPHA.into(), valid(ALPHA));
    s.set_head(head("rev-a", 1, &[ALPHA]), base);
    let status = at(&mut s, &live(), base, 3600).statuses[ALPHA].clone();
    assert_eq!(status.state, AutoState::Failed);
    assert_eq!(
        status.reason.as_deref(),
        Some("automatic re-run failed: cancelled")
    );
    assert_eq!(status.last_run_id.as_deref(), Some("a"));
}

#[test]
fn a_seeded_start_in_the_future_is_clamped_to_now() {
    let mut seeds = BTreeMap::new();
    seeds.insert(
        ALPHA.to_owned(),
        Seed {
            run_id: "a".into(),
            created_at: NOW_MS + 1_000_000,
            failure: None,
        },
    );
    let state = OwnerState::seeded(seeds, NOW_MS, Intervals::default());
    assert_eq!(state.last_start[ALPHA], NOW_MS);
}

#[test]
fn intervals_override_parses_every_key_or_none() {
    let parsed = Intervals::from_json(
        r#"{"quietMs":200,"minIntervalMs":0,"gateBackstopMs":300,"backstopMs":400}"#,
    )
    .unwrap();
    assert_eq!(parsed.quiet, Some(Duration::from_millis(200)));
    assert_eq!(parsed.min_interval, Some(Duration::ZERO));
    assert_eq!(parsed.gate_backstop, Duration::from_millis(300));
    assert_eq!(parsed.backstop, Duration::from_millis(400));
    assert_eq!(Intervals::from_json(r#"{"quietMs":200}"#), None);
    assert_eq!(Intervals::from_json("not json"), None);
}

// --- the loop, with fakes ---

struct FakeClock {
    base: Instant,
    offset_ms: Mutex<u64>,
}

impl FakeClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            base: Instant::now(),
            offset_ms: Mutex::new(0),
        })
    }

    fn advance(&self, by: Duration) {
        *self.offset_ms.lock().unwrap() += by.as_millis() as u64;
    }

    fn elapsed(&self) -> Duration {
        Duration::from_millis(*self.offset_ms.lock().unwrap())
    }
}

impl Clock for FakeClock {
    fn instant(&self) -> Instant {
        self.base + self.elapsed()
    }

    fn epoch_ms(&self) -> u64 {
        NOW_MS + self.elapsed().as_millis() as u64
    }
}

#[derive(Default)]
struct Rt {
    version: u64,
    /// Timeouts that fired without a signal.
    expired: u64,
    /// `(seen, expired)` while the owner is blocked in `wait_state_change`.
    waiting: Option<(u64, u64)>,
    waits: Vec<(u64, Duration)>,
    ready: bool,
    intent: bool,
    slot: Option<RunOrigin>,
    supersede_result: Option<AutoCancel>,
    supersedes: usize,
    launches: Vec<(String, String)>,
    outcomes: VecDeque<LaunchOutcome>,
    bump_on_launch: bool,
    abandon: bool,
}

struct FakeRuntime {
    state: Mutex<Rt>,
    changed: Condvar,
}

impl FakeRuntime {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(Rt {
                ready: true,
                supersede_result: Some(AutoCancel::Requested),
                ..Rt::default()
            }),
            changed: Condvar::new(),
        })
    }

    fn with(&self, change: impl FnOnce(&mut Rt)) {
        change(&mut self.state.lock().unwrap());
        self.changed.notify_all();
    }

    fn bump(&self) {
        self.with(|state| state.version += 1);
    }

    /// A timeout fired: the owner wakes without a state change.
    fn expire(&self) {
        self.with(|state| state.expired += 1);
    }

    fn wait_for(&self, what: &str, ready: impl Fn(&Rt) -> bool) {
        let deadline = Instant::now() + crate::testing::HANG_BACKSTOP;
        let mut state = self.state.lock().unwrap();
        while !ready(&state) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "{what} did not happen (hang)");
            state = self.changed.wait_timeout(state, left).unwrap().0;
        }
    }

    /// The owner has handled everything signalled so far and is blocked waiting for more.
    fn quiesce(&self) {
        self.wait_for("the owner to settle", |state| {
            state.waiting == Some((state.version, state.expired))
        });
    }

    fn launches(&self) -> Vec<(String, String)> {
        self.state.lock().unwrap().launches.clone()
    }

    fn finish(&self, outcome: LaunchOutcome) {
        self.with(|state| state.outcomes.push_back(outcome));
    }
}

impl AutoRunRuntime for FakeRuntime {
    fn alive(&self) -> bool {
        true
    }

    fn phase_ready(&self) -> bool {
        self.state.lock().unwrap().ready
    }

    fn intent_pending(&self) -> bool {
        self.state.lock().unwrap().intent
    }

    fn active_origin(&self) -> Option<RunOrigin> {
        self.state.lock().unwrap().slot
    }

    fn supersede(&self, _token: &SupersedeToken) -> AutoCancel {
        let mut state = self.state.lock().unwrap();
        state.supersedes += 1;
        let result = state.supersede_result.unwrap();
        self.changed.notify_all();
        result
    }

    fn state_version(&self) -> u64 {
        self.state.lock().unwrap().version
    }

    fn wait_state_change(&self, seen: u64, timeout: Duration) {
        let mut state = self.state.lock().unwrap();
        state.waits.push((seen, timeout));
        let expired = state.expired;
        state.waiting = Some((seen, expired));
        self.changed.notify_all();
        while state.version == seen && state.expired == expired {
            state = self.changed.wait(state).unwrap();
        }
        state.waiting = None;
    }

    fn wake(&self) {
        self.bump();
    }

    fn launch(&self, request: LaunchRequest) -> LaunchOutcome {
        let mut state = self.state.lock().unwrap();
        state
            .launches
            .push((request.approval.producer, request.expected_revision));
        if state.bump_on_launch {
            state.version += 1;
        }
        self.changed.notify_all();
        loop {
            if let Some(outcome) = state.outcomes.pop_front() {
                return outcome;
            }
            if state.abandon {
                return LaunchOutcome::NotReady;
            }
            state = self.changed.wait(state).unwrap();
        }
    }
}

struct Src {
    pin_revision: u64,
    native: String,
    due: Vec<String>,
    configs: VecDeque<ConfigState>,
    approvals: BTreeMap<String, ApprovalRead>,
    seed: BTreeMap<String, Seed>,
    head_error: bool,
    pin_calls: usize,
    head_calls: usize,
    config_calls: usize,
}

struct FakeSources(Mutex<Src>);

impl FakeSources {
    fn new(names: &[&str], enabled: ConfigState) -> Arc<Self> {
        Arc::new(Self(Mutex::new(Src {
            pin_revision: 1,
            native: "h1".into(),
            due: names.iter().map(|name| (*name).to_owned()).collect(),
            configs: VecDeque::from([enabled]),
            approvals: names
                .iter()
                .map(|name| ((*name).to_owned(), valid(name)))
                .collect(),
            seed: BTreeMap::new(),
            head_error: false,
            pin_calls: 0,
            head_calls: 0,
            config_calls: 0,
        })))
    }

    fn edit(&self, change: impl FnOnce(&mut Src)) {
        change(&mut self.0.lock().unwrap());
    }

    fn counts(&self) -> (usize, usize, usize) {
        let src = self.0.lock().unwrap();
        (src.pin_calls, src.head_calls, src.config_calls)
    }
}

impl AutoRunSources for FakeSources {
    fn pin(&self) -> Result<IndexPin> {
        let mut src = self.0.lock().unwrap();
        src.pin_calls += 1;
        Ok(pin(src.pin_revision))
    }

    fn head(&self, producers: &[&str]) -> Result<AutoRunHead> {
        let mut src = self.0.lock().unwrap();
        src.head_calls += 1;
        anyhow::ensure!(!src.head_error, "storage_busy: injected");
        Ok(AutoRunHead {
            pin: pin(src.pin_revision),
            native_revision_id: src.native.clone(),
            due: producers
                .iter()
                .map(|name| ((*name).to_owned(), src.due.iter().any(|due| due == name)))
                .collect(),
        })
    }

    fn config(&self) -> ConfigState {
        let mut src = self.0.lock().unwrap();
        src.config_calls += 1;
        if src.configs.len() > 1 {
            src.configs.pop_front().unwrap()
        } else {
            src.configs.front().cloned().unwrap()
        }
    }

    fn approval(&self, producer: &str) -> ApprovalRead {
        self.0
            .lock()
            .unwrap()
            .approvals
            .get(producer)
            .cloned()
            .unwrap_or(ApprovalRead::Absent)
    }

    fn seed(&self) -> BTreeMap<String, Seed> {
        self.0.lock().unwrap().seed.clone()
    }
}

struct Harness {
    rt: Arc<FakeRuntime>,
    clock: Arc<FakeClock>,
    published: Arc<Mutex<Published>>,
    closed: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

fn immediate() -> Intervals {
    Intervals {
        quiet: Some(Duration::ZERO),
        min_interval: Some(Duration::ZERO),
        gate_backstop: BACKSTOP,
        backstop: BACKSTOP,
    }
}

impl Harness {
    fn start(rt: &Arc<FakeRuntime>, src: &Arc<FakeSources>, intervals: Intervals) -> Self {
        Self::start_with(rt, src, intervals, thread_spawner())
    }

    fn start_with(
        rt: &Arc<FakeRuntime>,
        src: &Arc<FakeSources>,
        intervals: Intervals,
        spawn: Spawner,
    ) -> Self {
        let clock = FakeClock::new();
        let published = Arc::new(Mutex::new(Published::default()));
        let closed = Arc::new(AtomicBool::new(false));
        let deps = Deps {
            runtime: rt.clone(),
            sources: src.clone(),
            clock: clock.clone(),
            probes: Arc::new(NoProbes),
            intervals,
            spawn,
            closed: closed.clone(),
            published: published.clone(),
        };
        let thread = std::thread::spawn(move || run_loop(deps));
        Self {
            rt: rt.clone(),
            clock,
            published,
            closed,
            thread: Some(thread),
        }
    }

    fn status(&self, producer: &str) -> AutoRunStatus {
        let published = self.published.lock().unwrap();
        published
            .statuses
            .get(producer)
            .unwrap_or(&published.fallback)
            .clone()
    }

    /// Signal the runtime and wait for the owner to finish handling it.
    fn signal(&self) {
        self.rt.bump();
        self.rt.quiesce();
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        self.rt.with(|state| {
            state.abandon = true;
            state.version += 1;
        });
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
fn loop_off_reads_no_index() {
    for off in [
        ConfigState::Missing,
        config(&[ALPHA], 10, 60),
        ConfigState::Valid(BTreeMap::new()),
    ] {
        let off = match off {
            ConfigState::Valid(mut tables) => {
                for table in tables.values_mut() {
                    table.enabled = false;
                }
                ConfigState::Valid(tables)
            }
            other => other,
        };
        let rt = FakeRuntime::new();
        let src = FakeSources::new(&[ALPHA], off);
        let harness = Harness::start(&rt, &src, immediate());
        rt.quiesce();
        for _ in 0..3 {
            harness.signal();
        }
        let (pins, heads, configs) = src.counts();
        assert_eq!((pins, heads), (0, 0));
        assert!(configs >= 1);
        assert!(rt.launches().is_empty());
        assert_eq!(harness.status(ALPHA).state, AutoState::Off);
    }
}

#[test]
fn loop_config_enable_queries_head_in_same_pass() {
    let rt = FakeRuntime::new();
    let mut disabled = config(&[ALPHA], 3600, 3600);
    if let ConfigState::Valid(tables) = &mut disabled {
        tables.get_mut(ALPHA).unwrap().enabled = false;
    }
    let src = FakeSources::new(&[ALPHA], disabled);
    let harness = Harness::start(&rt, &src, Intervals::default());
    rt.quiesce();
    assert_eq!(src.counts().1, 0);

    src.edit(|src| src.configs = VecDeque::from([config(&[ALPHA], 3600, 3600)]));
    harness.clock.advance(BACKSTOP);
    rt.expire();
    rt.quiesce();
    assert_eq!(src.counts().1, 1);
    assert_eq!(harness.status(ALPHA).state, AutoState::Scheduled);

    // The pin was recorded by that pass: an unchanged pin asks nothing more.
    harness.signal();
    assert_eq!(src.counts().1, 1);
}

#[test]
fn loop_wait_timeouts_are_always_positive() {
    let rt = FakeRuntime::new();
    let src = FakeSources::new(&[ALPHA], config(&[ALPHA], 3600, 3600));
    let harness = Harness::start(&rt, &src, Intervals::default());
    rt.quiesce();
    harness.signal();
    harness.clock.advance(secs(30));
    harness.signal();
    harness.clock.advance(BACKSTOP);
    rt.expire();
    rt.quiesce();
    let waits = rt.state.lock().unwrap().waits.clone();
    assert!(waits.len() >= 4);
    assert!(
        waits.iter().all(|(_, timeout)| !timeout.is_zero()),
        "{waits:?}"
    );
}

#[test]
fn loop_starts_on_drain_signal_before_backstop() {
    let rt = FakeRuntime::new();
    rt.with(|state| state.intent = true);
    let src = FakeSources::new(&[ALPHA], config(&[ALPHA], 10, 60));
    let harness = Harness::start(&rt, &src, immediate());
    rt.quiesce();
    harness.signal();
    assert!(rt.launches().is_empty());
    assert_eq!(harness.status(ALPHA).state, AutoState::Scheduled);

    rt.with(|state| state.intent = false);
    rt.bump();
    rt.wait_for("the launch", |state| !state.launches.is_empty());
    assert_eq!(rt.launches(), [(ALPHA.to_owned(), "h1".to_owned())]);
    assert_eq!(rt.state.lock().unwrap().expired, 0);
    assert_eq!(harness.clock.elapsed(), Duration::ZERO);
}

#[test]
fn loop_slot_busy_waits_for_next_signal() {
    let rt = FakeRuntime::new();
    let src = FakeSources::new(&[ALPHA], config(&[ALPHA], 10, 60));
    let harness = Harness::start(&rt, &src, immediate());
    rt.finish(LaunchOutcome::SlotBusy);
    rt.wait_for("the launch", |state| !state.launches.is_empty());
    rt.wait_for("the outcome to be taken", |state| {
        state.outcomes.is_empty() && state.waiting.is_some_and(|(seen, _)| seen == state.version)
    });
    rt.quiesce();
    assert_eq!(rt.launches().len(), 1);
    // The next attempt waits for a newer signal or the backstop, nothing sooner.
    let last = *rt.state.lock().unwrap().waits.last().unwrap();
    assert_eq!(last.1, BACKSTOP);
    assert_eq!(harness.status(ALPHA).state, AutoState::Scheduled);

    rt.finish(LaunchOutcome::SlotBusy);
    harness.signal();
    rt.wait_for("the retry", |state| state.launches.len() == 2);
}

#[test]
fn loop_bump_before_outcome_store_starts_nothing() {
    let rt = FakeRuntime::new();
    rt.with(|state| state.bump_on_launch = true);
    let src = FakeSources::new(&[ALPHA, BETA], config(&[ALPHA], 10, 60));
    let harness = Harness::start(&rt, &src, immediate());
    rt.wait_for("the first launch", |state| state.launches.len() == 1);
    rt.quiesce();
    assert_eq!(harness.status(ALPHA).state, AutoState::Running);

    // Q becomes due while P is still launching; the signal arrives before P's outcome.
    harness.clock.advance(secs(10));
    src.edit(|src| src.configs = VecDeque::from([config(&[ALPHA, BETA], 10, 60)]));
    harness.signal();
    harness.signal();
    assert_eq!(rt.launches().len(), 1);

    // P's launch ends: it stores its outcome, then wakes the owner. Only then does Q start.
    src.edit(|src| src.due = vec![BETA.to_owned()]);
    rt.finish(finished(AutoOutcome::Imported));
    rt.wait_for("the second launch", |state| state.launches.len() == 2);
    assert_eq!(rt.launches()[1].0, BETA);
    let state = rt.state.lock().unwrap();
    // The fake clock never reached the backstop, and no timeout fired: the signal did it.
    assert_eq!(harness.clock.elapsed(), secs(10));
    assert_eq!(state.expired, 0);
    assert!(state.waits.iter().all(|(_, timeout)| *timeout <= BACKSTOP));
    assert!(
        state
            .waits
            .iter()
            .rev()
            .find(|(_, timeout)| *timeout != BACKSTOP)
            .is_some_and(|(_, timeout)| *timeout < BACKSTOP)
    );
}

#[test]
fn newer_native_head_supersedes_once() {
    let rt = FakeRuntime::new();
    let src = FakeSources::new(&[ALPHA], config(&[ALPHA], 10, 60));
    let harness = Harness::start(&rt, &src, immediate());
    rt.wait_for("the launch", |state| !state.launches.is_empty());
    rt.quiesce();
    assert_eq!(rt.state.lock().unwrap().supersedes, 0);

    src.edit(|src| {
        src.native = "h2".into();
        src.pin_revision = 2;
    });
    harness.signal();
    assert_eq!(rt.state.lock().unwrap().supersedes, 1);
    harness.signal();
    assert_eq!(rt.state.lock().unwrap().supersedes, 1);

    // The run ends cancelled as superseded: no failure, and the new head runs next.
    rt.finish(finished(AutoOutcome::Superseded));
    rt.wait_for("the rerun", |state| state.launches.len() == 2);
    assert_eq!(rt.launches()[1].1, "h2");
}

#[test]
fn a_supersede_before_install_is_retried_at_the_next_wake() {
    let rt = FakeRuntime::new();
    rt.with(|state| state.supersede_result = Some(AutoCancel::NoAutoRun));
    let src = FakeSources::new(&[ALPHA], config(&[ALPHA], 10, 60));
    let harness = Harness::start(&rt, &src, immediate());
    rt.wait_for("the launch", |state| !state.launches.is_empty());
    rt.quiesce();
    src.edit(|src| {
        src.native = "h2".into();
        src.pin_revision = 2;
    });
    harness.signal();
    assert_eq!(rt.state.lock().unwrap().supersedes, 1);
    harness.signal();
    assert_eq!(rt.state.lock().unwrap().supersedes, 2);
    rt.with(|state| state.supersede_result = Some(AutoCancel::NotCancellable));
    harness.signal();
    harness.signal();
    assert_eq!(rt.state.lock().unwrap().supersedes, 3);
}

#[test]
fn prelaunch_failure_backs_off_within_runtime() {
    let rt = FakeRuntime::new();
    let src = FakeSources::new(&[ALPHA], config(&[ALPHA], 10, 60));
    let harness = Harness::start(&rt, &src, immediate());
    rt.wait_for("the launch", |state| !state.launches.is_empty());
    rt.finish(finished(failed("plan_unavailable", "h1")));
    rt.wait_for("the failure", |state| state.outcomes.is_empty());
    rt.quiesce();
    harness.signal();
    harness.signal();
    assert_eq!(rt.launches().len(), 1);
    let status = harness.status(ALPHA);
    assert_eq!(status.state, AutoState::Failed);
    assert_eq!(
        status.reason.as_deref(),
        Some("automatic re-run failed: plan_unavailable")
    );
}

#[test]
fn a_thread_that_cannot_start_reruns_the_pass_at_once() {
    let rt = FakeRuntime::new();
    let src = FakeSources::new(&[ALPHA, BETA], config(&[ALPHA, BETA], 10, 60));
    let refused = Arc::new(AtomicBool::new(false));
    let spawn: Spawner = {
        let (refused, real) = (refused.clone(), thread_spawner());
        Arc::new(move |job| {
            if refused.swap(true, Ordering::SeqCst) {
                real(job)
            } else {
                Err(std::io::Error::other("no threads"))
            }
        })
    };
    let harness = Harness::start_with(&rt, &src, immediate(), spawn);
    // No bump, no timeout: the failed spawn alone leads to the next candidate.
    rt.wait_for("the second candidate", |state| !state.launches.is_empty());
    assert_eq!(rt.launches(), [(BETA.to_owned(), "h1".to_owned())]);
    rt.quiesce();
    assert_eq!(rt.state.lock().unwrap().expired, 0);
    let status = harness.status(ALPHA);
    assert_eq!(status.state, AutoState::Failed);
    assert_eq!(
        status.reason.as_deref(),
        Some("automatic re-run failed: internal_error")
    );
    // The in-memory failure holds at this head.
    harness.signal();
    assert_eq!(rt.launches().len(), 1);
}

#[test]
fn the_prespawn_recheck_replaces_the_first_decision() {
    let rt = FakeRuntime::new();
    // First read: both enabled, so alpha is chosen. The recheck reads alpha disabled and beta
    // enabled: alpha is not started, and the re-run pass starts beta.
    let mut recheck = config(&[ALPHA, BETA], 10, 60);
    if let ConfigState::Valid(tables) = &mut recheck {
        tables.get_mut(ALPHA).unwrap().enabled = false;
    }
    let src = FakeSources::new(&[ALPHA, BETA], config(&[ALPHA, BETA], 10, 60));
    src.edit(|src| src.configs.push_back(recheck));
    let harness = Harness::start(&rt, &src, immediate());
    rt.wait_for("a launch", |state| !state.launches.is_empty());
    rt.quiesce();
    assert_eq!(rt.launches(), [(BETA.to_owned(), "h1".to_owned())]);
    assert_eq!(rt.state.lock().unwrap().expired, 0);
    // The recheck's statuses were published.
    assert_eq!(harness.status(ALPHA).state, AutoState::Off);
    assert_eq!(harness.status(BETA).state, AutoState::Running);
}

#[test]
fn store_error_keeps_previous_statuses_and_due_since() {
    let rt = FakeRuntime::new();
    let src = FakeSources::new(&[ALPHA], config(&[ALPHA], 3600, 3600));
    let harness = Harness::start(&rt, &src, Intervals::default());
    rt.quiesce();
    let before = harness.status(ALPHA);
    assert_eq!(before.state, AutoState::Scheduled);
    assert!(before.due_since.is_some());

    src.edit(|src| {
        src.head_error = true;
        src.pin_revision = 2;
    });
    harness.clock.advance(secs(5));
    harness.signal();
    assert_eq!(harness.status(ALPHA), before);
    let failed_calls = src.counts().1;

    // The next wake retries the read, and the published head recovers.
    src.edit(|src| src.head_error = false);
    harness.signal();
    assert!(src.counts().1 > failed_calls);
    assert_eq!(harness.status(ALPHA).due_since, before.due_since);
}

#[test]
fn reactivation_seed_keeps_failure_and_interval() {
    let rt = FakeRuntime::new();
    let src = FakeSources::new(&[ALPHA], config(&[ALPHA], 10, 60));
    src.edit(|src| {
        src.native = "rev-a".into();
        src.seed.insert(
            ALPHA.into(),
            Seed {
                run_id: "a".into(),
                created_at: NOW_MS,
                failure: Some(Failure {
                    code: "producer_failed".into(),
                    basis: "rev-a".into(),
                }),
            },
        );
    });
    let harness = Harness::start(&rt, &src, Intervals::default());
    rt.quiesce();
    let status = harness.status(ALPHA);
    assert_eq!(status.state, AutoState::Failed);
    assert_eq!(status.last_run_id.as_deref(), Some("a"));
    assert!(rt.launches().is_empty());
}
