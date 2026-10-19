use super::*;
use crate::store::topology::{TopologyRoots, WorkspaceIdentity};

fn fixture() -> (
    tempfile::TempDir,
    Store,
    Arc<DaemonState>,
    TopologyRoots,
    WorkspaceIdentity,
) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let roots = TopologyRoots::isolated_for_tests(
        tmp.path().join("state/cache"),
        tmp.path().join("state/data"),
    );
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let old = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
    // An accepted Q0 creates the durable queue witness before damage to
    // derived index.db. Recovery must retain and drain it ahead of later IDs.
    old.enqueue_request(&IndexOptions::new(root.clone()), None)
        .unwrap();
    drop(old);
    std::fs::write(roots.index_db(&identity), b"bad sqlite index header").unwrap();
    let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
    assert!(store.is_recreate_pending());
    let state = new(
        store.clone(),
        IndexOptions::new(root),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    // These tests step the worker by hand: no background worker thread.
    state.worker_started.store(true, Ordering::Release);
    (tmp, store, state, roots, identity)
}

#[test]
fn damaged_index_without_accepted_head_never_guesses_startup_options() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let state_root = tmp.path().join("state");
    let roots =
        TopologyRoots::isolated_for_tests(state_root.join("cache"), state_root.join("data"));
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let old = Store::open_for_tests(&state_root, &root).unwrap();
    let options = IndexOptions::new(root.clone());
    let (_, owner) = crate::index_coordinator::reconcile_workspace(
        &old,
        &options,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert!(old.current_request().unwrap().is_none());
    drop(owner);
    drop(old);
    let index = roots.index_db(&identity);
    let damaged = b"bad sqlite index header";
    std::fs::write(&index, damaged).unwrap();
    let store = Store::open_for_tests(&state_root, &root).unwrap();
    assert!(store.is_recreate_pending());
    assert!(store.earliest_unfinished_request().unwrap().is_none());
    let state = new(
        store.clone(),
        options,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    let started = Instant::now();
    let error = state.worker_step().unwrap_err();
    let finished = Instant::now();
    assert!(
        error.to_string().starts_with("recovery_required:"),
        "{error:#}"
    );
    assert!(
        state
            .recovery_retry_after
            .lock()
            .unwrap()
            .is_some_and(|at| { at >= started && at <= finished + Duration::from_millis(500) })
    );
    assert!(state.runtime.lock().unwrap().session().is_none());
    assert!(store.current_request().unwrap().is_none());
    assert_eq!(std::fs::read(index).unwrap(), damaged);
}

#[test]
fn damaged_index_reconciles_startup_then_existing_q0_once() {
    let (_tmp, store, state, roots, identity) = fixture();
    let index = roots.index_db(&identity);
    let damaged = std::fs::read(&index).unwrap();
    let q0 = store.current_request().unwrap().unwrap();
    assert_eq!(q0.state, "queued");
    state.worker_step().unwrap();
    assert!(!store.is_recreate_pending());
    store
        .verify_writer_session(&state.retained_serving_session().unwrap())
        .unwrap();
    let done = store.request_by_id(&q0.id).unwrap().unwrap();
    assert_eq!(done.state, "done");
    let pin = done.revision.unwrap();
    assert_eq!(pin.index_revision, 2, "startup r1 must precede Q0 r2");
    assert_ne!(std::fs::read(&index).unwrap(), damaged);
    state.worker_step().unwrap();
    assert_eq!(store.status().unwrap().revision, pin);
}

#[tokio::test]
async fn damaged_index_drains_earlier_q0_before_browser_ack() {
    let (_tmp, store, state, _roots, _identity) = fixture();
    let q0 = store.current_request().unwrap().unwrap();
    let (code, Json(browser)) = start_index(State(state.clone()), Bytes::new())
        .await
        .unwrap();
    assert_eq!(code, StatusCode::ACCEPTED);
    let q1 = store.request_by_id(&browser.id).unwrap().unwrap();
    assert!(q0.seq < q1.seq);
    assert_eq!(q1.state, "queued");
    state.worker_step().unwrap();
    let first = store.request_by_id(&q0.id).unwrap().unwrap();
    let pending = store.request_by_id(&q1.id).unwrap().unwrap();
    assert_eq!(
        (first.state.as_str(), pending.state.as_str()),
        ("done", "queued")
    );
    assert_eq!(first.revision.unwrap().index_revision, 2);
    state.worker_step().unwrap();
    let second = store.request_by_id(&q1.id).unwrap().unwrap();
    assert_eq!(
        (second.id.as_str(), second.seq, second.state.as_str()),
        (q1.id.as_str(), q1.seq, "done")
    );
    assert_eq!(second.revision.unwrap().index_revision, 3);
    assert_eq!(
        first.revision.unwrap().index_generation,
        second.revision.unwrap().index_generation
    );
    assert_eq!(second.options_json, q1.options_json);
    store
        .verify_writer_session(&state.retained_serving_session().unwrap())
        .unwrap();
}

struct PausedRecoveryFixture {
    _tmp: tempfile::TempDir,
    store: Store,
    state: Arc<DaemonState>,
    roots: TopologyRoots,
    identity: WorkspaceIdentity,
    old_owner: Option<Arc<crate::store::topology::WriterSession>>,
    q2: crate::store::requests::Request,
    q1: crate::store::requests::Request,
    old_generation: uuid::Uuid,
    index_path: std::path::PathBuf,
}

fn paused_recovery_fixture() -> PausedRecoveryFixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let roots = TopologyRoots::isolated_for_tests(
        tmp.path().join("state/cache"),
        tmp.path().join("state/data"),
    );
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let index_path = roots.index_db(&identity);
    let old = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
    let options = IndexOptions::new(root.clone());
    let cancel = Arc::new(AtomicBool::new(false));
    let (pin, old_owner) =
        crate::index_coordinator::reconcile_workspace(&old, &options, &cancel, |_| {}).unwrap();
    let q2 = old.enqueue_request(&options, None).unwrap();
    assert_eq!(old.claim_request(&old_owner).unwrap().unwrap().id, q2.id);
    let q1 = old.enqueue_request(&options, None).unwrap();
    assert!(q2.seq < q1.seq);
    std::fs::write(&index_path, b"bad sqlite index header").unwrap();
    let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
    assert!(store.is_recreate_pending());
    let state = new(
        store.clone(),
        options,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    PausedRecoveryFixture {
        _tmp: tmp,
        store,
        state,
        roots,
        identity,
        old_owner: Some(old_owner),
        q2,
        q1,
        old_generation: pin.index_generation,
        index_path,
    }
}

#[tokio::test]
async fn browser_q1_then_cli_q2_exceptional_fifo_survives_reader_contention() {
    let (_tmp, store, state, roots, identity) = fixture();
    let reader = roots.index_use_existing(&identity).unwrap();
    let q0 = store.current_request().unwrap().unwrap();
    let (status, Json(browser)) = start_index(State(state.clone()), Bytes::new())
        .await
        .unwrap();
    assert_eq!(status, StatusCode::ACCEPTED);
    let q1 = store.request_by_id(&browser.id).unwrap().unwrap();
    let cli = Store::open_for_tests(
        &_tmp.path().join("state"),
        &state.config().options.workspace_root,
    )
    .unwrap();
    let q2 = cli.enqueue_request(&state.config().options, None).unwrap();
    assert!(q0.seq < q1.seq && q1.seq < q2.seq);
    assert_eq!(
        store.request_by_id(&q1.id).unwrap().unwrap().state,
        "queued"
    );
    assert_eq!(
        store.request_by_id(&q2.id).unwrap().unwrap().state,
        "queued"
    );
    drop(reader);
    state.worker_step().unwrap();
    let first = store.request_by_id(&q0.id).unwrap().unwrap();
    assert_eq!(first.state, "done");
    assert_eq!(
        store.request_by_id(&q1.id).unwrap().unwrap().state,
        "queued"
    );
    assert_eq!(
        store.request_by_id(&q2.id).unwrap().unwrap().state,
        "queued"
    );
    state.worker_step().unwrap();
    let browser_done = store.request_by_id(&q1.id).unwrap().unwrap();
    assert_eq!(browser_done.state, "done");
    assert_eq!(
        store.request_by_id(&q2.id).unwrap().unwrap().state,
        "queued"
    );
    state.worker_step().unwrap();
    let cli_done = store.request_by_id(&q2.id).unwrap().unwrap();
    assert_eq!(
        (
            first.state.as_str(),
            browser_done.state.as_str(),
            cli_done.state.as_str()
        ),
        ("done", "done", "done")
    );
    assert_eq!(first.revision.unwrap().index_revision, 2);
    assert_eq!(browser_done.revision.unwrap().index_revision, 3);
    assert_eq!(cli_done.revision.unwrap().index_revision, 4);
    assert_eq!(
        first.revision.unwrap().index_generation,
        browser_done.revision.unwrap().index_generation
    );
    assert_eq!(
        browser_done.revision.unwrap().index_generation,
        cli_done.revision.unwrap().index_generation
    );
    assert_eq!(
        (
            browser_done.id.as_str(),
            browser_done.seq,
            browser_done.options_json.as_str()
        ),
        (q1.id.as_str(), q1.seq, q1.options_json.as_str())
    );
    assert_eq!(
        (
            cli_done.id.as_str(),
            cli_done.seq,
            cli_done.options_json.as_str()
        ),
        (q2.id.as_str(), q2.seq, q2.options_json.as_str())
    );
}

#[tokio::test]
async fn cli_accepted_exceptional_request_without_local_pending_id_must_be_driven() {
    let (_tmp, store, state, _roots, _identity) = fixture();
    let q0 = store.current_request().unwrap().unwrap();
    let accepted = store
        .enqueue_request(&state.config().options, None)
        .unwrap();
    assert!(
        state.pending_requests.lock().unwrap().is_empty(),
        "a separate CLI process cannot populate daemon-local pending_requests"
    );
    let original_queue = std::fs::read(store.request_db_path()).unwrap();
    state.worker_step().unwrap();
    assert_eq!(store.request_by_id(&q0.id).unwrap().unwrap().state, "done");
    assert_eq!(
        store.request_by_id(&accepted.id).unwrap().unwrap().state,
        "queued"
    );
    state.worker_step().unwrap();
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
            accepted.options_json.as_str(),
            "done"
        ),
        "next tick must claim durable CLI row without a daemon-local pending ID"
    );
    assert_eq!(row.revision.unwrap(), store.status().unwrap().revision);
    let prior = store.request_by_id(&q0.id).unwrap().unwrap();
    assert_eq!(prior.state, "done");
    assert_eq!(prior.revision.unwrap().index_revision, 2);
    assert_eq!(
        row.revision.unwrap().index_revision,
        3,
        "startup r1 and earlier accepted Q0 r2 precede CLI r3"
    );
    assert!(state.retained_serving_session().unwrap().verify().is_ok());
    store
        .verify_writer_session(&state.retained_serving_session().unwrap())
        .unwrap();
    assert_ne!(
        std::fs::read(store.request_db_path()).unwrap(),
        original_queue,
        "accepted row needs a durable terminal transition"
    );
}

