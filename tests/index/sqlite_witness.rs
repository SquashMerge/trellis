use rusqlite::Connection;
use std::{path::Path, process::Command, time::Duration};
use trellis::{indexer::IndexOptions, store::Store};

// A separate process detects lost POSIX fcntl locks; a second thread cannot.
#[test]
fn sqlite_writer_child() {
    let Some(path) = std::env::var_os("TRELLIS_WITNESS_LOCK_CHILD") else {
        return;
    };
    let db = Connection::open(path).unwrap();
    db.busy_timeout(Duration::ZERO).unwrap();
    let update = if std::env::var_os("TRELLIS_WITNESS_QUEUE").is_some() {
        "UPDATE requests SET submitted_at=submitted_at||'!' WHERE seq=1"
    } else {
        "UPDATE index_metadata SET last_opened_at=last_opened_at+1"
    };
    match db.execute_batch(&format!("BEGIN IMMEDIATE; {update}; COMMIT")) {
        Err(rusqlite::Error::SqliteFailure(info, _))
            if matches!(
                info.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            std::process::exit(23)
        }
        Ok(()) => std::process::exit(29),
        Err(_) => std::process::exit(19),
    }
}

fn contender(path: &Path) -> i32 {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.arg("--exact")
        .arg(child_test_name("sqlite_writer_child"))
        .env("TRELLIS_WITNESS_LOCK_CHILD", path);
    if path.file_name().is_some_and(|name| name == "requests.db") {
        cmd.env("TRELLIS_WITNESS_QUEUE", "1");
    }
    cmd.output().unwrap().status.code().unwrap()
}

#[test]
fn live_index_writer_survives_transient_verification_and_status() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let index = std::fs::read_dir(state.path().join("cache/indexes"))
        .unwrap()
        .filter_map(|e| {
            let path = e.unwrap().path().join("index.db");
            path.exists().then_some(path)
        })
        .next()
        .unwrap();
    let writer = Connection::open(&index).unwrap();
    writer
        .execute_batch("BEGIN IMMEDIATE; UPDATE index_metadata SET last_opened_at=last_opened_at")
        .unwrap();
    let _ = store.status();
    let before: i64 = writer
        .query_row("SELECT last_opened_at FROM index_metadata", [], |r| {
            r.get(0)
        })
        .unwrap();
    let busy = contender(&index);
    let commit = writer.execute_batch("COMMIT");
    assert_eq!(
        busy, 23,
        "writer lock was not actually busy: child {busy}; commit: {commit:?}"
    );
    commit.unwrap();
    assert_eq!(
        contender(&index),
        29,
        "unlocked child did not execute a write"
    );
    let after: i64 = writer
        .query_row("SELECT last_opened_at FROM index_metadata", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        after,
        before + 1,
        "child's committed update was not durable"
    );
}

#[test]
fn live_request_writer_survives_queue_worker_reads() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let accepted = store
        .enqueue_request(&IndexOptions::new(workspace.path().to_owned()), None)
        .unwrap();
    let queue = store.request_db_path();
    let writer = Connection::open(&queue).unwrap();
    writer
        .execute_batch("BEGIN IMMEDIATE; UPDATE requests SET submitted_at=submitted_at")
        .unwrap();
    let selected = store.clone();
    std::thread::spawn(move || selected.request_by_id(&accepted.id).unwrap())
        .join()
        .unwrap();
    let before: String = writer
        .query_row("SELECT submitted_at FROM requests WHERE seq=1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let busy = contender(&queue);
    let commit = writer.execute_batch("COMMIT");
    assert_eq!(
        busy, 23,
        "queue writer lock was not actually busy: child {busy}; commit: {commit:?}"
    );
    commit.unwrap();
    assert_eq!(
        contender(&queue),
        29,
        "unlocked queue child did not execute a write"
    );
    let after: String = writer
        .query_row("SELECT submitted_at FROM requests WHERE seq=1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        after,
        format!("{before}!"),
        "child's committed queue update was not durable"
    );
}

