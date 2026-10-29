use super::*;

fn next_attempt(state: &Arc<DaemonState>) -> anyhow::Result<()> {
    // Advance only the worker-owned advisory deadline, not the recovery state.
    *state.recovery_retry_after.lock().unwrap() = None;
    state.worker_step()
}

fn assert_queued(store: &Store, accepted: &crate::store::requests::Request) {
    let row = store.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(row.seq, accepted.seq);
    assert_eq!(row.state, "queued");
    assert!(row.claim_incarnation.is_none());
    assert!(row.started_at.is_none());
    assert!(row.finished_at.is_none());
    assert!(row.error_code.is_none());
    assert!(row.revision.is_none());
}

#[test]
fn persistent_recreation_fault_fails_accepted_work_then_recovers_without_replaying_it() {
    let (_tmp, store, state, roots, identity) = super::exceptional_recovery_tests::fixture();
    let q0 = store.current_request().unwrap().unwrap();
    let q1 = store
        .enqueue_request(&state.config().options, None)
        .unwrap();
    let index = roots.index_db(&identity);
    let damaged = std::fs::read(&index).unwrap();
    let sidecar = index.with_file_name("index.db-wal");
    let injected = sidecar.clone();
    store.set_exclusive_recovery_hook(move || {
        std::fs::write(injected, b"unsupported recovery artifact").unwrap();
    });
    let terminal_generation = store.terminal_generation();

    for attempt in 1..=4 {
        let error = next_attempt(&state).unwrap_err();
        assert!(
            error.to_string().contains("unsupported index sidecar"),
            "{error:#}"
        );
        assert_eq!(state.startup_failures.load(Ordering::Acquire), attempt);
        assert!(state.recovery_retry_after.lock().unwrap().is_some());
        assert!(state.recovery_blocker().is_none());
        assert_queued(&store, &q0);
        assert_queued(&store, &q1);
        assert_eq!(store.terminal_generation(), terminal_generation);
    }

    let error = next_attempt(&state).unwrap_err();
    assert!(
        error.to_string().contains("unsupported index sidecar"),
        "{error:#}"
    );
    let blocker = state.recovery_blocker().unwrap();
    assert!(blocker.starts_with("recovery_blocked: "));
    assert!(blocker.contains("unsupported index sidecar"));
    assert!(blocker.contains(sidecar.to_str().unwrap()));
    assert!(
        store.terminal_generation() > terminal_generation,
        "confirmed failure must wake terminal waiters"
    );
    let failed: Vec<_> = [&q0, &q1]
        .into_iter()
        .map(|accepted| {
            let row = store.request_by_id(&accepted.id).unwrap().unwrap();
            assert_eq!(row.seq, accepted.seq);
            assert_eq!(row.options_json, accepted.options_json);
            assert_eq!(row.state, "failed");
            assert_eq!(row.error_code.as_deref(), Some(blocker.as_str()));
            assert!(row.started_at.is_some());
            assert!(row.finished_at.is_some());
            assert!(row.claim_incarnation.is_some());
            assert!(row.revision.is_none());
            (row.id.clone(), format!("{row:?}"))
        })
        .collect();
    assert_eq!(std::fs::read(&index).unwrap(), damaged);
    assert!(state.runtime.lock().unwrap().session().is_none());

    // Terminalizing the queue does not stop background recovery without work.
    let error = next_attempt(&state).unwrap_err();
    assert!(
        error.to_string().contains("unsupported index sidecar"),
        "{error:#}"
    );
    assert_eq!(state.startup_failures.load(Ordering::Acquire), 6);
    assert!(state.recovery_retry_after.lock().unwrap().is_some());
    for (id, before) in &failed {
        assert_eq!(
            format!("{:?}", store.request_by_id(id).unwrap().unwrap()),
            *before
        );
    }

    std::fs::remove_file(sidecar).unwrap();
    next_attempt(&state).unwrap();
    assert!(!store.is_recreate_pending());
    assert!(state.recovery_blocker().is_none());
    assert!(state.startup_error().is_none());
    assert_eq!(state.startup_failures.load(Ordering::Acquire), 0);
    assert!(state.recovery_retry_after.lock().unwrap().is_none());
    store
        .verify_writer_session(&state.retained_serving_session().unwrap())
        .unwrap();
    let fresh = store
        .enqueue_request(&state.config().options, None)
        .unwrap();
    state.worker_step().unwrap();
    let done = store.request_by_id(&fresh.id).unwrap().unwrap();
    assert_eq!(done.state, "done");
    assert_eq!(done.revision.unwrap(), store.status().unwrap().revision);
    for (id, before) in failed {
        assert_eq!(
            format!("{:?}", store.request_by_id(&id).unwrap().unwrap()),
            before
        );
    }
}

