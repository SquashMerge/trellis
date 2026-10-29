use super::*;

#[test]
fn recovery_blocked_disposes_only_queued_rows_for_the_captured_root() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let options = IndexOptions::new(root.path().to_owned());
    let queued = store.enqueue_request(&options, None).unwrap();
    let running = store.enqueue_request(&options, None).unwrap();
    let done = store.enqueue_request(&options, None).unwrap();
    let failed = store.enqueue_request(&options, None).unwrap();
    let foreign_device = store.enqueue_request(&options, None).unwrap();
    let foreign_inode = store.enqueue_request(&options, None).unwrap();
    let generation = Uuid::new_v4().to_string();
    let (_guard, db) = store.existing_request_connection().unwrap().unwrap();
    db.execute(
        "UPDATE requests SET state='running',claim_incarnation='prior',started_at='1' WHERE id=?1",
        [&running.id],
    )
    .unwrap();
    db.execute(
        "UPDATE requests SET state='done',claim_incarnation='prior',started_at='1',finished_at='2',result_generation=?1,result_revision=1 WHERE id=?2",
        params![generation, done.id],
    )
    .unwrap();
    db.execute(
        "UPDATE requests SET state='failed',claim_incarnation='prior',started_at='1',finished_at='2',error_code='prior_failure' WHERE id=?1",
        [&failed.id],
    )
    .unwrap();
    db.execute(
        "UPDATE requests SET root_device='foreign' WHERE id=?1",
        [&foreign_device.id],
    )
    .unwrap();
    db.execute(
        "UPDATE requests SET root_inode='foreign' WHERE id=?1",
        [&foreign_inode.id],
    )
    .unwrap();
    drop(db);
    drop(_guard);
    let terminal_generation = store.terminal_generation();
    assert_eq!(store.fail_queued_recovery_blocked("index I/O").unwrap(), 1);
    assert!(store.terminal_generation() > terminal_generation);
    let terminal = store.request_by_id(&queued.id).unwrap().unwrap();
    assert_eq!((terminal.seq, &terminal.id), (queued.seq, &queued.id));
    assert_eq!(terminal.state, "failed");
    assert_eq!(
        terminal.error_code.as_deref(),
        Some("recovery_blocked: index I/O")
    );
    Uuid::parse_str(terminal.claim_incarnation.as_deref().unwrap()).unwrap();
    assert!(terminal.started_at.is_some());
    assert_eq!(terminal.started_at, terminal.finished_at);
    assert!(terminal.revision.is_none());
    let running = store.request_by_id(&running.id).unwrap().unwrap();
    assert_eq!(running.state, "running");
    assert_eq!(running.claim_incarnation.as_deref(), Some("prior"));
    assert_eq!(running.started_at.as_deref(), Some("1"));
    assert!(running.finished_at.is_none() && running.error_code.is_none());
    let done = store.request_by_id(&done.id).unwrap().unwrap();
    assert_eq!(done.state, "done");
    assert_eq!(done.claim_incarnation.as_deref(), Some("prior"));
    assert_eq!(done.started_at.as_deref(), Some("1"));
    assert_eq!(done.finished_at.as_deref(), Some("2"));
    assert_eq!(
        done.revision.unwrap().index_generation.to_string(),
        generation
    );
    let failed = store.request_by_id(&failed.id).unwrap().unwrap();
    assert_eq!(failed.state, "failed");
    assert_eq!(failed.claim_incarnation.as_deref(), Some("prior"));
    assert_eq!(failed.started_at.as_deref(), Some("1"));
    assert_eq!(failed.finished_at.as_deref(), Some("2"));
    assert_eq!(failed.error_code.as_deref(), Some("prior_failure"));
    let (_guard, db) = store.existing_request_connection().unwrap().unwrap();
    for id in [foreign_device.id, foreign_inode.id] {
        let row = db
            .query_row(
                &format!("SELECT {COLUMNS} FROM requests WHERE id=?1"),
                [id],
                read,
            )
            .unwrap();
        assert_eq!(row.state, "queued");
        assert!(row.claim_incarnation.is_none() && row.finished_at.is_none());
    }
    drop(db);
    drop(_guard);
    let terminal_generation = store.terminal_generation();
    assert_eq!(store.fail_queued_recovery_blocked("again").unwrap(), 0);
    assert!(store.terminal_generation() > terminal_generation);
}

#[test]
fn recovery_blocked_confirms_an_ambiguous_commit_before_notifying() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let accepted = store
        .enqueue_request(&IndexOptions::new(root.path().to_owned()), None)
        .unwrap();
    let terminal_generation = store.terminal_generation();
    assert_eq!(
        store
            .fail_queued_recovery_blocked_with_hook("fault", || {
                assert_eq!(store.terminal_generation(), terminal_generation);
                anyhow::bail!("injected ambiguous COMMIT")
            })
            .unwrap(),
        1
    );
    assert!(store.terminal_generation() > terminal_generation);
    let row = store.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(row.state, "failed");
    assert_eq!(row.error_code.as_deref(), Some("recovery_blocked: fault"));
}

#[test]
fn recovery_blocked_does_not_notify_an_unconfirmed_ambiguous_commit() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let accepted = store
        .enqueue_request(&IndexOptions::new(root.path().to_owned()), None)
        .unwrap();
    let terminal_generation = store.terminal_generation();
    let error = store
        .fail_queued_recovery_blocked_with_hook("fault", || {
            // Model an ambiguous COMMIT whose intended transition was not durable.
            let (_guard, db) = store.existing_request_connection()?.unwrap();
            db.execute(
                "UPDATE requests SET state='queued',claim_incarnation=NULL,started_at=NULL,finished_at=NULL,error_code=NULL WHERE id=?1",
                [&accepted.id],
            )?;
            anyhow::bail!("injected ambiguous COMMIT")
        })
        .unwrap_err();
    assert!(error.is::<super::super::topology::StorageBusy>());
    assert_eq!(store.terminal_generation(), terminal_generation);
    assert_eq!(
        store.request_by_id(&accepted.id).unwrap().unwrap().state,
        "queued"
    );
    assert_eq!(store.fail_queued_recovery_blocked("fault").unwrap(), 1);
}

#[test]
fn recovery_blocked_does_not_bootstrap_an_absent_queue() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    assert!(!store.request_db_path().exists());
    assert_eq!(store.fail_queued_recovery_blocked("fault").unwrap(), 0);
    assert!(!store.request_db_path().exists());
}
