use super::{retention_clock_plausible, retention_due};

#[test]
fn exact_grace_boundary_is_inclusive() {
    assert!(!retention_due(1_899, 1_000));
    assert!(retention_due(1_900, 1_000));
    assert!(!retention_due(999, 1_000));
}

#[test]
fn forward_jump_waits_for_monotonic_elapsed_and_backward_step_is_allowed() {
    assert!(retention_clock_plausible(1_000, 900, 0));
    assert!(retention_clock_plausible(1_000, 1_005, 0));
    assert!(!retention_clock_plausible(1_000, 1_006, 0));
    assert!(retention_clock_plausible(1_000, 1_900, 895));
}
