//! Queue rows are durable independently of the native index and daemon-local writer sessions claim them.
use std::fs;
use trellis::{
    indexer::IndexOptions,
    store::{MaintenanceQueueState, QueueProbeAdmission, Store},
};

#[test]
fn extant_v0_queue_readers_refuse_incompatible_without_initializing_schema() {
    use std::os::unix::fs::PermissionsExt;
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let queue = store.request_db_path();
    let db = rusqlite::Connection::open(&queue).unwrap();
    drop(db);
    fs::set_permissions(&queue, fs::Permissions::from_mode(0o600)).unwrap();
    let before = fs::read(&queue).unwrap();
    for result in [
        store.current_request(),
        store.earliest_unfinished_request(),
        store.request_by_id(&uuid::Uuid::new_v4().to_string()),
    ] {
        let error = result.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("incompatible_queue: schema version"),
            "an extant version-0 queue cannot be mistaken for virgin: {error:#}"
        );
        assert_eq!(
            fs::read(&queue).unwrap(),
            before,
            "read path must not initialize a partially created queue"
        );
    }
}

#[test]
fn absent_queue_readers_are_existing_only_and_leave_home_bytes_unchanged() {
    for read in ["current", "by_id"] {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let queue = store.request_db_path();
        assert!(
            !queue.exists(),
            "fresh Ready index must not eagerly create requests.db"
        );
        let before = std::fs::read_dir(queue.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        let result = match read {
            "current" => store.current_request().unwrap(),
            "by_id" => store
                .request_by_id(&uuid::Uuid::new_v4().to_string())
                .unwrap(),
            _ => unreachable!(),
        };
        assert!(result.is_none());
        assert!(!queue.exists(), "{read} unexpectedly created requests.db");
        let after = std::fs::read_dir(queue.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(before, after, "{read} mutated the Ready queue directory");
    }
}

#[test]
fn first_startup_bootstraps_virgin_queue_but_published_missing_queue_is_refused() {
    use std::sync::{Arc, atomic::AtomicBool};
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let queue = store.request_db_path();
    assert!(!queue.exists(), "virgin revision-0 store has no queue yet");
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let session =
        trellis::index_coordinator::establish_serving_session(&store, Some(&options), &cancel)
            .unwrap();
    assert_eq!(store.index_baseline().unwrap().index_revision, 1);
    assert!(
        queue.is_file(),
        "first startup reconciliation must create the durable queue"
    );
    assert!(store.earliest_unfinished_request().unwrap().is_none());
    drop(session);
    drop(store);

    // Once startup reconciliation published a head, a missing queue may contain lost accepted IDs.
    // The next daemon instance must refuse startup reconciliation instead of creating a fresh queue.
    fs::remove_file(&queue).unwrap();
    let reopened = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let index = queue.with_file_name("index.db");
    let before = fs::read(&index).unwrap();
    let error =
        trellis::index_coordinator::establish_serving_session(&reopened, Some(&options), &cancel)
            .unwrap_err();
    assert!(
        error.to_string().contains("incompatible_queue"),
        "{error:#}"
    );
    assert!(
        !queue.exists(),
        "refusal must not replace the missing queue"
    );
    assert_eq!(fs::read(&index).unwrap(), before, "refusal must precede H");
}

#[test]
fn durable_fifo_and_writer_session_fence() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let a = store.enqueue_request(&options, None).unwrap();
    let b = store.enqueue_request(&options, None).unwrap();
    assert_eq!(
        (
            a.state.as_str(),
            a.started_at.as_deref(),
            a.finished_at.as_deref()
        ),
        ("queued", None, None)
    );
    assert!(b.seq > a.seq);
    assert_eq!(store.current_request().unwrap().unwrap().id, b.id);
    assert!(store.request_by_id(&a.id).unwrap().is_some());
    let (_, owner) = trellis::index_coordinator::reconcile_workspace(
        &store,
        &options,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let first = store.claim_request(&owner).unwrap().unwrap();
    assert_eq!(first.id, a.id);
    assert_eq!(first.state, "running");
    assert!(first.started_at.is_some());
    assert!(
        store.claim_request(&owner).unwrap().is_none(),
        "running head cannot be skipped"
    );
    store
        .finish_request(&owner, &first, Err(anyhow::anyhow!("index failed")))
        .unwrap();
    let second = store.claim_request(&owner).unwrap().unwrap();
    assert_eq!(second.id, b.id);
    store
        .finish_request(&owner, &second, Err(anyhow::anyhow!("index failed")))
        .unwrap();
    drop(owner);
    drop(store);
    let reopened = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let saved = reopened.request_by_id(&a.id).unwrap().unwrap();
    assert_eq!(saved.state, "failed");
    assert!(saved.finished_at.is_some());
    assert_eq!(reopened.current_request().unwrap().unwrap().id, b.id);
    let more = reopened.enqueue_request(&options, None).unwrap();
    assert!(more.seq > b.seq);
}

#[test]
fn published_pin_before_completion_crash_child() {
    let Ok(state) = std::env::var("TRELLIS_TEST_CRASH_GAP_STATE") else {
        return;
    };
    let workspace =
        std::path::PathBuf::from(std::env::var("TRELLIS_TEST_CRASH_GAP_WORKSPACE").unwrap());
    let proof = std::path::PathBuf::from(std::env::var("TRELLIS_TEST_CRASH_GAP_PROOF").unwrap());
    let store = Store::open_for_tests(std::path::Path::new(&state), &workspace).unwrap();
    let options = IndexOptions::new(workspace);
    // A new daemon instance must commit startup reconciliation before it claims the durable running gap.
    let (_, owner) = trellis::index_coordinator::reconcile_workspace(
        &store,
        &options,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let claimed = store.claim_request(&owner).unwrap().unwrap();
    assert_eq!(claimed.state, "running");
    let coordinator = trellis::index_coordinator::IndexJobCoordinator::prepare_with_session(
        &store,
        claimed.expected,
        owner.clone(),
    )
    .unwrap();
    let pin = coordinator
        .run(
            &options,
            &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
    assert_eq!(
        store.status().unwrap().revision,
        pin,
        "publication committed"
    );
    assert_eq!(
        store.request_by_id(&claimed.id).unwrap().unwrap().state,
        "running",
        "no terminal write has happened yet"
    );
    let mut marker = fs::File::create(&proof).unwrap();
    use std::io::Write;
    writeln!(
        marker,
        "{} {} {}",
        claimed.id, pin.index_generation, pin.index_revision
    )
    .unwrap();
    marker.sync_all().unwrap();
    // Exit without Rust drops or request completion. Locks vanish as with a crash.
    std::process::exit(37);
}

#[test]
fn real_process_death_after_request_publish_reclaims_without_false_done() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let (before, owner) = trellis::index_coordinator::reconcile_workspace(
        &store,
        &options,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    drop(owner);
    let accepted = store.enqueue_request(&options, None).unwrap();
    let proof = state.path().join("post-publication-proof");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("published_pin_before_completion_crash_child")
        .env("TRELLIS_TEST_CRASH_GAP_STATE", state.path())
        .env("TRELLIS_TEST_CRASH_GAP_WORKSPACE", workspace.path())
        .env("TRELLIS_TEST_CRASH_GAP_PROOF", &proof)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(37),
        "child must exit after verified commit; stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let proof = fs::read_to_string(&proof).unwrap();
    let fields: Vec<_> = proof.split_whitespace().collect();
    assert_eq!(fields.len(), 3);
    assert_eq!(fields[0], accepted.id);
    let parent = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let after_commit = parent.index_baseline().unwrap();
    assert_eq!(after_commit.index_generation.to_string(), fields[1]);
    assert_eq!(after_commit.index_revision.to_string(), fields[2]);
    assert!(after_commit.index_revision > before.index_revision);
    let unfinished = parent.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(
        unfinished.state, "running",
        "the crash cannot pretend the ACK finished"
    );
    assert!(unfinished.revision.is_none());
    let (recovered_h, new_owner) = trellis::index_coordinator::reconcile_workspace(
        &parent,
        &options,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert!(recovered_h.index_revision > after_commit.index_revision);
    assert_eq!(
        trellis::index_coordinator::drain_requests(&parent, &new_owner).unwrap(),
        1
    );
    let done = parent.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(done.state, "done");
    assert_eq!(
        done.revision.unwrap(),
        recovered_h,
        "repeat-after-commit across process death is allowed, false earlier completion is not"
    );
    assert_ne!(done.revision.unwrap(), after_commit);
}

#[test]
fn daemon_restart_reclaims_running_head_before_later_queued_row() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
    let original = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let (_, old_owner) = trellis::index_coordinator::reconcile_workspace(
        &original,
        &options,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let first = original.enqueue_request(&options, None).unwrap();
    let running = original.claim_request(&old_owner).unwrap().unwrap();
    assert_eq!(running.id, first.id);
    assert_eq!(running.state, "running");
    let old_claim = running.claim_incarnation.unwrap();
    let next = original.enqueue_request(&options, None).unwrap();
    assert!(
        original.claim_request(&old_owner).unwrap().is_none(),
        "do not skip running FIFO head"
    );
    drop(old_owner);
    drop(original);
    let replacement = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let (_, new_owner) = trellis::index_coordinator::reconcile_workspace(
        &replacement,
        &options,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let order = std::sync::Mutex::new(Vec::new());
    assert_eq!(
        trellis::index_coordinator::drain_requests_observed(&replacement, &new_owner, |id, p| {
            if p.phase.starts_with("mode:") {
                order.lock().unwrap().push(id.to_owned());
            }
        })
        .unwrap(),
        2
    );
    let a = replacement.request_by_id(&first.id).unwrap().unwrap();
    let b = replacement.request_by_id(&next.id).unwrap().unwrap();
    assert_eq!((a.state.as_str(), b.state.as_str()), ("done", "done"));
    assert_ne!(a.claim_incarnation.unwrap(), old_claim);
    assert_eq!(
        order.into_inner().unwrap(),
        vec![first.id.clone(), next.id.clone()],
        "reclaimed running head must run before later queue admission"
    );
    assert!(
        a.finished_at.unwrap().parse::<u128>().unwrap()
            <= b.started_at.unwrap().parse::<u128>().unwrap(),
        "reclaimed running head must finish before the later row is claimed"
    );
}

#[test]
fn request_rejects_relative_external_input_before_ack() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let mut options = IndexOptions::new(workspace.path().to_owned());
    options.max_file_bytes = 0;
    assert!(store.enqueue_request(&options, None).is_err());
    assert!(store.current_request().unwrap().is_none());
}

#[cfg(unix)]
#[test]
fn request_rejects_symlink_to_captured_workspace() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let link = state.path().join("root-link");
    std::os::unix::fs::symlink(workspace.path(), &link).unwrap();
    let options = IndexOptions::new(link);
    assert!(store.enqueue_request(&options, None).is_err());
    assert!(store.current_request().unwrap().is_none());
}

#[test]
fn first_daemon_activation_reconciles_then_claims_fifo_head() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function seed() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let accepted = store.enqueue_request(&options, None).unwrap();
    let (h, session) =
        trellis::index_coordinator::reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
    assert_eq!(
        h.index_revision, 1,
        "mandatory startup reconciliation commits before the first claim"
    );
    assert_eq!(store.current_request().unwrap().unwrap().state, "queued");
    assert_eq!(
        trellis::index_coordinator::drain_requests(&store, &session).unwrap(),
        1
    );
    let head = store.request_by_id(&accepted.id).unwrap().unwrap();
    let pin = head.revision.unwrap();
    assert_eq!(pin, h, "an unchanged claim completes at the startup head");
    assert_eq!(head.state, "done");
    assert_eq!(store.status().unwrap().revision, pin);
}

#[test]
fn concurrent_first_queue_open_is_atomic_and_fifo() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
    let workers = (0..2)
        .map(|_| {
            let store = store.clone();
            let options = options.clone();
            let gate = gate.clone();
            std::thread::spawn(move || {
                gate.wait();
                store.enqueue_request(&options, None).unwrap()
            })
        })
        .collect::<Vec<_>>();
    let mut accepted = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    accepted.sort_by_key(|request| request.seq);
    assert_eq!((accepted[0].seq, accepted[1].seq), (1, 2));
    assert_eq!(store.current_request().unwrap().unwrap().id, accepted[1].id);
    assert_eq!(
        store.earliest_unfinished_request().unwrap().unwrap().id,
        accepted[0].id
    );
}

#[test]
fn incompatible_or_corrupt_existing_queue_never_gets_fresh_ack() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let accepted = store.enqueue_request(&options, None).unwrap();
    let path = store.request_db_path();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.pragma_update(None, "user_version", 3).unwrap();
    drop(db);
    assert!(
        store
            .enqueue_request(&options, None)
            .unwrap_err()
            .to_string()
            .contains("incompatible_queue")
    );
    let db = rusqlite::Connection::open(&path).unwrap();
    db.pragma_update(None, "user_version", 2).unwrap();
    db.execute(
        "UPDATE queue_identity SET root_key='wrong' WHERE singleton=1",
        [],
    )
    .unwrap();
    drop(db);
    assert!(
        store
            .enqueue_request(&options, None)
            .unwrap_err()
            .to_string()
            .contains("root_key_collision")
    );
    fs::write(&path, b"not a sqlite queue").unwrap();
    assert!(store.enqueue_request(&options, None).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"not a sqlite queue");
    assert!(!accepted.id.is_empty());
}

#[test]
fn queue_hot_journal_child() {
    let Some(path) = std::env::var_os("TRELLIS_QUEUE_HOT_JOURNAL_CHILD") else {
        return;
    };
    let db = rusqlite::Connection::open(path).unwrap();
    db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA cache_size=1; BEGIN IMMEDIATE; UPDATE queue_identity SET root_key='bad'; UPDATE requests SET options_json=zeroblob(65536)").unwrap();
    // Abrupt process exit intentionally leaves SQLite's actual uncommitted rollback journal.
    unsafe { libc::_exit(17) }
}

#[test]
fn hot_journal_rolls_back_before_queue_identity_and_ack() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let accepted = store.enqueue_request(&options, None).unwrap();
    let db_path = store.request_db_path();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("queue_hot_journal_child")
        .env("TRELLIS_QUEUE_HOT_JOURNAL_CHILD", &db_path)
        .output()
        .unwrap();
    assert_eq!(
        child.status.code(),
        Some(17),
        "journal helper did not exit as planned"
    );
    let journal = db_path.with_file_name(format!(
        "{}-journal",
        db_path.file_name().unwrap().to_string_lossy()
    ));
    assert!(journal.exists(), "helper must leave an actual hot journal");
    let next = store.enqueue_request(&options, None).unwrap();
    assert!(next.seq > accepted.seq);
    let original = store.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(original.options_json, accepted.options_json);
    assert_eq!(original.state, "queued");
}

