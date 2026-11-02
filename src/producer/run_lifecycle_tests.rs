// Terminal-record reconciliation through the actual run owner and status path.

use super::tests::frozen_run::{Scenario, artifact, exercise};

#[test]
fn failed_unknown_terminal_write_is_visible_and_reconciled_by_owner() {
    exercise(
        artifact(&["a.js", "b.js"], false),
        "failed",
        false,
        false,
        false,
        Scenario::TerminalWriteFailure,
    );
}

#[test]
fn terminal_post_rename_handoff_and_unrelated_history_require_no_read_fsync() {
    exercise(
        artifact(&["a.js", "b.js"], false),
        "failed",
        false,
        false,
        false,
        Scenario::TerminalAfterRename,
    );
    exercise(
        artifact(&["a.js", "b.js"], false),
        "failed",
        false,
        false,
        false,
        Scenario::TerminalRenameLost,
    );
}

#[test]
fn accepted_same_daemon_import_survives_new_run_until_settlement() {
    exercise(
        artifact(&["a.js", "b.js"], false),
        "failed",
        false,
        false,
        false,
        Scenario::AcceptedWaiterExit,
    );
    exercise(
        artifact(&["a.js", "b.js"], false),
        "failed",
        false,
        false,
        false,
        Scenario::AcceptedWaiterFailure,
    );
}

#[test]
fn persistent_fault_releases_owner_on_root_loss_and_replacement_recovers() {
    exercise(
        artifact(&["a.js", "b.js"], false),
        "failed",
        false,
        false,
        false,
        Scenario::TerminalWriteShutdown,
    );
}

#[test]
fn owner_published_active_run_is_not_misclassified_as_abandoned() {
    exercise(
        artifact(&["a.js", "b.js"], false),
        "failed",
        false,
        false,
        false,
        Scenario::ActiveStatus,
    );
}

#[test]
fn pending_queue_contention_is_bounded_typed_and_preserves_cause() {
    use std::cell::Cell;
    let calls = Cell::new(0);
    let waits = Cell::new(0);
    let busy = || {
        rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY), None)
    };
    let value = super::pending_dirs_with_retry(
        || {
            calls.set(calls.get() + 1);
            if calls.get() < 3 {
                Err(busy().into())
            } else {
                Ok(42)
            }
        },
        || false,
        || waits.set(waits.get() + 1),
    )
    .unwrap();
    assert_eq!((value, calls.get(), waits.get()), (42, 3, 2));
    let error = super::pending_dirs_with_retry::<()>(
        || Err(busy().into()),
        || true,
        || panic!("expired contention cannot wait"),
    )
    .unwrap_err();
    assert!(error.to_string().starts_with("storage_busy:"));
    assert!(error.downcast_ref::<rusqlite::Error>().is_some());
    let error = super::pending_dirs_with_retry::<()>(
        || Err(anyhow::Error::new(busy()).context("root_changed: injected")),
        || false,
        || panic!("a root guard cannot be retried"),
    )
    .unwrap_err();
    assert!(error.to_string().starts_with("root_changed:"));
    assert!(error.downcast_ref::<rusqlite::Error>().is_some());
    let error = super::pending_dirs_with_retry::<()>(
        || Err(anyhow::anyhow!("storage_busy: invariant")),
        || false,
        || panic!("invariant errors cannot be retried"),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "storage_busy: invariant");
    let error = super::pending_dirs_with_retry::<()>(
        || Err(anyhow::Error::new(busy()).context("unclassified guard")),
        || false,
        || panic!("wrapped contention cannot be retried"),
    )
    .unwrap_err();
    assert!(error.to_string().starts_with("store_unavailable:"));
    assert!(error.downcast_ref::<rusqlite::Error>().is_some());
}

#[test]
fn sweep_boundary_types_queue_and_strip_faults_without_losing_pending_inputs() {
    use super::*;
    use crate::store::requests::GeneratedImportOptions;
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let workspace = fs::canonicalize(workspace).unwrap();
    let store = Store::open_for_tests(&temp.path().join("state"), &workspace).unwrap();
    let (roots, identity) = store.state_topology();
    let state = StateDir::open(roots, identity).unwrap();
    let id = "a".repeat(32);
    let dir = fs::canonicalize(state.create_run_dir(&id).unwrap()).unwrap();
    for name in ["worktree", "out", "batch"] {
        fs::create_dir(dir.join(name)).unwrap();
    }
    fs::write(dir.join("out/index.scip"), b"pending").unwrap();
    fs::write(dir.join("batch/descriptor.json"), b"{}").unwrap();
    let record = RunRecord {
        version: 1,
        run_id: id.clone(),
        producer: "scip-typescript".into(),
        trust_profile: super::super::TrustProfile::Restricted,
        plan_sha256: "0".repeat(64),
        source_revision: "revision".into(),
        state: RunState::Running,
        outcome: None,
        error_code: None,
        daemon: DaemonIdentity {
            pid: -1,
            start_time: 0,
        },
        process: None,
        request_id: None,
        created_at: 1,
        finished_at: None,
        diagnostics: None,
    };
    state.write_run_record(&record).unwrap();
    let row = store
        .enqueue_generated_semantic_import(
            &workspace,
            &GeneratedImportOptions {
                version: 2,
                descriptor: dir.join("batch/descriptor.json").to_str().unwrap().into(),
                descriptor_sha256: "0".repeat(64),
                origin: GenerationOrigin::TrellisRun,
                generation_id: id,
            },
        )
        .unwrap();
    for (fault, expected) in [("io", "store_unavailable:"), ("root", "root_changed:")] {
        let stage = dir.join(format!(".run.json.tmp-{}", std::process::id()));
        if fault == "root" {
            fs::create_dir(&stage).unwrap();
        }
        TEST_SWEEP_QUEUE_FAULT.with(|flag| flag.set(Some(fault)));
        let error = sweep_state(&store, false).unwrap_err();
        assert!(error.to_string().starts_with(expected), "{error:#}");
        assert!(error.downcast_ref::<rusqlite::Error>().is_some());
        if fault == "root" {
            fs::remove_dir(stage).unwrap();
        }
        assert_eq!(
            store.request_by_id(&row.id).unwrap().unwrap().state,
            "queued"
        );
        assert_eq!(fs::read(dir.join("out/index.scip")).unwrap(), b"pending");
        assert!(dir.join("batch/descriptor.json").exists());
    }
    crate::producer::state::fail_next_strip();
    let error = sweep_state(&store, false).unwrap_err();
    assert!(
        error.to_string().starts_with("store_unavailable:"),
        "{error:#}"
    );
    assert!(error.downcast_ref::<std::io::Error>().is_some());
    assert_eq!(
        store.request_by_id(&row.id).unwrap().unwrap().state,
        "queued"
    );
    assert_eq!(fs::read(dir.join("out/index.scip")).unwrap(), b"pending");
    assert!(dir.join("batch/descriptor.json").exists());
}
