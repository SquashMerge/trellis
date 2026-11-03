use super::*;
use crate::model::CancelFlag;

#[test]
fn native_wake_cancels_the_running_operation_before_the_worker_consumes_it() {
    let inbox = Arc::new(WorkerInbox::default());
    let running = inbox.begin_semantic();
    assert!(!running.cancel.load(Ordering::Acquire));
    inbox.native_wake();
    assert!(running.cancel.load(Ordering::Acquire));
    assert!(inbox.wake_pending());
    drop(running);
    inbox.wait(None);
    assert!(!inbox.wake_pending());
    let successor = inbox.begin_semantic();
    assert!(!successor.cancel.load(Ordering::Acquire));
}

#[test]
fn ordinary_wake_does_not_cancel_a_running_semantic_operation() {
    let inbox = Arc::new(WorkerInbox::default());
    let operation = inbox.begin_semantic();
    inbox.wake();
    assert!(inbox.wake_pending());
    assert!(!operation.cancel.load(Ordering::Acquire));
}

#[test]
fn pending_wake_and_shutdown_do_not_admit_semantic_work() {
    for shutdown in [false, true] {
        let inbox = Arc::new(WorkerInbox::default());
        if shutdown {
            inbox.shut_down();
        } else {
            inbox.wake();
        }
        let operation = inbox.begin_semantic();
        assert!(operation.cancel.load(Ordering::Acquire));
    }
}

#[test]
fn dropped_operation_does_not_clear_a_successors_cancellation_slot() {
    let inbox = Arc::new(WorkerInbox::default());
    let older = inbox.begin_semantic();
    let successor = inbox.begin_semantic();
    drop(older);
    let registered: CancelFlag = inbox.semantic.lock().unwrap().as_ref().unwrap().clone();
    assert!(Arc::ptr_eq(&registered, &successor.cancel));
    inbox.native_wake();
    assert!(successor.cancel.load(Ordering::Acquire));
}

#[test]
fn operation_drop_cancels_and_clears_its_own_slot() {
    let inbox = Arc::new(WorkerInbox::default());
    let operation = inbox.begin_semantic();
    let token = operation.cancel.clone();
    drop(operation);
    assert!(token.load(Ordering::Acquire));
    assert!(inbox.semantic.lock().unwrap().is_none());
}

#[test]
fn shutdown_stop_cancels_running_and_future_operations() {
    let inbox = Arc::new(WorkerInbox::default());
    let running = inbox.begin_semantic();
    inbox.stop_semantic();
    assert!(running.cancel.load(Ordering::Acquire));
    drop(running);
    assert!(inbox.begin_semantic().cancel.load(Ordering::Acquire));
}

#[test]
fn final_fence_rejects_newer_ingress_without_holding_the_watcher_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let watcher = crate::watch::WatchSignals::synthetic_for_tests(tmp.path().to_owned());
    let fence = watcher.publication_fence();
    assert!(fence.lock().is_ok());
    watcher.submit_event(Ok(notify::Event::new(notify::EventKind::Any)));
    assert!(!fence.unchanged());
    assert!(fence.lock().is_err());
}

#[test]
fn explicit_config_replacement_cancels_a_running_operation() {
    let (_tmp, _store, state, options) =
        crate::http::exceptional_recovery_tests::serving_queue_watch_fixture();
    if state.inbox.wake_pending() {
        state.inbox.wait(None);
    }
    let operation = state.inbox.begin_semantic();
    let config = super::super::BrowserConfig::build(
        options.clone(),
        true,
        &options.workspace_root,
        Vec::new(),
        None,
    )
    .unwrap();
    state.apply_config(config);
    assert!(operation.cancel.load(Ordering::Acquire));
    assert!(state.explicit_pending.lock().unwrap().is_some());
}

#[test]
fn retirement_cancels_background_without_a_client_admission_lease() {
    let (_tmp, _store, state, _options) =
        crate::http::exceptional_recovery_tests::serving_queue_watch_fixture();
    if state.inbox.wake_pending() {
        state.inbox.wait(None);
    }
    let operation = state.inbox.begin_semantic();
    assert_eq!(state.leases(), 0);
    assert_eq!(state.request_retire(|_| true), RetireOutcome::Requested);
    assert!(operation.cancel.load(Ordering::Acquire));
}

