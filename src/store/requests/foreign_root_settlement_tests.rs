use super::*;

#[test]
fn settles_queued_and_running_foreign_rows_without_touching_current_fifo() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let options = IndexOptions::new(root.path().to_owned());
    let old_queued = store.enqueue_request(&options, None).unwrap();
    let old_running = store.enqueue_request(&options, None).unwrap();
    let current = store.enqueue_request(&options, None).unwrap();
    let db = Connection::open(store.request_db_path()).unwrap();
    db.execute(
        "UPDATE requests SET root_device='old',root_inode='old' WHERE id IN (?1,?2)",
        params![old_queued.id, old_running.id],
    )
    .unwrap();
    db.execute(
        "UPDATE requests SET state='running',claim_incarnation='prior',started_at='1' WHERE id=?1",
        [&old_running.id],
    )
    .unwrap();
    drop(db);
    assert_eq!(store.settle_foreign_root_requests().unwrap(), 2);
    for accepted in [&old_queued, &old_running] {
        let terminal = store.request_by_id(&accepted.id).unwrap_err();
        assert!(terminal.to_string().contains("root_changed"));
        let db = Connection::open(store.request_db_path()).unwrap();
        let row = db
            .query_row(
                &format!("SELECT {COLUMNS} FROM requests WHERE id=?1"),
                [&accepted.id],
                read,
            )
            .unwrap();
        assert_eq!(row.id, accepted.id);
        assert_eq!(row.seq, accepted.seq);
        assert_eq!(row.state, "failed");
        assert_eq!(row.error_code.as_deref(), Some("root_changed"));
        assert!(row.finished_at.is_some());
    }
    let pending = store.earliest_unfinished_request().unwrap().unwrap();
    assert_eq!((pending.id, pending.seq), (current.id, current.seq));
    assert_eq!(store.settle_foreign_root_requests().unwrap(), 0);
}

#[test]
fn ambiguous_commit_reopens_and_confirms_terminal_rows() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let accepted = store
        .enqueue_request(&IndexOptions::new(root.path().to_owned()), None)
        .unwrap();
    let db = Connection::open(store.request_db_path()).unwrap();
    db.execute(
        "UPDATE requests SET root_inode='old' WHERE id=?1",
        [&accepted.id],
    )
    .unwrap();
    drop(db);
    assert_eq!(
        store
            .settle_foreign_root_requests_with_hook(|| anyhow::bail!("injected ambiguous COMMIT"))
            .unwrap(),
        1
    );
    let db = Connection::open(store.request_db_path()).unwrap();
    let row = db
        .query_row(
            &format!("SELECT {COLUMNS} FROM requests WHERE id=?1"),
            [&accepted.id],
            read,
        )
        .unwrap();
    assert_eq!(
        (
            row.id,
            row.seq,
            row.state.as_str(),
            row.error_code.as_deref()
        ),
        (accepted.id, accepted.seq, "failed", Some("root_changed"))
    );
}

#[test]
fn locked_post_commit_reopen_returns_typed_busy_without_losing_terminal_row() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let accepted = store
        .enqueue_request(&IndexOptions::new(root.path().to_owned()), None)
        .unwrap();
    let path = store.request_db_path();
    let setup = Connection::open(&path).unwrap();
    setup
        .execute(
            "UPDATE requests SET root_inode='old' WHERE id=?1",
            [&accepted.id],
        )
        .unwrap();
    drop(setup);
    let blocker = Connection::open(&path).unwrap();
    let error = store
        .settle_foreign_root_requests_with_hook(|| {
            blocker.execute_batch("BEGIN EXCLUSIVE")?;
            Ok(())
        })
        .unwrap_err();
    assert_eq!(error.to_string(), "storage_busy: SQLite lock contention");
    assert!(
        error
            .downcast_ref::<super::super::SqliteContention>()
            .is_some()
    );
    assert!(
        !error
            .chain()
            .any(|cause| cause.downcast_ref::<rusqlite::Error>().is_some())
    );
    blocker.execute_batch("ROLLBACK").unwrap();
    assert_eq!(store.settle_foreign_root_requests().unwrap(), 0);
    let db = Connection::open(&path).unwrap();
    let terminal = db
        .query_row(
            &format!("SELECT {COLUMNS} FROM requests WHERE id=?1"),
            [&accepted.id],
            read,
        )
        .unwrap();
    assert_eq!(
        (
            terminal.id,
            terminal.seq,
            terminal.state.as_str(),
            terminal.error_code.as_deref()
        ),
        (accepted.id, accepted.seq, "failed", Some("root_changed"))
    );
}

