use super::pickup_watch::{Intervals, Stat, Step, Tracker, parse_intervals};
use std::time::{Duration, Instant};

fn intervals() -> Intervals {
    Intervals {
        debounce: Duration::from_millis(200),
        stable: Duration::from_millis(100),
        check: Duration::from_millis(1000),
        retry_ms: 500,
    }
}

fn ms(base: Instant, n: u64) -> Instant {
    base + Duration::from_millis(n)
}

fn stat_of(content: &[u8]) -> Option<Stat> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.scip");
    std::fs::write(&path, content).unwrap();
    Stat::of(&path)
}

#[test]
fn event_waits_out_debounce_then_stability_before_importing() {
    let (iv, t0) = (intervals(), Instant::now());
    let first = stat_of(b"one");
    let mut tracker = Tracker::new(t0, &iv);
    tracker.event(t0, &iv);
    assert_eq!(tracker.step(ms(t0, 199), &iv, || first), Step::Wait);
    // The debounce ended: the first stat sample starts the stability window.
    assert_eq!(tracker.step(ms(t0, 200), &iv, || first), Step::Wait);
    assert_eq!(tracker.step(ms(t0, 299), &iv, || first), Step::Wait);
    assert_eq!(tracker.step(ms(t0, 300), &iv, || first), Step::Import);
    assert!(tracker.is_idle());
}

#[test]
fn a_new_event_restarts_the_debounce_and_a_changing_stat_the_stability_window() {
    let (iv, t0) = (intervals(), Instant::now());
    let (one, two) = (stat_of(b"one"), stat_of(b"second"));
    let mut tracker = Tracker::new(t0, &iv);
    tracker.event(t0, &iv);
    tracker.event(ms(t0, 150), &iv);
    assert_eq!(tracker.step(ms(t0, 200), &iv, || one), Step::Wait);
    assert!(!tracker.is_idle());
    assert_eq!(tracker.step(ms(t0, 350), &iv, || one), Step::Wait);
    // Sampled at 350, then the file grows: the window restarts from the change.
    assert_eq!(tracker.step(ms(t0, 400), &iv, || two), Step::Wait);
    assert_eq!(tracker.step(ms(t0, 499), &iv, || two), Step::Wait);
    assert_eq!(tracker.step(ms(t0, 500), &iv, || two), Step::Import);
}

#[test]
fn a_vanished_file_ends_the_attempt_without_importing() {
    let (iv, t0) = (intervals(), Instant::now());
    let mut tracker = Tracker::new(t0, &iv);
    tracker.event(t0, &iv);
    assert_eq!(tracker.step(ms(t0, 200), &iv, || stat_of(b"x")), Step::Wait);
    assert_eq!(tracker.step(ms(t0, 300), &iv, || None), Step::Wait);
    assert!(tracker.is_idle());
}

#[test]
fn activation_skips_the_debounce_but_not_the_stability_window() {
    let (iv, t0) = (intervals(), Instant::now());
    let seen = stat_of(b"one");
    let mut tracker = Tracker::new(t0, &iv);
    tracker.activation(t0);
    assert_eq!(tracker.step(t0, &iv, || seen), Step::Wait);
    assert_eq!(tracker.step(ms(t0, 99), &iv, || seen), Step::Wait);
    assert_eq!(tracker.step(ms(t0, 100), &iv, || seen), Step::Import);
}

#[test]
fn the_stat_check_stats_only_when_due_and_only_stabilizes_a_changed_stat() {
    let (iv, t0) = (intervals(), Instant::now());
    let seen = stat_of(b"one");
    let mut tracker = Tracker::new(t0, &iv);
    tracker.activation(t0);
    tracker.step(t0, &iv, || seen);
    assert_eq!(tracker.step(ms(t0, 100), &iv, || seen), Step::Import);
    // Not due yet: the stat closure must not run.
    assert_eq!(
        tracker.step(ms(t0, 999), &iv, || panic!(
            "no stat before the check is due"
        )),
        Step::Wait
    );
    // Due and unchanged: no hashing, only the retry question.
    assert_eq!(tracker.step(ms(t0, 1100), &iv, || seen), Step::Retry);
    // Due and changed: the new bytes go through the stability window first.
    let changed = stat_of(b"changed bytes");
    assert_eq!(tracker.step(ms(t0, 2100), &iv, || changed), Step::Wait);
    assert!(!tracker.is_idle());
    assert_eq!(tracker.step(ms(t0, 2200), &iv, || changed), Step::Import);
}

#[test]
fn an_event_for_unchanged_stat_only_asks_about_a_retry() {
    let (iv, t0) = (intervals(), Instant::now());
    let seen = stat_of(b"one");
    let mut tracker = Tracker::new(t0, &iv);
    tracker.activation(t0);
    tracker.step(t0, &iv, || seen);
    assert_eq!(tracker.step(ms(t0, 100), &iv, || seen), Step::Import);
    tracker.event(ms(t0, 150), &iv);
    tracker.step(ms(t0, 350), &iv, || seen);
    assert_eq!(tracker.step(ms(t0, 450), &iv, || seen), Step::Retry);
}

#[test]
fn a_forgotten_attempt_is_looked_at_again_at_the_next_check() {
    let (iv, t0) = (intervals(), Instant::now());
    let seen = stat_of(b"one");
    let mut tracker = Tracker::new(t0, &iv);
    tracker.activation(t0);
    tracker.step(t0, &iv, || seen);
    assert_eq!(tracker.step(ms(t0, 100), &iv, || seen), Step::Import);
    tracker.forget();
    assert_eq!(tracker.step(ms(t0, 1100), &iv, || seen), Step::Wait);
    assert_eq!(tracker.step(ms(t0, 1200), &iv, || seen), Step::Import);
}

#[test]
fn test_intervals_override_only_named_fields_and_reject_bad_values() {
    let parsed = parse_intervals(r#"{"debounceMs":10,"retryMs":200}"#).unwrap();
    assert_eq!(parsed.debounce, Duration::from_millis(10));
    assert_eq!(parsed.retry_ms, 200);
    assert_eq!(parsed.stable, Intervals::default().stable);
    assert_eq!(parsed.check, Intervals::default().check);
    assert!(parse_intervals(r#"{"debounceMs":"soon"}"#).is_none());
    assert!(parse_intervals("not json").is_none());
}