#[test]
fn accepted_fifo_and_watcher_ingress_cancel_without_a_waiting_publisher() {
    let (_tmp, store, state, options) =
        crate::http::exceptional_recovery_tests::serving_queue_watch_fixture();
    // Consume activation hints as the real owner loop does before its next step.
    if state.inbox.wake_pending() {
        state.inbox.wait(None);
    }
    let running = state.inbox.begin_semantic();
    store.enqueue_request(&options, None).unwrap();
    assert!(running.cancel.load(Ordering::Acquire));
    assert!(state.fifo_pending.load(Ordering::Acquire));
    drop(running);
    state.inbox.wait(None);
    let running = state.inbox.begin_semantic();
    state
        .watcher_work
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .1
        .submit_watch_event_for_tests(Ok(notify::Event::new(notify::EventKind::Any)));
    assert!(running.cancel.load(Ordering::Acquire));
}

#[cfg(feature = "test-support")]
fn pending_owner_fixture() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    crate::store::Store,
    crate::indexer::IndexOptions,
    Arc<DaemonState>,
    Arc<WriterSession>,
    crate::model::IndexPin,
) {
    let (_state_dir, _root_dir, store, options, _base, native_pin) =
        crate::store::native_first_fixture_for_tests();
    let cancel = Arc::new(AtomicBool::new(false));
    let pending_before = store.pending_semantic_rejoin_at_head().unwrap().unwrap();
    // The storage-only fixture has never accepted a request. Use real queue
    // admission and unchanged H/FIFO completion before making it a daemon owner.
    let accepted = store.enqueue_request(&options, None).unwrap();
    let session =
        crate::index_coordinator::establish_serving_session(&store, Some(&options), &cancel)
            .unwrap();
    assert_eq!(
        crate::index_coordinator::drain_requests(&store, &session).unwrap(),
        1
    );
    let completed = store.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(completed.state, "done");
    assert_eq!(completed.revision, Some(native_pin));
    assert_eq!(store.index_baseline().unwrap(), native_pin);
    assert_eq!(
        store.pending_semantic_rejoin_at_head().unwrap(),
        Some(pending_before)
    );
    let state = crate::http::new(
        store.clone(),
        options.clone(),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    state.worker_started.store(true, Ordering::Release);
    *state.runtime.lock().unwrap() = RuntimeState::Ready(session.clone());
    let mut watcher =
        crate::index_coordinator::WatcherWork::new(&store, &session, &options).unwrap();
    watcher.disable_native_watcher_for_tests();
    watcher.set_waker(state.watch_waker.clone());
    assert!(
        watcher
            .reconcile_due(&store, &session, &options, &cancel, true)
            .unwrap()
    );
    state.publish_watch_status(&watcher, &options);
    *state.watcher_work.lock().unwrap() = Some((Arc::downgrade(&session), watcher));
    if state.inbox.wake_pending() {
        state.inbox.wait(None);
    }
    (
        _state_dir, _root_dir, store, options, state, session, native_pin,
    )
}

#[cfg(feature = "test-support")]
#[test]
fn watcher_ingress_cancels_live_semantic_transaction_before_next_native_publication() {
    use std::sync::mpsc;

    let origin = Instant::now();
    let stamp = || {
        let monotonic = Instant::now();
        let wall_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        (monotonic, wall_ns)
    };
    let (_state_dir, _root_dir, store, options, state, _session, native_pin) =
        pending_owner_fixture();
    let original = store.pending_semantic_rejoin_at_head().unwrap().unwrap();
    let (active_tx, active_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    store.set_semantic_rejoin_active_hook_for_tests(move || {
        active_tx.send(stamp()).unwrap();
        resume_rx.recv().unwrap();
    });
    let (released_tx, released_rx) = mpsc::channel();
    state.set_semantic_hook_for_tests(Arc::new(move |point| {
        if point == "writer_released" {
            released_tx.send(stamp()).unwrap();
        }
    }));
    let owner = state.clone();
    let running = std::thread::spawn(move || owner.worker_step());
    // This hook is inside the live transaction. No native publication is
    // waiting on its permit; only actual accepted watch ingress cancels it.
    let sql_active = active_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let b_text = "function f1() { return 99; }\n";
    let b_write_begin = stamp();
    std::fs::write(options.workspace_root.join("f1.js"), b_text).unwrap();
    let b_write_close = stamp();
    {
        let work = state.watcher_work.lock().unwrap();
        work.as_ref()
            .unwrap()
            .1
            .submit_watch_event_for_tests(Ok(notify::Event::new(notify::EventKind::Any)));
    }
    let watch_accepted = stamp();
    assert!(
        state
            .inbox
            .semantic
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .load(Ordering::Acquire)
    );
    let resume_send = stamp();
    resume_tx.send(()).unwrap();
    running.join().unwrap().unwrap();
    let first_writer_released = released_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(state.inbox.semantic.lock().unwrap().is_none());
    assert_eq!(store.index_baseline().unwrap(), native_pin);
    assert_eq!(
        store.pending_semantic_rejoin_at_head().unwrap(),
        Some(original.clone())
    );

    // Make the existing inventory due rather than waiting out debounce. The
    // same owner must publish native first and return before semantic rejoin.
    state.inbox.wait(None);
    state
        .watcher_work
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .1
        .force_periodic_inventory_for_tests();
    state.worker_step().unwrap();
    let newer_native = store.index_baseline().unwrap();
    assert_eq!(newer_native.index_revision, native_pin.index_revision + 1);
    let latest = store.pending_semantic_rejoin_at_head().unwrap().unwrap();
    assert_eq!(latest.target_pin, newer_native);
    assert_ne!(latest, original);
    assert!(state.inbox.semantic.lock().unwrap().is_none());
    let response = store.evidence_response().unwrap();
    let source = response
        .source_at("f1.js", Some(newer_native))
        .unwrap()
        .unwrap();
    let status = response
        .document_semantic_status_at("f1.js", Some(newer_native))
        .unwrap();
    let (source, native_status) = response.finish((source, status)).unwrap();
    drop(response);
    assert_eq!(source.0, newer_native);
    assert_eq!(source.1.text, b_text);
    assert_eq!(source.1.hash, crate::scip_raw::sha(b_text.as_bytes()));
    assert_eq!(
        native_status.state,
        crate::model::SemanticAvailability::CatchingUp
    );
    let b_native_readable = stamp();
    let b_hash = source.1.hash;

    state.worker_step().unwrap();
    let latest_writer_released = released_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let semantic_pin = store.index_baseline().unwrap();
    assert_eq!(semantic_pin.index_revision, newer_native.index_revision + 1);
    assert!(store.pending_semantic_rejoin_at_head().unwrap().is_none());
    assert!(state.inbox.semantic.lock().unwrap().is_none());
    let response = store.evidence_response().unwrap();
    let source = response
        .source_at("f1.js", Some(semantic_pin))
        .unwrap()
        .unwrap();
    let status = response
        .document_semantic_status_at("f1.js", Some(semantic_pin))
        .unwrap();
    let (source, semantic_status) = response.finish((source, status)).unwrap();
    drop(response);
    assert_eq!(source.0, semantic_pin);
    assert_eq!(source.1.text, b_text);
    assert_eq!(source.1.hash, b_hash);
    assert_eq!(
        semantic_status.state,
        crate::model::SemanticAvailability::Unavailable
    );
    let semantic = store.semantic_at(semantic_pin).unwrap();
    let coverage = semantic
        .coverage
        .iter()
        .find(|coverage| {
            coverage.document_path == "f1.js"
                && Some(&coverage.producer_id) == semantic_status.producer_id.as_ref()
        })
        .unwrap();
    assert!(coverage.requested && coverage.selected);
    assert_eq!(
        coverage.state,
        crate::semantic_evidence::CoverageState::Failed
    );
    assert_eq!(
        coverage.diagnostic.as_deref(),
        Some("unmeasurable_source_anchor")
    );
    let b_semantic_publication_completed = stamp();
    assert_eq!(
        store
            .evidence_response()
            .unwrap()
            .document_semantic_status_at("f0.js", Some(native_pin))
            .unwrap()
            .state,
        crate::model::SemanticAvailability::CatchingUp,
    );
    let event = |value: (Instant, u128)| {
        serde_json::json!({
            "monotonicOffsetNs": value.0.duration_since(origin).as_nanos(),
            "wallTimeNs": value.1,
        })
    };
    let elapsed = |a: (Instant, u128), b: (Instant, u128)| b.0.duration_since(a.0).as_nanos();
    println!(
        "{}",
        serde_json::json!({
            "kind": "native_first_small_fixture_overlap_timing",
            "scope": "normal-size admitted raw fixture; transaction-active hook; not production corpus",
            "pins": {"pendingA": native_pin, "nativeB": newer_native, "semanticB": semantic_pin},
            "source": {"path": "f1.js", "sha256": b_hash,
                "nativeSemanticStatus": native_status, "completedSemanticStatus": semantic_status,
                "usableFreshNotAchieved": true, "coverageState": coverage.state,
                "coverageDiagnostic": coverage.diagnostic},
            "events": {
                "semanticSqlTransactionActive": event(sql_active),
                "bWriteBegin": event(b_write_begin), "bWriteClose": event(b_write_close),
                "watchAcceptedObserved": event(watch_accepted), "resumeSend": event(resume_send),
                "cancelledWriterReleased": event(first_writer_released),
                "bNativeReadable": event(b_native_readable),
                "latestWriterReleased": event(latest_writer_released),
                "semanticPublicationCompleted": event(b_semantic_publication_completed)
            },
            "durationsNs": {
                "bWriteCloseToAcceptedWatch": elapsed(b_write_close, watch_accepted),
                "resumeSendToWriterReleased": elapsed(resume_send, first_writer_released),
                "bWriteCloseToNativeReadable": elapsed(b_write_close, b_native_readable),
                "bWriteCloseToSemanticPublicationCompleted": elapsed(b_write_close, b_semantic_publication_completed),
                "deliberateBarrierHold": elapsed(sql_active, resume_send)
            },
            "notes": [
                "resume-to-release excludes deliberate hook hold but includes channel handoff and unwind",
                "save-to-readable includes test controller, barrier and forced-inventory overhead",
                "changed source completed with failed coverage; usable fresh evidence not achieved",
                "no latency threshold asserted"
            ]
        })
    );
}

#[cfg(feature = "test-support")]
#[test]
fn semantic_operational_contention_defers_same_target_but_not_new_native_work() {
    let (_state_dir, _root_dir, store, options, state, session, native_pin) =
        pending_owner_fixture();
    let now = Instant::now();
    *state.test_worker_now.lock().unwrap() = Some(now);
    assert!(state.delay_timed_work_for_tests());
    state.inbox.wait(None);
    let pending = store.pending_semantic_rejoin_at_head().unwrap().unwrap();
    // A held existing permit injects an ordinary bounded admission failure.
    // No source/index mutation or sleeping is needed to create contention.
    let permit = store.maintenance_try_enter().unwrap();
    state.run_pending_semantic(&session).unwrap();
    let due = {
        let retry = state.semantic_retry.lock().unwrap();
        let retry = retry.as_ref().unwrap();
        assert_eq!(retry.pending, pending);
        assert_eq!(retry.delay, STARTUP_BACKOFF_FIRST);
        retry.due
    };
    assert_eq!(due, now + STARTUP_BACKOFF_FIRST);
    assert_eq!(state.next_wake(), Some(due));
    assert_eq!(store.index_baseline().unwrap(), native_pin);
    drop(permit);

    state.inbox.wake();
    state.inbox.wait(None);
    state.run_pending_semantic(&session).unwrap();
    assert_eq!(
        state.semantic_retry.lock().unwrap().as_ref().unwrap().due,
        due
    );
    assert_eq!(store.index_baseline().unwrap(), native_pin);

    // Native work ignores semantic backoff. Its new target gets a fresh budget.
    std::fs::write(
        options.workspace_root.join("f1.js"),
        "function f1(){return 77;}\n",
    )
    .unwrap();
    state
        .watcher_work
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .1
        .force_periodic_inventory_for_tests();
    state.worker_step().unwrap();
    let newer = store.index_baseline().unwrap();
    assert_eq!(newer.index_revision, native_pin.index_revision + 1);
    let latest = store.pending_semantic_rejoin_at_head().unwrap().unwrap();
    assert_ne!(pending, latest);
    state.worker_step().unwrap();
    assert!(store.pending_semantic_rejoin_at_head().unwrap().is_none());
    assert!(state.semantic_retry.lock().unwrap().is_none());
    assert_eq!(
        store.index_baseline().unwrap().index_revision,
        newer.index_revision + 1
    );
}

#[cfg(feature = "test-support")]
#[test]
fn semantic_operational_retry_backoff_grows_to_existing_cap() {
    let (_state_dir, _root_dir, store, _options, state, _session, _pin) = pending_owner_fixture();
    let pending = store.pending_semantic_rejoin_at_head().unwrap().unwrap();
    let now = Instant::now();
    *state.test_worker_now.lock().unwrap() = Some(now);
    let mut expected = STARTUP_BACKOFF_FIRST;
    for _ in 0..10 {
        state.defer_semantic(pending.clone());
        let retry = state.semantic_retry.lock().unwrap();
        let retry = retry.as_ref().unwrap();
        assert_eq!(retry.delay, expected);
        assert_eq!(retry.due, now + expected);
        expected = expected.saturating_mul(2).min(STARTUP_BACKOFF_MAX);
    }
    assert!(!state.semantic_retry_due(&pending));
    *state.test_worker_now.lock().unwrap() = Some(now + STARTUP_BACKOFF_MAX);
    assert!(state.semantic_retry_due(&pending));
}