#[test]
fn current_request_rejects_existing_v0_without_creating_schema() {
    use std::os::unix::fs::OpenOptionsExt;
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let path = store.request_db_path();
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    let mut attempts = 0;
    let error = store
        .current_request_with_retry(std::time::Duration::from_secs(3), || {
            attempts += 1;
        })
        .unwrap_err();
    assert_eq!(error.to_string(), "incompatible_queue: schema version");
    assert_eq!(attempts, 0);
}

#[test]
fn current_request_v0_is_incompatible_even_at_retry_deadline() {
    use std::os::unix::fs::OpenOptionsExt;
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let path = store.request_db_path();
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    let error = store
        .current_request_with_retry(std::time::Duration::ZERO, || {
            panic!("no retry after deadline")
        })
        .unwrap_err();
    assert_eq!(error.to_string(), "incompatible_queue: schema version");
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        0
    );
    let objects: i64 = db
        .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
        .unwrap();
    assert_eq!(objects, 0);
}

#[test]
fn private_stage_fault_before_publish_leaves_main_absent_and_retries() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let path = store.request_db_path();
    let mut stage_path = None;
    let failure = store
        .bootstrap_missing_queue(
            |stage| {
                let db =
                    Connection::open_with_flags(stage, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                assert_eq!(
                    db.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))?,
                    2
                );
                assert!(!path.exists());
                stage_path = Some(stage.to_owned());
                anyhow::bail!("injected error before queue publication")
            },
            |_| Ok(()),
        )
        .unwrap_err();
    assert_eq!(
        failure.to_string(),
        "injected error before queue publication"
    );
    let stage = stage_path.unwrap();
    assert!(
        !stage.exists(),
        "ordinary errors remove their private stage"
    );
    assert!(!path.exists());
    assert!(
        fs::read_dir(path.parent().unwrap()).unwrap().all(|entry| {
            matches!(
                entry.unwrap().file_name().to_str(),
                Some("index.db" | "requests.db")
            )
        }),
        "failed stage must not change candidate GC inventory"
    );
    assert_eq!(store.settle_foreign_root_requests().unwrap(), 0);
    assert!(path.exists());
}

#[test]
fn crashed_stage_orphan_sibling_does_not_change_candidate_gc_inventory() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let queue = store.request_db_path();
    let mut orphan = None;
    let _ = store.bootstrap_missing_queue(
        |stage| {
            let sibling = stage.with_extension("crash-orphan");
            fs::copy(stage, &sibling)?;
            orphan = Some(sibling);
            anyhow::bail!("simulate orphan from process death")
        },
        |_| Ok(()),
    );
    let orphan = orphan.unwrap();
    assert!(orphan.exists());
    assert_eq!(orphan.parent(), queue.parent().unwrap().parent());
    assert!(!queue.exists());
    assert!(fs::read_dir(queue.parent().unwrap()).unwrap().all(|entry| {
        matches!(
            entry.unwrap().file_name().to_str(),
            Some("index.db" | "requests.db")
        )
    }));
    assert_eq!(store.settle_foreign_root_requests().unwrap(), 0);
}

#[test]
fn publish_between_dir_syncs_reopens_v2_without_losing_witness_gate() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let queue = store.request_db_path();
    let error = store
        .bootstrap_missing_queue(
            |_| Ok(()),
            |published| {
                assert_eq!(published, queue);
                anyhow::bail!("injected failure between source and destination sync")
            },
        )
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "injected failure between source and destination sync"
    );
    assert!(queue.exists(), "published v1 must not be discarded");
    assert!(store.request_file_witness.lock().unwrap().is_none());
    // Existing-only admission validates v1 and syncs both source and
    // destination directories before setting its first process witness.
    assert!(store.existing_request_connection().unwrap().is_some());
    assert!(store.request_file_witness.lock().unwrap().is_some());
    assert!(store.current_request().unwrap().is_none());
}

