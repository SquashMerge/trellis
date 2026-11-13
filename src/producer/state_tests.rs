use super::process_info;
use super::state::{
    DaemonIdentity, PickupConfig, PickupEntry, RETAINED_RUNS, RunOrigin, RunRecord, RunState,
    StateDir,
};
use super::{GroupStatus, ProcessIdentity, TrustProfile};
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
        group_status: None,
        failure: None,
        origin: RunOrigin::Manual,
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

/// Owner ruling 2026-11-02 (#624): recovery has only a recorded PID and start time, which
/// cannot atomically prove the group is still the run's, so it never signals it. Even a
/// leader that still has the recorded start time is left running; termination is
/// unestablished and membership unknown.
#[test]
fn sweep_never_signals_a_recorded_group_and_reports_it_unknown() {
    let fixture = fixture();
    let state = state(&fixture);
    for (n, offset) in [(1, 0), (2, 1)] {
        let mut child = orphan_group();
        abandoned_run(&state, n, &child, offset);
        let pid = child.id() as i32;
        let report = state
            .sweep(&DaemonIdentity::current(), &BTreeSet::new())
            .unwrap();
        assert_eq!(report.unestablished_groups, [pid]);
        assert_eq!(report.abandoned_runs, [run_id(n)]);
        // The leader is this test's child: a signalled leader would have exited (and a
        // zombie still counts as a group member, so a group probe could not tell).
        assert!(
            child.try_wait().unwrap().is_none(),
            "recovery never signals a recorded group"
        );
        let after = state.read_run_record(&run_id(n)).unwrap().unwrap();
        assert_eq!(after.state, RunState::Failed);
        assert_eq!(after.outcome.as_deref(), Some("abandoned"));
        assert_eq!(
            after.error_code.as_deref(),
            Some("abandoned: producer termination unestablished")
        );
        assert_eq!(after.group_status, Some(GroupStatus::Unknown));
        assert!(after.finished_at.is_some());
        let dir = state.run_dir(&run_id(n)).unwrap();
        assert!(!dir.join("worktree").exists());
        assert!(!dir.join("out").exists());
        kill_group(child);
    }
}

/// A recorded group ID with no process at all is proven gone, so the run is plainly
/// abandoned with an empty group; one whose producer phase had ended keeps what its owner
/// established.
#[test]
fn sweep_records_a_vanished_group_empty_and_keeps_an_established_status() {
    let fixture = fixture();
    let state = state(&fixture);
    // This case needs a group whose every member can be reaped by this test.
    // The shell fixture has a background child that can outlive its leader's wait.
    let child = Command::new("/bin/sleep")
        .arg("60")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    abandoned_run(&state, 1, &child, 0);
    kill_group(child);
    state.create_run_dir(&run_id(2)).unwrap();
    let mut importing = record(2, RunState::Importing, other_daemon());
    importing.process = Some(ProcessIdentity {
        pgid: i32::MAX,
        leader_start_time: 1,
    });
    importing.group_status = Some(GroupStatus::Unknown);
    state.write_run_record(&importing).unwrap();
    let report = state
        .sweep(&DaemonIdentity::current(), &BTreeSet::new())
        .unwrap();
    assert_eq!(report.unestablished_groups, [i32::MAX]);
    let vanished = state.read_run_record(&run_id(1)).unwrap().unwrap();
    assert_eq!(vanished.error_code.as_deref(), Some("abandoned"));
    assert_eq!(vanished.group_status, Some(GroupStatus::Empty));
    let established = state.read_run_record(&run_id(2)).unwrap().unwrap();
    assert_eq!(established.group_status, Some(GroupStatus::Unknown));
    assert_eq!(
        established.error_code.as_deref(),
        Some("abandoned: producer termination unestablished")
    );
}

#[test]
fn sweep_ignores_runs_this_daemon_owns() {
    let fixture = fixture();
    let state = state(&fixture);
    state.create_run_dir(&run_id(2)).unwrap();
    state
        .write_run_record(&record(2, RunState::Running, DaemonIdentity::current()))
        .unwrap();
    let report = state
        .sweep(&DaemonIdentity::current(), &BTreeSet::new())
        .unwrap();
    assert!(report.abandoned_runs.is_empty());
    assert_eq!(
        state.read_run_record(&run_id(2)).unwrap().unwrap().state,
        RunState::Running
    );
}

