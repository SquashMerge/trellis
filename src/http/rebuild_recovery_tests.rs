use super::*;
use crate::store::topology::{TopologyRoots, WorkspaceIdentity};

#[test]
fn next_worker_step_rebuilds_corruption_without_a_request_and_retains_the_writer() {
    repair_corrupt_metadata("stats");
}

#[test]
fn active_recovery_uses_the_watchers_actual_nondefault_options_when_saved_options_are_corrupt() {
    repair_corrupt_metadata("reconcile_options");
}

fn repair_corrupt_metadata(column: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let state_root = tmp.path().join("state");
    let roots =
        TopologyRoots::isolated_for_tests(state_root.join("cache"), state_root.join("data"));
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let store = Store::open_for_tests(&state_root, &root).unwrap();
    let mut options = IndexOptions::new(root.clone());
    options.max_file_bytes = 8192;
    let (before, session) = crate::index_coordinator::reconcile_workspace(
        &store,
        &options,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let state = new(
        store.clone(),
        IndexOptions::new(root),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    state.retain_serving_session_without_tick_for_tests(session.clone());
    let watcher = crate::index_coordinator::WatcherWork::publishing(
        &store,
        &session,
        &options,
        state.watcher_health.clone(),
    )
    .unwrap();
    *state.watcher_work.lock().unwrap() = Some((Arc::downgrade(&session), watcher));
    assert_ne!(
        state.config().options.max_file_bytes,
        options.max_file_bytes
    );
    assert!(store.earliest_unfinished_request().unwrap().is_none());
    // The retained writer holds use locks, not a SQLite connection.
    let db = rusqlite::Connection::open(roots.index_db(&identity)).unwrap();
    db.execute(
        &format!("UPDATE index_metadata SET {column}='not-json' WHERE singleton=1"),
        [],
    )
    .unwrap();
    drop(db);
    assert!(store.status().is_err());
    assert!(!store.is_ready_disposition());
    state.worker_step().unwrap();
    let after = store.status().unwrap().revision;
    assert_ne!(after.index_generation, before.index_generation);
    assert!(store.is_ready_disposition());
    assert_eq!(
        store
            .recorded_index_options()
            .unwrap()
            .unwrap()
            .max_file_bytes,
        8192
    );
    assert!(Arc::ptr_eq(
        &session,
        &state.retained_serving_session().unwrap()
    ));
    assert!(store.source_at("a.js", Some(after)).unwrap().is_some());
    assert!(store.earliest_unfinished_request().unwrap().is_none());
}

#[test]
fn cold_recovery_uses_fifo_options_or_existing_defaults_when_recorded_options_are_corrupt() {
    for (queued, explicit, missing_metadata) in [
        (false, false, false),
        (true, false, false),
        (false, true, false),
        (false, false, true),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.js"), "function a() {}\n").unwrap();
        let state_root = tmp.path().join("state");
        let roots =
            TopologyRoots::isolated_for_tests(state_root.join("cache"), state_root.join("data"));
        let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
        let store = Store::open_for_tests(&state_root, &root).unwrap();
        let defaults = IndexOptions::new(root);
        let (_, session) = crate::index_coordinator::reconcile_workspace(
            &store,
            &defaults,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let mut requested = defaults.clone();
        requested.max_file_bytes = 8192;
        if queued {
            store.enqueue_request(&requested, None).unwrap();
        }
        drop(session);
        drop(store);
        let db = rusqlite::Connection::open(roots.index_db(&identity)).unwrap();
        let damage = if missing_metadata {
            "DELETE FROM index_metadata"
        } else {
            "UPDATE index_metadata SET reconcile_options='not-json' WHERE singleton=1"
        };
        db.execute(damage, []).unwrap();
        drop(db);
        let store = Store::open_for_tests(&state_root, &defaults.workspace_root).unwrap();
        let state = new(
            store.clone(),
            defaults.clone(),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        if explicit {
            state.apply_config(
                BrowserConfig::build(
                    requested.clone(),
                    true,
                    &defaults.workspace_root,
                    Vec::new(),
                    None,
                    None,
                    None,
                )
                .unwrap(),
            );
        }
        state.begin_startup_reconciliation();
        state.worker_step().unwrap();
        assert!(store.is_ready_disposition());
        assert_eq!(
            store
                .recorded_index_options()
                .unwrap()
                .unwrap()
                .max_file_bytes,
            if queued || explicit {
                requested.max_file_bytes
            } else {
                defaults.max_file_bytes
            }
        );
        assert!(store.source_at("a.js", None).unwrap().is_some());
    }
}