#[test]
fn old_holder_fails_queued_and_running_rows_after_root_is_moved() {
    let state = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), &root).unwrap();
    let options = IndexOptions::new(root.clone());
    let first = store.enqueue_request(&options, None).unwrap();
    let second = store.enqueue_request(&options, None).unwrap();
    let (_, owner) = trellis::index_coordinator::reconcile_workspace(
        &store,
        &options,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert_eq!(store.claim_request(&owner).unwrap().unwrap().id, first.id);
    fs::rename(&root, parent.path().join("old-workspace")).unwrap();
    assert_eq!(store.fail_changed_root_requests().unwrap(), 2);
    assert_eq!(store.fail_changed_root_requests().unwrap(), 0);
    let db = rusqlite::Connection::open(store.request_db_path()).unwrap();
    for id in [first.id, second.id] {
        let (state, code, finished): (String, String, String) = db
            .query_row(
                "SELECT state,error_code,finished_at FROM requests WHERE id=?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((state.as_str(), code.as_str()), ("failed", "root_changed"));
        assert!(!finished.is_empty());
    }
    assert!(store.claim_request(&owner).is_err());
}

#[test]
fn replacement_activation_settles_old_root_before_startup_and_new_fifo() {
    let state = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function oldName() {}\n").unwrap();
    let old = Store::open_for_tests(state.path(), &root).unwrap();
    let old_request = old
        .enqueue_request(&IndexOptions::new(root.clone()), None)
        .unwrap();
    drop(old);
    fs::rename(&root, parent.path().join("old-workspace")).unwrap();
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function newName() {}\n").unwrap();
    let replacement = Store::open_for_tests(state.path(), &root).unwrap();
    let options = IndexOptions::new(root.clone());
    let new_request = replacement.enqueue_request(&options, None).unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (h, session) =
        trellis::index_coordinator::reconcile_workspace(&replacement, &options, &cancel, |_| {})
            .unwrap();
    let db = rusqlite::Connection::open(replacement.request_db_path()).unwrap();
    let (state, code): (String, String) = db
        .query_row(
            "SELECT state,error_code FROM requests WHERE id=?1",
            [&old_request.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((state.as_str(), code.as_str()), ("failed", "root_changed"));
    assert_eq!(
        replacement
            .request_by_id(&new_request.id)
            .unwrap()
            .unwrap()
            .state,
        "queued"
    );
    assert_eq!(
        trellis::index_coordinator::drain_requests(&replacement, &session).unwrap(),
        1
    );
    let done = replacement.request_by_id(&new_request.id).unwrap().unwrap();
    assert_eq!(done.state, "done");
    assert_eq!(
        done.revision,
        Some(h),
        "an unchanged claim completes at the startup head"
    );
}

#[test]
fn unchanged_root_with_changed_workspace_marker_cannot_fail_queue_rows() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join(".git")).unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let row = store
        .enqueue_request(&IndexOptions::new(root.path().to_owned()), None)
        .unwrap();
    fs::write(
        root.path().join(".git/trellis/workspace-id"),
        uuid::Uuid::new_v4().to_string(),
    )
    .unwrap();
    assert!(store.fail_changed_root_requests().is_err());
    let db = rusqlite::Connection::open(store.request_db_path()).unwrap();
    let state: String = db
        .query_row("SELECT state FROM requests WHERE id=?1", [&row.id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(state, "queued");
}

#[test]
fn replacement_rechecks_old_index_under_ex_before_any_rename() {
    use std::os::unix::fs::MetadataExt;
    let state = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    fs::create_dir(&root).unwrap();
    let old = Store::open_for_tests(state.path(), &root).unwrap();
    let accepted = old
        .enqueue_request(&IndexOptions::new(root.clone()), None)
        .unwrap();
    let queue = old.request_db_path();
    let queue_inode = fs::metadata(&queue).unwrap().ino();
    fs::rename(&root, parent.path().join("old-workspace")).unwrap();
    fs::create_dir(&root).unwrap();
    let replacement = Store::open_for_tests(state.path(), &root).unwrap();
    let index = replacement.request_db_path().with_file_name("index.db");
    let db = rusqlite::Connection::open(&index).unwrap();
    db.execute(
        "UPDATE index_metadata SET root_inode=?1",
        [fs::metadata(&root).unwrap().ino().to_string()],
    )
    .unwrap();
    drop(db);
    let before = fs::read(&index).unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let error = trellis::index_coordinator::reconcile_workspace(
        &replacement,
        &IndexOptions::new(root),
        &cancel,
        |_| {},
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("replacement authority changed"),
        "{error:#}"
    );
    assert_eq!(fs::read(&index).unwrap(), before);
    assert_eq!(replacement.request_db_path(), queue);
    assert_eq!(fs::metadata(&queue).unwrap().ino(), queue_inode);
    let db = rusqlite::Connection::open(&queue).unwrap();
    let (state, code): (String, String) = db
        .query_row(
            "SELECT state,error_code FROM requests WHERE id=?1",
            [&accepted.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((state.as_str(), code.as_str()), ("failed", "root_changed"));
    drop(old);
}

#[test]
fn replacement_queue_write_refusal_preserves_old_index_before_ex() {
    let state = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    fs::create_dir(&root).unwrap();
    let old = Store::open_for_tests(state.path(), &root).unwrap();
    let old_request = old
        .enqueue_request(&IndexOptions::new(root.clone()), None)
        .unwrap();
    fs::rename(&root, parent.path().join("old-workspace")).unwrap();
    fs::create_dir(&root).unwrap();
    let replacement = Store::open_for_tests(state.path(), &root).unwrap();
    let index = replacement.request_db_path().with_file_name("index.db");
    let db = rusqlite::Connection::open(replacement.request_db_path()).unwrap();
    db.execute_batch("CREATE TRIGGER refuse_root_failure BEFORE UPDATE ON requests WHEN NEW.error_code='root_changed' BEGIN SELECT RAISE(ABORT,'injected queue write failure'); END;").unwrap();
    drop(db);
    let index_before = fs::read(&index).unwrap();
    let queue_before = fs::read(replacement.request_db_path()).unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    assert!(
        trellis::index_coordinator::reconcile_workspace(
            &replacement,
            &IndexOptions::new(root),
            &cancel,
            |_| {}
        )
        .is_err()
    );
    assert_eq!(fs::read(&index).unwrap(), index_before);
    assert_eq!(
        fs::read(replacement.request_db_path()).unwrap(),
        queue_before
    );
    let db = rusqlite::Connection::open(replacement.request_db_path()).unwrap();
    let state: String = db
        .query_row(
            "SELECT state FROM requests WHERE id=?1",
            [&old_request.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "queued");
}

#[test]
fn replacement_refuses_foreign_index_marker_and_sidecars_without_queue_transition() {
    for foreign in ["marker", "wal", "journal"] {
        let state = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        fs::create_dir(&root).unwrap();
        let old = Store::open_for_tests(state.path(), &root).unwrap();
        let accepted = old
            .enqueue_request(&IndexOptions::new(root.clone()), None)
            .unwrap();
        fs::rename(&root, parent.path().join("old-workspace")).unwrap();
        fs::create_dir(&root).unwrap();
        let replacement = Store::open_for_tests(state.path(), &root).unwrap();
        let index = replacement.request_db_path().with_file_name("index.db");
        match foreign {
            "marker" => {
                let db = rusqlite::Connection::open(&index).unwrap();
                db.execute("UPDATE index_metadata SET root_spelling='foreign'", [])
                    .unwrap();
            }
            "wal" => fs::write(index.with_file_name("index.db-wal"), b"foreign").unwrap(),
            _ => fs::write(index.with_file_name("index.db-journal"), b"foreign").unwrap(),
        }
        let index_before = fs::read(&index).unwrap();
        let queue = replacement.request_db_path();
        let queue_inode = std::os::unix::fs::MetadataExt::ino(&fs::metadata(&queue).unwrap());
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        assert!(
            trellis::index_coordinator::reconcile_workspace(
                &replacement,
                &IndexOptions::new(root),
                &cancel,
                |_| {}
            )
            .is_err(),
            "{foreign}"
        );
        assert_eq!(fs::read(&index).unwrap(), index_before, "{foreign}");
        assert_eq!(replacement.request_db_path(), queue);
        assert_eq!(
            std::os::unix::fs::MetadataExt::ino(&fs::metadata(&queue).unwrap()),
            queue_inode
        );
        let db = rusqlite::Connection::open(&queue).unwrap();
        let (state, code): (String, String) = db
            .query_row(
                "SELECT state,error_code FROM requests WHERE id=?1",
                [&accepted.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (state.as_str(), code.as_str()),
            ("failed", "root_changed"),
            "{foreign}"
        );
        drop(old);
    }
}

#[test]
fn replacement_refuses_missing_or_foreign_queue_without_swapping_index() {
    for fault in ["missing", "foreign", "incompatible", "empty"] {
        let state = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        fs::create_dir(&root).unwrap();
        let old = Store::open_for_tests(state.path(), &root).unwrap();
        old.enqueue_request(&IndexOptions::new(root.clone()), None)
            .unwrap();
        fs::rename(&root, parent.path().join("old-workspace")).unwrap();
        fs::create_dir(&root).unwrap();
        let replacement = Store::open_for_tests(state.path(), &root).unwrap();
        let queue = replacement.request_db_path();
        let index = queue.with_file_name("index.db");
        match fault {
            "missing" => fs::remove_file(&queue).unwrap(),
            "empty" => {
                fs::remove_file(&queue).unwrap();
                fs::write(&queue, b"").unwrap();
            }
            _ => {
                let db = rusqlite::Connection::open(&queue).unwrap();
                if fault == "foreign" {
                    db.execute("UPDATE queue_identity SET root_key='foreign'", [])
                        .unwrap();
                } else {
                    db.pragma_update(None, "user_version", 3).unwrap();
                }
            }
        }
        let before_index = fs::read(&index).unwrap();
        let before_queue = if queue.exists() {
            Some(fs::read(&queue).unwrap())
        } else {
            None
        };
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        assert!(
            trellis::index_coordinator::reconcile_workspace(
                &replacement,
                &IndexOptions::new(root),
                &cancel,
                |_| {}
            )
            .is_err(),
            "{fault}"
        );
        assert_eq!(fs::read(&index).unwrap(), before_index, "{fault}");
        assert_eq!(queue.exists(), before_queue.is_some(), "{fault}");
        if let Some(bytes) = before_queue {
            assert_eq!(fs::read(&queue).unwrap(), bytes, "{fault}");
        }
        drop(old);
    }
}

#[test]
fn root_failure_commit_error_never_authorizes_index_swap() {
    let state = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    fs::create_dir(&root).unwrap();
    let old = Store::open_for_tests(state.path(), &root).unwrap();
    let row = old
        .enqueue_request(&IndexOptions::new(root.clone()), None)
        .unwrap();
    fs::rename(&root, parent.path().join("old-workspace")).unwrap();
    fs::create_dir(&root).unwrap();
    let replacement = Store::open_for_tests(state.path(), &root).unwrap();
    let queue = replacement.request_db_path();
    let queue_inode = std::os::unix::fs::MetadataExt::ino(&fs::metadata(&queue).unwrap());
    let index = queue.with_file_name("index.db");
    let db = rusqlite::Connection::open(&queue).unwrap();
    db.execute_batch("CREATE TABLE commit_parent(id INTEGER PRIMARY KEY); CREATE TABLE commit_failure(id INTEGER REFERENCES commit_parent(id) DEFERRABLE INITIALLY DEFERRED); CREATE TRIGGER fail_commit AFTER UPDATE ON requests WHEN NEW.error_code='root_changed' BEGIN INSERT INTO commit_failure(id) VALUES (1); END;").unwrap();
    drop(db);
    let before_index = fs::read(&index).unwrap();
    let before_queue = fs::read(&queue).unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let error = trellis::index_coordinator::reconcile_workspace(
        &replacement,
        &IndexOptions::new(root),
        &cancel,
        |_| {},
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("storage_busy: root settlement not confirmed"),
        "{error:#}"
    );
    assert_eq!(fs::read(&index).unwrap(), before_index);
    assert_eq!(fs::read(&queue).unwrap(), before_queue);
    assert_eq!(replacement.request_db_path(), queue);
    assert_eq!(
        std::os::unix::fs::MetadataExt::ino(&fs::metadata(&queue).unwrap()),
        queue_inode
    );
    let db = rusqlite::Connection::open(&queue).unwrap();
    let state: String = db
        .query_row("SELECT state FROM requests WHERE id=?1", [&row.id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(state, "queued");
}

#[test]
fn live_old_holder_rejects_replaced_same_key_queue_inode() {
    let state = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    fs::create_dir(&root).unwrap();
    let store = Store::open_for_tests(state.path(), &root).unwrap();
    store
        .enqueue_request(&IndexOptions::new(root.clone()), None)
        .unwrap();
    let queue = store.request_db_path();
    let copy = queue.with_file_name("requests.db.tmp-copy");
    fs::copy(&queue, &copy).unwrap();
    fs::rename(&copy, &queue).unwrap();
    fs::rename(&root, parent.path().join("old-workspace")).unwrap();
    let index = queue.with_file_name("index.db");
    let before = fs::read(&index).unwrap();
    let error = store.fail_changed_root_requests().unwrap_err();
    assert!(error.to_string().contains("inode replaced"), "{error:#}");
    assert_eq!(fs::read(&index).unwrap(), before);
}

#[test]
fn replacement_writer_never_recreates_deleted_accepted_queue_on_enqueue() {
    let state = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    fs::create_dir(&root).unwrap();
    let old = Store::open_for_tests(state.path(), &root).unwrap();
    old.enqueue_request(&IndexOptions::new(root.clone()), None)
        .unwrap();
    fs::rename(&root, parent.path().join("old-workspace")).unwrap();
    fs::create_dir(&root).unwrap();
    let replacement = Store::open_for_tests(state.path(), &root).unwrap();
    let queue = replacement.request_db_path();
    let index = queue.with_file_name("index.db");
    let index_before = fs::read(&index).unwrap();
    fs::remove_file(&queue).unwrap();
    assert!(
        replacement
            .enqueue_request(&IndexOptions::new(root.clone()), None)
            .is_err()
    );
    assert!(
        !queue.exists(),
        "replacement ACK must not create an empty queue"
    );
    assert_eq!(fs::read(&index).unwrap(), index_before);
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    assert!(
        trellis::index_coordinator::reconcile_workspace(
            &replacement,
            &IndexOptions::new(root),
            &cancel,
            |_| {}
        )
        .is_err()
    );
    assert!(!queue.exists());
    assert_eq!(fs::read(&index).unwrap(), index_before);
}

/// Rows a published revision adds (its header, manifest, capture inputs and
/// producer binding, plus the immutable versions and projections) and the
/// head's `indexed_at`, which only a publication advances.
fn revision_row_counts(store: &Store) -> Vec<(&'static str, String)> {
    let db =
        rusqlite::Connection::open(store.request_db_path().with_file_name("index.db")).unwrap();
    let mut state: Vec<(&'static str, String)> = [
        "native_revisions",
        "revision_documents",
        "revision_capture_inputs",
        "revision_producer_bindings",
        "native_revision_supersessions",
        "document_versions",
        "graph_projections",
        "class_projections",
    ]
    .into_iter()
    .map(|table| {
        let count: i64 = db
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        (table, count.to_string())
    })
    .collect();
    let indexed_at: String = db
        .query_row(
            "SELECT indexed_at FROM index_metadata WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    state.push(("index_metadata.indexed_at", indexed_at));
    state
}

#[test]
fn unchanged_startup_keeps_head_and_adds_no_rows() {
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::index_coordinator::{drain_requests, establish_serving_session};
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    drop(establish_serving_session(&store, Some(&options), &cancel).unwrap());
    let head = store.index_baseline().unwrap();
    let rows = revision_row_counts(&store);
    drop(store);
    for restart in 0..3 {
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let session = establish_serving_session(&store, Some(&options), &cancel).unwrap();
        assert_eq!(
            store.index_baseline().unwrap(),
            head,
            "unchanged startup {restart} must keep {{indexGeneration, indexRevision}}"
        );
        assert_eq!(store.status().unwrap().revision, head);
        assert_eq!(
            revision_row_counts(&store),
            rows,
            "unchanged startup {restart} must add no rows"
        );
        // Startup reconciliation still admits FIFO claims on the kept head.
        let request = store.enqueue_request(&options, Some(head)).unwrap();
        assert_eq!(drain_requests(&store, &session).unwrap(), 1);
        let row = store.request_by_id(&request.id).unwrap().unwrap();
        assert_eq!((row.state.as_str(), row.revision), ("done", Some(head)));
    }
    // A real change still publishes a new revision at startup.
    fs::write(workspace.path().join("a.js"), "function b() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    drop(establish_serving_session(&store, Some(&options), &cancel).unwrap());
    let changed = store.index_baseline().unwrap();
    assert_eq!(changed.index_generation, head.index_generation);
    assert_eq!(changed.index_revision, head.index_revision + 1);
}

#[test]
fn claimed_unchanged_fifo_completes_done_without_new_revision() {
    use std::sync::{Arc, Mutex, atomic::AtomicBool};
    use trellis::index_coordinator::{drain_requests_observed, reconcile_workspace};
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let (baseline, owner) =
        reconcile_workspace(&store, &options, &Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
    let rows = revision_row_counts(&store);
    let first = store.enqueue_request(&options, None).unwrap();
    let second = store.enqueue_request(&options, None).unwrap();
    let modes = Mutex::new(Vec::new());
    assert_eq!(
        drain_requests_observed(&store, &owner, |id, p| {
            if p.phase.starts_with("mode:") {
                modes.lock().unwrap().push((id.to_owned(), p.phase));
            }
        })
        .unwrap(),
        2
    );
    assert_eq!(
        modes.into_inner().unwrap(),
        vec![
            (first.id.clone(), "mode:unchanged".into()),
            (second.id.clone(), "mode:unchanged".into()),
        ],
        "both claimed requests must take the guarded unchanged path"
    );
    let a = store.request_by_id(&first.id).unwrap().unwrap();
    let b = store.request_by_id(&second.id).unwrap().unwrap();
    assert_eq!((a.state.as_str(), b.state.as_str()), ("done", "done"));
    assert_eq!((a.revision, b.revision), (Some(baseline), Some(baseline)));
    assert_eq!(store.index_baseline().unwrap(), baseline);
    assert_eq!(
        revision_row_counts(&store),
        rows,
        "a no-op request must add no revision rows"
    );
}

#[test]
fn pin_taken_before_noop_request_still_reads_and_cas_holds() {
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::index_coordinator::{drain_requests, reconcile_workspace};
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let (pin, owner) =
        reconcile_workspace(&store, &options, &Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
    let before = store.graph_at(Some(pin)).unwrap();
    let noop = store.enqueue_request(&options, Some(pin)).unwrap();
    assert_eq!(drain_requests(&store, &owner).unwrap(), 1);
    let row = store.request_by_id(&noop.id).unwrap().unwrap();
    assert_eq!((row.state.as_str(), row.revision), ("done", Some(pin)));
    // The pin is still the current head, so a pinned read and a CAS on it hold.
    assert_eq!(store.status().unwrap().revision, pin);
    assert_eq!(store.graph_at(Some(pin)).unwrap(), before);
    fs::write(workspace.path().join("a.js"), "function b() {}\n").unwrap();
    let changed = store.enqueue_request(&options, Some(pin)).unwrap();
    assert_eq!(drain_requests(&store, &owner).unwrap(), 1);
    let row = store.request_by_id(&changed.id).unwrap().unwrap();
    assert_eq!(row.state, "done");
    let next = row.revision.unwrap();
    assert_eq!(
        (next.index_generation, next.index_revision),
        (pin.index_generation, pin.index_revision + 1),
        "a real change after the no-op still publishes a new revision"
    );
    // A CAS on the now superseded pin conflicts instead of completing.
    let stale = store.enqueue_request(&options, Some(pin)).unwrap();
    assert_eq!(drain_requests(&store, &owner).unwrap(), 1);
    let row = store.request_by_id(&stale.id).unwrap().unwrap();
    assert_eq!(row.state, "failed", "stale CAS must not complete");
    assert_eq!(store.index_baseline().unwrap(), next);
}

#[test]
fn changed_claimed_source_uses_native_fallback_not_unchanged() {
    use std::sync::{Arc, Mutex, atomic::AtomicBool};
    use trellis::index_coordinator::{drain_requests_observed, reconcile_workspace};
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let (_, owner) =
        reconcile_workspace(&store, &options, &Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
    let request = store.enqueue_request(&options, None).unwrap();
    fs::write(workspace.path().join("a.js"), "function b() {}\n").unwrap();
    let modes = Mutex::new(Vec::new());
    assert_eq!(
        drain_requests_observed(&store, &owner, |_, p| {
            if p.phase.starts_with("mode:") {
                modes.lock().unwrap().push(p.phase);
            }
        })
        .unwrap(),
        1
    );
    assert_eq!(
        modes.into_inner().unwrap(),
        vec!["mode:full"],
        "declaration change is not #67 local"
    );
    assert_eq!(
        store.request_by_id(&request.id).unwrap().unwrap().state,
        "done"
    );
}

#[test]
fn claimed_unchanged_guard_failure_never_acks_or_changes_selected_pair() {
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::index_coordinator::{drain_requests, reconcile_workspace};
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let (head, leader) =
        reconcile_workspace(&store, &options, &Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
    let request = store.enqueue_request(&options, None).unwrap();
    let db =
        rusqlite::Connection::open(store.request_db_path().with_file_name("index.db")).unwrap();
    db.execute(
        "UPDATE document_versions SET source_bytes=?1 WHERE path='a.js'",
        [b"function b() {}\n".as_slice()],
    )
    .unwrap();
    drop(db);
    assert_eq!(drain_requests(&store, &leader).unwrap(), 1);
    let row = store.request_by_id(&request.id).unwrap().unwrap();
    assert_eq!(row.state, "failed", "guard failure must not ACK done");
    assert!(row.revision.is_none());
    assert_eq!(
        store.index_baseline().unwrap(),
        head,
        "selected pair must remain unchanged"
    );
}

#[test]
fn changed_capture_input_and_options_take_full_claimed_fallback() {
    use std::sync::{Arc, Mutex, atomic::AtomicBool};
    use trellis::index_coordinator::{drain_requests_observed, reconcile_workspace};
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let (_, owner) =
        reconcile_workspace(&store, &options, &Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
    fs::write(workspace.path().join(".gitignore"), "absent.js\n").unwrap();
    let input_request = store.enqueue_request(&options, None).unwrap();
    let mut other_options = options.clone();
    other_options.max_file_bytes = 1024;
    let option_request = store.enqueue_request(&other_options, None).unwrap();
    let modes = Mutex::new(Vec::new());
    assert_eq!(
        drain_requests_observed(&store, &owner, |id, p| {
            if p.phase.starts_with("mode:") {
                modes.lock().unwrap().push((id.to_owned(), p.phase));
            }
        })
        .unwrap(),
        2
    );
    assert_eq!(
        modes.into_inner().unwrap(),
        vec![
            (input_request.id.clone(), "mode:full".into()),
            (option_request.id.clone(), "mode:full".into()),
        ],
        "new ignore input and changed options cannot reuse selected facts via unchanged path"
    );
    for request in [input_request, option_request] {
        assert_eq!(
            store.request_by_id(&request.id).unwrap().unwrap().state,
            "done"
        );
    }
}

#[test]
fn executable_drift_claim_child() {
    let Ok(state) = std::env::var("TRELLIS_DRIFT_CLAIM_STATE") else {
        return;
    };
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex, atomic::AtomicBool};
    use trellis::index_coordinator::{drain_requests_observed_with_native, reconcile_workspace};
    use trellis::native_evidence::FullNativeStage;
    let workspace =
        std::path::PathBuf::from(std::env::var("TRELLIS_DRIFT_CLAIM_WORKSPACE").unwrap());
    let proof = std::path::PathBuf::from(std::env::var("TRELLIS_DRIFT_CLAIM_PROOF").unwrap());
    let store = Store::open_for_tests(std::path::Path::new(&state), &workspace).unwrap();
    let options = IndexOptions::new(workspace);
    let stage = std::env::var("TRELLIS_DRIFT_CLAIM_STAGE").unwrap_or_else(|_| "old".into());
    if stage == "new" {
        // The rebuilt image's first claimed publication, as at daemon startup.
        let previous = store.index_baseline().unwrap();
        // Mandatory startup reconciliation, observed by its mode.
        let modes = Mutex::new(Vec::new());
        trellis::index_coordinator::IndexJobCoordinator::prepare(&store, None)
            .unwrap()
            .run_serving(&options, &Arc::new(AtomicBool::new(false)), |p| {
                if p.phase.starts_with("mode:") {
                    modes.lock().unwrap().push(p.phase);
                }
            })
            .unwrap();
        let fresh = store.status().unwrap().revision;
        assert_eq!(fresh.index_generation, previous.index_generation);
        assert_eq!(fresh.index_revision, previous.index_revision + 1);
        for path in ["a.js", "b.js"] {
            assert!(store.source_at(path, Some(previous)).unwrap().is_some());
            assert!(store.source_at(path, Some(fresh)).unwrap().is_some());
        }
        let db =
            rusqlite::Connection::open(store.request_db_path().with_file_name("index.db")).unwrap();
        let id = format!("pin:v1:{}:{}", fresh.index_generation, fresh.index_revision);
        let (producer_sha, binding_sha): (String, String) = db.query_row(
            "SELECT producer_sha,binding_sha FROM revision_producer_bindings WHERE revision_id=?1",
            [&id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        let manifest: Vec<String> = db
            .prepare(
                "SELECT m.path||' '||v.id||' '||('graph:v1:'||g.graph_hash)||' '||
                 coalesce('class-projection:v1:'||cp.content_hash,'-')||' '||m.coverage_state
                 FROM revision_documents m JOIN document_versions v ON v.vid=m.vid
                 JOIN graph_projections g ON g.gid=m.gid
                 LEFT JOIN class_projections cp ON cp.cid=m.cid
                 WHERE m.revision_id=?1 ORDER BY m.ordinal",
            )
            .unwrap()
            .query_map([&id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let extractor: Option<String> = db
            .query_row(
                "SELECT json_extract(payload,'$.hash') FROM revision_capture_inputs
                 WHERE revision_id=?1 AND input_key='extractor:trellis.native.syntax'",
                [&id],
                |r| r.get(0),
            )
            .ok();
        // The published result; the pin is reported separately.
        let graph = serde_json::to_value(store.graph_at(Some(fresh)).unwrap()).unwrap();
        let result = serde_json::json!({"modes":modes.into_inner().unwrap(),
            "producerSha":producer_sha,"bindingSha":binding_sha,"extractor":extractor,
            "generation":fresh.index_generation.to_string(),"revision":fresh.index_revision,
            "manifest":manifest,"graph":graph});
        fs::write(proof, serde_json::to_vec(&result).unwrap()).unwrap();
        return;
    }
    assert_eq!(stage, "old");
    let (old, owner) =
        reconcile_workspace(&store, &options, &Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
    println!("DRIFT_BASELINE_READY");
    std::io::stdout().flush().unwrap();
    let mut signal = [0u8; 1];
    std::io::stdin().read_exact(&mut signal).unwrap();
    assert_eq!(signal[0], b'!');
    let request = store.enqueue_request(&options, None).unwrap();
    let inode_before =
        std::os::unix::fs::MetadataExt::ino(&fs::metadata(store.request_db_path()).unwrap());
    let modes = Mutex::new(Vec::new());
    let native_events = Mutex::new(Vec::new());
    assert_eq!(
        drain_requests_observed_with_native(
            &store,
            &owner,
            |_, p| {
                if p.phase.starts_with("mode:") {
                    modes.lock().unwrap().push(p.phase);
                }
            },
            |id, key, stage| {
                native_events.lock().unwrap().push((
                    id.to_owned(),
                    key.path.clone(),
                    match stage {
                        FullNativeStage::Measured => "measured",
                        FullNativeStage::Validated => "validated",
                    },
                ));
            }
        )
        .unwrap(),
        1
    );
    let done = store.request_by_id(&request.id).unwrap().unwrap();
    assert_eq!(done.state, "done");
    let fresh = done.revision.unwrap();
    assert_eq!(store.status().unwrap().revision, fresh);
    assert_eq!(fresh.index_generation, old.index_generation);
    // An unchanged claim keeps the head (Decision 0006 §3); a local edit publishes.
    let unchanged = modes.lock().unwrap().as_slice() == ["mode:unchanged"];
    assert_eq!(
        fresh.index_revision,
        old.index_revision + u64::from(!unchanged)
    );
    for path in ["a.js", "b.js"] {
        assert!(
            store.source_at(path, Some(old)).unwrap().is_some(),
            "both old pinned sources stay readable"
        );
        assert!(
            store.source_at(path, Some(fresh)).unwrap().is_some(),
            "full measurement must select both fresh sources"
        );
    }
    let db =
        rusqlite::Connection::open(store.request_db_path().with_file_name("index.db")).unwrap();
    let selected: (String, String) = db
        .query_row(
            "SELECT producer_sha,binding_sha FROM revision_producer_bindings WHERE revision_id=?1",
            [format!(
                "pin:v1:{}:{}",
                fresh.index_generation, fresh.index_revision
            )],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let manifests: i64 = db
        .query_row(
            "SELECT count(*) FROM revision_documents WHERE revision_id=?1",
            [format!(
                "pin:v1:{}:{}",
                fresh.index_generation, fresh.index_revision
            )],
            |r| r.get(0),
        )
        .unwrap();
    let old_manifests: i64 = db
        .query_row(
            "SELECT count(*) FROM revision_documents WHERE revision_id=?1",
            [format!(
                "pin:v1:{}:{}",
                old.index_generation, old.index_revision
            )],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!((manifests, old_manifests), (2, 2));
    let inode_after =
        std::os::unix::fs::MetadataExt::ino(&fs::metadata(store.request_db_path()).unwrap());
    assert_eq!(
        inode_after, inode_before,
        "requests.db must not be recreated"
    );
    let result = serde_json::json!({"modes":modes.into_inner().unwrap(),"producerSha":selected.0,
        "bindingSha":selected.1,"generation":fresh.index_generation.to_string(),
        "revision":fresh.index_revision,"requestId":request.id,"queueInode":inode_after,
        "nativeEvents":native_events.into_inner().unwrap()});
    fs::write(proof, serde_json::to_vec(&result).unwrap()).unwrap();
}

fn asserted_claim_under_real_executable_drift(body_edit: bool, drift: bool) {
    use sha2::{Digest, Sha256};
    use std::io::{BufRead, Write};
    use std::process::{Command, Stdio};
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let padding = "x".repeat(145_000);
    let local = |value| {
        format!(
            "function local() {{ return {value}; /*{padding}*/ }}\nfunction checked() {{ const local=1, other=2; return local+other; }}\n"
        )
    };
    fs::write(workspace.path().join("a.js"), local("1")).unwrap();
    fs::write(
        workspace.path().join("b.js"),
        "function stable() { return 3; }\n",
    )
    .unwrap();
    let binary = state.path().join("native-drift-requests");
    fs::copy(std::env::current_exe().unwrap(), &binary).unwrap();
    let old_hash = hex::encode(Sha256::digest(fs::read(&binary).unwrap()));
    let proof = state.path().join("drift-proof.json");
    let mut child = Command::new(&binary)
        .arg("--exact")
        .arg("executable_drift_claim_child")
        .arg("--nocapture")
        .env("TRELLIS_DRIFT_CLAIM_STATE", state.path())
        .env("TRELLIS_DRIFT_CLAIM_WORKSPACE", workspace.path())
        .env("TRELLIS_DRIFT_CLAIM_PROOF", &proof)
        .env("TRELLIS_DRIFT_CLAIM_STAGE", "old")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert!(
            stdout.read_line(&mut line).unwrap() > 0,
            "child exited before baseline ready"
        );
        if line.contains("DRIFT_BASELINE_READY") {
            break;
        }
    }
    // Replace the binary pathname atomically while the old inode executes.
    // The next normal claimed capture hashes the changed current_exe pathname.
    let drift_binary = |marker: &[u8]| {
        let replacement = state.path().join("native-drift-replacement");
        fs::copy(&binary, &replacement).unwrap();
        let mut bytes = fs::OpenOptions::new()
            .append(true)
            .open(&replacement)
            .unwrap();
        bytes.write_all(marker).unwrap();
        bytes.sync_all().unwrap();
        drop(bytes);
        fs::rename(&replacement, &binary).unwrap();
        hex::encode(Sha256::digest(fs::read(&binary).unwrap()))
    };
    let executing_hash = if drift {
        drift_binary(b"TRELLIS-TEST-PRODUCER-DRIFT-V1")
    } else {
        hex::encode(Sha256::digest(fs::read(&binary).unwrap()))
    };
    if body_edit {
        fs::write(workspace.path().join("a.js"), local("2")).unwrap();
    }
    child.stdin.take().unwrap().write_all(b"!").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "drift child failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&fs::read(&proof).unwrap()).unwrap();
    assert_eq!(
        result["modes"],
        serde_json::json!([if body_edit {
            "mode:local"
        } else {
            "mode:unchanged"
        }]),
        "the old running image remains the producer after its pathname is replaced"
    );
    assert_eq!(
        result["nativeEvents"],
        serde_json::json!([]),
        "the unchanged running producer must not remeasure both documents"
    );
    assert_eq!(
        result["producerSha"], old_hash,
        "old-process facts must bind to the old running image, never the replacement pathname"
    );
    assert_eq!(result["bindingSha"].as_str().unwrap().len(), 64);
    // The unchanged claim completes at the baseline r1; a body edit publishes r2.
    let claimed_revision = if body_edit { 2 } else { 1 };
    assert_eq!(result["revision"], claimed_revision);
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let row = store
        .request_by_id(result["requestId"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(row.state, "done");
    assert_eq!(row.revision.unwrap().index_revision, claimed_revision);
    assert_eq!(
        row.revision.unwrap().index_generation.to_string(),
        result["generation"]
    );
    if drift {
        assert_ne!(old_hash, executing_hash);
        let rebuilt =
            |label: &str, executable: &str, extractor: Option<&str>, edit: Option<String>| {
                let next_proof = state.path().join(format!("drift-proof-{label}.json"));
                let mut command = Command::new(&binary);
                command
                    .arg("--exact")
                    .arg("executable_drift_claim_child")
                    .arg("--nocapture")
                    .env("TRELLIS_DRIFT_CLAIM_STATE", state.path())
                    .env("TRELLIS_DRIFT_CLAIM_WORKSPACE", workspace.path())
                    .env("TRELLIS_DRIFT_CLAIM_PROOF", &next_proof)
                    .env("TRELLIS_DRIFT_CLAIM_STAGE", "new")
                    .env_remove("TRELLIS_TEST_EXTRACTOR_HASH");
                if let Some(hash) = extractor {
                    command.env("TRELLIS_TEST_EXTRACTOR_HASH", hash);
                }
                // The edit is written here: its text exceeds Linux's
                // per-string environment limit.
                if let Some(edit) = edit {
                    fs::write(workspace.path().join("a.js"), edit).unwrap();
                }
                let next = command.output().unwrap();
                assert!(
                    next.status.success(),
                    "new image failed: stdout={} stderr={}",
                    String::from_utf8_lossy(&next.stdout),
                    String::from_utf8_lossy(&next.stderr)
                );
                let proof: serde_json::Value =
                    serde_json::from_slice(&fs::read(&next_proof).unwrap()).unwrap();
                assert_eq!(proof["producerSha"], executable);
                assert_eq!(proof["generation"], result["generation"]);
                assert_eq!(proof["bindingSha"].as_str().unwrap().len(), 64);
                assert_eq!(proof["manifest"].as_array().unwrap().len(), 2);
                proof
            };
        // Decision 0005 (2026-10 amendment): the rebuilt image has the same
        // extractor producer hash, so it keeps the validated facts. It
        // publishes a new revision bound to its own executable SHA.
        let reused = rebuilt("reused", &executing_hash, None, None);
        assert_eq!(reused["modes"], serde_json::json!(["mode:unchanged"]));
        assert_eq!(reused["revision"], claimed_revision + 1);
        // A changed producer hash forces full re-extraction of every document,
        // and that re-extraction publishes the same facts and projections.
        let changed = "f".repeat(64);
        let remeasured = rebuilt("remeasured", &executing_hash, Some(&changed), None);
        assert_eq!(remeasured["modes"], serde_json::json!(["mode:full"]));
        assert_eq!(remeasured["revision"], claimed_revision + 2);
        assert_eq!(remeasured["extractor"], changed);
        assert_ne!(reused["extractor"], changed);
        assert_eq!(remeasured["manifest"], reused["manifest"]);
        assert_eq!(remeasured["graph"], reused["graph"]);
        // Rebuild again with that same producer hash: a body edit stays local.
        let second_hash = drift_binary(b"TRELLIS-TEST-PRODUCER-DRIFT-V2");
        assert_ne!(second_hash, executing_hash);
        let edited = rebuilt("local", &second_hash, Some(&changed), Some(local("7")));
        assert_eq!(edited["modes"], serde_json::json!(["mode:local"]));
        assert_eq!(edited["revision"], claimed_revision + 3);
    }
}

#[cfg(feature = "test-support")]
#[test]
fn rebuilt_executable_reuses_facts_only_while_the_extractor_hash_is_unchanged() {
    asserted_claim_under_real_executable_drift(false, true);
}

#[cfg(feature = "test-support")]
#[test]
fn body_edit_then_rebuilt_executable_reuses_facts_only_while_the_extractor_hash_is_unchanged() {
    asserted_claim_under_real_executable_drift(true, true);
}

#[test]
fn same_body_edit_without_executable_drift_is_proven_local_control() {
    asserted_claim_under_real_executable_drift(true, false);
}

/// The parent holds the probe open while this separate process admits durable
/// work or an exclusive queue writer. No timing or polling is needed.
#[test]
fn maintenance_probe_external_child() {
    let Ok(mode) = std::env::var("TRELLIS_TEST_MAINTENANCE_PROBE_CHILD") else {
        return;
    };
    let state = std::path::Path::new(&std::env::var("TRELLIS_PROBE_STATE").unwrap()).to_path_buf();
    let workspace =
        std::path::Path::new(&std::env::var("TRELLIS_PROBE_WORKSPACE").unwrap()).to_path_buf();
    let store = Store::open_for_tests(&state, &workspace).unwrap();
    if mode == "enqueue" || mode == "enqueue_busy" {
        let result = store.enqueue_request(&IndexOptions::new(workspace), None);
        if mode == "enqueue_busy" {
            let error = result.expect_err("virgin queue publication cannot pass probe SH");
            assert!(error.to_string().contains("storage_busy"), "{error:#}");
            assert!(!store.request_db_path().exists());
        } else {
            result.unwrap();
        }
    } else if mode == "exclusive" || mode == "wal" || mode == "journal" {
        let db = rusqlite::Connection::open(store.request_db_path()).unwrap();
        db.busy_timeout(std::time::Duration::ZERO).unwrap();
        if mode == "exclusive" {
            db.execute_batch("BEGIN EXCLUSIVE").unwrap();
        } else if mode == "journal" {
            db.execute_batch("BEGIN IMMEDIATE; UPDATE queue_identity SET root_key=root_key||'x'")
                .unwrap();
        } else {
            db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; UPDATE queue_identity SET root_key=root_key;").unwrap();
        }
        use std::io::{Read, Write};
        std::io::stdout().write_all(b"@").unwrap();
        std::io::stdout().flush().unwrap();
        let mut release = [0];
        std::io::stdin().read_exact(&mut release).unwrap();
        if mode == "exclusive" || mode == "journal" {
            db.execute_batch("ROLLBACK").unwrap();
        }
    } else {
        panic!("unknown probe child mode: {mode}");
    }
}

fn maintenance_probe_child(
    mode: &str,
    state: &std::path::Path,
    workspace: &std::path::Path,
) -> std::process::Child {
    std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("maintenance_probe_external_child")
        .arg("--nocapture")
        .env("TRELLIS_TEST_MAINTENANCE_PROBE_CHILD", mode)
        .env("TRELLIS_PROBE_STATE", state)
        .env("TRELLIS_PROBE_WORKSPACE", workspace)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn maintenance_probe_virgin_blocks_first_publication_and_v0_is_unknown() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let probe = store.open_maintenance_queue_probe().unwrap();
    assert!(matches!(probe, QueueProbeAdmission::AbsentVirgin(_)));
    assert_eq!(probe.check(), MaintenanceQueueState::Clear);
    // The probe's index-use SH prevents the first queue's EX-protected
    // publication. A separate process must report typed contention, not ACK.
    let output = maintenance_probe_child("enqueue_busy", state.path(), workspace.path())
        .wait_with_output()
        .unwrap();
    assert!(
        output.status.success(),
        "external virgin refusal failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!store.request_db_path().exists());
    assert_eq!(probe.check(), MaintenanceQueueState::Clear);
    drop(probe);
    let output = maintenance_probe_child("enqueue", state.path(), workspace.path())
        .wait_with_output()
        .unwrap();
    assert!(
        output.status.success(),
        "external FIFO failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        store.open_maintenance_queue_probe().unwrap().check(),
        MaintenanceQueueState::Pending
    );

    // A different fresh workspace with an interrupted first queue creator
    // must never initialize that version-zero inode from the probe.
    let other_state = tempfile::tempdir().unwrap();
    let other_workspace = tempfile::tempdir().unwrap();
    fs::write(other_workspace.path().join("a.js"), "function a() {}\n").unwrap();
    let other = Store::open_for_tests(other_state.path(), other_workspace.path()).unwrap();
    let path = other.request_db_path();
    let db = rusqlite::Connection::open(&path).unwrap();
    drop(db);
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let before = fs::read(&path).unwrap();
    assert_eq!(
        other.open_maintenance_queue_probe().unwrap().check(),
        MaintenanceQueueState::Unknown
    );
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn maintenance_probe_external_fifo_busy_and_inode_replacement() {
    use std::io::{Read, Write};
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let (_, leader) = trellis::index_coordinator::reconcile_workspace(
        &store,
        &options,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let request = store.enqueue_request(&options, None).unwrap();
    let claimed = store.claim_request(&leader).unwrap().unwrap();
    assert_eq!(request.id, claimed.id);
    store
        .finish_request(&leader, &claimed, Err(anyhow::anyhow!("test failure")))
        .unwrap();
    let probe = store.open_maintenance_queue_probe().unwrap();
    assert!(matches!(probe, QueueProbeAdmission::Ready(_)));
    assert_eq!(probe.check(), MaintenanceQueueState::Clear);
    let output = maintenance_probe_child("enqueue", state.path(), workspace.path())
        .wait_with_output()
        .unwrap();
    assert!(
        output.status.success(),
        "external FIFO failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(probe.check(), MaintenanceQueueState::Pending);
    drop(probe);

    // A writer's EXCLUSIVE lock must cause immediate Unknown, not the normal
    // queue opener's three-second busy wait. The pipe is a deterministic hold.
    let mut child = maintenance_probe_child("exclusive", state.path(), workspace.path());
    let mut ready = [0];
    // The Rust test harness may print its own preamble before the marker.
    loop {
        child
            .stdout
            .as_mut()
            .unwrap()
            .read_exact(&mut ready)
            .unwrap();
        if ready == *b"@" {
            break;
        }
    }
    let busy = store.open_maintenance_queue_probe().unwrap();
    assert_eq!(busy.check(), MaintenanceQueueState::Unknown);
    child.stdin.as_mut().unwrap().write_all(b"R").unwrap();
    assert!(child.wait().unwrap().success());

    let probe = store.open_maintenance_queue_probe().unwrap();
    let path = store.request_db_path();
    let old = path.with_extension("old-queue");
    fs::rename(&path, &old).unwrap();
    fs::copy(&old, &path).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(probe.check(), MaintenanceQueueState::Unknown);
}

/// WAL is an ordinary unsupported queue format, not an attacker. Even a
/// read-only SQLite open may create shared-memory sidecars for WAL: reject it
/// before opening, without changing queue/index bytes or sidecar names/bytes.
#[test]
fn maintenance_probe_rejects_wal_with_and_without_sidecars_without_mutation() {
    use std::io::{Read, Write};
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    store
        .enqueue_request(&IndexOptions::new(workspace.path().to_owned()), None)
        .unwrap();
    let path = store.request_db_path();
    let roots = trellis::store::topology::TopologyRoots::isolated_for_tests(
        state.path().join("cache"),
        state.path().join("data"),
    );
    let identity = trellis::store::topology::WorkspaceIdentity::discover(
        Some(workspace.path()),
        workspace.path(),
    )
    .unwrap();
    let index_path = roots.index_db(&identity);
    let sidecar_bytes = || {
        ["-journal", "-wal", "-shm"]
            .into_iter()
            .map(|suffix| {
                let sidecar = path.with_file_name(format!(
                    "{}{}",
                    path.file_name().unwrap().to_string_lossy(),
                    suffix
                ));
                match fs::read(&sidecar) {
                    Ok(bytes) => Some(bytes),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => panic!("unexpected sidecar read error: {error}"),
                }
            })
            .collect::<Vec<_>>()
    };
    let mut child = maintenance_probe_child("wal", state.path(), workspace.path());
    let mut ready = [0];
    loop {
        child
            .stdout
            .as_mut()
            .unwrap()
            .read_exact(&mut ready)
            .unwrap();
        if ready == *b"@" {
            break;
        }
    }
    let with_sidecars = sidecar_bytes();
    assert!(with_sidecars[1].is_some() && with_sidecars[2].is_some());
    let main_before = fs::read(&path).unwrap();
    assert_eq!(&main_before[18..20], &[2, 2]);
    let index_before = fs::read(&index_path).unwrap();
    assert_eq!(
        store.open_maintenance_queue_probe().unwrap().check(),
        MaintenanceQueueState::Unknown
    );
    assert_eq!(fs::read(&path).unwrap(), main_before);
    assert_eq!(fs::read(&index_path).unwrap(), index_before);
    assert_eq!(sidecar_bytes(), with_sidecars);

    child.stdin.as_mut().unwrap().write_all(b"R").unwrap();
    assert!(child.wait().unwrap().success());
    let without_sidecars = sidecar_bytes();
    assert_eq!(without_sidecars, vec![None, None, None]);
    let main_before = fs::read(&path).unwrap();
    assert_eq!(&main_before[18..20], &[2, 2]);
    let index_before = fs::read(&index_path).unwrap();
    assert_eq!(
        store.open_maintenance_queue_probe().unwrap().check(),
        MaintenanceQueueState::Unknown
    );
    assert_eq!(fs::read(&path).unwrap(), main_before);
    assert_eq!(fs::read(&index_path).unwrap(), index_before);
    assert_eq!(sidecar_bytes(), without_sidecars);
}

#[test]
fn maintenance_probe_detects_uncommitted_journal_after_initial_guard() {
    use std::io::{Read, Write};
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    store
        .enqueue_request(&IndexOptions::new(workspace.path().to_owned()), None)
        .unwrap();
    // An initial pending queue would return Pending on the unchanged-version
    // fast path. A held uncommitted writer must instead force Unknown.
    let probe = store.open_maintenance_queue_probe().unwrap();
    assert_eq!(probe.check(), MaintenanceQueueState::Pending);
    let queue = store.request_db_path();
    let journal = queue.with_file_name(format!(
        "{}-journal",
        queue.file_name().unwrap().to_string_lossy()
    ));
    let mut held = None;
    let mut while_held = None;
    let state_at_check = probe.check_with_hook(|| {
        let mut child = maintenance_probe_child("journal", state.path(), workspace.path());
        let mut ready = [0];
        loop {
            child
                .stdout
                .as_mut()
                .unwrap()
                .read_exact(&mut ready)
                .unwrap();
            if ready == *b"@" {
                break;
            }
        }
        assert!(
            journal.exists(),
            "writer must create rollback journal before version read"
        );
        while_held = Some((fs::read(&queue).unwrap(), fs::read(&journal).unwrap()));
        held = Some(child);
    });
    assert_eq!(state_at_check, MaintenanceQueueState::Unknown);
    assert_eq!(
        (fs::read(&queue).unwrap(), fs::read(&journal).unwrap()),
        while_held.unwrap(),
        "maintenance probe cannot mutate held writer database or journal"
    );
    let mut child = held.unwrap();
    child.stdin.as_mut().unwrap().write_all(b"R").unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(probe.check(), MaintenanceQueueState::Pending);
}