#[test]
fn pending_disposition_confirms_and_wakes_before_successful_recovery() {
    let (_tmp, store, state, roots, identity) = super::exceptional_recovery_tests::fixture();
    let accepted = store.current_request().unwrap().unwrap();
    let generation = store.terminal_generation();
    *state.recovery_disposition.lock().unwrap() = Some("persistent fault".into());
    let mut confirmation_blocker = None;
    let error = store
        .fail_queued_recovery_blocked_with_hook("persistent fault", || {
            // COMMIT is durable but its protected confirmation cannot acquire SH.
            confirmation_blocker = Some(roots.index_use_exclusive_existing(&identity)?);
            Err(crate::store::topology::StorageBusy.into())
        })
        .unwrap_err();
    assert!(crate::store::transient_storage_contention(&error));
    assert_eq!(store.terminal_generation(), generation);
    next_attempt(&state).unwrap();
    assert!(state.recovery_disposition.lock().unwrap().is_some());
    assert_eq!(store.terminal_generation(), generation);
    drop(confirmation_blocker);
    // The recreation fault itself is repairable now. The worker must confirm
    // the prior disposition before startup succeeds, even with an empty queue.
    next_attempt(&state).unwrap();
    assert!(state.recovery_disposition.lock().unwrap().is_none());
    assert!(store.terminal_generation() > generation);
    assert!(state.retained_serving_session().is_ok());
    assert!(state.recovery_blocker().is_none());
    let row = store.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(row.state, "failed");
    assert_eq!(
        row.error_code.as_deref(),
        Some("recovery_blocked: persistent fault")
    );
}

#[test]
fn recreation_reader_contention_never_exhausts_recovery_allowance() {
    let (_tmp, store, state, roots, identity) = super::exceptional_recovery_tests::fixture();
    let accepted = store.current_request().unwrap().unwrap();
    let index = roots.index_db(&identity);
    let damaged = std::fs::read(&index).unwrap();
    let reader = roots.index_use_existing(&identity).unwrap();
    let terminal_generation = store.terminal_generation();
    for _ in 0..7 {
        next_attempt(&state).unwrap();
        assert_eq!(state.startup_failures.load(Ordering::Acquire), 0);
        assert!(state.recovery_blocker().is_none());
        assert!(state.recovery_retry_after.lock().unwrap().is_some());
        assert_queued(&store, &accepted);
        assert_eq!(store.terminal_generation(), terminal_generation);
        assert_eq!(std::fs::read(&index).unwrap(), damaged);
    }
    drop(reader);
    next_attempt(&state).unwrap();
    assert_eq!(
        store.request_by_id(&accepted.id).unwrap().unwrap().state,
        "done"
    );
    assert!(state.recovery_blocker().is_none());
    assert!(state.startup_error().is_none());
}

#[cfg(feature = "test-support")]
#[test]
fn persistent_startup_fault_is_bounded_without_recreation() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
    let options = IndexOptions::new(root);
    let accepted = store.enqueue_request(&options, None).unwrap();
    let state = new(
        store.clone(),
        options,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    state.worker_started.store(true, Ordering::Release);
    assert!(!store.is_recreate_pending());
    for attempt in 1..=5 {
        state.set_startup_hook_for_tests(Arc::new(|| panic!("persistent startup fault")));
        let error = next_attempt(&state).unwrap_err();
        assert!(
            error.to_string().contains("persistent startup fault"),
            "{error:#}"
        );
        if attempt < 5 {
            assert_queued(&store, &accepted);
            assert!(state.recovery_blocker().is_none());
        }
    }
    let failed = store.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(failed.state, "failed");
    assert_eq!(failed.error_code, state.recovery_blocker());
    assert!(
        failed
            .error_code
            .unwrap()
            .contains("persistent startup fault")
    );
    next_attempt(&state).unwrap();
    assert!(state.recovery_blocker().is_none());
    assert!(state.startup_error().is_none());
    assert_eq!(
        store.request_by_id(&accepted.id).unwrap().unwrap().state,
        "failed"
    );
}
