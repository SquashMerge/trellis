use super::*;

#[test]
fn message_only_recreation_contention_never_exhausts_recovery_allowance() {
    for reason in [
        "storage_busy: obsolete index has live SQLite connections",
        "storage_busy: obsolete index witness still borrowed",
        "storage_busy: staged index still has live SQLite connections",
    ] {
        let (_tmp, store, state, _roots, _identity) =
            crate::http::exceptional_recovery_tests::fixture();
        let queued = store.current_request().unwrap().unwrap();
        for _ in 0..7 {
            *state.test_startup_error.lock().unwrap() = Some(anyhow::anyhow!(reason));
            *state.recovery_retry_after.lock().unwrap() = None;
            state.worker_step().unwrap();
            assert_eq!(state.startup_failures.load(Ordering::Acquire), 0);
            assert!(state.recovery_blocker().is_none());
            assert_eq!(
                store.request_by_id(&queued.id).unwrap().unwrap().state,
                "queued"
            );
        }
        *state.recovery_retry_after.lock().unwrap() = None;
        state.worker_step().unwrap();
        assert_eq!(
            store.request_by_id(&queued.id).unwrap().unwrap().state,
            "done"
        );
    }
}

#[test]
fn real_worker_loop_honors_recovery_backoff_even_when_woken_early() {
    let (_tmp, store, state, _roots, _identity) =
        crate::http::exceptional_recovery_tests::fixture();
    let accepted = store.current_request().unwrap().unwrap();
    let start = Instant::now();
    *state.test_worker_now.lock().unwrap() = Some(start);
    let mut attempts = 0;
    let mut early_wake = false;
    let mut last_attempt = start;
    let mut delay = STARTUP_BACKOFF_FIRST;
    worker_loop_with_wait(Arc::downgrade(&state), state.inbox.clone(), |deadline| {
        let expected = if attempts == 0 {
            start
        } else {
            last_attempt + delay
        };
        assert_eq!(deadline, Some(expected));
        assert_eq!(state.startup_failures.load(Ordering::Acquire), attempts);
        let row = store.request_by_id(&accepted.id).unwrap().unwrap();
        assert_eq!(
            row.state,
            if attempts < STARTUP_FAILURE_LIMIT {
                "queued"
            } else {
                "failed"
            }
        );
        if attempts == STARTUP_FAILURE_LIMIT {
            assert_eq!(
                last_attempt.duration_since(start),
                Duration::from_millis(3750)
            );
        }
        if attempts == 10 {
            assert_eq!(
                last_attempt.duration_since(start),
                Duration::from_millis(91750)
            );
            state.inbox.shut_down();
            return;
        }
        if attempts != 0 && !early_wake {
            // A caller wake just before the scheduled retry is only a hint.
            *state.test_worker_now.lock().unwrap() = Some(expected - Duration::from_millis(1));
            assert!(
                !state
                    .startup_due(|| panic!("backoff must precede queue probing"))
                    .unwrap()
            );
            early_wake = true;
            return;
        }
        *state.test_worker_now.lock().unwrap() = Some(expected);
        *state.test_startup_error.lock().unwrap() =
            Some(anyhow::anyhow!("persistent recovery fault"));
        last_attempt = expected;
        if attempts != 0 {
            delay = delay.saturating_mul(2).min(STARTUP_BACKOFF_MAX);
        }
        early_wake = false;
        attempts += 1;
    });
}

#[test]
fn failed_step_uses_a_future_recovery_deadline_or_a_bounded_retry() {
    let (_tmp, _store, state, _roots, _identity) =
        crate::http::exceptional_recovery_tests::fixture();
    let now = Instant::now();
    *state.test_worker_now.lock().unwrap() = Some(now);
    for retry in [None, Some(now), Some(now - Duration::from_secs(1))] {
        *state.recovery_retry_after.lock().unwrap() = retry;
        assert_eq!(state.failed_step_retry(), now + STEP_RETRY);
    }
    *state.recovery_retry_after.lock().unwrap() = Some(now + Duration::from_secs(2));
    assert_eq!(state.failed_step_retry(), now + Duration::from_secs(2));
}
