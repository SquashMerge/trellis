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