fn assert_reclaimed_fifo(f: &PausedRecoveryFixture, old_incarnation: uuid::Uuid, queue_inode: u64) {
    use std::os::unix::fs::MetadataExt;
    let q2 = f.store.request_by_id(&f.q2.id).unwrap().unwrap();
    let q1 = f.store.request_by_id(&f.q1.id).unwrap().unwrap();
    assert_eq!((q2.state.as_str(), q1.state.as_str()), ("done", "done"));
    assert_eq!(q2.revision.unwrap().index_revision, 2);
    assert_eq!(q1.revision.unwrap().index_revision, 3);
    assert_eq!(
        q2.revision.unwrap().index_generation,
        q1.revision.unwrap().index_generation
    );
    assert_ne!(q1.revision.unwrap().index_generation, f.old_generation);
    assert_eq!(f.store.status().unwrap().revision, q1.revision.unwrap());
    assert_eq!(
        std::fs::metadata(f.store.request_db_path()).unwrap().ino(),
        queue_inode,
        "index-only recreation must not replace requests.db"
    );
    let session = f.state.retained_serving_session().unwrap();
    f.store.verify_writer_session(&session).unwrap();
    assert_ne!(session.incarnation(), old_incarnation);
    f.store.verify_writer_session(&session).unwrap();
}

#[test]
fn independent_shared_reader_process() {
    use std::io::{Read, Write};
    let Some(path) = std::env::var_os("TRELLIS_TEST_INDEX_USE_SH") else {
        return;
    };
    let guard = crate::store::topology::UseGuard::acquire_existing(
        std::path::Path::new(&path),
        false,
        false,
    )
    .unwrap();
    println!("SH_READY");
    std::io::stdout().flush().unwrap();
    let mut signal = [0];
    std::io::stdin().read_exact(&mut signal).unwrap();
    drop(guard);
}

