use super::TrustProfile;
use super::run::{CancelRequest, RunRequest, sweep_state};
use super::state::{DaemonIdentity, RunRecord, RunState, StateDir};
use crate::semantic_batch::descriptor::GenerationOrigin;
use crate::store::{Store, requests::GeneratedImportOptions};
use serde_json::json;
use std::{fs, path::Path};

fn run_id(n: u32) -> String {
    format!("{n:032x}")
}

fn abandoned_record(id: &str) -> RunRecord {
    RunRecord {
        version: 1,
        run_id: id.to_owned(),
        producer: "scip-typescript".into(),
        trust_profile: TrustProfile::Restricted,
        plan_sha256: "0".repeat(64),
        source_revision: "revision".into(),
        state: RunState::Importing,
        outcome: None,
        error_code: None,
        daemon: DaemonIdentity {
            pid: 1,
            start_time: 0,
        },
        process: None,
        request_id: None,
        created_at: 1,
        finished_at: None,
        diagnostics: None,
    }
}

fn populate(dir: &Path, names: &[&str]) {
    for name in names {
        fs::create_dir_all(dir.join(name)).unwrap();
        fs::write(dir.join(name).join("file"), b"x").unwrap();
    }
}

/// The directories named by queued generated imports are the sweep's keep set: a run another
/// daemon left mid-import keeps its batch for the queued request, and a pickup directory goes
/// unless a request names it.
#[test]
fn startup_sweep_keeps_directories_named_by_queued_generated_imports() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("work");
    fs::create_dir(&workspace).unwrap();
    let workspace = fs::canonicalize(workspace).unwrap();
    let store = Store::open_for_tests(&temp.path().join("state"), &workspace).unwrap();
    let (roots, identity) = store.state_topology();
    let state = StateDir::open(roots, identity).unwrap();

    let (queued_run, other_run) = (run_id(1), run_id(2));
    let (queued_pickup, other_pickup) = (run_id(3), run_id(4));
    for id in [&queued_run, &other_run] {
        let dir = state.create_run_dir(id).unwrap();
        state.write_run_record(&abandoned_record(id)).unwrap();
        populate(&dir, &["worktree", "out", "batch"]);
    }
    for id in [&queued_pickup, &other_pickup] {
        let dir = state.create_pickup_dir(id).unwrap();
        populate(&dir, &["out", "batch"]);
    }
    for (origin, id) in [
        (GenerationOrigin::TrellisRun, &queued_run),
        (GenerationOrigin::OwnerPickup, &queued_pickup),
    ] {
        let dir = store.producer_generation_dir(origin, id).unwrap();
        fs::write(dir.join("batch/descriptor.json"), b"{}").unwrap();
        store
            .enqueue_generated_semantic_import(
                &workspace,
                &GeneratedImportOptions {
                    version: 2,
                    descriptor: dir.join("batch/descriptor.json").to_str().unwrap().into(),
                    descriptor_sha256: "0".repeat(64),
                    origin,
                    generation_id: id.clone(),
                },
            )
            .unwrap();
    }
    assert_eq!(store.pending_generation_ids().unwrap().len(), 2);
    assert_eq!(store.pending_generation_dirs().unwrap().len(), 2);

    let report = sweep_state(&store, false).expect("sweep ran");
    assert_eq!(report.abandoned_runs.len(), 2);
    assert_eq!(report.removed_pickups.len(), 1);
    assert_eq!(report.removed_pickups[0], other_pickup);

    let queued = state.run_dir(&queued_run).unwrap();
    assert!(queued.join("out").exists() && queued.join("batch").exists());
    assert!(!queued.join("worktree").exists());
    let other = state.run_dir(&other_run).unwrap();
    assert!(other.join("run.json").exists());
    assert!(!other.join("out").exists() && !other.join("batch").exists());
    assert!(state.pickup_dir(&queued_pickup).unwrap().exists());
    assert!(!state.pickup_dir(&other_pickup).unwrap().exists());
    for id in [&queued_run, &other_run] {
        let record = state.read_run_record(id).unwrap().unwrap();
        assert_eq!(record.outcome.as_deref(), Some("abandoned"));
    }
}

/// A sweep taken while this daemon may be importing a pickup leaves pickup directories alone.
#[test]
fn run_time_sweep_keeps_pickup_directories() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("work");
    fs::create_dir(&workspace).unwrap();
    let workspace = fs::canonicalize(workspace).unwrap();
    let store = Store::open_for_tests(&temp.path().join("state"), &workspace).unwrap();
    let (roots, identity) = store.state_topology();
    let state = StateDir::open(roots, identity).unwrap();
    let dir = state.create_pickup_dir(&run_id(7)).unwrap();
    populate(&dir, &["out"]);
    sweep_state(&store, true).expect("sweep ran");
    assert!(dir.exists());
    sweep_state(&store, false).expect("sweep ran");
    assert!(!dir.exists());
}

#[test]
fn run_request_requires_a_lowercase_plan_digest() {
    let payload = |hash: serde_json::Value| {
        json!({
            "version": 1,
            "producer": "scip-typescript",
            "trustProfile": "restricted",
            "limits": {"timeoutSecs": 10, "memoryMiB": 128},
            "executable": null,
            "environment": {"PATH": "/usr/bin", "HOME": "/tmp"},
            "planSha256": hash,
        })
    };
    assert!(RunRequest::parse(&payload(json!("a".repeat(64)))).is_ok());
    for bad in [json!("A".repeat(64)), json!("a".repeat(63)), json!(7)] {
        let error = RunRequest::parse(&payload(bad)).unwrap_err().to_string();
        assert!(error.starts_with("invalid_request"), "{error}");
    }
    let mut missing = payload(json!("a".repeat(64)));
    missing.as_object_mut().unwrap().remove("planSha256");
    assert!(RunRequest::parse(&missing).is_err());
    assert!(CancelRequest::parse(&json!({"version": 1, "runId": run_id(1)})).is_ok());
    assert!(CancelRequest::parse(&json!({"version": 1, "runId": "x", "extra": 1})).is_err());
}
