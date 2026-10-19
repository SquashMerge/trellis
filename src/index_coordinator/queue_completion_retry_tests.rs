use super::*;
use std::fs;

fn fixture() -> (
    tempfile::TempDir,
    Store,
    IndexOptions,
    IndexPin,
    Arc<WriterSession>,
) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
    let options = IndexOptions::new(root);
    let (baseline, session) = reconcile_workspace(
        &store,
        &options,
        &Arc::new(std::sync::atomic::AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    (tmp, store, options, baseline, session)
}

#[test]
fn failed_terminal_write_retries_exact_published_head_before_next_fifo_claim() {
    let (_tmp, store, options, baseline, session) = fixture();
    let first = store.enqueue_request(&options, None).unwrap();
    let second = store.enqueue_request(&options, None).unwrap();
    assert!(first.seq < second.seq);
    store.inject_queue_finish_failure(false);
    let error = drain_requests(&store, &session).unwrap_err();
    assert!(error.to_string().contains("storage_busy"));
    let published = store.status().unwrap().revision;
    assert_eq!(published.index_revision, baseline.index_revision + 1);
    assert_eq!(
        store.request_by_id(&first.id).unwrap().unwrap().state,
        "running"
    );
    assert_eq!(
        store.request_by_id(&second.id).unwrap().unwrap().state,
        "queued"
    );
    assert_eq!(drain_requests(&store, &session).unwrap(), 2);
    let q1 = store.request_by_id(&first.id).unwrap().unwrap();
    let q2 = store.request_by_id(&second.id).unwrap().unwrap();
    assert_eq!((q1.state.as_str(), q2.state.as_str()), ("done", "done"));
    assert_eq!(q1.revision.unwrap(), published);
    assert_eq!(
        q2.revision.unwrap().index_revision,
        published.index_revision + 1
    );
    assert_eq!(store.status().unwrap().revision, q2.revision.unwrap());
}

#[test]
fn cached_completion_consumes_one_daemon_tick_before_next_fifo_claim() {
    let (_tmp, store, options, baseline, session) = fixture();
    let first = store.enqueue_request(&options, None).unwrap();
    let mut next_options = options.clone();
    next_options.max_file_bytes = 1_048_576;
    let second = store.enqueue_request(&next_options, None).unwrap();
    assert!(first.seq < second.seq);
    assert_ne!(first.options_json, second.options_json);

    store.inject_queue_finish_failure(false);
    let error = drain_one_request_observed(&store, &session, |_, _| {}).unwrap_err();
    assert!(error.to_string().contains("storage_busy"), "{error:#}");
    let published = store.status().unwrap().revision;
    assert_eq!(published.index_revision, baseline.index_revision + 1);
    assert_eq!(
        store.request_by_id(&first.id).unwrap().unwrap().state,
        "running"
    );
    assert_eq!(
        store.request_by_id(&second.id).unwrap().unwrap().state,
        "queued"
    );

    assert_eq!(
        drain_one_request_observed(&store, &session, |_, _| {}).unwrap(),
        1
    );
    let q1 = store.request_by_id(&first.id).unwrap().unwrap();
    let pending = store.request_by_id(&second.id).unwrap().unwrap();
    assert_eq!(q1.state, "done");
    assert_eq!(q1.revision, Some(published));
    assert_eq!(pending.state, "queued");
    assert!(pending.started_at.is_none());
    assert_eq!(store.status().unwrap().revision, published);
    assert_eq!(
        (&q1.id, q1.seq, &q1.options_json, q1.expected),
        (&first.id, first.seq, &first.options_json, first.expected)
    );
    assert_eq!(
        (
            &pending.id,
            pending.seq,
            &pending.options_json,
            pending.expected
        ),
        (
            &second.id,
            second.seq,
            &second.options_json,
            second.expected
        )
    );

    assert_eq!(
        drain_one_request_observed(&store, &session, |_, _| {}).unwrap(),
        1
    );
    let q1 = store.request_by_id(&first.id).unwrap().unwrap();
    let q2 = store.request_by_id(&second.id).unwrap().unwrap();
    assert_eq!(q1.revision, Some(published));
    assert_eq!(q2.state, "done");
    let next_pin = q2.revision.unwrap();
    assert_eq!(next_pin.index_revision, published.index_revision + 1);
    assert_ne!(next_pin, published);
    assert_eq!(store.status().unwrap().revision, next_pin);
    assert_eq!(
        (&q2.id, q2.seq, &q2.options_json, q2.expected),
        (
            &second.id,
            second.seq,
            &second.options_json,
            second.expected
        )
    );
}

#[test]
fn ambiguous_committed_terminal_is_verified_without_republishing_head() {
    let (_tmp, store, options, baseline, session) = fixture();
    let first = store.enqueue_request(&options, None).unwrap();
    let second = store.enqueue_request(&options, None).unwrap();
    store.inject_queue_finish_failure(true);
    assert_eq!(drain_requests(&store, &session).unwrap(), 2);
    let q1 = store.request_by_id(&first.id).unwrap().unwrap();
    let q2 = store.request_by_id(&second.id).unwrap().unwrap();
    assert_eq!((q1.state.as_str(), q2.state.as_str()), ("done", "done"));
    assert_eq!(
        q1.revision.unwrap().index_revision,
        baseline.index_revision + 1
    );
    assert_eq!(
        q2.revision.unwrap().index_revision,
        baseline.index_revision + 2
    );
}
#[test]
fn cli_cached_claim_mismatch_fails_closed_instead_of_retrying_forever() {
    let (_tmp, store, options, baseline, session) = fixture();
    let q1 = store.enqueue_request(&options, None).unwrap();
    let q2 = store.enqueue_request(&options, None).unwrap();
    store.inject_queue_finish_failure(false);
    let initial = drain_requests(&store, &session).unwrap_err();
    assert!(
        initial
            .to_string()
            .contains("injected terminal write failure")
    );
    let published = store.status().unwrap().revision;
    assert_eq!(published.index_revision, baseline.index_revision + 1);
    let db = rusqlite::Connection::open(store.request_db_path()).unwrap();
    db.execute(
        "UPDATE requests SET claim_incarnation='changed-incarnation' WHERE id=?1",
        [&q1.id],
    )
    .unwrap();
    drop(db);
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let error = retry_cli_recorded_completion(&store, &session, &cancel, initial).unwrap_err();
    assert!(
        error.to_string().contains("cached claim changed"),
        "{error:#}"
    );
    assert_eq!(
        store.request_by_id(&q1.id).unwrap().unwrap().state,
        "running"
    );
    assert_eq!(
        store.request_by_id(&q2.id).unwrap().unwrap().state,
        "queued"
    );
    assert_eq!(store.status().unwrap().revision, published);
}

#[test]
fn cached_done_acks_only_original_watcher_batch_after_confirmed_terminal() {
    let (_tmp, store, options, _baseline, session) = fixture();
    let mut watcher = WatcherWork::new(&store, &session, &options).unwrap();
    watcher.disable_native_watcher_for_tests();
    let watcher = std::sync::Mutex::new(Some((Arc::downgrade(&session), watcher)));
    let first = store.enqueue_request(&options, None).unwrap();
    let second = store.enqueue_request(&options, None).unwrap();
    store.inject_queue_finish_failure(false);
    let error = drain_one_request_with_watcher(&store, &session, &watcher, |_, _| {}, || Ok(true))
        .unwrap_err();
    assert!(error.to_string().contains("storage_busy"), "{error:#}");
    let published = store.status().unwrap().revision;
    {
        let binding = watcher.lock().unwrap();
        let cert = binding
            .as_ref()
            .unwrap()
            .1
            .request_coverage
            .as_ref()
            .expect("missing coverage after failed terminal write");
        assert!(cert.verified, "capture inventory/options did not certify");
        assert_eq!(cert.pin, Some(published));
    }
    assert_eq!(
        watcher
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .1
            .acknowledged_generation(),
        0
    );
    assert_eq!(
        store.request_by_id(&second.id).unwrap().unwrap().state,
        "queued"
    );
    assert_eq!(
        drain_one_request_with_watcher(&store, &session, &watcher, |_, _| {}, || Ok(true)).unwrap(),
        1
    );
    let binding = watcher.lock().unwrap();
    let work = &binding.as_ref().unwrap().1;
    assert_eq!(work.accounted_watch_generation(), Some(1));
    assert_eq!(work.accounted_watch_pin(), Some(published));
    assert_eq!(
        store.request_by_id(&first.id).unwrap().unwrap().revision,
        Some(published)
    );
    assert_eq!(
        store.request_by_id(&second.id).unwrap().unwrap().state,
        "queued"
    );
    assert_eq!(store.status().unwrap().revision, published);
}

#[test]
fn newer_intent_during_cached_completion_is_not_acked_as_old_batch() {
    let (_tmp, store, options, _baseline, session) = fixture();
    let mut watcher = WatcherWork::new(&store, &session, &options).unwrap();
    watcher.disable_native_watcher_for_tests();
    watcher.force_periodic_inventory_for_tests();
    let watcher = std::sync::Mutex::new(Some((Arc::downgrade(&session), watcher)));
    store.enqueue_request(&options, None).unwrap();
    store.inject_queue_finish_failure(false);
    drain_one_request_with_watcher(&store, &session, &watcher, |_, _| {}, || Ok(true)).unwrap_err();
    watcher
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .1
        .watch
        .require_full();
    assert_eq!(
        drain_one_request_with_watcher(&store, &session, &watcher, |_, _| {}, || Ok(true)).unwrap(),
        1
    );
    let binding = watcher.lock().unwrap();
    let work = &binding.as_ref().unwrap().1;
    assert_eq!(work.acknowledged_generation(), 0);
    assert_eq!(work.accounted_watch_pin(), None);
    assert!(work.periodic_inventory_due_for_tests());
    assert!(work.accepted_watch_intent(&options));
}

#[test]
fn differently_configured_request_does_not_ack_existing_full_watch() {
    let (_tmp, store, options, _baseline, session) = fixture();
    let mut watcher = WatcherWork::new(&store, &session, &options).unwrap();
    watcher.disable_native_watcher_for_tests();
    watcher.force_periodic_inventory_for_tests();
    let watcher = std::sync::Mutex::new(Some((Arc::downgrade(&session), watcher)));
    let mut different = options.clone();
    different.max_file_bytes /= 2;
    let row = store.enqueue_request(&different, None).unwrap();
    assert_eq!(
        drain_one_request_with_watcher(&store, &session, &watcher, |_, _| {}, || Ok(true)).unwrap(),
        1
    );
    assert_eq!(store.request_by_id(&row.id).unwrap().unwrap().state, "done");
    let binding = watcher.lock().unwrap();
    let work = &binding.as_ref().unwrap().1;
    assert_eq!(work.acknowledged_generation(), 0);
    assert_eq!(work.accounted_watch_pin(), None);
    assert!(work.periodic_inventory_due_for_tests());
    assert!(work.accepted_watch_intent(&different));
}

#[test]
fn periodic_full_inventory_is_certified_by_matching_request_capture() {
    let (_tmp, store, options, _baseline, session) = fixture();
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut watcher = WatcherWork::new(&store, &session, &options).unwrap();
    watcher.disable_native_watcher_for_tests();
    assert!(
        watcher
            .reconcile_due(&store, &session, &options, &cancel, true)
            .unwrap()
    );
    watcher.force_periodic_inventory_for_tests();
    assert!(watcher.periodic_inventory_due_for_tests());
    let watcher = std::sync::Mutex::new(Some((Arc::downgrade(&session), watcher)));
    let row = store.enqueue_request(&options, None).unwrap();
    assert_eq!(
        drain_one_request_with_watcher(&store, &session, &watcher, |_, _| {}, || Ok(true)).unwrap(),
        1
    );
    let pin = store
        .request_by_id(&row.id)
        .unwrap()
        .unwrap()
        .revision
        .unwrap();
    let binding = watcher.lock().unwrap();
    let work = &binding.as_ref().unwrap().1;
    assert_eq!(work.accounted_watch_pin(), Some(pin));
    assert!(!work.periodic_inventory_due_for_tests());
}

#[test]
fn failed_and_cached_failed_rows_keep_watcher_full_due() {
    for cached in [false, true] {
        let (_tmp, store, options, _baseline, session) = fixture();
        let mut work = WatcherWork::new(&store, &session, &options).unwrap();
        work.disable_native_watcher_for_tests();
        work.force_periodic_inventory_for_tests();
        let watcher = std::sync::Mutex::new(Some((Arc::downgrade(&session), work)));
        fs::write(options.workspace_root.join("bad.js"), [0xff, 0xfe]).unwrap();
        let row = store.enqueue_request(&options, None).unwrap();
        if cached {
            store.inject_queue_finish_failure(false);
            assert!(
                drain_one_request_with_watcher(&store, &session, &watcher, |_, _| {}, || Ok(true))
                    .is_err()
            );
            assert_eq!(
                store.request_by_id(&row.id).unwrap().unwrap().state,
                "running"
            );
        }
        assert_eq!(
            drain_one_request_with_watcher(&store, &session, &watcher, |_, _| {}, || Ok(true))
                .unwrap(),
            1
        );
        assert_eq!(
            store.request_by_id(&row.id).unwrap().unwrap().state,
            "failed"
        );
        let guard = watcher.lock().unwrap();
        let work = &guard.as_ref().unwrap().1;
        assert_eq!(work.acknowledged_generation(), 0);
        assert_eq!(work.accounted_watch_pin(), None);
        assert!(work.periodic_inventory_due_for_tests());
        assert!(work.accepted_watch_intent(&options));
    }
}

#[test]
fn drifted_source_set_and_selected_input_do_not_ack_request() {
    for source_set in [false, true] {
        let (_tmp, store, options, _baseline, session) = fixture();
        let mut work = WatcherWork::new(&store, &session, &options).unwrap();
        work.disable_native_watcher_for_tests();
        work.force_periodic_inventory_for_tests();
        let watcher = std::sync::Mutex::new(Some((Arc::downgrade(&session), work)));
        fs::write(
            options.workspace_root.join("b.js"),
            "function before() {}\n",
        )
        .unwrap();
        let row = store.enqueue_request(&options, None).unwrap();
        let file = if source_set {
            options.workspace_root.join("later.js")
        } else {
            options.workspace_root.join("a.js")
        };
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let touched = fired.clone();
        store.set_publication_before_commit_hook_for_tests(move || {
            touched.store(true, std::sync::atomic::Ordering::SeqCst);
            fs::write(&file, "function changed() {}\n").unwrap();
        });
        let observed =
            drain_one_request_with_watcher(&store, &session, &watcher, |_, _| {}, || Ok(true))
                .unwrap();
        assert!(
            fired.load(std::sync::atomic::Ordering::SeqCst),
            "publication hook did not run"
        );
        let terminal = store.request_by_id(&row.id).unwrap().unwrap();
        if source_set {
            assert_eq!(observed, 1);
            assert_eq!(terminal.state, "failed");
        } else {
            assert_eq!(observed, 0);
            assert_eq!(terminal.state, "queued");
        }
        let guard = watcher.lock().unwrap();
        let work = &guard.as_ref().unwrap().1;
        assert_eq!(work.acknowledged_generation(), 0);
        assert_eq!(work.accounted_watch_pin(), None);
        assert!(work.periodic_inventory_due_for_tests());
        assert!(work.accepted_watch_intent(&options));
    }
}

#[test]
fn drifted_selected_non_source_input_does_not_ack_request() {
    let (_tmp, store, options, _baseline, session) = fixture();
    let manifest = options.workspace_root.join("package.json");
    fs::write(&manifest, "{\"name\":\"before\"}\n").unwrap();
    let mut work = WatcherWork::new(&store, &session, &options).unwrap();
    work.disable_native_watcher_for_tests();
    work.force_periodic_inventory_for_tests();
    let watcher = std::sync::Mutex::new(Some((Arc::downgrade(&session), work)));
    let row = store.enqueue_request(&options, None).unwrap();
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let touched = fired.clone();
    store.set_publication_before_commit_hook_for_tests(move || {
        touched.store(true, std::sync::atomic::Ordering::SeqCst);
        fs::write(&manifest, "{\"name\":\"after\"}\n").unwrap();
    });
    let observed =
        drain_one_request_with_watcher(&store, &session, &watcher, |_, _| {}, || Ok(true)).unwrap();
    assert!(fired.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(observed, 0);
    assert_eq!(
        store.request_by_id(&row.id).unwrap().unwrap().state,
        "queued"
    );
    let guard = watcher.lock().unwrap();
    let work = &guard.as_ref().unwrap().1;
    assert_eq!(work.acknowledged_generation(), 0);
    assert_eq!(work.accounted_watch_pin(), None);
    assert!(work.periodic_inventory_due_for_tests());
    assert!(work.accepted_watch_intent(&options));
}

#[test]
fn changed_watcher_root_cannot_certify_matching_request() {
    let (_tmp, store, options, _baseline, session) = fixture();
    let other_root = options.workspace_root.parent().unwrap().join("other");
    fs::create_dir(&other_root).unwrap();
    let mut other = options.clone();
    other.workspace_root = other_root;
    let mut work = WatcherWork::new(&store, &session, &other).unwrap();
    work.disable_native_watcher_for_tests();
    work.force_periodic_inventory_for_tests();
    let watcher = std::sync::Mutex::new(Some((Arc::downgrade(&session), work)));
    let row = store.enqueue_request(&options, None).unwrap();
    assert_eq!(
        drain_one_request_with_watcher(&store, &session, &watcher, |_, _| {}, || Ok(true)).unwrap(),
        1
    );
    assert_eq!(store.request_by_id(&row.id).unwrap().unwrap().state, "done");
    let guard = watcher.lock().unwrap();
    let work = &guard.as_ref().unwrap().1;
    assert_eq!(work.acknowledged_generation(), 0);
    assert_eq!(work.accounted_watch_pin(), None);
    assert!(work.periodic_inventory_due_for_tests());
    assert!(work.accepted_watch_intent(&options));
}