/// The inputs of a queued import, the frozen snapshot included, survive an abandoned run
/// whose group is unknown (owner ruling 2026-11-02).
#[test]
fn sweep_keeps_dirs_in_keep_set() {
    let fixture = fixture();
    let state = state(&fixture);
    let child = orphan_group();
    abandoned_run(&state, 1, &child, 1);
    populate(&state.run_dir(&run_id(1)).unwrap(), &["batch", "snapshot"]);
    let keep = BTreeSet::from([fs::canonicalize(state.run_dir(&run_id(1)).unwrap()).unwrap()]);
    state.sweep(&DaemonIdentity::current(), &keep).unwrap();
    let dir = state.run_dir(&run_id(1)).unwrap();
    assert!(dir.join("out/file").exists());
    assert!(dir.join("batch/file").exists());
    assert!(dir.join("snapshot/file").exists());
    assert!(!dir.join("worktree").exists(), "phase directories still go");
    assert_eq!(
        state.read_run_record(&run_id(1)).unwrap().unwrap().state,
        RunState::Failed
    );
    kill_group(child);

    // Once no request names the directory, the next sweep strips what was kept.
    let report = state
        .sweep(&DaemonIdentity::current(), &BTreeSet::new())
        .unwrap();
    assert!(report.abandoned_runs.is_empty());
    assert!(dir.join("run.json").exists());
    assert!(!dir.join("out").exists() && !dir.join("batch").exists());
    assert!(!dir.join("snapshot").exists());
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
    assert_eq!(
        state.read_run_record(&run_id(1)).unwrap().unwrap().state,
        RunState::Importing,
        "no terminal record before the directory is stripped"
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
    let names: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, ["run.json"]);
}

#[test]
fn origin_round_trips_and_a_record_without_it_reads_as_manual() {
    let fixture = fixture();
    let state = state(&fixture);
    state.create_run_dir(&run_id(1)).unwrap();
    let mut auto = record(1, RunState::Done, DaemonIdentity::current());
    auto.origin = RunOrigin::Auto;
    state.write_run_record(&auto).unwrap();
    let path = state.run_dir(&run_id(1)).unwrap().join("run.json");
    let json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(json["origin"], "auto");
    assert_eq!(state.read_run_record(&run_id(1)).unwrap(), Some(auto));

    // A record written before #660 has no `origin`, and still passes the closed parse.
    let mut old = json;
    old.as_object_mut().unwrap().remove("origin");
    fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
    let read = state.read_run_record(&run_id(1)).unwrap().unwrap();
    assert_eq!(read.origin, RunOrigin::Manual);
}

/// The sweep and run retention scan `runs/` and `pickups/`: an approval is neither.
#[test]
fn approvals_are_untouched_by_the_sweep_and_by_prune_runs() {
    let fixture = fixture();
    let state = state(&fixture);
    state
        .write_approval_bytes("rust-analyzer", b"approved")
        .unwrap();
    for n in 1..=(RETAINED_RUNS as u32 + 3) {
        state.create_run_dir(&run_id(n)).unwrap();
        state
            .write_run_record(&record(n, RunState::Done, DaemonIdentity::current()))
            .unwrap();
    }
    state.create_run_dir(&run_id(100)).unwrap();
    state
        .write_run_record(&record(100, RunState::Running, other_daemon()))
        .unwrap();
    state.prune_runs(&BTreeSet::new()).unwrap();
    state
        .sweep(&DaemonIdentity::current(), &BTreeSet::new())
        .unwrap();
    assert_eq!(
        state.read_approval_bytes("rust-analyzer").unwrap(),
        Some(b"approved".to_vec())
    );
}

#[test]
fn approval_bytes_are_private_bounded_and_named_safely() {
    let fixture = fixture();
    let state = state(&fixture);
    assert_eq!(state.read_approval_bytes("rust-analyzer").unwrap(), None);
    assert!(!state.path().join("approvals").exists());
    state.write_approval_bytes("rust-analyzer", b"{}").unwrap();
    let dir = state.path().join("approvals");
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join("rust-analyzer.json")), 0o600);
    assert_eq!(state.read_approval_bytes("scip-java").unwrap(), None);

    for name in ["", "../x", "Rust", "a/b", &"a".repeat(65)] {
        assert!(state.approval_path(name).is_err(), "{name:?}");
        assert!(state.write_approval_bytes(name, b"x").is_err(), "{name:?}");
        assert!(state.read_approval_bytes(name).is_err(), "{name:?}");
    }

    fs::write(dir.join("rust-analyzer.json"), vec![b'x'; 64 * 1024]).unwrap();
    assert!(
        state
            .read_approval_bytes("rust-analyzer")
            .unwrap()
            .is_some()
    );
    fs::write(dir.join("rust-analyzer.json"), vec![b'x'; 64 * 1024 + 1]).unwrap();
    assert!(state.read_approval_bytes("rust-analyzer").is_err());

    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(state.read_approval_bytes("rust-analyzer").is_err());
    assert!(state.write_approval_bytes("rust-analyzer", b"x").is_err());
}