#[test]
fn published_stage_reopens_with_committed_identity() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    store
        .bootstrap_missing_queue(|_| Ok(()), |_| Ok(()))
        .unwrap();
    let path = store.request_db_path();
    let expected = (
        store.workspace_root.clone(),
        store.identity.root_key.clone(),
    );
    drop(store);
    let db =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let identity: (String, String) = db
        .query_row(
            "SELECT root_spelling,root_key FROM queue_identity",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(identity, expected);
    drop(db);
    let reopened = Store::open_for_tests(state.path(), root.path()).unwrap();
    assert!(reopened.current_request().unwrap().is_none());
}

#[test]
fn accepted_before_startup_accidentally_truncated_v0_is_never_initialized() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let accepted = store
        .enqueue_request(&IndexOptions::new(root.path().to_owned()), None)
        .unwrap();
    assert!(!accepted.id.is_empty());
    let path = store.request_db_path();
    drop(store);
    fs::write(&path, []).unwrap();
    let reopened = Store::open_for_tests(state.path(), root.path()).unwrap();
    let error = reopened.settle_foreign_root_requests().unwrap_err();
    assert!(
        error.to_string().contains("incompatible_queue"),
        "{error:#}"
    );
    drop(reopened);
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        0
    );
}

#[test]
fn concurrent_creator_cannot_publish_over_first_queue() {
    use std::sync::mpsc;
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let second = store.clone();
    let (staged_tx, staged_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        second.bootstrap_missing_queue(
            |_| {
                staged_tx.send(()).unwrap();
                resume_rx
                    .recv_timeout(crate::testing::HANG_BACKSTOP)
                    .unwrap();
                Ok(())
            },
            |_| Ok(()),
        )
    });
    staged_rx
        .recv_timeout(crate::testing::HANG_BACKSTOP)
        .unwrap();
    let main_absent = !store.request_db_path().exists();
    let contender = store.bootstrap_missing_queue(|_| Ok(()), |_| Ok(()));
    resume_tx.send(()).unwrap();
    worker.join().unwrap().unwrap();
    assert!(main_absent);
    assert!(
        contender
            .unwrap_err()
            .is::<super::super::topology::StorageBusy>()
    );
    let inode = fs::metadata(store.request_db_path()).unwrap().ino();
    store
        .bootstrap_missing_queue(|_| panic!("loser must reopen winner"), |_| Ok(()))
        .unwrap();
    assert_eq!(fs::metadata(store.request_db_path()).unwrap().ino(), inode);
    assert!(store.current_request().unwrap().is_none());
}

