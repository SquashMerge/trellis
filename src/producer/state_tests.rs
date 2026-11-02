use super::process_info;
use super::state::{
    DaemonIdentity, PickupConfig, PickupEntry, RETAINED_RUNS, RunRecord, RunState, StateDir,
};
use super::{ProcessIdentity, TrustProfile};
use crate::store::topology::{TopologyRoots, WorkspaceIdentity};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    roots: TopologyRoots,
    identity: WorkspaceIdentity,
    workspace: PathBuf,
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("work");
    fs::create_dir(&workspace).unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let identity = WorkspaceIdentity::discover(Some(&workspace), &workspace).unwrap();
    Fixture {
        _temp: temp,
        roots,
        identity,
        workspace,
    }
}

fn state(fixture: &Fixture) -> StateDir {
    StateDir::open(&fixture.roots, &fixture.identity).unwrap()
}

fn run_id(n: u32) -> String {
    format!("{n:032x}")
}

fn other_daemon() -> DaemonIdentity {
    DaemonIdentity {
        pid: 1,
        start_time: 0,
    }
}

fn record(n: u32, state: RunState, daemon: DaemonIdentity) -> RunRecord {
    RunRecord {
        version: 1,
        run_id: run_id(n),
        producer: "scip-typescript".into(),
        trust_profile: TrustProfile::Restricted,
        plan_sha256: "0".repeat(64),
        source_revision: "revision".into(),
        state,
        outcome: None,
        error_code: None,
        daemon,
        process: None,
        request_id: None,
        created_at: u64::from(n),
        finished_at: None,
        diagnostics: None,
    }
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

fn populate(dir: &Path, names: &[&str]) {
    for name in names {
        fs::create_dir_all(dir.join(name)).unwrap();
        fs::write(dir.join(name).join("file"), b"x").unwrap();
    }
}

#[test]
fn run_dir_is_private_and_outside_workspace() {
    let fixture = fixture();
    let state = state(&fixture);
    let dir = state.create_run_dir(&run_id(1)).unwrap();
    assert!(!dir.starts_with(&fixture.workspace));
    assert_eq!(
        dir,
        fixture
            .roots
            .producer_state_dir(&fixture.identity)
            .join("runs")
            .join(run_id(1))
    );
    for level in [
        fixture.roots.cache.join("producers"),
        state.path().to_owned(),
        state.path().join("runs"),
        dir,
    ] {
        assert_eq!(mode(&level), 0o700, "{}", level.display());
    }
    assert!(state.run_dir("../escape").is_err());
    assert!(state.run_dir("ABCDEF").is_err());
    assert!(state.pickup_dir(&"G".repeat(32)).is_err());
}

#[test]
fn rejects_symlinked_or_foreign_state_dir() {
    let fixture = fixture();
    let state = state(&fixture);
    state.create_run_dir(&run_id(1)).unwrap();
    let path = state.path().to_owned();

    // Another user's or group-accessible state is not private.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
    let error = StateDir::open(&fixture.roots, &fixture.identity).unwrap_err();
    assert!(
        error.to_string().contains("unsafe producer state"),
        "{error}"
    );
    assert!(state.create_run_dir(&run_id(2)).is_err());
    assert!(StateDir::existing(&fixture.roots, &fixture.identity).is_err());
    assert_eq!(
        mode(&path),
        0o750,
        "unsafe state is refused, never repaired"
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();

    // A symlink in place of the state directory, or of a level below it.
    let elsewhere = fixture.workspace.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_dir_all(&path).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
    let error = StateDir::open(&fixture.roots, &fixture.identity).unwrap_err();
    assert!(
        error.to_string().contains("unsafe producer state"),
        "{error}"
    );
    fs::remove_file(&path).unwrap();

    let state = StateDir::open(&fixture.roots, &fixture.identity).unwrap();
    std::os::unix::fs::symlink(&elsewhere, state.path().join("runs")).unwrap();
    assert!(state.create_run_dir(&run_id(3)).is_err());
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
}

#[test]
fn run_record_round_trips_and_corrupt_record_is_typed() {
    let fixture = fixture();
    let state = state(&fixture);
    state.create_run_dir(&run_id(1)).unwrap();
    assert_eq!(state.read_run_record(&run_id(1)).unwrap(), None);
    let written = record(1, RunState::Running, DaemonIdentity::current());
    state.write_run_record(&written).unwrap();
    assert_eq!(state.read_run_record(&run_id(1)).unwrap(), Some(written));
    let path = state.run_dir(&run_id(1)).unwrap().join("run.json");
    assert_eq!(mode(&path), 0o600);
    fs::write(&path, br#"{"version":1,"unknown":true}"#).unwrap();
    let error = state.read_run_record(&run_id(1)).unwrap_err();
    assert!(error.to_string().starts_with("producer_state_corrupt"));
    assert!(state.runs().unwrap().is_empty());
}

#[test]
fn producer_phase_end_strips_work_dirs() {
    let fixture = fixture();
    let state = state(&fixture);
    let dir = state.create_run_dir(&run_id(1)).unwrap();
    state
        .write_run_record(&record(1, RunState::Running, DaemonIdentity::current()))
        .unwrap();
    populate(
        &dir,
        &[
            "worktree",
            "bin",
            "home",
            "tmp",
            "target",
            "producer-config",
            "logs",
            "out",
            "batch",
        ],
    );
    state.end_producer_phase(&run_id(1)).unwrap();
    for gone in [
        "worktree",
        "bin",
        "home",
        "tmp",
        "target",
        "producer-config",
        "logs",
    ] {
        assert!(!dir.join(gone).exists(), "{gone}");
    }
    assert!(dir.join("out/file").exists());
    assert!(dir.join("batch/file").exists());
    assert!(dir.join("run.json").exists());
    state.end_producer_phase(&run_id(1)).unwrap();
}

#[test]
fn terminal_run_strips_to_record() {
    let fixture = fixture();
    let state = state(&fixture);
    let dir = state.create_run_dir(&run_id(1)).unwrap();
    state
        .write_run_record(&record(1, RunState::Done, DaemonIdentity::current()))
        .unwrap();
    populate(&dir, &["worktree", "logs", "out", "batch"]);
    state.strip_to_record(&run_id(1)).unwrap();
    let left: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(left, ["run.json"]);
}

#[test]
fn retains_ten_newest_run_records() {
    let fixture = fixture();
    let state = state(&fixture);
    for n in 1..=(RETAINED_RUNS as u32 + 3) {
        state.create_run_dir(&run_id(n)).unwrap();
        state
            .write_run_record(&record(n, RunState::Done, DaemonIdentity::current()))
            .unwrap();
    }
    // A live run is never pruned, whatever its age.
    state.create_run_dir(&run_id(100)).unwrap();
    let mut live = record(100, RunState::Running, DaemonIdentity::current());
    live.created_at = 0;
    state.write_run_record(&live).unwrap();
    state.prune_runs(&BTreeSet::new()).unwrap();
    let kept: BTreeSet<String> = state
        .runs()
        .unwrap()
        .into_iter()
        .map(|r| r.run_id)
        .collect();
    let expected: BTreeSet<String> = (4..=13).chain([100]).map(run_id).collect();
    assert_eq!(kept, expected);
    for n in 1..=3 {
        assert!(!state.run_dir(&run_id(n)).unwrap().exists());
    }
}

#[test]
fn pickups_do_not_consume_run_retention() {
    let fixture = fixture();
    let state = state(&fixture);
    for n in 1..=RETAINED_RUNS as u32 {
        state.create_run_dir(&run_id(n)).unwrap();
        state
            .write_run_record(&record(n, RunState::Done, DaemonIdentity::current()))
            .unwrap();
    }
    for n in 1000..1020 {
        let dir = state.create_pickup_dir(&run_id(n)).unwrap();
        populate(&dir, &["out", "batch"]);
    }
    state.prune_runs(&BTreeSet::new()).unwrap();
    assert_eq!(state.runs().unwrap().len(), RETAINED_RUNS);
    state.remove_pickup_dir(&run_id(1000)).unwrap();
    assert!(!state.pickup_dir(&run_id(1000)).unwrap().exists());
    assert!(state.pickup_dir(&run_id(1001)).unwrap().exists());
}

#[test]
fn pickup_config_round_trips_and_clears() {
    let fixture = fixture();
    let state = state(&fixture);
    assert_eq!(state.read_pickup_config().unwrap(), None);
    let config = PickupConfig {
        version: 1,
        entries: vec![PickupEntry {
            producer: "scip-typescript".into(),
            artifact: "/tmp/index.scip".into(),
            last_imported: None,
        }],
    };
    state.write_pickup_config(&config).unwrap();
    assert_eq!(state.read_pickup_config().unwrap(), Some(config));
    state.clear_pickup_config().unwrap();
    assert_eq!(state.read_pickup_config().unwrap(), None);
    fs::write(state.path().join("pickup.json"), b"[]").unwrap();
    let error = state.read_pickup_config().unwrap_err();
    assert!(error.to_string().starts_with("producer_state_corrupt"));
}

/// A single owned process, standing in for a producer orphaned by a daemon.
/// No other code waits for it: until `try_wait` reaps it, its PID cannot be reused.
struct OwnedProcess {
    child: Child,
    reaped: bool,
}

impl OwnedProcess {
    fn is_running(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) => {
                self.reaped = true;
                false
            }
            Err(error) => {
                // ECHILD means we no longer own a waitable PID; for other
                // errors the drop guard still cleans up the unreaped child.
                self.reaped = error.raw_os_error() == Some(libc::ECHILD);
                panic!("check owned child: {error}");
            }
        }
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if !self.reaped {
            // Child::kill targets only our unreaped child, including on panic.
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn orphan_process() -> OwnedProcess {
    let mut command = Command::new("/bin/sleep");
    command
        .arg("300")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    OwnedProcess {
        child: command.spawn().unwrap(),
        reaped: false,
    }
}

fn abandoned_run(state: &StateDir, n: u32, child: &OwnedProcess, start_offset: u64) {
    state.create_run_dir(&run_id(n)).unwrap();
    let mut running = record(n, RunState::Running, other_daemon());
    let pgid = child.child.id() as i32;
    running.process = Some(ProcessIdentity {
        pgid,
        leader_start_time: process_info::start_time(pgid).unwrap() + start_offset,
    });
    state.write_run_record(&running).unwrap();
    populate(&state.run_dir(&run_id(n)).unwrap(), &["worktree", "out"]);
}

#[test]
fn recovery_never_signals_a_matching_recorded_identity() {
    let fixture = fixture();
    let state = state(&fixture);
    let mut child = orphan_process();
    abandoned_run(&state, 1, &child, 0);
    let result = state.sweep(&DaemonIdentity::current(), &BTreeSet::new());
    let running = child.is_running();
    let report = result.unwrap();
    assert!(running, "recovery must not signal the owned group");
    assert!(report.killed_groups.is_empty());
    let after = state.read_run_record(&run_id(1)).unwrap().unwrap();
    assert_eq!(after.state, RunState::Failed);
    assert_eq!(after.outcome.as_deref(), Some("abandoned"));
    assert_eq!(
        after.error_code.as_deref(),
        Some("termination_unestablished")
    );
    assert!(after.finished_at.is_some());
    let dir = state.run_dir(&run_id(1)).unwrap();
    assert!(!dir.join("worktree").exists());
    assert!(!dir.join("out").exists());
    // The --run status response serializes this same record, while lastRun
    // copies this errorCode directly from it.
    let visible = serde_json::to_value(&after).unwrap();
    assert_eq!(visible["errorCode"], "termination_unestablished");
}

#[test]
fn recovery_reports_unknown_for_missing_and_unreadable_process_identities() {
    let fixture = fixture();
    let state = state(&fixture);
    for n in [1, 2] {
        state.create_run_dir(&run_id(n)).unwrap();
        let mut running = record(n, RunState::Running, other_daemon());
        if n == 2 {
            // This out-of-range PID has no readable process identity.
            running.process = Some(ProcessIdentity {
                pgid: i32::MAX,
                leader_start_time: 1,
            });
            assert_eq!(process_info::start_time(i32::MAX), None);
        }
        state.write_run_record(&running).unwrap();
    }
    let report = state
        .sweep(&DaemonIdentity::current(), &BTreeSet::new())
        .unwrap();
    assert!(report.killed_groups.is_empty());
    for n in [1, 2] {
        let after = state.read_run_record(&run_id(n)).unwrap().unwrap();
        assert_eq!(after.state, RunState::Failed);
        assert_eq!(after.outcome.as_deref(), Some("abandoned"));
        assert_eq!(
            after.error_code.as_deref(),
            Some("termination_unestablished")
        );
    }
}

#[test]
fn recovery_leaves_existing_abandoned_terminal_record_unchanged() {
    let fixture = fixture();
    let state = state(&fixture);
    let dir = state.create_run_dir(&run_id(1)).unwrap();
    let mut old = record(1, RunState::Failed, other_daemon());
    old.outcome = Some("abandoned".into());
    old.error_code = Some("abandoned".into());
    state.write_run_record(&old).unwrap();
    populate(&dir, &["out", "batch"]);
    state
        .sweep(&DaemonIdentity::current(), &BTreeSet::new())
        .unwrap();
    assert_eq!(state.read_run_record(&run_id(1)).unwrap(), Some(old));
    assert!(!dir.join("out").exists() && !dir.join("batch").exists());
}

#[test]
fn recovery_status_exposes_termination_unestablished() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("work");
    fs::create_dir(&workspace).unwrap();
    let workspace = fs::canonicalize(workspace).unwrap();
    fs::write(workspace.join("example.js"), "function example() {}\n").unwrap();
    let store =
        crate::store::Store::open_for_tests(&temp.path().join("state"), &workspace).unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (graph, native, capture) = crate::indexer::index_workspace_bundle(
        &crate::indexer::IndexOptions::new(workspace.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let writer = store.writer().unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            &writer,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    drop(writer);
    let (roots, identity) = store.state_topology();
    let state = StateDir::open(roots, identity).unwrap();
    let dir = state.create_run_dir(&run_id(1)).unwrap();
    state
        .write_run_record(&record(1, RunState::Running, other_daemon()))
        .unwrap();
    let request = super::plan::StatusRequest {
        version: 1,
        run_id: Some(run_id(1)),
        environment: super::detect::Environment {
            home: workspace.to_str().unwrap().to_owned(),
            ..Default::default()
        },
    };
    let stage = dir.join(format!(".run.json.tmp-{}", std::process::id()));
    fs::create_dir(&stage).unwrap();
    let error = super::run::sweep_state(&store, false).unwrap_err();
    assert!(
        error.to_string().starts_with("store_unavailable:"),
        "{error:#}"
    );
    let scheduler = crate::http::new(
        store.clone(),
        crate::indexer::IndexOptions::new(workspace.clone()),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:1".parse().unwrap(),
    )
    .unwrap();
    scheduler.force_root_transition_tick_for_tests().unwrap();
    assert!(store.index_baseline().is_ok());
    assert!(dir.join("run.json").exists());
    assert_eq!(
        state
            .read_run_record(&run_id(1))
            .unwrap()
            .unwrap()
            .error_code,
        None
    );
    let status = super::plan::status(&store, &request, None).unwrap();
    assert_eq!(status["run"]["recoveryTermination"], "unknown");
    assert_eq!(status["run"]["errorCode"], serde_json::Value::Null);
    let last = status["producers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "scip-typescript")
        .unwrap();
    assert_eq!(last["lastRun"]["recoveryTermination"], "unknown");
    assert_eq!(last["lastRun"]["errorCode"], serde_json::Value::Null);
    fs::remove_dir(stage).unwrap();
    if unsafe { libc::geteuid() } != 0 {
        let locked = dir.join("worktree/locked");
        populate(&locked, &["inner"]);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
        let result = state.sweep(&DaemonIdentity::current(), &BTreeSet::new());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
        let status = super::plan::status(&store, &request, None).unwrap();
        assert_eq!(status["run"]["state"], "running");
        assert_eq!(status["run"]["errorCode"], "termination_unestablished");
        let last_run = status["producers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|producer| producer["name"] == "scip-typescript")
            .unwrap();
        assert_eq!(last_run["lastRun"]["state"], "running");
        assert_eq!(
            last_run["lastRun"]["errorCode"],
            "termination_unestablished"
        );
    }
    state
        .sweep(&DaemonIdentity::current(), &BTreeSet::new())
        .unwrap();
    let status = super::plan::status(&store, &request, None).unwrap();
    assert_eq!(status["run"]["outcome"], "abandoned");
    assert_eq!(status["run"]["errorCode"], "termination_unestablished");
    let last_run = status["producers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|producer| producer["name"] == "scip-typescript")
        .unwrap();
    assert_eq!(last_run["lastRun"]["outcome"], "abandoned");
    assert_eq!(
        last_run["lastRun"]["errorCode"],
        "termination_unestablished"
    );
}

#[test]
fn sweep_ignores_mismatched_start_time() {
    let fixture = fixture();
    let state = state(&fixture);
    let mut child = orphan_process();
    abandoned_run(&state, 1, &child, 1);
    // A run this daemon owns is left alone as well.
    state.create_run_dir(&run_id(2)).unwrap();
    state
        .write_run_record(&record(2, RunState::Running, DaemonIdentity::current()))
        .unwrap();
    let report = state
        .sweep(&DaemonIdentity::current(), &BTreeSet::new())
        .unwrap();
    assert!(report.killed_groups.is_empty());
    assert_eq!(report.abandoned_runs, [run_id(1)]);
    assert!(
        child.is_running(),
        "a process whose start time differs is never signalled"
    );
    assert_eq!(
        state.read_run_record(&run_id(2)).unwrap().unwrap().state,
        RunState::Running
    );
    assert_eq!(
        state
            .read_run_record(&run_id(1))
            .unwrap()
            .unwrap()
            .error_code
            .as_deref(),
        Some("termination_unestablished")
    );
}

#[test]
fn sweep_keeps_dirs_in_keep_set() {
    let fixture = fixture();
    let state = state(&fixture);
    let child = orphan_process();
    abandoned_run(&state, 1, &child, 1);
    populate(&state.run_dir(&run_id(1)).unwrap(), &["batch"]);
    let keep = BTreeSet::from([fs::canonicalize(state.run_dir(&run_id(1)).unwrap()).unwrap()]);
    state.sweep(&DaemonIdentity::current(), &keep).unwrap();
    let dir = state.run_dir(&run_id(1)).unwrap();
    assert!(dir.join("out/file").exists());
    assert!(dir.join("batch/file").exists());
    assert!(!dir.join("worktree").exists(), "phase directories still go");
    assert_eq!(
        state.read_run_record(&run_id(1)).unwrap().unwrap().state,
        RunState::Failed
    );
    assert_eq!(
        state
            .read_run_record(&run_id(1))
            .unwrap()
            .unwrap()
            .error_code
            .as_deref(),
        Some("termination_unestablished")
    );
    drop(child);

    // Once no request names the directory, the next sweep strips what was kept.
    let report = state
        .sweep(&DaemonIdentity::current(), &BTreeSet::new())
        .unwrap();
    assert!(report.abandoned_runs.is_empty());
    assert!(dir.join("run.json").exists());
    assert!(!dir.join("out").exists() && !dir.join("batch").exists());
}

#[test]
fn sweep_removes_corrupt_records_but_keeps_unreadable_ones() {
    let fixture = fixture();
    let state = state(&fixture);
    for n in [1, 2] {
        state.create_run_dir(&run_id(n)).unwrap();
    }
    let corrupt = state.run_dir(&run_id(1)).unwrap();
    fs::write(corrupt.join("run.json"), br#"{"unknown":true}"#).unwrap();
    // A directory where the record belongs fails to read for a non-corrupt reason.
    let unreadable = state.run_dir(&run_id(2)).unwrap();
    fs::create_dir(unreadable.join("run.json")).unwrap();
    let report = state
        .sweep(&DaemonIdentity::current(), &BTreeSet::new())
        .unwrap();
    assert!(!corrupt.exists());
    assert!(unreadable.join("run.json").is_dir());
    assert_eq!(report.unreadable_runs, [run_id(2)]);
    assert!(report.abandoned_runs.is_empty());
}

#[test]
fn sweep_removes_unreferenced_pickup_dirs() {
    let fixture = fixture();
    let state = state(&fixture);
    for n in [1, 2] {
        populate(&state.create_pickup_dir(&run_id(n)).unwrap(), &["out"]);
    }
    let keep = BTreeSet::from([state.pickup_dir(&run_id(2)).unwrap()]);
    let report = state.sweep(&DaemonIdentity::current(), &keep).unwrap();
    assert_eq!(report.removed_pickups, [run_id(1)]);
    assert!(!state.pickup_dir(&run_id(1)).unwrap().exists());
    assert!(
        state
            .pickup_dir(&run_id(2))
            .unwrap()
            .join("out/file")
            .exists()
    );
}

/// Retention never deletes the inputs of a run whose generated import is still pending: an
/// old importing run whose wait was lost, ten newer terminal runs and a restart sweep. Once
/// the request settles, the next sweep reclaims it.
#[test]
fn retention_keeps_a_pending_run_until_its_request_settles() {
    let fixture = fixture();
    let state = state(&fixture);
    state.create_run_dir(&run_id(1)).unwrap();
    let mut importing = record(1, RunState::Importing, other_daemon());
    importing.request_id = Some("request".into());
    state.write_run_record(&importing).unwrap();
    let pending = state.run_dir(&run_id(1)).unwrap();
    populate(&pending, &["out", "batch"]);
    for n in 2..=(RETAINED_RUNS as u32 + 1) {
        state.create_run_dir(&run_id(n)).unwrap();
        let mut failed = record(n, RunState::Failed, DaemonIdentity::current());
        failed.outcome = Some("producer_failed".into());
        state.write_run_record(&failed).unwrap();
    }
    let keep = BTreeSet::from([fs::canonicalize(&pending).unwrap()]);

    // The restart sweep makes the old record terminal but keeps its inputs.
    let report = state.sweep(&DaemonIdentity::current(), &keep).unwrap();
    assert_eq!(report.abandoned_runs, [run_id(1)]);
    assert!(pending.join("out/file").exists() && pending.join("batch/file").exists());
    // So does the retention pass at another run's completion.
    state.prune_runs(&keep).unwrap();
    assert!(pending.join("out/file").exists() && pending.join("batch/file").exists());
    assert_eq!(state.runs().unwrap().len(), RETAINED_RUNS + 1);

    // Settled: no request names the directory, and the oldest record goes with its inputs.
    state
        .sweep(&DaemonIdentity::current(), &BTreeSet::new())
        .unwrap();
    assert!(!pending.exists());
    let kept: BTreeSet<String> = state
        .runs()
        .unwrap()
        .into_iter()
        .map(|r| r.run_id)
        .collect();
    let expected: BTreeSet<String> = (2..=(RETAINED_RUNS as u32 + 1)).map(run_id).collect();
    assert_eq!(kept, expected);
}

/// A terminal record means the sweep is done with the directory: the strip comes first, so a
/// strip that fails leaves the record non-terminal. A stripped directory under a non-terminal
/// record, as a crash between the two leaves it, is abandoned by the next sweep.
#[test]
fn sweep_publishes_the_abandoned_record_only_after_the_strip() {
    if unsafe { libc::geteuid() } == 0 {
        // Permission bits do not stop root from removing the locked directory.
        return;
    }
    let fixture = fixture();
    let state = state(&fixture);
    state.create_run_dir(&run_id(1)).unwrap();
    state
        .write_run_record(&record(1, RunState::Importing, other_daemon()))
        .unwrap();
    let dir = state.run_dir(&run_id(1)).unwrap();
    populate(&dir, &["out", "batch"]);
    let locked = dir.join("out").join("locked");
    populate(&locked, &["inner"]);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();

    let swept = state.sweep(&DaemonIdentity::current(), &BTreeSet::new());
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        swept.is_err(),
        "the strip must fail on the locked directory"
    );
    let error = swept.unwrap_err();
    assert!(
        error.to_string().starts_with("store_unavailable:"),
        "{error:#}"
    );
    assert!(error.downcast_ref::<std::io::Error>().is_some());
    let after_failure = state.read_run_record(&run_id(1)).unwrap().unwrap();
    assert_eq!(after_failure.state, RunState::Importing);
    assert_eq!(after_failure.outcome, None);
    assert_eq!(
        after_failure.error_code.as_deref(),
        Some("termination_unestablished"),
        "termination uncertainty must be visible even when stripping fails"
    );

    // The crash shape: stripped contents under a non-terminal record.
    fs::remove_dir_all(dir.join("out")).unwrap();
    fs::remove_dir_all(dir.join("batch")).unwrap();
    let report = state
        .sweep(&DaemonIdentity::current(), &BTreeSet::new())
        .unwrap();
    assert_eq!(report.abandoned_runs, [run_id(1)]);
    let after = state.read_run_record(&run_id(1)).unwrap().unwrap();
    assert_eq!(after.state, RunState::Failed);
    assert_eq!(after.outcome.as_deref(), Some("abandoned"));
    assert_eq!(
        after.error_code.as_deref(),
        Some("termination_unestablished")
    );
    let names: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, ["run.json"]);
}

#[test]
fn sweep_reports_typed_failure_if_unknown_cannot_be_published() {
    let fixture = fixture();
    let state = state(&fixture);
    let dir = state.create_run_dir(&run_id(1)).unwrap();
    let original = record(1, RunState::Running, other_daemon());
    state.write_run_record(&original).unwrap();
    populate(&dir, &["worktree", "out", "batch"]);
    // A directory at the atomic-write staging path deterministically makes
    // opening the staged record fail, even when tests run as root.
    let stage = dir.join(format!(".run.json.tmp-{}", std::process::id()));
    fs::create_dir(&stage).unwrap();
    let error = state
        .sweep(&DaemonIdentity::current(), &BTreeSet::new())
        .unwrap_err();
    assert!(
        error.to_string().starts_with("store_unavailable:"),
        "{error:#}"
    );
    assert!(error.downcast_ref::<std::io::Error>().is_some());
    assert_eq!(state.read_run_record(&run_id(1)).unwrap(), Some(original));
    assert!(dir.join("worktree/file").exists());
    assert!(dir.join("out/file").exists());
    assert!(dir.join("batch/file").exists());
    fs::remove_dir(stage).unwrap();
}

#[test]
fn unknown_status_without_keep_set_is_best_effort_for_each_run() {
    let fixture = fixture();
    let state = state(&fixture);
    for n in [1, 2] {
        let dir = state.create_run_dir(&run_id(n)).unwrap();
        state
            .write_run_record(&record(n, RunState::Running, other_daemon()))
            .unwrap();
        populate(&dir, &["worktree"]);
    }
    let first_dir = state.run_dir(&run_id(1)).unwrap();
    let stage = first_dir.join(format!(".run.json.tmp-{}", std::process::id()));
    fs::create_dir(&stage).unwrap();
    let error = state
        .note_recovery_unknown(&DaemonIdentity::current())
        .unwrap_err();
    assert!(error.to_string().starts_with("store_unavailable:"));
    let first = state.read_run_record(&run_id(1)).unwrap().unwrap();
    let second = state.read_run_record(&run_id(2)).unwrap().unwrap();
    assert_eq!(first.error_code, None);
    assert_eq!(
        second.error_code.as_deref(),
        Some("termination_unestablished")
    );
    assert_eq!(second.state, RunState::Running);
    assert!(first_dir.join("worktree/file").exists());
    assert!(
        state
            .run_dir(&run_id(2))
            .unwrap()
            .join("worktree/file")
            .exists()
    );
    fs::remove_dir(stage).unwrap();
}

#[test]
fn failed_strip_retries_without_changing_queued_import() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("work");
    fs::create_dir(&workspace).unwrap();
    let workspace = fs::canonicalize(workspace).unwrap();
    let store =
        crate::store::Store::open_for_tests(&temp.path().join("state"), &workspace).unwrap();
    let (roots, identity) = store.state_topology();
    let state = StateDir::open(roots, identity).unwrap();
    let id = run_id(1);
    let dir = state.create_run_dir(&id).unwrap();
    populate(&dir, &["worktree/locked/inner", "out", "batch"]);
    let descriptor = dir.join("batch/descriptor.json");
    fs::write(&descriptor, b"{}").unwrap();
    let descriptor = fs::canonicalize(descriptor).unwrap();
    let request = store
        .enqueue_generated_semantic_import(
            &workspace,
            &crate::store::requests::GeneratedImportOptions {
                version: 2,
                descriptor: descriptor.to_str().unwrap().into(),
                descriptor_sha256: "0".repeat(64),
                origin: crate::semantic_batch::descriptor::GenerationOrigin::TrellisRun,
                generation_id: id.clone(),
            },
        )
        .unwrap();
    let mut importing = record(1, RunState::Importing, other_daemon());
    importing.request_id = Some(request.id.clone());
    state.write_run_record(&importing).unwrap();
    let keep = store.pending_generation_dirs().unwrap();
    let locked = dir.join("worktree/locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
    let first = state.sweep(&DaemonIdentity::current(), &keep);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(first.is_err());
    let failed = state.read_run_record(&id).unwrap().unwrap();
    assert_eq!(failed.state, RunState::Importing);
    assert_eq!(
        failed.error_code.as_deref(),
        Some("termination_unestablished")
    );
    assert_eq!(failed.request_id.as_deref(), Some(request.id.as_str()));
    assert_eq!(
        store.request_by_id(&request.id).unwrap().unwrap().state,
        "queued"
    );
    assert_eq!(store.pending_generation_ids().unwrap().len(), 1);
    assert_eq!(fs::read(&descriptor).unwrap(), b"{}");
    assert_eq!(fs::read(dir.join("out/file")).unwrap(), b"x");

    let report = state.sweep(&DaemonIdentity::current(), &keep).unwrap();
    assert_eq!(report.abandoned_runs.as_slice(), std::slice::from_ref(&id));
    let after = state.read_run_record(&id).unwrap().unwrap();
    assert_eq!(after.state, RunState::Failed);
    assert_eq!(after.request_id.as_deref(), Some(request.id.as_str()));
    assert_eq!(
        after.error_code.as_deref(),
        Some("termination_unestablished")
    );
    assert_eq!(
        store.request_by_id(&request.id).unwrap().unwrap().state,
        "queued"
    );
    assert_eq!(store.pending_generation_ids().unwrap().len(), 1);
    assert_eq!(fs::read(&descriptor).unwrap(), b"{}");
    assert_eq!(fs::read(dir.join("out/file")).unwrap(), b"x");
    assert!(
        state
            .sweep(&DaemonIdentity::current(), &keep)
            .unwrap()
            .abandoned_runs
            .is_empty()
    );
    assert_eq!(store.pending_generation_ids().unwrap().len(), 1);
}

#[test]
fn unresolved_pending_generation_defers_cleanup_and_retry_preserves_inputs() {
    use crate::semantic_batch::descriptor::GenerationOrigin;
    use crate::store::requests::GeneratedImportOptions;

    for origin in [GenerationOrigin::TrellisRun, GenerationOrigin::OwnerPickup] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("work");
        fs::create_dir(&workspace).unwrap();
        let workspace = fs::canonicalize(workspace).unwrap();
        let store =
            crate::store::Store::open_for_tests(&temp.path().join("state"), &workspace).unwrap();
        let (roots, identity) = store.state_topology();
        let state = StateDir::open(roots, identity).unwrap();
        let id = run_id(1);
        let dir = match origin {
            GenerationOrigin::TrellisRun => state.create_run_dir(&id).unwrap(),
            GenerationOrigin::OwnerPickup => state.create_pickup_dir(&id).unwrap(),
        };
        populate(&dir, &["batch"]);
        let descriptor = dir.join("batch/descriptor.json");
        fs::write(&descriptor, b"{}").unwrap();
        let descriptor = fs::canonicalize(descriptor).unwrap();
        let request = store
            .enqueue_generated_semantic_import(
                &workspace,
                &GeneratedImportOptions {
                    version: 2,
                    descriptor: descriptor.to_str().unwrap().into(),
                    descriptor_sha256: "0".repeat(64),
                    origin,
                    generation_id: id.clone(),
                },
            )
            .unwrap();
        let other = state.create_run_dir(&run_id(2)).unwrap();
        state
            .write_run_record(&record(2, RunState::Running, other_daemon()))
            .unwrap();
        populate(&other, &["worktree"]);

        // Simulate an ordinary lookup race: the pending generation disappears
        // during resolution and reappears before the next recovery attempt.
        let held = dir.with_extension("held");
        fs::rename(&dir, &held).unwrap();
        let failed = store.pending_generation_dirs();
        let deferred = super::run::sweep_state(&store, false);
        fs::rename(&held, &dir).unwrap();
        let error =
            failed.expect_err("a missing pending generation must not yield a partial keep set");
        assert!(error.to_string().starts_with("store_unavailable:"));
        assert!(deferred.is_err());
        let observed = state.read_run_record(&run_id(2)).unwrap().unwrap();
        assert_eq!(observed.state, RunState::Running);
        assert_eq!(
            observed.error_code.as_deref(),
            Some("termination_unestablished")
        );
        assert!(other.join("worktree/file").exists());
        assert_eq!(fs::read(dir.join("batch/file")).unwrap(), b"x");
        assert_eq!(
            store.request_by_id(&request.id).unwrap().unwrap().state,
            "queued"
        );
        assert_eq!(store.pending_generation_ids().unwrap().len(), 1);

        let report = super::run::sweep_state(&store, false)
            .unwrap()
            .expect("retry succeeds");
        assert_eq!(report.abandoned_runs, [run_id(2)]);
        assert_eq!(
            state.read_run_record(&run_id(2)).unwrap().unwrap().state,
            RunState::Failed
        );
        assert_eq!(fs::read(dir.join("batch/file")).unwrap(), b"x");
        assert_eq!(
            store.request_by_id(&request.id).unwrap().unwrap().state,
            "queued"
        );
        assert_eq!(store.pending_generation_ids().unwrap().len(), 1);
    }
}

#[cfg(feature = "test-support")]
#[test]
fn unreadable_pending_semantic_options_defer_cleanup() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("work");
    fs::create_dir(&workspace).unwrap();
    let workspace = fs::canonicalize(workspace).unwrap();
    let store =
        crate::store::Store::open_for_tests(&temp.path().join("state"), &workspace).unwrap();
    let (roots, identity) = store.state_topology();
    let state = StateDir::open(roots, identity).unwrap();
    let dir = state.create_run_dir(&run_id(1)).unwrap();
    state
        .write_run_record(&record(1, RunState::Running, other_daemon()))
        .unwrap();
    populate(&dir, &["worktree"]);
    store
        .enqueue_semantic_options_json_for_tests(&workspace, &serde_json::json!({"version": 99}))
        .unwrap();
    let error = store.pending_generation_dirs().unwrap_err();
    assert!(error.to_string().starts_with("incompatible_queue:"));
    assert!(super::run::sweep_state(&store, false).is_err());
    let after = state.read_run_record(&run_id(1)).unwrap().unwrap();
    assert_eq!(after.state, RunState::Running);
    assert_eq!(
        after.error_code.as_deref(),
        Some("termination_unestablished")
    );
    assert!(dir.join("worktree/file").exists());
}
