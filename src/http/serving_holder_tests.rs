use super::*;
use std::fs;

#[test]
fn takeover_head_manifest_options_survive_ack_and_idle_watcher() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
    let defaults = IndexOptions::new(root.clone());
    let (_, old) = crate::index_coordinator::reconcile_workspace(
        &store,
        &defaults,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    drop(old);
    let mut head = defaults.clone();
    head.manifest_path = Some(root.join("optional-manifest.json"));
    let queued = store.enqueue_request(&head, None).unwrap();
    let state = new(
        store.clone(),
        defaults,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    state.worker_step().unwrap();
    let done = store.request_by_id(&queued.id).unwrap().unwrap();
    assert_eq!(done.state, "done");
    let pin = done.revision.unwrap();
    assert_eq!(store.status().unwrap().revision, pin);
    assert_eq!(
        store
            .recorded_index_options()
            .unwrap()
            .unwrap()
            .manifest_path,
        head.manifest_path
    );
    state.worker_step().unwrap();
    assert_eq!(
        store.status().unwrap().revision,
        pin,
        "idle watcher must not silently switch to daemon defaults"
    );
    assert_eq!(
        store
            .recorded_index_options()
            .unwrap()
            .unwrap()
            .manifest_path,
        head.manifest_path
    );
}

#[test]
fn takeover_retains_reconciled_owner_before_done_becomes_visible() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
    let options = IndexOptions::new(root);
    let state = new(
        store.clone(),
        options.clone(),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    assert!(state.retained_serving_session().is_err());
    let accepted = store.enqueue_request(&options, None).unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(0);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(0);
    state.test_queue_after_startup_drain.set(move || {
        // The terminal row is committed, but the worker step has not returned.
        done_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    let worker = state.clone();
    let tick = std::thread::spawn(move || worker.worker_step());
    done_rx
        .recv_timeout(crate::testing::HANG_BACKSTOP)
        .expect("takeover did not commit the accepted head");
    let observed_row = store.request_by_id(&accepted.id);
    let observed_status = store.status();
    let observed_leader = state
        .retained_serving_session()
        .is_ok_and(|s| s.verify().is_ok());
    // Release and join even if an observed value is wrong. The old ordering
    // fails below without stranding the worker on this test's channel.
    let release_result = release_tx.send(());
    let tick_result = tick.join();
    release_result.unwrap();
    tick_result.unwrap().unwrap();
    let row = observed_row.unwrap().unwrap();
    assert_eq!(row.state, "done");
    assert_eq!(observed_status.unwrap().revision, row.revision.unwrap());
    assert!(
        observed_leader,
        "a committed done row must not precede retention of its reconciled owner"
    );
}

#[tokio::test]
async fn daemon_local_writer_validates_selected_head_before_draining_queue() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
    let options = IndexOptions::new(root);
    let (old_pin, prior) = crate::index_coordinator::reconcile_workspace(
        &store,
        &options,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    drop(prior);
    let state = new(
        store.clone(),
        options.clone(),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    let accepted = store.enqueue_request(&options, None).unwrap();
    state
        .pending_requests
        .lock()
        .unwrap()
        .push(accepted.id.clone());
    state.worker_step().unwrap();
    let session = state.retained_serving_session().unwrap();
    store.verify_writer_session(&session).unwrap();
    let completed = store.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(completed.state, "done");
    // The selected startup reconciliation must publish before the accepted FIFO request.
    assert_eq!(
        completed.revision.unwrap().index_revision,
        old_pin.index_revision + 2
    );
}

#[tokio::test]
async fn failed_pending_read_preserves_id_until_durable_completion_is_observed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
    let options = IndexOptions::new(root);
    let state = new(
        store.clone(),
        options.clone(),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    let request = store.enqueue_request(&options, None).unwrap();
    state
        .pending_requests
        .lock()
        .unwrap()
        .push(request.id.clone());
    state.test_pending_read_failures.store(1, Ordering::Release);
    state.worker_step().unwrap();
    assert_eq!(
        *state.pending_requests.lock().unwrap(),
        vec![request.id.clone()]
    );
    let completed = store.request_by_id(&request.id).unwrap().unwrap();
    assert_eq!(completed.state, "done");
    state.worker_step().unwrap();
    assert!(state.pending_requests.lock().unwrap().is_empty());
    let done = store.request_by_id(&request.id).unwrap().unwrap();
    assert_eq!(done.revision, completed.revision);
    assert_eq!(
        (done.id, done.seq, done.state.as_str()),
        (request.id, request.seq, "done")
    );
    store
        .verify_writer_session(&state.retained_serving_session().unwrap())
        .unwrap();
}

#[test]
fn closed_stderr_pipe_cannot_stop_queue_diagnostics() {
    struct ClosedPipe;
    impl std::io::Write for ClosedPipe {
        fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    best_effort_queue_stderr(
        ClosedPipe,
        format_args!("checkout worker step failed: storage_busy\n"),
    );
    best_effort_queue_stderr(
        ClosedPipe,
        format_args!("queue takeover failed for accepted ID\n"),
    );
    // Reaching here proves a broken diagnostic pipe cannot panic the
    // timer thread or prevent its next accepted-work tick.
}

#[tokio::test]
async fn verified_writer_busy_requeue_gets_bounded_backoff_before_same_ack_retry() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
    let options = IndexOptions::new(root.clone());
    let (old_pin, owner) = crate::index_coordinator::reconcile_workspace(
        &store,
        &options,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let mut options = options;
    options.max_file_bytes = 1024;
    let state = new(
        store.clone(),
        options.clone(),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    state.retain_serving_session_without_tick_for_tests(owner);
    let accepted = store.enqueue_request(&options, None).unwrap();
    store.fail_next_live_publish_commit_busy();
    let retry_started = Instant::now();
    state.worker_step().unwrap();
    let retry_finished = Instant::now();
    let scheduled_retry = *state.recovery_retry_after.lock().unwrap();
    assert!(
        scheduled_retry.is_some_and(|when| {
            when >= retry_started && when <= retry_finished + Duration::from_millis(250)
        }),
        "BUSY requeue must schedule its bounded 250ms retry"
    );
    let deferred = store.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(
        (deferred.seq, deferred.state.as_str()),
        (accepted.seq, "queued")
    );
    assert!(deferred.finished_at.is_none() && deferred.error_code.is_none());
    assert_eq!(store.index_baseline().unwrap(), old_pin);
    // This is the daemon-owned selected read path, not a second Store or
    // SQLite opener. A deferred typed BUSY must not hide the prior head.
    let selected = store.evidence_response().unwrap();
    let (served_pin, source) = selected.source_at("a.js", Some(old_pin)).unwrap().unwrap();
    assert_eq!(served_pin, old_pin);
    assert_eq!(source.text, "function a() {}\n");
    selected.finish(()).unwrap();
    drop(selected);
    *state.recovery_retry_after.lock().unwrap() =
        Some(Instant::now() + crate::testing::HANG_BACKSTOP);
    state.worker_step().unwrap();
    assert_eq!(
        store.request_by_id(&accepted.id).unwrap().unwrap().state,
        "queued"
    );
    *state.recovery_retry_after.lock().unwrap() = None;
    state.worker_step().unwrap();
    let done = store.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(done.state, "done");
    assert_eq!(done.seq, accepted.seq);
    assert_eq!(
        done.revision.unwrap().index_revision,
        old_pin.index_revision + 1
    );
}

#[test]
fn typed_fifo_head_read_busy_defers_startup_and_retries_same_accepted_id() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
    let defaults = IndexOptions::new(root.clone());
    let (old_pin, owner) = crate::index_coordinator::reconcile_workspace(
        &store,
        &defaults,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    drop(owner);
    let mut requested = defaults.clone();
    requested.max_file_bytes = 4096;
    let accepted = store.enqueue_request(&requested, None).unwrap();
    let state = new(
        store.clone(),
        defaults,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    // A local accepted ACK bypasses the earlier !pending_local probe.
    state
        .pending_requests
        .lock()
        .unwrap()
        .push(accepted.id.clone());
    state
        .test_activation_head_busy_once
        .store(true, Ordering::Release);
    let started = Instant::now();
    state.worker_step().unwrap();
    let finished = Instant::now();
    assert!(
        state
            .recovery_retry_after
            .lock()
            .unwrap()
            .is_some_and(|at| { at >= started && at <= finished + Duration::from_millis(250) }),
        "typed FIFO head-read BUSY must schedule the 250ms retry"
    );
    assert!(
        state.runtime.lock().unwrap().session().is_none(),
        "H must not start with unknown head"
    );
    assert_eq!(store.index_baseline().unwrap(), old_pin);
    let queued = store.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(
        (
            queued.id.as_str(),
            queued.seq,
            queued.state.as_str(),
            queued.options_json.as_str()
        ),
        (
            accepted.id.as_str(),
            accepted.seq,
            "queued",
            accepted.options_json.as_str()
        )
    );
    assert!(queued.finished_at.is_none() && queued.revision.is_none());
    // The immediate timer tick honors the backoff without claiming Q.
    *state.recovery_retry_after.lock().unwrap() =
        Some(Instant::now() + crate::testing::HANG_BACKSTOP);
    state.worker_step().unwrap();
    assert_eq!(
        store.request_by_id(&accepted.id).unwrap().unwrap().state,
        "queued"
    );
    *state.recovery_retry_after.lock().unwrap() = None;
    state.worker_step().unwrap();
    let done = store.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(
        (
            done.id.as_str(),
            done.seq,
            done.state.as_str(),
            done.options_json.as_str()
        ),
        (
            accepted.id.as_str(),
            accepted.seq,
            "done",
            accepted.options_json.as_str()
        )
    );
    assert!(done.revision.unwrap().index_revision > old_pin.index_revision);
}

#[tokio::test]
async fn transient_publish_commit_busy_cannot_fail_accepted_takeover_head() {
    use crate::store::topology::{TopologyRoots, WorkspaceIdentity};
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let state_root = tmp.path().join("state");
    let original = Store::open_for_tests(&state_root, &root).unwrap();
    let options = IndexOptions::new(root.clone());
    let (old_pin, owner) = crate::index_coordinator::reconcile_workspace(
        &original,
        &options,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(state_root.join("cache"), state_root.join("data"));
    let follower_store = Store::open_for_tests(&state_root, &root).unwrap();
    let state = new(
        follower_store.clone(),
        options.clone(),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    let ack = follower_store.enqueue_request(&options, None).unwrap();
    state.pending_requests.lock().unwrap().push(ack.id.clone());
    let encoded = serde_json::to_string(&crate::indexer::ReconcileOptions::from(&options)).unwrap();
    // This legacy fixture emits the plain-string pre-COMMIT BUSY. It is
    // not the direct typed mandatory startup reconciliation retry hook.
    follower_store.fail_next_live_publish_commit_busy();
    drop(owner);
    // Plain-string BUSY can be retried inside the same local writer tick.
    // Typed BUSY has its own test below for a mandatory startup reconciliation backoff.
    state.worker_step().unwrap();
    let after_busy = follower_store.request_by_id(&ack.id).unwrap().unwrap();
    assert_eq!(
        (
            after_busy.id.as_str(),
            after_busy.seq,
            after_busy.options_json.as_str()
        ),
        (ack.id.as_str(), ack.seq, encoded.as_str())
    );
    assert_ne!(after_busy.state, "failed", "BUSY cannot fail accepted work");
    if after_busy.state == "queued" {
        assert!(after_busy.revision.is_none() && after_busy.finished_at.is_none());
        assert_eq!(
            follower_store.index_baseline().unwrap(),
            old_pin,
            "pre-COMMIT BUSY cannot publish part of startup reconciliation or Q1"
        );
        *state.recovery_retry_after.lock().unwrap() = None;
        state.worker_step().unwrap();
    }
    let done = follower_store.request_by_id(&ack.id).unwrap().unwrap();
    assert_eq!(
        (
            done.id.as_str(),
            done.seq,
            done.options_json.as_str(),
            done.state.as_str()
        ),
        (ack.id.as_str(), ack.seq, encoded.as_str(), "done")
    );
    assert!(done.finished_at.is_some() && done.error_code.is_none());
    let done_pin = done.revision.unwrap();
    let startup_pin = crate::model::IndexPin {
        index_generation: old_pin.index_generation,
        index_revision: old_pin.index_revision + 1,
    };
    assert_eq!(done_pin.index_generation, startup_pin.index_generation);
    assert_eq!(done_pin.index_revision, startup_pin.index_revision + 1);
    assert_eq!(follower_store.status().unwrap().revision, done_pin);
    assert!(state.retained_serving_session().unwrap().verify().is_ok());
    // Post-ready historical read authenticates the distinct startup reconciliation publication
    // before Q1. Drop the reader before opening native metadata.
    let read = follower_store.evidence_response().unwrap();
    read.validate_pin(old_pin).unwrap();
    read.validate_pin(startup_pin).unwrap();
    read.validate_pin(done_pin).unwrap();
    assert_eq!(
        read.source_at("a.js", Some(startup_pin))
            .unwrap()
            .unwrap()
            .1
            .text,
        "function a() {}\n"
    );
    assert_eq!(
        read.source_at("a.js", Some(done_pin))
            .unwrap()
            .unwrap()
            .1
            .text,
        "function a() {}\n"
    );
    read.finish(()).unwrap();
    drop(read);
    let db = rusqlite::Connection::open_with_flags(
        roots.index_db(&identity),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let key = format!(
        "pin:v1:{}:{}",
        startup_pin.index_generation, startup_pin.index_revision
    );
    let startup_json: String = db.query_row(
            "SELECT reconcile_options FROM native_revisions WHERE id=?1 AND published_index_revision=?2",
            rusqlite::params![key, startup_pin.index_revision as i64],
            |row| row.get(0),
        ).unwrap();
    assert_eq!(
        serde_json::from_str::<crate::indexer::ReconcileOptions>(&startup_json).unwrap(),
        crate::indexer::ReconcileOptions::from(&options),
        "immutable startup reconciliation pin must retain persisted selected options"
    );
}

#[tokio::test]
async fn selected_startup_failure_must_not_fail_valid_q1_before_reconciliation() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let state_root = tmp.path().join("state");
    let original = Store::open_for_tests(&state_root, &root).unwrap();
    let mut h = IndexOptions::new(root.clone());
    h.max_file_bytes = 128;
    let mut q1_options = h.clone();
    q1_options.max_file_bytes = 512;
    let (prior, old_owner) = crate::index_coordinator::reconcile_workspace(
        &original,
        &h,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let store = Store::open_for_tests(&state_root, &root).unwrap();
    let state = new(
        store.clone(),
        IndexOptions::new(root.clone()),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    let q1 = store.enqueue_request(&q1_options, None).unwrap();
    state.pending_requests.lock().unwrap().push(q1.id.clone());
    let encoded =
        serde_json::to_string(&crate::indexer::ReconcileOptions::from(&q1_options)).unwrap();
    assert_eq!(q1.options_json, encoded);
    // startup reconciliation cannot admit this source, but Q1's own larger limit could.
    let large = format!("function a() {{}}\n// {}\n", "x".repeat(180));
    assert!(large.len() > 128 && large.len() <= 512);
    fs::write(root.join("a.js"), large).unwrap();
    drop(old_owner);
    let error = state.worker_step().unwrap_err();
    assert!(format!("{error:#}").contains("unsafe or oversized input"));
    let row = store.request_by_id(&q1.id).unwrap().unwrap();
    assert_eq!(
        (
            row.id.as_str(),
            row.seq,
            row.options_json.as_str(),
            row.state.as_str()
        ),
        (q1.id.as_str(), q1.seq, encoded.as_str(), "queued")
    );
    assert!(row.revision.is_none() && row.finished_at.is_none() && row.error_code.is_none());
    assert_eq!(store.index_baseline().unwrap(), prior);
    assert!(state.retained_serving_session().is_err());
    assert!(state.watcher_work.lock().unwrap().is_none());
}

#[tokio::test]
async fn selected_startup_precommit_busy_retries_startup_before_fifo() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    let path = root.join("a.js");
    fs::write(&path, "function a() {}\n").unwrap();
    let state_root = tmp.path().join("state");
    let original = Store::open_for_tests(&state_root, &root).unwrap();
    let mut h = IndexOptions::new(root.clone());
    h.max_file_bytes = 128;
    let mut a = h.clone();
    a.max_file_bytes = 32;
    let mut b = h.clone();
    b.max_file_bytes = 512;
    let (prior, old_owner) = crate::index_coordinator::reconcile_workspace(
        &original,
        &h,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let store = Store::open_for_tests(&state_root, &root).unwrap();
    let state = new(
        store.clone(),
        IndexOptions::new(root.clone()),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    let q1 = store.enqueue_request(&a, None).unwrap();
    let q2 = store.enqueue_request(&b, None).unwrap();
    assert!(q1.seq < q2.seq && q1.id != q2.id);
    let encoded_a = serde_json::to_string(&crate::indexer::ReconcileOptions::from(&a)).unwrap();
    let encoded_b = serde_json::to_string(&crate::indexer::ReconcileOptions::from(&b)).unwrap();
    state
        .pending_requests
        .lock()
        .unwrap()
        .extend([q1.id.clone(), q2.id.clone()]);
    fs::write(&path, "function b() {}\n").unwrap();
    store.fail_next_live_publish_commit_typed_busy();
    drop(old_owner);
    state.worker_step().unwrap(); // RetryMandatory, not an ACK or retained owner.
    for (accepted, options) in [(&q1, &encoded_a), (&q2, &encoded_b)] {
        let row = store.request_by_id(&accepted.id).unwrap().unwrap();
        assert_eq!(
            (
                row.id.as_str(),
                row.seq,
                row.options_json.as_str(),
                row.state.as_str()
            ),
            (
                accepted.id.as_str(),
                accepted.seq,
                options.as_str(),
                "queued"
            )
        );
        assert!(row.revision.is_none() && row.finished_at.is_none() && row.error_code.is_none());
    }
    assert_eq!(store.index_baseline().unwrap(), prior);
    assert!(state.retained_serving_session().is_err());
    assert!(state.watcher_work.lock().unwrap().is_none());
    assert!(state.recovery_retry_after.lock().unwrap().is_some());
    // Only the advisory timer is advanced. Success requires a fresh startup reconciliation
    // publication and the same accepted FIFO rows, not elapsed time.
    *state.recovery_retry_after.lock().unwrap() = Some(Instant::now() - Duration::from_secs(1));
    state.worker_step().unwrap();
    let first = store.request_by_id(&q1.id).unwrap().unwrap();
    let second = store.request_by_id(&q2.id).unwrap().unwrap();
    assert_eq!(
        (
            first.id.as_str(),
            first.seq,
            first.options_json.as_str(),
            first.state.as_str()
        ),
        (q1.id.as_str(), q1.seq, encoded_a.as_str(), "done")
    );
    assert_eq!(
        (
            second.id.as_str(),
            second.seq,
            second.options_json.as_str(),
            second.state.as_str()
        ),
        (q2.id.as_str(), q2.seq, encoded_b.as_str(), "queued")
    );
    assert!(first.finished_at.is_some() && first.error_code.is_none());
    let q1_pin = first.revision.unwrap();
    assert!(q1_pin.index_revision > prior.index_revision + 1);
    assert_eq!(store.status().unwrap().revision, q1_pin);
    assert!(state.retained_serving_session().unwrap().verify().is_ok());
    state.worker_step().unwrap();
    let finished_q1 = store.request_by_id(&q1.id).unwrap().unwrap();
    let finished_q2 = store.request_by_id(&q2.id).unwrap().unwrap();
    assert_eq!(
        (finished_q1.state.as_str(), finished_q1.revision),
        ("done", Some(q1_pin))
    );
    assert_eq!(
        (
            finished_q2.id.as_str(),
            finished_q2.seq,
            finished_q2.options_json.as_str(),
            finished_q2.state.as_str()
        ),
        (q2.id.as_str(), q2.seq, encoded_b.as_str(), "done")
    );
    assert!(finished_q2.finished_at.is_some() && finished_q2.error_code.is_none());
    assert_eq!(
        finished_q2.revision.unwrap().index_revision,
        q1_pin.index_revision + 1
    );
    assert_eq!(
        store.status().unwrap().revision,
        finished_q2.revision.unwrap()
    );
}

#[tokio::test]
async fn local_writer_uses_claimed_clients_options_not_daemon_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let state_root = tmp.path().join("state");
    let owner_store = Store::open_for_tests(&state_root, &root).unwrap();
    let own_options = IndexOptions::new(root.clone());
    let (_, owner) = crate::index_coordinator::reconcile_workspace(
        &owner_store,
        &own_options,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let follower_store = Store::open_for_tests(&state_root, &root).unwrap();
    let mut bad_daemon_options = own_options.clone();
    bad_daemon_options.scip_path = Some(root.clone()); // directory is an invalid optional input
    let state = new(
        follower_store.clone(),
        bad_daemon_options,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    let client = follower_store.enqueue_request(&own_options, None).unwrap();
    state
        .pending_requests
        .lock()
        .unwrap()
        .push(client.id.clone());
    drop(owner);
    state.worker_step().unwrap();
    let done = follower_store.request_by_id(&client.id).unwrap().unwrap();
    assert_eq!(
        done.state, "done",
        "a client's valid inputs may not be failed by unrelated daemon defaults"
    );
    assert!(done.error_code.is_none() && done.revision.is_some());
    assert!(state.retained_serving_session().unwrap().verify().is_ok());
}

#[test]
fn root_loss_settles_accepted_queue_and_retires_local_writer() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function before() {}\n").unwrap();
    let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
    let options = IndexOptions::new(root.clone());
    let state = new(
        store.clone(),
        options.clone(),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    let accepted = store.enqueue_request(&options, None).unwrap();
    state.worker_step().unwrap();
    assert_eq!(
        store.request_by_id(&accepted.id).unwrap().unwrap().state,
        "done"
    );
    let next = store.enqueue_request(&options, None).unwrap();
    fs::rename(&root, tmp.path().join("former-workspace")).unwrap();
    fs::create_dir(&root).unwrap();
    state.worker_step().unwrap();
    // Normal reads reject a replaced captured root. Inspect only the durable
    // request row to prove settlement survived loss of that root.
    let db = rusqlite::Connection::open(store.request_db_path()).unwrap();
    let (status, error): (String, Option<String>) = db
        .query_row(
            "SELECT state, error_code FROM requests WHERE id=?1",
            [&next.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "failed");
    assert_eq!(error.as_deref(), Some("root_changed"));
    assert_eq!(
        state.root_loss_retirement_for_tests(),
        (false, false, false)
    );
}

#[test]
fn empty_fifo_busy_retries_selected_b_before_read() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    let source = root.join("a.js");
    fs::write(&source, "function old() {}\n").unwrap();
    let state_root = tmp.path().join("state");
    let original = Store::open_for_tests(&state_root, &root).unwrap();
    let mut selected_b = IndexOptions::new(root.clone());
    selected_b.max_file_bytes = 4096;
    let (old_pin, owner) = crate::index_coordinator::reconcile_workspace(
        &original,
        &selected_b,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let follower_store = Store::open_for_tests(&state_root, &root).unwrap();
    let daemon_defaults_a = IndexOptions::new(root.clone());
    let state = new(
        follower_store.clone(),
        daemon_defaults_a,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    assert!(follower_store.current_request().unwrap().is_none());
    fs::write(&source, "function after_busy() {}\n").unwrap();
    // Typed pre-COMMIT BUSY defers mandatory startup reconciliation without exposing a
    // partially committed selected revision.
    follower_store.fail_next_live_publish_commit_typed_busy();
    drop(owner);
    // A due startup reconciliation admits a local writer even without a
    // queued request; a bare timer tick has no reason to start one.
    *state.runtime.lock().unwrap() = RuntimeState::Reconciling {
        attempts: 0,
        waiting: false,
    };
    // Typed pre-COMMIT contention is deferred with a 250ms deadline;
    // inspect the scheduled bound before diagnostic reads can outlast it.
    let retry_started = Instant::now();
    state.worker_step().unwrap();
    let retry_finished = Instant::now();
    let scheduled_retry = *state.recovery_retry_after.lock().unwrap();
    assert!(
        scheduled_retry.is_some_and(|t| {
            t >= retry_started && t <= retry_finished + Duration::from_millis(250)
        }),
        "failed empty-FIFO reconciliation must schedule its bounded 250ms retry"
    );
    assert_eq!(
        follower_store.index_baseline().unwrap(),
        old_pin,
        "pre-COMMIT BUSY must not publish a partial revision"
    );
    assert!(follower_store.current_request().unwrap().is_none());
    // The reads above may run beyond 250ms under CI load. Hold the same
    // retry branch open for the immediate-tick assertion without a clock race.
    *state.recovery_retry_after.lock().unwrap() =
        Some(Instant::now() + crate::testing::HANG_BACKSTOP);
    state.worker_step().unwrap();
    *state.recovery_retry_after.lock().unwrap() = None;
    state.worker_step().unwrap();
    let selected = follower_store.status().unwrap().revision;
    assert_eq!(selected.index_generation, old_pin.index_generation);
    assert!(selected.index_revision > old_pin.index_revision);
    assert!(
        state
            .runtime
            .lock()
            .unwrap()
            .session()
            .unwrap()
            .verify()
            .is_ok()
    );
    assert_eq!(
        follower_store
            .recorded_index_options()
            .unwrap()
            .unwrap()
            .max_file_bytes,
        4096
    );
    let read = follower_store.evidence_response().unwrap();
    assert_eq!(
        read.source_at("a.js", Some(selected))
            .unwrap()
            .unwrap()
            .1
            .text,
        "function after_busy() {}\n"
    );
    read.finish(()).unwrap();
    assert!(follower_store.current_request().unwrap().is_none());
}

#[tokio::test]
async fn failed_startup_reconciliation_backs_off_without_failing_fifo_head() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    let source = root.join("a.js");
    fs::write(&source, "function a() {}\n").unwrap();
    let state_root = tmp.path().join("state");
    let owner_store = Store::open_for_tests(&state_root, &root).unwrap();
    let options = IndexOptions::new(root.clone());
    let (old_pin, owner) = crate::index_coordinator::reconcile_workspace(
        &owner_store,
        &options,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let follower_store = Store::open_for_tests(&state_root, &root).unwrap();
    let state = new(
        follower_store.clone(),
        options.clone(),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    let accepted = follower_store.enqueue_request(&options, None).unwrap();
    state
        .pending_requests
        .lock()
        .unwrap()
        .push(accepted.id.clone());
    let encoded = serde_json::to_string(&crate::indexer::ReconcileOptions::from(&options)).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(
        fs::read(&source).is_err(),
        "fixture must fail real captured source reads"
    );
    drop(owner);
    let before_tick = Instant::now();
    let error = state
        .worker_step()
        .expect_err("unreadable selected startup reconciliation must fail before Q1 claim");
    assert!(
        format!("{error:#}").contains("opening input:"),
        "mandatory startup reconciliation must fail its captured source read"
    );
    let deadline = state
        .recovery_retry_after
        .lock()
        .unwrap()
        .expect("H nonbusy failure must set an advisory retry");
    assert!(
        deadline > before_tick,
        "H retry must have a future advisory deadline"
    );
    assert!(deadline <= Instant::now() + Duration::from_millis(500));
    let after_first = follower_store.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(
        (
            after_first.id.as_str(),
            after_first.seq,
            after_first.options_json.as_str(),
            after_first.state.as_str()
        ),
        (
            accepted.id.as_str(),
            accepted.seq,
            encoded.as_str(),
            "queued"
        ),
        "mandatory startup reconciliation capture failure cannot be attributed to Q1"
    );
    assert!(
        after_first.revision.is_none()
            && after_first.finished_at.is_none()
            && after_first.error_code.is_none()
    );
    assert_eq!(follower_store.index_baseline().unwrap(), old_pin);
    assert!(state.retained_serving_session().is_err());
    assert!(state.watcher_work.lock().unwrap().is_none());
    // Pin the advisory deadline beyond these immediate calls so CPU load
    // cannot turn a bounded-rate assertion into elapsed-time flakiness.
    *state.recovery_retry_after.lock().unwrap() =
        Some(Instant::now() + crate::testing::HANG_BACKSTOP);
    for _ in 0..3 {
        state.worker_step().unwrap();
    }
    assert_eq!(
        follower_store
            .request_by_id(&accepted.id)
            .unwrap()
            .unwrap()
            .state,
        "queued"
    );
    fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
    // Repair startup reconciliation's source, then advance ONLY the advisory retry timer.
    *state.recovery_retry_after.lock().unwrap() = Some(Instant::now() - Duration::from_secs(1));
    state.worker_step().unwrap();
    let done = follower_store.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(
        (
            done.id.as_str(),
            done.seq,
            done.options_json.as_str(),
            done.state.as_str()
        ),
        (accepted.id.as_str(), accepted.seq, encoded.as_str(), "done")
    );
    assert!(done.finished_at.is_some() && done.error_code.is_none());
    let q1_pin = done.revision.unwrap();
    let startup_pin = crate::model::IndexPin {
        index_generation: old_pin.index_generation,
        index_revision: old_pin.index_revision + 1,
    };
    assert_eq!(q1_pin.index_generation, startup_pin.index_generation);
    assert_eq!(q1_pin.index_revision, startup_pin.index_revision + 1);
    let read = follower_store.evidence_response().unwrap();
    read.validate_pin(startup_pin).unwrap();
    read.validate_pin(q1_pin).unwrap();
    assert_eq!(read.status().unwrap().revision, q1_pin);
    read.finish(()).unwrap();
    drop(read);
    assert!(state.retained_serving_session().unwrap().verify().is_ok());
}