#[test]
fn enqueue_waits_for_other_store_bootstrap_before_single_accept() {
    use std::sync::mpsc;
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let builder = Store::open_for_tests(state.path(), root.path()).unwrap();
    let caller = Store::open_for_tests(state.path(), root.path()).unwrap();
    let (staged_tx, staged_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let creator = std::thread::spawn(move || {
        builder.bootstrap_missing_queue(
            |_| {
                staged_tx.send(()).unwrap();
                resume_rx
                    .recv_timeout(crate::testing::HANG_BACKSTOP)
                    .unwrap();
                Ok(())
            },
            |_| Ok(()),
        )
    });
    staged_rx
        .recv_timeout(crate::testing::HANG_BACKSTOP)
        .unwrap();
    let (retry_tx, retry_rx) = mpsc::channel();
    let hook = caller.test_queue_before_shared_hook.clone();
    caller.test_queue_before_shared_hook.set(move || {
        hook.set(move || retry_tx.send(()).unwrap());
    });
    let options = IndexOptions::new(root.path().to_owned());
    let queued = std::thread::spawn(move || caller.enqueue_request(&options, None));
    // The second SH attempt proves the first one failed under builder EX.
    let reached_retry = retry_rx.recv_timeout(crate::testing::HANG_BACKSTOP);
    // Release EX and join even when the expected retry signal was missed.
    resume_tx.send(()).unwrap();
    creator.join().unwrap().unwrap();
    let accepted = queued.join().unwrap().unwrap();
    reached_retry.unwrap();
    let reopened = Store::open_for_tests(state.path(), root.path()).unwrap();
    let (_guard, db) = reopened.request_connection().unwrap();
    let count: i64 = db
        .query_row("SELECT count(*) FROM requests", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(
        reopened.request_by_id(&accepted.id).unwrap().unwrap().id,
        accepted.id
    );
}

#[test]
fn exhausted_preaccept_admission_returns_busy_without_row() {
    use std::sync::mpsc;
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let builder = Store::open_for_tests(state.path(), root.path()).unwrap();
    let caller = Store::open_for_tests(state.path(), root.path()).unwrap();
    let (staged_tx, staged_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let creator = std::thread::spawn(move || {
        builder.bootstrap_missing_queue(
            |_| {
                staged_tx.send(()).unwrap();
                resume_rx
                    .recv_timeout(crate::testing::HANG_BACKSTOP)
                    .unwrap();
                Ok(())
            },
            |_| Ok(()),
        )
    });
    staged_rx
        .recv_timeout(crate::testing::HANG_BACKSTOP)
        .unwrap();
    let error = caller
        .request_connection_for_enqueue_until(Instant::now())
        .err()
        .unwrap();
    assert!(error.is::<super::super::topology::StorageBusy>());
    assert!(!caller.request_db_path().exists());
    resume_tx.send(()).unwrap();
    creator.join().unwrap().unwrap();
    let (_guard, db) = caller.request_connection().unwrap();
    let count: i64 = db
        .query_row("SELECT count(*) FROM requests", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn bootstrap_reopen_during_root_loss_is_root_changed_not_a_vanished_queue() {
    use std::sync::mpsc;
    let state = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    fs::create_dir(&root).unwrap();
    let store = Store::open_for_tests(state.path(), &root).unwrap();
    assert!(!store.request_db_path().exists());
    let (arrived_tx, arrived_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let hook = store.test_queue_before_shared_hook.clone();
    store.test_queue_before_shared_hook.set(move || {
        hook.set(move || {
            arrived_tx.send(()).unwrap();
            resume_rx
                .recv_timeout(crate::testing::HANG_BACKSTOP)
                .unwrap();
        });
    });
    let reader = store.clone();
    let worker = std::thread::spawn(move || reader.request_connection().map(|_| ()));
    // The second admission is the recursive reopen after publication.
    let arrived = arrived_rx.recv_timeout(crate::testing::HANG_BACKSTOP);
    let published = store.request_db_path().exists();
    let renamed = arrived
        .as_ref()
        .ok()
        .map(|_| fs::rename(&root, parent.path().join("old-workspace")));
    // Release and join before asserting, including on a missed arrival.
    let _ = resume_tx.send(());
    let result = worker.join();
    arrived.unwrap();
    assert!(
        published,
        "bootstrap must publish before the recursive reopen"
    );
    renamed.unwrap().unwrap();
    assert!(!root.exists());
    let error = result.unwrap().unwrap_err();
    assert!(error.to_string().starts_with("root_changed"), "{error:#}");
}

#[test]
fn bootstrap_reopen_with_vanished_queue_is_incompatible_queue() {
    use std::sync::mpsc;
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    assert!(!store.request_db_path().exists());
    let (arrived_tx, arrived_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let hook = store.test_queue_before_shared_hook.clone();
    store.test_queue_before_shared_hook.set(move || {
        hook.set(move || {
            arrived_tx.send(()).unwrap();
            resume_rx
                .recv_timeout(crate::testing::HANG_BACKSTOP)
                .unwrap();
        });
    });
    let reader = store.clone();
    let worker = std::thread::spawn(move || reader.request_connection().map(|_| ()));
    let arrived = arrived_rx.recv_timeout(crate::testing::HANG_BACKSTOP);
    let published = store.request_db_path().exists();
    let removed = arrived
        .as_ref()
        .ok()
        .map(|_| fs::remove_file(store.request_db_path()));
    let _ = resume_tx.send(());
    let result = worker.join();
    arrived.unwrap();
    assert!(
        published,
        "bootstrap must publish before the recursive reopen"
    );
    removed.unwrap().unwrap();
    store.identity.verify().unwrap();
    let error = result.unwrap().unwrap_err();
    assert_eq!(
        error.to_string(),
        "incompatible_queue: published requests.db disappeared"
    );
}

#[test]
fn root_replaced_missing_queue_does_not_publish() {
    let state = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    fs::create_dir(&root).unwrap();
    let store = Store::open_for_tests(state.path(), &root).unwrap();
    fs::rename(&root, parent.path().join("old-workspace")).unwrap();
    fs::create_dir(&root).unwrap();
    assert!(store.request_connection().is_err());
    assert!(!store.request_db_path().exists());
}

#[test]
fn missing_virgin_queue_bootstraps_empty_queue_before_first_startup() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    assert!(!store.request_db_path().exists());
    assert_eq!(store.settle_foreign_root_requests().unwrap(), 0);
    assert!(store.request_db_path().exists());
    assert!(store.current_request().unwrap().is_none());
}

#[test]
fn v1_accepted_and_terminal_rows_migrate_atomically_without_optional_inputs() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let path = store.request_db_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let db = Connection::open(&path).unwrap();
    db.execute_batch(V1_SCHEMA).unwrap();
    db.execute(
        "INSERT INTO queue_identity VALUES (1,?1,?2)",
        params![store.workspace_root, store.identity.root_key],
    )
    .unwrap();
    let generation = Uuid::new_v4().to_string();
    let ids: Vec<_> = (0..4).map(|_| Uuid::new_v4().to_string()).collect();
    let old_options = serde_json::json!({"version":1,"maxFileBytes":8192,"scipPath":"/missing/old.scip","manifestPath":"/missing/old.json"}).to_string();
    for (i, id) in ids.iter().enumerate() {
        let (status, claim, result, err, started, finished) = match i {
            0 => ("queued", None, None, None, None, None),
            1 => ("running", Some("prior"), None, None, Some("102"), None),
            2 => (
                "done",
                Some("prior"),
                Some(9),
                None,
                Some("103"),
                Some("104"),
            ),
            _ => (
                "failed",
                Some("prior"),
                None,
                Some("index_failed"),
                Some("105"),
                Some("106"),
            ),
        };
        db.execute("INSERT INTO requests(seq,id,root_device,root_inode,options_json,expected_generation,expected_revision,state,claim_incarnation,result_generation,result_revision,error_code,submitted_at,started_at,finished_at) VALUES (?1,?2,?3,?4,?5,?6,7,?7,?8,?9,?10,?11,?12,?13,?14)", params![i as i64+1,id,store.identity.device.to_string(),store.identity.inode.to_string(),old_options,generation,status,claim,result.map(|_| generation.clone()),result,err,format!("10{i}"),started,finished]).unwrap();
    }
    drop(db);
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let queued = store.earliest_unfinished_request().unwrap().unwrap();
    assert_eq!(queued.id, ids[0]);
    assert_eq!(queued.options(root.path()).unwrap().max_file_bytes, 8192);
    let db = Connection::open(&path).unwrap();
    let version: i64 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 2);
    validate_gc_queue(&db, &store.workspace_root, &store.identity.root_key).unwrap();
    for (i, id) in ids.iter().enumerate() {
        let row = db
            .query_row(
                &format!("SELECT {COLUMNS} FROM requests WHERE id=?1"),
                [id],
                read,
            )
            .unwrap();
        assert_eq!(row.seq, i as i64 + 1);
        assert_eq!(row.id, *id);
        assert_eq!(row.root_device, store.identity.device.to_string());
        assert_eq!(row.root_inode, store.identity.inode.to_string());
        assert_eq!(
            row.expected.unwrap().index_generation.to_string(),
            generation
        );
        assert_eq!(row.expected.unwrap().index_revision, 7);
        assert_eq!(row.submitted_at, format!("10{i}"));
        assert_eq!(row.request_kind, RequestKind::Index);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&row.options_json).unwrap(),
            serde_json::json!({"version":2,"maxFileBytes":8192})
        );
        assert_eq!(row.state, ["queued", "running", "done", "failed"][i]);
        assert_eq!(
            row.claim_incarnation.as_deref(),
            [None, Some("prior"), Some("prior"), Some("prior")][i]
        );
        assert_eq!(
            row.started_at.as_deref(),
            [None, Some("102"), Some("103"), Some("105")][i]
        );
        assert_eq!(
            row.finished_at.as_deref(),
            [None, None, Some("104"), Some("106")][i]
        );
        assert_eq!(
            row.error_code.as_deref(),
            if i == 3 { Some("index_failed") } else { None }
        );
        assert_eq!(
            row.revision.map(|p| p.index_revision),
            if i == 2 { Some(9) } else { None }
        );
        assert_eq!(
            row.revision.map(|p| p.index_generation.to_string()),
            if i == 2 {
                Some(generation.clone())
            } else {
                None
            }
        );
    }
    drop(db);
}

#[test]
fn malformed_v1_row_does_not_partially_migrate_queue() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let path = store.request_db_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let db = Connection::open(&path).unwrap();
    db.execute_batch(V1_SCHEMA).unwrap();
    db.execute(
        "INSERT INTO queue_identity VALUES (1,?1,?2)",
        params![store.workspace_root, store.identity.root_key],
    )
    .unwrap();
    for (i, options) in [
        r#"{"version":1,"maxFileBytes":1024,"scipPath":"/old","manifestPath":null}"#,
        r#"{"version":1,"maxFileBytes":1024,"unexpected":true}"#,
    ]
    .iter()
    .enumerate()
    {
        db.execute("INSERT INTO requests(id,root_device,root_inode,options_json,state,submitted_at) VALUES (?1,?2,?3,?4,'queued','100')",params![Uuid::new_v4().to_string(),store.identity.device.to_string(),store.identity.inode.to_string(),options]).unwrap();
        assert_eq!(
            i as i64 + 1,
            db.query_row("SELECT count(*) FROM requests", [], |r| r.get::<_, i64>(0))
                .unwrap()
        );
    }
    drop(db);
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    assert!(
        store
            .earliest_unfinished_request()
            .unwrap_err()
            .to_string()
            .contains("incompatible_queue")
    );
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM requests", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert!(!db.prepare("SELECT request_kind FROM requests").is_ok());
}

#[test]
fn migrated_v1_native_fifo_executes_without_old_artifact_paths() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let path = store.request_db_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let db = Connection::open(&path).unwrap();
    db.execute_batch(V1_SCHEMA).unwrap();
    db.execute(
        "INSERT INTO queue_identity VALUES (1,?1,?2)",
        params![store.workspace_root, store.identity.root_key],
    )
    .unwrap();
    let ids = [Uuid::new_v4().to_string(), Uuid::new_v4().to_string()];
    for id in &ids {
        db.execute("INSERT INTO requests(id,root_device,root_inode,options_json,state,submitted_at) VALUES (?1,?2,?3,?4,'queued','100')",params![id,store.identity.device.to_string(),store.identity.inode.to_string(),serde_json::json!({"version":1,"maxFileBytes":8192,"scipPath":"/does-not-exist/old.scip","manifestPath":"/does-not-exist/old.json"}).to_string()]).unwrap();
    }
    drop(db);
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let options = IndexOptions::new(root.path().to_owned());
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (_, owner) =
        crate::index_coordinator::reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
    assert_eq!(
        crate::index_coordinator::drain_requests(&store, &owner).unwrap(),
        2
    );
    for id in ids {
        let row = store.request_by_id(&id).unwrap().unwrap();
        assert_eq!(row.state, "done");
        assert_eq!(row.options(root.path()).unwrap().max_file_bytes, 8192);
    }
    assert_eq!(
        store
            .graph_at(None)
            .unwrap()
            .nodes
            .iter()
            .filter(|n| n.name == "a")
            .count(),
        1
    );
}