// Run exceptional recreation in its own process so retained descriptors do not
// mask leaked old inodes behind other tests' process-wide cache.
#[test]
fn corrupt_index_recreation_child() {
    if std::env::var_os("TRELLIS_RECREATE_CHILD").is_none() {
        return;
    }
    use std::{
        os::unix::fs::MetadataExt,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::index_coordinator::reconcile_workspace;
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("a.js"),
        "function a() {}
",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let queue = store
        .enqueue_request(&IndexOptions::new(workspace.path().to_owned()), None)
        .unwrap();
    let requests = store.request_db_path();
    let queue_inode = std::fs::metadata(&requests).unwrap().ino();
    let index = std::fs::read_dir(state.path().join("cache/indexes"))
        .unwrap()
        .filter_map(|e| {
            let path = e.unwrap().path().join("index.db");
            path.exists().then_some(path)
        })
        .next()
        .unwrap();
    let old_inode = std::fs::metadata(&index).unwrap().ino();
    drop(store);
    std::fs::write(&index, b"short").unwrap();
    let recovering = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let (pin, _writer) = reconcile_workspace(
        &recovering,
        &IndexOptions::new(workspace.path().to_owned()),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert_eq!(pin.index_revision, 1);
    assert_ne!(std::fs::metadata(&index).unwrap().ino(), old_inode);
    assert_eq!(std::fs::metadata(&requests).unwrap().ino(), queue_inode);
    assert_eq!(
        recovering.request_by_id(&queue.id).unwrap().unwrap().id,
        queue.id
    );
}

#[test]
fn corrupt_index_recreation_preserves_queue_and_replaces_only_index_inode() {
    let outcome = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg(child_test_name("corrupt_index_recreation_child"))
        .env("TRELLIS_RECREATE_CHILD", "1")
        .output()
        .unwrap();
    assert!(
        outcome.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&outcome.stdout),
        String::from_utf8_lossy(&outcome.stderr)
    );
}

#[test]
fn gc_releases_only_deleted_candidate_witnesses_child() {
    if std::env::var_os("TRELLIS_GC_WITNESS_CHILD").is_none() {
        return;
    }
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::{
        index_coordinator::reconcile_workspace,
        store::{
            retained_sqlite_witness_count_for_tests,
            topology::{TopologyRoots, WorkspaceIdentity},
        },
    };
    let state = tempfile::tempdir().unwrap();
    let old = tempfile::tempdir().unwrap();
    let current = tempfile::tempdir().unwrap();
    std::fs::write(
        old.path().join("a.js"),
        "function a() {}
",
    )
    .unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(state.path().join("cache"), state.path().join("data"));
    let old_id = WorkspaceIdentity::discover(Some(old.path()), old.path()).unwrap();
    let original = Store::open_for_tests(state.path(), old.path()).unwrap();
    let queue = original
        .enqueue_request(&IndexOptions::new(old.path().to_owned()), None)
        .unwrap();
    let (pin, old_writer) = reconcile_workspace(
        &original,
        &IndexOptions::new(old.path().to_owned()),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert!(roots.requests_db(&old_id).exists());
    drop(old_writer);
    drop(original);
    let live = Store::open_for_tests(state.path(), current.path()).unwrap();
    let live_id =
        Arc::new(WorkspaceIdentity::discover(Some(current.path()), current.path()).unwrap());
    let writer = roots.writer(live_id.clone()).unwrap();
    let before = retained_sqlite_witness_count_for_tests();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 86_400;
    Connection::open(roots.index_db(&old_id))
        .unwrap()
        .execute(
            "UPDATE index_metadata SET last_opened_at=?1",
            [now - 31 * 24 * 60 * 60],
        )
        .unwrap();
    assert_eq!(live.automatic_gc_at(&writer, now).unwrap(), 1);
    assert_eq!(
        retained_sqlite_witness_count_for_tests(),
        before - 2,
        "both deleted SQLite inodes must release their retained handles"
    );
    assert!(roots.index_db(&live_id).exists());
    assert!(!roots.index_dir(&old_id).exists());
    assert!(!roots.index_use_lock(&old_id).exists());
    assert_eq!(queue.state, "queued");
    let replacement = Store::open_for_tests(state.path(), old.path()).unwrap();
    assert_ne!(
        replacement.index_baseline().unwrap().index_generation,
        pin.index_generation
    );
    assert!(
        replacement.files_at(Some(pin), 0, 100).is_err(),
        "old generation pins cannot reopen"
    );
}

#[test]
fn gc_releases_only_deleted_candidate_witnesses() {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            child_test_name("gc_releases_only_deleted_candidate_witnesses_child").as_str(),
            "--nocapture",
        ])
        .env("TRELLIS_GC_WITNESS_CHILD", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn gc_fault_after_first_unlink_reports_partial_failure_not_success() {
    use trellis::store::topology::{GcStage, TopologyRoots, WorkspaceIdentity};
    let state = tempfile::tempdir().unwrap();
    let current_root = tempfile::tempdir().unwrap();
    let candidate_root = tempfile::tempdir().unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(state.path().join("cache"), state.path().join("data"));
    let current = std::sync::Arc::new(
        WorkspaceIdentity::discover(Some(current_root.path()), current_root.path()).unwrap(),
    );
    let candidate = std::sync::Arc::new(
        WorkspaceIdentity::discover(Some(candidate_root.path()), candidate_root.path()).unwrap(),
    );
    drop(Store::open_for_tests(state.path(), current_root.path()).unwrap());
    let store = Store::open_for_tests(state.path(), candidate_root.path()).unwrap();
    store
        .enqueue_request(&IndexOptions::new(candidate_root.path().to_owned()), None)
        .unwrap();
    drop(store);
    drop(roots.writer(candidate.clone()).unwrap());
    let writer = roots.writer(current.clone()).unwrap();
    let now = 1_800_000_000_i64;
    let index = roots.index_db(&candidate);
    Connection::open(&index)
        .unwrap()
        .execute(
            "UPDATE index_metadata SET last_opened_at=?1",
            [now - 31 * 24 * 60 * 60],
        )
        .unwrap();
    let requests = roots.requests_db(&candidate);
    let queue_before = std::fs::read(&requests).unwrap();
    let mut hit = 0;
    let result = roots.automatic_gc_at_with_hook(&current, &writer, now, &mut |stage| {
        if stage == GcStage::AfterFirstDbUnlink {
            hit += 1;
            assert!(!index.exists(), "first derived unlink must have happened");
            anyhow::bail!("injected failure after first derived unlink");
        }
        Ok(())
    });
    assert_eq!(hit, 1);
    assert!(
        result.is_err(),
        "partial deletion must not count as GC success"
    );
    assert!(!index.exists());
    assert_eq!(std::fs::read(&requests).unwrap(), queue_before);
    assert!(roots.index_use_lock(&candidate).exists());
    assert!(roots.index_dir(&current).exists());
}

#[test]
fn gc_fault_after_parent_sync_reports_error_before_last_lock_removal() {
    use trellis::store::topology::{GcStage, TopologyRoots, WorkspaceIdentity};
    let state = tempfile::tempdir().unwrap();
    let current_root = tempfile::tempdir().unwrap();
    let candidate_root = tempfile::tempdir().unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(state.path().join("cache"), state.path().join("data"));
    let current = std::sync::Arc::new(
        WorkspaceIdentity::discover(Some(current_root.path()), current_root.path()).unwrap(),
    );
    let candidate = std::sync::Arc::new(
        WorkspaceIdentity::discover(Some(candidate_root.path()), candidate_root.path()).unwrap(),
    );
    drop(Store::open_for_tests(state.path(), current_root.path()).unwrap());
    drop(Store::open_for_tests(state.path(), candidate_root.path()).unwrap());
    drop(roots.writer(candidate.clone()).unwrap());
    let writer = roots.writer(current.clone()).unwrap();
    let now = 1_800_000_000_i64;
    Connection::open(roots.index_db(&candidate))
        .unwrap()
        .execute(
            "UPDATE index_metadata SET last_opened_at=?1",
            [now - 31 * 24 * 60 * 60],
        )
        .unwrap();
    let mut hit = 0;
    let result = roots.automatic_gc_at_with_hook(&current, &writer, now, &mut |stage| {
        if stage == GcStage::AfterParentSync {
            hit += 1;
            assert!(!roots.index_dir(&candidate).exists());
            assert!(roots.index_use_lock(&candidate).exists());
            anyhow::bail!("injected failure after parent sync");
        }
        Ok(())
    });
    assert_eq!(hit, 1);
    assert!(
        result.is_err(),
        "post-sync failure must not claim successful GC"
    );
    assert!(!roots.index_dir(&candidate).exists());
    assert!(
        roots.index_use_lock(&candidate).exists(),
        "beside-directory use lock is always last"
    );
}

/// A child test's libtest name in the grouped test binary, which names this
/// module's tests `<module>::<test>`.
fn child_test_name(test: &str) -> String {
    format!("{}::{test}", module_path!().split_once("::").unwrap().1)
}
