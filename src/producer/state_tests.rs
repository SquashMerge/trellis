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
    state.prune_runs().unwrap();
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
    state.prune_runs().unwrap();
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

/// A shell that leads its own group and sleeps, standing in for a producer
/// orphaned by a crashed daemon.
fn orphan_group() -> Child {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "sleep 60 & wait"])
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
    command.spawn().unwrap()
}

fn abandoned_run(state: &StateDir, n: u32, child: &Child, start_offset: u64) {
    state.create_run_dir(&run_id(n)).unwrap();
    let mut running = record(n, RunState::Running, other_daemon());
    let pgid = child.id() as i32;
    running.process = Some(ProcessIdentity {
        pgid,
        leader_start_time: process_info::start_time(pgid).unwrap() + start_offset,
    });
    state.write_run_record(&running).unwrap();
    populate(&state.run_dir(&run_id(n)).unwrap(), &["worktree", "out"]);
}

fn kill_group(mut child: Child) {
    unsafe { libc::killpg(child.id() as i32, libc::SIGKILL) };
    let _ = child.wait();
}

#[test]
fn sweep_kills_abandoned_group_with_matching_identity() {
    let fixture = fixture();
    let state = state(&fixture);
    let mut child = orphan_group();
    abandoned_run(&state, 1, &child, 0);
    let pid = child.id() as i32;
    // The real orphan's parent is gone and init reaps it; reap here so the
    // killed leader does not linger as a zombie group member.
    let reaper = std::thread::spawn(move || child.wait().unwrap());
    let report = state
        .sweep(&DaemonIdentity::current(), &BTreeSet::new())
        .unwrap();
    assert_eq!(report.killed_groups, [pid]);
    // The sweep's SIGKILL ended the leader.
    let status = reaper.join().unwrap();
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(libc::SIGKILL)
    );
    let after = state.read_run_record(&run_id(1)).unwrap().unwrap();
    assert_eq!(after.state, RunState::Failed);
    assert_eq!(after.outcome.as_deref(), Some("abandoned"));
    assert!(after.finished_at.is_some());
    let dir = state.run_dir(&run_id(1)).unwrap();
    assert!(!dir.join("worktree").exists());
    assert!(!dir.join("out").exists());
}

#[test]
fn sweep_ignores_mismatched_start_time() {
    let fixture = fixture();
    let state = state(&fixture);
    let child = orphan_group();
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
        process_info::group_exists(child.id() as i32),
        "a process whose start time differs is never signalled"
    );
    assert_eq!(
        state.read_run_record(&run_id(2)).unwrap().unwrap().state,
        RunState::Running
    );
    kill_group(child);
}

#[test]
fn sweep_keeps_dirs_in_keep_set() {
    let fixture = fixture();
    let state = state(&fixture);
    let child = orphan_group();
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
    kill_group(child);
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