#[tokio::test]
async fn idle_tick_cannot_drain_request_admitted_after_empty_pending_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let roots = TopologyRoots::isolated_for_tests(
        tmp.path().join("state/cache"),
        tmp.path().join("state/data"),
    );
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let index_path = roots.index_db(&identity);
    let old = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
    let options = IndexOptions::new(root.clone());
    let (prior, old_owner) = crate::index_coordinator::reconcile_workspace(
        &old,
        &options,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    std::fs::write(&index_path, b"bad sqlite index header").unwrap();
    let corrupt_bytes = std::fs::read(&index_path).unwrap();
    let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
    assert!(store.is_recreate_pending());
    let state = new(
        store.clone(),
        options.clone(),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    // Own the old serving SH without starting the periodic timer. Drive the
    // exact empty-snapshot race with one explicitly paused tick instead.
    *state.runtime.lock().unwrap() = RuntimeState::Ready(old_owner);
    let reader = roots.index_use_existing(&identity).unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    state.test_queue_after_pending_snapshot.set(move || {
        let _ = entered_tx.send(());
        release_rx.recv().unwrap();
    });
    let worker = state.clone();
    let idle_tick = tokio::task::spawn_blocking(move || worker.worker_step());
    tokio::time::timeout(crate::testing::HANG_BACKSTOP, entered_rx)
        .await
        .expect("idle tick did not snapshot empty pending list")
        .unwrap();
    let request = store.enqueue_request(&options, None).unwrap();
    state
        .pending_requests
        .lock()
        .unwrap()
        .push(request.id.clone());
    release_tx.send(()).unwrap();
    tokio::time::timeout(crate::testing::HANG_BACKSTOP, idle_tick)
        .await
        .expect("idle tick did not leave the snapshot barrier")
        .unwrap()
        .unwrap();
    let admitted = store.request_by_id(&request.id).unwrap().unwrap();
    assert_eq!(
        admitted.state, "queued",
        "idle tick drained a post-snapshot recovery request"
    );
    assert!(admitted.finished_at.is_none());
    assert!(
        state.retained_serving_session().is_err(),
        "durable post-snapshot ACK prompted EX attempt and released old holder; independent reader still fences recreation"
    );
    let worker = state.clone();
    tokio::time::timeout(
        crate::testing::HANG_BACKSTOP,
        tokio::task::spawn_blocking(move || worker.worker_step()),
    )
    .await
    .expect("recovery tick did not attempt protected EX")
    .unwrap()
    .unwrap();
    assert!(state.retained_serving_session().is_err());
    assert_eq!(std::fs::read(&index_path).unwrap(), corrupt_bytes);
    let blocked = store.request_by_id(&request.id).unwrap().unwrap();
    assert_eq!(blocked.state, "queued");
    assert!(blocked.finished_at.is_none());
    drop(reader);
    // Advance the advisory retry deadline without sleeping. The held SH
    // had fenced EX; the durable row remains queued for this next tick.
    *state.recovery_retry_after.lock().unwrap() = None;
    // A timed-out blocking tick keeps running after Tokio drops its join
    // handle. Record its last completed phase before reporting a RED.
    let retry_stage = Arc::new(std::sync::atomic::AtomicU8::new(0));
    let retry_started = Instant::now();
    {
        let stage = retry_stage.clone();
        state.test_queue_after_stream.set(move || {
            stage.store(1, Ordering::Release);
        });
    }
    {
        let stage = retry_stage.clone();
        state.test_queue_after_pending_snapshot.set(move || {
            stage.store(2, Ordering::Release);
        });
    }
    {
        let stage = retry_stage.clone();
        state.test_queue_after_startup_drain.set(move || {
            stage.store(3, Ordering::Release);
        });
    }
    let retry_terminal = Arc::new(tokio::sync::Notify::new());
    store.notify_on_terminal_request_for_tests(&request.id, retry_terminal.clone());
    let watcher_returned = Arc::new(tokio::sync::Notify::new());
    {
        let returned = watcher_returned.clone();
        state.test_queue_after_watcher_reconcile.set(move || {
            returned.notify_one();
        });
    }
    let worker = state.clone();
    let retry_tick = tokio::task::spawn_blocking(move || worker.worker_step());
    // The accepted ID signals only after its terminal queue COMMIT. The
    // same tick may then spend a full inventory on the watcher while still
    // holding the serial stream; bound those two stages separately.
    tokio::time::timeout(crate::testing::HANG_BACKSTOP, retry_terminal.notified())
        .await
        .expect("accepted request retry did not commit its terminal row");
    tokio::time::timeout(crate::testing::HANG_BACKSTOP, watcher_returned.notified())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "accepted request watcher timed out at stage {} after {}ms (0=before stream, 1=stream, 2=pending snapshot, 3=after startup reconciliation and Q)",
                    retry_stage.load(Ordering::Acquire),
                    retry_started.elapsed().as_millis()
                )
            });
    tokio::time::timeout(crate::testing::HANG_BACKSTOP, retry_tick)
        .await
        .expect("accepted request retry tick did not leave stream after watcher")
        .unwrap()
        .unwrap();
    let done = store.request_by_id(&request.id).unwrap().unwrap();
    assert_eq!(done.state, "done");
    assert_eq!(done.revision.unwrap().index_revision, 2);
    assert_ne!(
        done.revision.unwrap().index_generation,
        prior.index_generation
    );
}

#[tokio::test]
async fn timer_tick_with_independent_shared_reader_defers_without_changing_both_dbs() {
    use std::io::{BufRead, Write};
    use std::os::unix::fs::MetadataExt;
    use std::process::{Command, Stdio};
    let mut fixture = paused_recovery_fixture();
    let old_incarnation = fixture.old_owner.as_ref().unwrap().incarnation();
    let queue_path = fixture.store.request_db_path();
    let queue_inode = std::fs::metadata(&queue_path).unwrap().ino();
    let queue_bytes = std::fs::read(&queue_path).unwrap();
    let corrupt_index = std::fs::read(&fixture.index_path).unwrap();
    let lock_path = fixture.roots.index_use_lock(&fixture.identity);
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        }
    }
    let mut child = ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "http::exceptional_recovery_tests::independent_shared_reader_process",
                "--nocapture",
            ])
            .env("TRELLIS_TEST_INDEX_USE_SH", &lock_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let drain = tokio::task::spawn_blocking(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut ready = Some(ready_tx);
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                break;
            }
            if line.contains("SH_READY")
                && let Some(sender) = ready.take()
            {
                let _ = sender.send(());
            }
        }
        assert!(ready.is_none(), "reader exited before SH_READY");
    });
    tokio::time::timeout(crate::testing::HANG_BACKSTOP, ready_rx)
        .await
        .expect("independent SH reader did not start")
        .unwrap();
    let (selected_tx, selected_rx) = tokio::sync::oneshot::channel();
    let (release_select_tx, release_select_rx) = std::sync::mpsc::channel();
    fixture.store.set_queue_select_hook(move || {
        let _ = selected_tx.send(());
        let _ = release_select_rx.recv();
    });
    // Distinguish a blocked watcher inventory from a later FIFO retry.
    // A timed-out blocking tick continues after its Tokio join is dropped.
    let queue_phase = Arc::new(std::sync::atomic::AtomicU8::new(0));
    let watcher_returned = Arc::new(tokio::sync::Notify::new());
    {
        let phase = queue_phase.clone();
        fixture.state.test_queue_after_startup_drain.set(move || {
            phase.store(1, Ordering::Release);
        });
    }
    {
        let phase = queue_phase.clone();
        fixture
            .state
            .test_queue_before_watcher_reconcile
            .set(move || {
                phase.store(2, Ordering::Release);
            });
    }
    {
        let phase = queue_phase.clone();
        let returned = watcher_returned.clone();
        fixture
            .state
            .test_queue_after_watcher_reconcile
            .set(move || {
                phase.store(3, Ordering::Release);
                returned.notify_one();
            });
    }
    fixture
        .state
        .pending_requests
        .lock()
        .unwrap()
        .push(fixture.q1.id.clone());
    fixture
        .state
        .retain_serving_session(fixture.old_owner.take().unwrap());
    tokio::time::timeout(crate::testing::HANG_BACKSTOP, selected_rx)
        .await
        .expect("20-ms timer did not pause in protected SELECT")
        .unwrap();
    assert_eq!(std::fs::read(&queue_path).unwrap(), queue_bytes);
    assert_eq!(std::fs::read(&fixture.index_path).unwrap(), corrupt_index);
    release_select_tx.send(()).unwrap();
    tokio::time::timeout(crate::testing::HANG_BACKSTOP, async {
        loop {
            if fixture.state.retained_serving_session().is_err()
                && fixture.state.step_lock.try_lock().is_ok()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("tick did not defer after external SH prevented EX");
    assert_eq!(
        std::fs::read(&queue_path).unwrap(),
        queue_bytes,
        "corruption-only EX BUSY must not change queued/running rows"
    );
    assert_eq!(std::fs::read(&fixture.index_path).unwrap(), corrupt_index);
    assert!(
        fixture
            .state
            .pending_requests
            .lock()
            .unwrap()
            .contains(&fixture.q1.id)
    );
    // Use the protected queue read; closing a raw SQLite connection on a
    // live queue inode would drop this process's unrelated SQLite locks.
    for (id, expected) in [(&fixture.q2.id, "running"), (&fixture.q1.id, "queued")] {
        let row = fixture.store.request_by_id(id).unwrap().unwrap();
        assert_eq!(row.state, expected);
        assert!(row.finished_at.is_none());
    }
    let q1_terminal = Arc::new(tokio::sync::Notify::new());
    fixture
        .store
        .notify_on_terminal_request_for_tests(&fixture.q1.id, q1_terminal.clone());
    child.0.stdin.take().unwrap().write_all(b"x").unwrap();
    let child_status = tokio::time::timeout(crate::testing::HANG_BACKSTOP, async {
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("independent SH reader did not exit after release");
    assert!(
        child_status.success(),
        "independent SH reader exited unsuccessfully"
    );
    tokio::time::timeout(crate::testing::HANG_BACKSTOP, drain)
        .await
        .expect("child output drain did not finish")
        .unwrap();
    // The serial timer admits one FIFO row per turn, then reconciles the
    // post-takeover watcher inventory before it may claim the next row.
    // Bound those two independent stages separately. Both notifications
    // come from existing transition hooks, not selected-status SH polling.
    let watcher_returned_in_time =
        tokio::time::timeout(crate::testing::HANG_BACKSTOP, watcher_returned.notified())
            .await
            .is_ok();
    let timeout_result = if watcher_returned_in_time {
        Some(
            tokio::time::timeout(crate::testing::HANG_BACKSTOP, async {
                // The accepted ID signals only after its durable terminal COMMIT.
                // Do not take repeated SH queue reads while the daemon needs EX.
                q1_terminal.notified().await;
                loop {
                    match fixture.store.request_by_id(&fixture.q1.id) {
                        Ok(Some(row)) => break row,
                        Err(error) if error.is::<crate::store::topology::StorageBusy>() => {
                            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        }
                        other => {
                            panic!("accepted Q1 lookup failed after terminal signal: {other:?}")
                        }
                    }
                }
            })
            .await,
        )
    } else {
        None
    };
    let observed = if let Some(Ok(row)) = timeout_result {
        row
    } else {
        let last_seen = fixture
            .store
            .request_by_id(&fixture.q1.id)
            .map(|row| row.map(|row| (row.state, row.error_code, row.finished_at.is_some())));
        let at_deadline = (
            fixture
                .state
                .test_queue_current_phase
                .load(Ordering::Acquire),
            fixture.store.test_pending_completion_probe_for_tests(),
            fixture.state.step_lock.try_lock().is_ok(),
        );
        // Failure-only probe: keep a missed watcher or Q1 deadline RED,
        // then give the writer one quiet second without this test's SH poll.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let after_quiet = fixture
            .store
            .request_by_id(&fixture.q1.id)
            .map(|row| row.map(|row| (row.state, row.finished_at.is_some(), row.revision)));
        let q2 = fixture
            .store
            .request_by_id(&fixture.q2.id)
            .map(|row| row.map(|row| (row.state, row.error_code, row.finished_at.is_some())));
        let owner = fixture
            .state
            .retained_serving_session()
            .map(|session| session.verify().is_ok());
        let retry_ms = fixture
            .state
            .recovery_retry_after
            .lock()
            .unwrap()
            .map(|at| at.saturating_duration_since(Instant::now()).as_millis());
        let pending = fixture.state.pending_requests.lock().unwrap().clone();
        let completion = fixture.store.test_pending_completion_probe_for_tests();
        panic!(
            "same accepted Q1 ID did not finish after watcher reconcile: watcher_returned_in_time={watcher_returned_in_time}, last_row={last_seen:?}, q2={q2:?}, pending_recovery={}, owner_valid={owner:?}, retry_ms={retry_ms:?}, pending_ids={pending:?}, at_deadline={at_deadline:?}, after_quiet={after_quiet:?}, queue_phase={} (0=pre-startup reconciliation, 1=after-startup reconciliation/Q, 2=watcher reconcile, 3=watcher returned), live_phase={} (1=stream, 2=drain, 3=after drain, 4=watcher reconcile, 5=after watcher), completion={completion:?}, tick_started={}, step_lock_free={}, index_revision={:?}",
            fixture.store.is_recreate_pending(),
            queue_phase.load(Ordering::Acquire),
            fixture
                .state
                .test_queue_current_phase
                .load(Ordering::Acquire),
            fixture.state.worker_started.load(Ordering::Acquire),
            fixture.state.step_lock.try_lock().is_ok(),
            fixture
                .store
                .status()
                .map(|status| status.revision.index_revision)
        )
    };
    assert!(
        observed.finished_at.is_some(),
        "same accepted Q1 ID did not retry after external SH release: row={}/{:?}, pending_recovery={}",
        observed.state,
        observed.error_code,
        fixture.store.is_recreate_pending()
    );
    assert_reclaimed_fifo(&fixture, old_incarnation, queue_inode);
}

#[tokio::test]
async fn timer_tick_quiesces_protected_select_before_ex_and_fences_injected_tick() {
    use std::os::unix::fs::MetadataExt;
    let mut fixture = paused_recovery_fixture();
    let old_incarnation = fixture.old_owner.as_ref().unwrap().incarnation();
    let queue_path = fixture.store.request_db_path();
    let queue_inode = std::fs::metadata(&queue_path).unwrap().ino();
    let queue_bytes = std::fs::read(&queue_path).unwrap();
    let corrupt_index = std::fs::read(&fixture.index_path).unwrap();
    let (selected_tx, selected_rx) = tokio::sync::oneshot::channel();
    let (release_select_tx, release_select_rx) = std::sync::mpsc::channel();
    fixture.store.set_queue_select_hook(move || {
        let _ = selected_tx.send(());
        let _ = release_select_rx.recv();
    });
    let (exclusive_tx, exclusive_rx) = tokio::sync::oneshot::channel();
    let (release_ex_tx, release_ex_rx) = std::sync::mpsc::channel();
    fixture.store.set_exclusive_recovery_hook(move || {
        let _ = exclusive_tx.send(());
        let _ = release_ex_rx.recv();
    });
    fixture
        .state
        .pending_requests
        .lock()
        .unwrap()
        .push(fixture.q1.id.clone());
    fixture
        .state
        .retain_serving_session(fixture.old_owner.take().unwrap());
    tokio::time::timeout(crate::testing::HANG_BACKSTOP, selected_rx)
        .await
        .expect("20-ms timer did not reach protected SELECT")
        .unwrap();
    // This is the genuine daemon timer spawn_blocking worker, paused while
    // its short-lived request connection and shared use guard are both open.
    assert!(fixture.state.step_lock.try_lock().is_err());
    assert_eq!(std::fs::read(&queue_path).unwrap(), queue_bytes);
    assert_eq!(std::fs::read(&fixture.index_path).unwrap(), corrupt_index);
    release_select_tx.send(()).unwrap();
    tokio::time::timeout(crate::testing::HANG_BACKSTOP, exclusive_rx)
        .await
        .expect("timer recovery did not acquire fresh EX")
        .unwrap();
    assert!(
        fixture.state.retained_serving_session().is_err(),
        "old Arc must be dropped before EX"
    );
    assert!(
        fixture.state.step_lock.try_lock().is_err(),
        "EX stays inside native stream"
    );
    assert_eq!(std::fs::read(&queue_path).unwrap(), queue_bytes);
    assert_eq!(std::fs::read(&fixture.index_path).unwrap(), corrupt_index);
    let (before_tx, before_rx) = tokio::sync::oneshot::channel();
    let (after_tx, mut after_rx) = tokio::sync::oneshot::channel();
    fixture.state.test_queue_before_stream.set(move || {
        let _ = before_tx.send(());
    });
    fixture.state.test_queue_after_stream.set(move || {
        let _ = after_tx.send(());
    });
    // The production 20-ms timer awaits its first worker; this SECOND call
    // is deliberately injected on the SAME daemon to test the stream gate.
    let state = fixture.state.clone();
    let injected = tokio::task::spawn_blocking(move || state.worker_step());
    tokio::time::timeout(crate::testing::HANG_BACKSTOP, before_rx)
        .await
        .expect("injected same-daemon tick did not reach stream")
        .unwrap();
    assert!(fixture.state.step_lock.try_lock().is_err());
    assert!(
        matches!(
            after_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "injected tick must not open a queue handle while EX is held"
    );
    assert_eq!(std::fs::read(&queue_path).unwrap(), queue_bytes);
    assert_eq!(std::fs::read(&fixture.index_path).unwrap(), corrupt_index);
    let watcher_returned = Arc::new(tokio::sync::Notify::new());
    {
        let returned = watcher_returned.clone();
        fixture
            .state
            .test_queue_after_watcher_reconcile
            .set(move || {
                returned.notify_one();
            });
    }
    release_ex_tx.send(()).unwrap();
    // The first worker step keeps the step lock through its Q2 drain and
    // full post-takeover watcher inventory. The injected tick is eligible
    // for the stream only after that watcher returns.
    tokio::time::timeout(crate::testing::HANG_BACKSTOP, watcher_returned.notified())
        .await
        .expect("timer watcher did not reconcile after EX");
    tokio::time::timeout(crate::testing::HANG_BACKSTOP, after_rx)
        .await
        .expect("injected tick never passed stream after watcher")
        .unwrap();
    tokio::time::timeout(crate::testing::HANG_BACKSTOP, injected)
        .await
        .expect("injected same-daemon tick did not finish")
        .unwrap()
        .unwrap();
    let mut last_seen = None;
    let timeout_result = tokio::time::timeout(crate::testing::HANG_BACKSTOP, async {
        loop {
            match fixture.store.request_by_id(&fixture.q1.id) {
                Ok(Some(row)) if row.finished_at.is_some() => break row,
                Ok(Some(row)) => last_seen = Some((row.state, row.error_code)),
                Err(error) if error.is::<crate::store::topology::StorageBusy>() => {}
                other => panic!("accepted Q1 lookup failed during EX recovery: {other:?}"),
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    let observed = timeout_result.unwrap_or_else(|_| {
        panic!(
            "same queued ID never finished after EX: last_row={last_seen:?}, pending_recovery={}",
            fixture.store.is_recreate_pending()
        )
    });
    assert!(
        observed.finished_at.is_some(),
        "same queued ID never finished after EX: row={}/{:?}, pending_recovery={}",
        observed.state,
        observed.error_code,
        fixture.store.is_recreate_pending()
    );
    assert_reclaimed_fifo(&fixture, old_incarnation, queue_inode);
}

#[tokio::test]
#[cfg(feature = "test-support")]
async fn concurrent_posts_queue_two_requests_across_exceptional_recovery() {
    use tower::ServiceExt;
    let (_tmp, store, state, _roots, _identity) = fixture();
    // The fixture has no worker thread (and so no maintenance): drive one
    // FIFO head per explicit step.
    let q0 = store.current_request().unwrap().unwrap();
    assert_eq!(q0.state, "queued");
    let expected_options = serde_json::to_string(&crate::indexer::ReconcileOptions::from(
        &state.config().options,
    ))
    .unwrap();
    assert_eq!(q0.options_json, expected_options);
    let app = router(state.clone());
    let mut ids = Vec::new();
    for _ in 0..2 {
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/api/index")
            .header("host", "127.0.0.1:7331")
            .header("origin", "http://127.0.0.1:7331")
            .header(
                "authorization",
                "Bearer 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            )
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let ack: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(ack["state"], "queued");
        assert!(ack["startedAt"].is_null());
        ids.push(ack["id"].as_str().unwrap().to_owned());
    }
    assert_ne!(ids[0], ids[1]);
    assert!(
        state.jobs.lock().unwrap().jobs.is_empty(),
        "legacy job map must not own queued work"
    );
    async fn tick_one(state: Arc<DaemonState>, phase: &'static str) {
        let work = tokio::task::spawn_blocking(move || state.worker_step());
        let result = tokio::time::timeout(crate::testing::HANG_BACKSTOP, work)
            .await
            .unwrap_or_else(|_| panic!("{phase}: worker_step did not finish (hang)"))
            .unwrap_or_else(|error| panic!("{phase}: worker_step thread: {error}"));
        result.unwrap_or_else(|error| panic!("{phase}: worker_step failed: {error:#}"));
    }
    let first_queued = store.request_by_id(&ids[0]).unwrap().unwrap();
    let second_queued = store.request_by_id(&ids[1]).unwrap().unwrap();
    assert!(q0.seq < first_queued.seq && first_queued.seq < second_queued.seq);
    assert_eq!(first_queued.state, "queued");
    assert_eq!(second_queued.state, "queued");
    assert!(first_queued.started_at.is_none() && second_queued.started_at.is_none());
    for row in [&first_queued, &second_queued] {
        assert_eq!(row.options_json, expected_options);
        assert!(row.revision.is_none() && row.finished_at.is_none());
    }
    tick_one(state.clone(), "H+Q0").await;
    let q0_done = store.request_by_id(&q0.id).unwrap().unwrap();
    let first_queued_after_startup = store.request_by_id(&ids[0]).unwrap().unwrap();
    let second_queued_after_startup = store.request_by_id(&ids[1]).unwrap().unwrap();
    assert_eq!(
        (q0_done.id.as_str(), q0_done.seq, q0_done.state.as_str()),
        (q0.id.as_str(), q0.seq, "done")
    );
    let q0_pin = q0_done.revision.unwrap();
    assert_eq!(
        q0_pin.index_revision, 2,
        "startup r1 must precede preexisting Q0 r2"
    );
    assert_eq!(
        store.status().unwrap().revision,
        q0_pin,
        "no partial publication after startup reconciliation+Q0"
    );
    for (before, after) in [
        (&first_queued, &first_queued_after_startup),
        (&second_queued, &second_queued_after_startup),
    ] {
        assert_eq!(
            (
                after.id.as_str(),
                after.seq,
                after.options_json.as_str(),
                after.state.as_str()
            ),
            (
                before.id.as_str(),
                before.seq,
                before.options_json.as_str(),
                "queued"
            )
        );
        assert!(after.revision.is_none() && after.finished_at.is_none());
    }
    tick_one(state.clone(), "Q1").await;
    let first = store.request_by_id(&ids[0]).unwrap().unwrap();
    let second_still_queued = store.request_by_id(&ids[1]).unwrap().unwrap();
    assert_eq!(
        (
            first.id.as_str(),
            first.seq,
            first.options_json.as_str(),
            first.state.as_str()
        ),
        (
            first_queued.id.as_str(),
            first_queued.seq,
            expected_options.as_str(),
            "done"
        )
    );
    let first_pin = first.revision.unwrap();
    assert_eq!(first_pin.index_generation, q0_pin.index_generation);
    assert_eq!(first_pin.index_revision, q0_pin.index_revision + 1);
    assert_eq!(
        store.status().unwrap().revision,
        first_pin,
        "no partial Q2 publication"
    );
    assert_eq!(
        (
            second_still_queued.id.as_str(),
            second_still_queued.seq,
            second_still_queued.options_json.as_str(),
            second_still_queued.state.as_str()
        ),
        (
            second_queued.id.as_str(),
            second_queued.seq,
            expected_options.as_str(),
            "queued"
        )
    );
    assert!(second_still_queued.revision.is_none() && second_still_queued.finished_at.is_none());
    tick_one(state.clone(), "Q2").await;
    let second = store.request_by_id(&ids[1]).unwrap().unwrap();
    assert_eq!(
        (
            second.id.as_str(),
            second.seq,
            second.options_json.as_str(),
            second.state.as_str()
        ),
        (
            second_queued.id.as_str(),
            second_queued.seq,
            expected_options.as_str(),
            "done"
        )
    );
    let second_pin = second.revision.unwrap();
    assert_eq!(second_pin.index_generation, first_pin.index_generation);
    assert_eq!(second_pin.index_revision, first_pin.index_revision + 1);
    assert_eq!(store.status().unwrap().revision, second_pin);
    assert!(store.earliest_unfinished_request().unwrap().is_none());
    let newest = store.current_request().unwrap().unwrap();
    assert_eq!(
        (newest.id.as_str(), newest.seq),
        (second.id.as_str(), second.seq)
    );
    let owner = state.retained_serving_session().unwrap();
    store.verify_writer_session(&owner).unwrap();
    store
        .verify_writer_session(&state.retained_serving_session().unwrap())
        .unwrap();
    assert!(state.jobs.lock().unwrap().jobs.is_empty());
}

#[tokio::test]
async fn multiple_posts_commit_fifo_with_local_writer() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
    let state = new(
        store.clone(),
        IndexOptions::new(root),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    let (first_code, first) = start_index(State(state.clone()), Bytes::new())
        .await
        .unwrap();
    let (second_code, second) = start_index(State(state.clone()), Bytes::new())
        .await
        .unwrap();
    assert_eq!(
        (first_code, second_code),
        (StatusCode::ACCEPTED, StatusCode::ACCEPTED)
    );
    assert_eq!(
        (first.0.state.as_str(), second.0.state.as_str()),
        ("queued", "queued")
    );
    let a = store.request_by_id(&first.0.id).unwrap().unwrap();
    let b = store.request_by_id(&second.0.id).unwrap().unwrap();
    assert!(a.seq < b.seq);
    assert_eq!(store.current_request().unwrap().unwrap().id, b.id);
    tokio::time::timeout(crate::testing::HANG_BACKSTOP, async {
        loop {
            if store
                .request_by_id(&second.0.id)
                .unwrap()
                .unwrap()
                .finished_at
                .is_some()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(store.request_by_id(&a.id).unwrap().unwrap().state, "done");
    assert_eq!(store.request_by_id(&b.id).unwrap().unwrap().state, "done");
}

fn serving_queue_watch_fixture() -> (tempfile::TempDir, Store, Arc<DaemonState>, IndexOptions) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
    let options = IndexOptions::new(root);
    let (_, owner) = crate::index_coordinator::reconcile_workspace(
        &store,
        &options,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let state = new(
        store.clone(),
        options.clone(),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    state.worker_started.store(true, Ordering::Release);
    *state.runtime.lock().unwrap() = RuntimeState::Ready(owner.clone());
    let mut watcher = crate::index_coordinator::WatcherWork::new(&store, &owner, &options).unwrap();
    watcher.disable_native_watcher_for_tests();
    watcher.set_waker(state.watch_waker.clone());
    *state.watcher_work.lock().unwrap() = Some((Arc::downgrade(&owner), watcher));
    (tmp, store, state, options)
}

#[test]
fn queued_row_certifies_existing_full_batch_without_watcher_republication() {
    let (_tmp, store, state, options) = serving_queue_watch_fixture();
    let first = store.enqueue_request(&options, None).unwrap();
    let second = store.enqueue_request(&options, None).unwrap();
    let baseline = store.status().unwrap().revision;
    state.worker_step().unwrap();
    let terminal = store.request_by_id(&first.id).unwrap().unwrap();
    let pin = terminal.revision.unwrap();
    assert_eq!(terminal.state, "done");
    assert_eq!(pin.index_revision, baseline.index_revision + 1);
    assert_eq!(store.status().unwrap().revision, pin);
    assert_eq!(
        store.request_by_id(&second.id).unwrap().unwrap().state,
        "queued"
    );
    let guard = state.watcher_work.lock().unwrap();
    let work = &guard.as_ref().unwrap().1;
    assert_eq!(work.accounted_watch_generation(), Some(1));
    assert_eq!(work.accounted_watch_pin(), Some(pin));
    drop(guard);
    state.worker_step().unwrap();
    assert_eq!(
        store.request_by_id(&second.id).unwrap().unwrap().state,
        "done"
    );
}

#[test]
fn later_full_intent_reconciles_before_next_fifo_claim_without_debounce() {
    let (_tmp, store, state, options) = serving_queue_watch_fixture();
    let first = store.enqueue_request(&options, None).unwrap();
    let second = store.enqueue_request(&options, None).unwrap();
    let work = state.watcher_work.lock().unwrap();
    work.as_ref()
        .unwrap()
        .1
        .submit_watch_event_for_tests(Ok(notify::Event::new(notify::EventKind::Create(
            notify::event::CreateKind::File,
        ))
        .add_path(options.workspace_root.join("new.js"))));
    drop(work);
    // This pre-admission create belongs to the exact admitted full source inventory.
    std::fs::write(
        options.workspace_root.join("new.js"),
        "function fresh() {}\n",
    )
    .unwrap();
    state.worker_step().unwrap();
    assert_eq!(
        store.request_by_id(&first.id).unwrap().unwrap().state,
        "done"
    );
    assert_eq!(
        store.request_by_id(&second.id).unwrap().unwrap().state,
        "queued"
    );
    assert!(
        !state
            .watcher_work
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .1
            .accepted_watch_intent(&options)
    );
}

#[test]
fn post_row_nonurgent_create_gate_precedes_next_fifo_claim() {
    let (_tmp, store, state, options) = serving_queue_watch_fixture();
    let first = store.enqueue_request(&options, None).unwrap();
    let second = store.enqueue_request(&options, None).unwrap();
    let incoming = options.workspace_root.join("new.js");
    let watch = state.clone();
    state.test_queue_after_startup_drain.set(move || {
        std::fs::write(&incoming, "function later() {}\n").unwrap();
        watch
            .watcher_work
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .1
            .submit_watch_event_for_tests(Ok(notify::Event::new(notify::EventKind::Create(
                notify::event::CreateKind::File,
            ))
            .add_path(incoming)));
    });
    state.worker_step().unwrap();
    let first_done = store.request_by_id(&first.id).unwrap().unwrap();
    assert_eq!(first_done.state, "done");
    assert_eq!(
        store.request_by_id(&second.id).unwrap().unwrap().state,
        "queued"
    );
    let gate_pin = store.status().unwrap().revision;
    assert!(gate_pin.index_revision > first_done.revision.unwrap().index_revision);
    assert!(
        !state
            .watcher_work
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .1
            .accepted_watch_intent(&options)
    );
    state.worker_step().unwrap();
    let last = store.request_by_id(&second.id).unwrap().unwrap();
    assert_eq!(last.state, "done");
    assert!(last.revision.unwrap().index_revision > gate_pin.index_revision);
}

#[test]
fn failed_nonurgent_full_gate_retries_before_next_fifo_claim() {
    let (_tmp, store, state, options) = serving_queue_watch_fixture();
    let first = store.enqueue_request(&options, None).unwrap();
    let second = store.enqueue_request(&options, None).unwrap();
    let incoming = options.workspace_root.join("new.js");
    let watch = state.clone();
    state.test_queue_after_startup_drain.set(move || {
        std::fs::write(&incoming, [0xff, 0xfe]).unwrap();
        watch
            .watcher_work
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .1
            .submit_watch_event_for_tests(Ok(notify::Event::new(notify::EventKind::Create(
                notify::event::CreateKind::File,
            ))
            .add_path(incoming)));
    });
    assert!(
        state.worker_step().is_err(),
        "the full gate must reject unreadable source text"
    );
    let first_pin = store
        .request_by_id(&first.id)
        .unwrap()
        .unwrap()
        .revision
        .unwrap();
    assert_eq!(
        store.request_by_id(&second.id).unwrap().unwrap().state,
        "queued"
    );
    std::fs::write(
        options.workspace_root.join("new.js"),
        "function recovered() {}\n",
    )
    .unwrap();
    state.worker_step().unwrap();
    assert_eq!(
        store.request_by_id(&second.id).unwrap().unwrap().state,
        "queued",
        "retry delay cannot admit Q1"
    );
    state
        .watcher_work
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .1
        .expire_retry_for_tests();
    let observed = store.clone();
    let second_id = second.id.clone();
    state.test_queue_after_watcher_reconcile.set(move || {
        assert_eq!(
            observed.request_by_id(&second_id).unwrap().unwrap().state,
            "queued"
        );
        assert!(observed.status().unwrap().revision.index_revision > first_pin.index_revision);
    });
    state.worker_step().unwrap();
    assert_eq!(
        store.request_by_id(&second.id).unwrap().unwrap().state,
        "done"
    );
}

/// The guarded browser route and the blocking CLI route admit the same FIFO
/// work. Each row can certify a full watcher batch only at its own terminal
/// pin; an unfinished successor cannot borrow that certificate.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn browser_and_cli_routes_share_certified_watcher_gate() {
    use crate::daemon::registry::CheckoutRegistry;
    use tower::ServiceExt;

    async fn hold_worker(
        state: Arc<DaemonState>,
    ) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _hold = state.hold_worker_for_tests();
            entered_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        tokio::time::timeout(crate::testing::HANG_BACKSTOP, entered_rx)
            .await
            .expect("worker hold did not enter")
            .unwrap();
        (release_tx, holder)
    }

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.js"), "function initial() {}\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let roots = TopologyRoots::isolated_for_tests(
        tmp.path().join("state/cache"),
        tmp.path().join("state/data"),
    );
    let mut registry = CheckoutRegistry::with_roots(roots);
    registry.attach_launch(1, &identity).unwrap();
    let runtime = registry.activate(&identity.root_key).unwrap();
    let state = runtime.browser_scheduler().unwrap();
    // Startup readiness is owner-published on this condition variable, not
    // inferred from a sleep or from a sampled worker flag.
    let startup = state.clone();
    tokio::time::timeout(
        crate::testing::HANG_BACKSTOP,
        tokio::task::spawn_blocking(move || {
            let mut current = startup.runtime.lock().unwrap();
            while current.session().is_none() {
                current = startup.state_changed.wait(current).unwrap();
            }
        }),
    )
    .await
    .expect("startup did not become ready")
    .unwrap();
    let (release_browser, browser_holder) = hold_worker(state.clone()).await;
    let owner = state.retained_serving_session().unwrap();
    let options = state.config().options.clone();
    let mut watcher = crate::index_coordinator::WatcherWork::publishing(
        &state.store,
        &owner,
        &options,
        state.watcher_health.clone(),
    )
    .unwrap();
    watcher.disable_native_watcher_for_tests();
    watcher.set_waker(state.watch_waker.clone());
    let browser_generation = watcher.generation_handle().load(Ordering::SeqCst);
    state.publish_watch_status(&watcher, &options);
    *state.watcher_work.lock().unwrap() = Some((Arc::downgrade(&owner), watcher));
    assert!(state.store.earliest_unfinished_request().unwrap().is_none());

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/api/index")
        .header("host", state.hosts[0].as_str())
        .header("origin", state.origins[0].as_str())
        .header("authorization", format!("Bearer {}", state.token))
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let accepted: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    let browser_id = accepted["id"].as_str().unwrap().to_owned();
    let browser_queued = state.store.request_by_id(&browser_id).unwrap().unwrap();
    assert_eq!(browser_queued.state, "queued");
    let browser_terminal = Arc::new(tokio::sync::Notify::new());
    state
        .store
        .notify_on_terminal_request_for_tests(&browser_id, browser_terminal.clone());
    release_browser.send(()).unwrap();
    browser_holder.join().unwrap();
    tokio::time::timeout(crate::testing::HANG_BACKSTOP, browser_terminal.notified())
        .await
        .expect("browser row did not commit");
    let (release_cli, cli_holder) = hold_worker(state.clone()).await;
    let browser_done = state.store.request_by_id(&browser_id).unwrap().unwrap();
    assert_eq!(browser_done.state, "done");
    let browser_pin = browser_done.revision.unwrap();
    {
        let work = state.watcher_work.lock().unwrap();
        let work = &work.as_ref().unwrap().1;
        assert_eq!(work.accounted_watch_generation(), Some(browser_generation));
        assert_eq!(work.accounted_watch_pin(), Some(browser_pin));
        assert_eq!(work.full_capture_attempts_for_tests(), 0);
    }
    assert_eq!(state.store.status().unwrap().revision, browser_pin);

    // A new watcher epoch has its own initial full inventory. The previous
    // completed-row gate must not be borrowed by the later CLI request.
    let mut cli_watcher = crate::index_coordinator::WatcherWork::publishing(
        &state.store,
        &owner,
        &options,
        state.watcher_health.clone(),
    )
    .unwrap();
    cli_watcher.disable_native_watcher_for_tests();
    cli_watcher.set_waker(state.watch_waker.clone());
    state.publish_watch_status(&cli_watcher, &options);
    *state.watcher_work.lock().unwrap() = Some((Arc::downgrade(&owner), cli_watcher));
    let new_file = root.join("new.js");
    std::fs::write(&new_file, "function later() {}\n").unwrap();
    assert!(
        state.submit_watch_event_for_tests(Ok(notify::Event::new(notify::EventKind::Create(
            notify::event::CreateKind::File
        ))
        .add_path(new_file),))
    );
    let cli_generation = state
        .watcher_work
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .1
        .generation_handle()
        .load(Ordering::SeqCst);
    assert!(
        cli_generation >= 2,
        "accepted CLI event advanced its watcher epoch"
    );
    let (enqueued_tx, enqueued_rx) = tokio::sync::oneshot::channel();
    state
        .store
        .set_after_enqueue_commit_hook_for_tests(move || {
            let _ = enqueued_tx.send(());
        });
    let cli_runtime = runtime.clone();
    let cli_options = options.clone();
    let cli = tokio::task::spawn_blocking(move || cli_runtime.cli_index(&cli_options));
    tokio::time::timeout(crate::testing::HANG_BACKSTOP, enqueued_rx)
        .await
        .expect("CLI did not durably enqueue")
        .unwrap();
    let cli_queued = state.store.earliest_unfinished_request().unwrap().unwrap();
    assert_eq!(cli_queued.state, "queued");
    assert!(browser_queued.seq < cli_queued.seq);
    let trailing = state.enqueue_index(&options, None).unwrap();
    let next = state.store.request_by_id(&trailing.id).unwrap().unwrap();
    assert_eq!(next.state, "queued");
    assert!(cli_queued.seq < next.seq);

    // Hold the worker after CLI terminal confirmation but before it may
    // reconcile the watcher or claim the next FIFO row.
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_step, step_wait) = std::sync::mpsc::channel();
    state.test_queue_after_startup_drain.set(move || {
        let _ = entered_tx.send(());
        let _ = step_wait.recv();
    });
    release_cli.send(()).unwrap();
    cli_holder.join().unwrap();
    tokio::time::timeout(crate::testing::HANG_BACKSTOP, entered_rx)
        .await
        .expect("CLI step did not reach post-terminal hook")
        .unwrap();
    let cli_done = state.store.request_by_id(&cli_queued.id).unwrap().unwrap();
    let cli_pin = cli_done.revision.unwrap();
    assert_eq!(cli_done.state, "done");
    assert!(cli_pin.index_revision > browser_pin.index_revision);
    let next_still_queued = state.store.request_by_id(&next.id).unwrap().unwrap();
    assert_eq!(next_still_queued.state, "queued");
    assert!(next_still_queued.started_at.is_none());
    assert_eq!(
        state
            .store
            .earliest_unfinished_request()
            .unwrap()
            .unwrap()
            .id,
        next.id
    );
    {
        let work = state.watcher_work.lock().unwrap();
        let work = &work.as_ref().unwrap().1;
        assert_eq!(work.accounted_watch_generation(), Some(cli_generation));
        assert_eq!(work.accounted_watch_pin(), Some(cli_pin));
        assert_eq!(work.full_capture_attempts_for_tests(), 0);
    }
    // An internal ACK covers this batch, but the public WATCH_ACK requires an
    // empty FIFO. The queued next row is the witness guard's negative premise.
    assert_eq!(state.store.status().unwrap().revision, cli_pin);
    release_step.send(()).unwrap();
    let cli_result = tokio::time::timeout(crate::testing::HANG_BACKSTOP, cli)
        .await
        .expect("CLI did not finish")
        .unwrap()
        .unwrap();
    assert_eq!(
        cli_result["publishedRevision"],
        serde_json::to_value(cli_pin).unwrap()
    );
    let next_terminal = Arc::new(tokio::sync::Notify::new());
    state
        .store
        .notify_on_terminal_request_for_tests(&next.id, next_terminal.clone());
    if state.store.request_by_id(&next.id).unwrap().unwrap().state != "done" {
        tokio::time::timeout(crate::testing::HANG_BACKSTOP, next_terminal.notified())
            .await
            .expect("successor did not finish");
    }
    let next_done = state.store.request_by_id(&next.id).unwrap().unwrap();
    assert_eq!(next_done.state, "done");
    assert!(next_done.revision.unwrap().index_revision > cli_pin.index_revision);
    assert_eq!(
        state
            .watcher_work
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .1
            .full_capture_attempts_for_tests(),
        0,
    );
}
