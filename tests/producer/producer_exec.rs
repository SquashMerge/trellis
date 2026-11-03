//! Process control of `spawn_producer`: bounds, whole-group termination,
//! diagnostics and the exact environment.
#![cfg(feature = "test-support")]
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use trellis::producer::GroupStatus;
use trellis::producer::exec::{
    Ending, Hooks, LimitKind, Limits, ProcessOutcome, ProducerSpec, TAIL_BYTES, spawn_producer,
};

use crate::support;
use support::{stub_path, write_mode};

struct Run {
    dir: tempfile::TempDir,
}

impl Run {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("work")).unwrap();
        fs::create_dir(dir.path().join("logs")).unwrap();
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn spec(&self, mode: &Value) -> ProducerSpec {
        let mode_file = write_mode(self.dir.path(), "mode.json", mode);
        ProducerSpec {
            argv: vec![
                stub_path().into(),
                "--mode-file".into(),
                mode_file.into(),
                "index".into(),
                "--output".into(),
                self.path("out.scip").into(),
            ],
            cwd: self.path("work"),
            env: BTreeMap::new(),
            artifact: self.path("out.scip"),
            log_dir: Some(self.path("logs")),
        }
    }
}

fn fast() -> Limits {
    Limits {
        timeout: Duration::from_secs(120),
        grace: Duration::from_millis(200),
        sample: Duration::from_millis(50),
        ..Limits::default()
    }
}

fn run(spec: &ProducerSpec, limits: &Limits) -> ProcessOutcome {
    spawn_producer(spec, limits, &AtomicBool::new(false), &mut Hooks::default()).unwrap()
}

fn pids(pidfile: &Path) -> Vec<libc::pid_t> {
    fs::read_to_string(pidfile)
        .unwrap()
        .lines()
        .map(|line| line.parse().unwrap())
        .collect()
}

/// Kills the descendants a test started if an assertion fails first.
struct Reaper(PathBuf, bool);
impl Reaper {
    fn new(path: PathBuf) -> Self {
        Reaper(path, true)
    }

    /// Call once every recorded process is confirmed gone; the PIDs may be reused afterwards.
    fn disarm(&mut self) {
        self.1 = false;
    }
}
impl Drop for Reaper {
    fn drop(&mut self) {
        if !self.1 {
            return;
        }
        if let Ok(bytes) = fs::read(self.0.with_extension("identity.json"))
            && let Ok(identities) =
                serde_json::from_slice::<Vec<trellis::testing::ProcessIdentity>>(&bytes)
        {
            for identity in identities {
                let _ = identity.signal(libc::SIGKILL);
            }
        }
    }
}

#[test]
fn reaper_does_not_signal_a_reused_recorded_pid() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("pids");
    let mut child = std::process::Command::new("sleep")
        .arg("120")
        .spawn()
        .unwrap();
    let pid = child.id();
    let identity = trellis::testing::ProcessIdentity::capture(pid as libc::pid_t).unwrap();
    let mut stale = serde_json::to_value(identity).unwrap();
    stale["start_time"] = json!(stale["start_time"].as_u64().unwrap() + 1);
    fs::write(
        marker.with_extension("identity.json"),
        serde_json::to_vec(&vec![stale]).unwrap(),
    )
    .unwrap();
    drop(Reaper::new(marker));
    let alive = child.try_wait().unwrap().is_none();
    let _ = child.kill();
    let _ = child.wait();
    assert!(alive, "reaper signaled a different process identity");
}

fn wait_all_gone(pids: &[libc::pid_t]) {
    trellis::testing::wait_until("every process of the tree exited", || {
        pids.iter()
            .all(|pid| !trellis::testing::process_running(*pid))
    });
}

#[test]
fn timeout_terminates_group() {
    let run_dir = Run::new();
    let pidfile = run_dir.path("pids");
    let mut reaper = Reaper::new(pidfile.clone());
    let spec = run_dir.spec(&json!({"tree": pidfile, "sleep": true}));
    let limits = Limits {
        timeout: Duration::from_millis(1500),
        ..fast()
    };
    let outcome = run(&spec, &limits);
    assert_eq!(outcome.ending, Ending::TimedOut);
    assert_ne!(outcome.diagnostics.group_status, GroupStatus::Survived);
    assert!(outcome.diagnostics.exit.signal_name.is_some());
    wait_all_gone(&pids(&pidfile));
    reaper.disarm();
}

#[test]
fn memory_limit_terminates_group() {
    let run_dir = Run::new();
    let spec = run_dir.spec(&json!({"alloc": 256, "sleep": true}));
    let limits = Limits {
        memory_bytes: 64 * 1024 * 1024,
        ..fast()
    };
    let outcome = run(&spec, &limits);
    assert_eq!(outcome.ending, Ending::LimitExceeded(LimitKind::Memory));
    assert_eq!(LimitKind::Memory.reason(), "producer memory");
    assert!(outcome.diagnostics.peak_rss_bytes > 64 * 1024 * 1024);
    assert_eq!(outcome.diagnostics.memory_enforcement, "sampled");
    assert_ne!(outcome.diagnostics.group_status, GroupStatus::Survived);
    assert!(!trellis::testing::process_running(outcome.process.pgid));
}

#[test]
fn artifact_cap_terminates_group() {
    let run_dir = Run::new();
    let spec = run_dir.spec(&json!({"grow": 8192, "sleep": true}));
    let limits = Limits {
        artifact_bytes: 1024,
        ..fast()
    };
    let outcome = run(&spec, &limits);
    assert_eq!(
        outcome.ending,
        Ending::LimitExceeded(LimitKind::ArtifactBytes)
    );
    assert!(outcome.diagnostics.artifact_bytes > 1024);
    assert!(!trellis::testing::process_running(outcome.process.pgid));
}

#[test]
fn artifact_over_cap_after_exit_is_caught() {
    let run_dir = Run::new();
    let spec = run_dir.spec(&json!({"grow": 8192}));
    let limits = Limits {
        artifact_bytes: 1024,
        // No periodic sample sees the file before the process is gone.
        sample: Duration::from_secs(60),
        ..fast()
    };
    let outcome = run(&spec, &limits);
    assert_eq!(
        outcome.ending,
        Ending::LimitExceeded(LimitKind::ArtifactBytes)
    );
}

#[test]
fn log_streams_are_capped_and_tailed() {
    let run_dir = Run::new();
    let spec = run_dir.spec(&json!({"stderr": 5000}));
    let limits = Limits {
        log_bytes: 1000,
        ..fast()
    };
    let outcome = run(&spec, &limits);
    assert_eq!(outcome.ending, Ending::Exited);
    let diagnostics = &outcome.diagnostics;
    assert!(diagnostics.stderr_bytes > 50_000);
    assert!(diagnostics.truncated);
    assert_eq!(fs::read(run_dir.path("logs/stderr")).unwrap().len(), 1000);
    assert_eq!(fs::read(run_dir.path("logs/stdout")).unwrap().len(), 0);
    assert!(diagnostics.stderr_tail.len() <= TAIL_BYTES);
    assert!(diagnostics.stderr_tail.len() > TAIL_BYTES - 100);
    assert!(diagnostics.stderr_tail.ends_with("stub stderr line 4999\n"));
}

#[test]
fn crash_reports_signal_and_stderr_tail() {
    let run_dir = Run::new();
    let spec = run_dir.spec(&json!({"stderr": 3, "abort": true}));
    let outcome = run(&spec, &fast());
    assert_eq!(outcome.ending, Ending::Exited);
    let exit = &outcome.diagnostics.exit;
    assert_eq!(exit.code, None);
    assert_eq!(exit.signal, Some(libc::SIGABRT));
    assert_eq!(exit.signal_name.as_deref(), Some("SIGABRT"));
    assert_eq!(
        outcome.diagnostics.stderr_tail,
        "stub stderr line 0\nstub stderr line 1\nstub stderr line 2\n"
    );
    assert_ne!(outcome.diagnostics.group_status, GroupStatus::Survived);
}

#[test]
fn nonzero_exit_reports_code_and_tail() {
    let run_dir = Run::new();
    let spec = run_dir.spec(&json!({"stderr": 2, "exit": 3}));
    let outcome = run(&spec, &fast());
    assert_eq!(outcome.ending, Ending::Exited);
    assert_eq!(outcome.diagnostics.exit.code, Some(3));
    assert_eq!(outcome.diagnostics.exit.signal, None);
    assert!(outcome.diagnostics.stderr_tail.ends_with("line 1\n"));
    assert_eq!(outcome.diagnostics.stdout_bytes, 0);
}

#[test]
fn cancel_terminates_whole_tree() {
    let run_dir = Run::new();
    let pidfile = run_dir.path("pids");
    let mut reaper = Reaper::new(pidfile.clone());
    let spec = run_dir.spec(&json!({"tree": pidfile, "sleep": true}));
    let cancel = AtomicBool::new(false);
    let group = std::sync::Mutex::new(None);
    let outcome = std::thread::scope(|scope| {
        let canceller = scope.spawn(|| {
            trellis::testing::wait_until("descendants started", || pidfile.exists());
            // Spawned hook has run by then; cancel the live tree.
            cancel.store(true, Ordering::SeqCst);
        });
        let mut record = |process: &trellis::producer::ProcessIdentity| {
            *group.lock().unwrap() = Some(*process);
        };
        let mut hooks = Hooks {
            spawned: Some(&mut record),
            ..Hooks::default()
        };
        let outcome = spawn_producer(&spec, &fast(), &cancel, &mut hooks).unwrap();
        canceller.join().unwrap();
        outcome
    });
    assert_eq!(outcome.ending, Ending::Cancelled);
    assert_eq!(group.lock().unwrap().unwrap(), outcome.process);
    assert_ne!(outcome.process.leader_start_time, 0);
    let all = pids(&pidfile);
    assert_eq!(all.len(), 3);
    assert_eq!(all[0], outcome.process.pgid);
    wait_all_gone(&all);
    reaper.disarm();
}

#[test]
fn normal_exit_reaps_lingering_descendants() {
    let run_dir = Run::new();
    let pidfile = run_dir.path("pids");
    let mut reaper = Reaper::new(pidfile.clone());
    let spec = run_dir.spec(&json!({"tree": pidfile, "exit": 0}));
    let outcome = run(&spec, &fast());
    assert_eq!(outcome.ending, Ending::Exited);
    assert_eq!(outcome.diagnostics.exit.code, Some(0));
    assert_ne!(outcome.diagnostics.group_status, GroupStatus::Survived);
    let all = pids(&pidfile);
    assert_eq!(all.len(), 3);
    wait_all_gone(&all);
    reaper.disarm();
}

#[test]
fn env_is_exactly_declared_set() {
    let run_dir = Run::new();
    let dump = run_dir.path("env.json");
    let mut spec = run_dir.spec(&json!({"envdump": dump}));
    spec.env = BTreeMap::from([
        ("DECLARED".to_owned(), "value".to_owned()),
        ("HOME".to_owned(), "/declared/home".to_owned()),
    ]);
    // The parent has its own PATH, HOME and cargo variables; none may leak.
    assert!(std::env::var_os("PATH").is_some());
    assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some() || std::env::var_os("HOME").is_some());
    let outcome = run(&spec, &fast());
    assert_eq!(outcome.diagnostics.exit.code, Some(0));
    let report: Value = serde_json::from_slice(&fs::read(&dump).unwrap()).unwrap();
    let env: BTreeMap<String, String> = serde_json::from_value(report["env"].clone()).unwrap();
    assert_eq!(env, spec.env);
    assert_eq!(
        fs::canonicalize(report["cwd"].as_str().unwrap()).unwrap(),
        fs::canonicalize(&spec.cwd).unwrap()
    );
}

/// Whether `pid` is still this process's unreaped child (running or a zombie), observed
/// without reaping it. A reaped leader answers `ECHILD`.
fn held(pid: libc::pid_t) -> bool {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &raw mut info, flags) == 0 }
}

type Signals = Vec<(i32, bool)>;

/// Runs `spec`, recording every group signal with whether the leader was still held then.
fn run_recording_signals(
    spec: &ProducerSpec,
    limits: &Limits,
    on_spawn: &mut dyn FnMut(&trellis::producer::ProcessIdentity),
) -> (anyhow::Result<ProcessOutcome>, i32, Signals) {
    let pgid = std::sync::atomic::AtomicI32::new(0);
    let mut signals = Vec::new();
    let result = {
        let mut spawned = |process: &trellis::producer::ProcessIdentity| {
            pgid.store(process.pgid, Ordering::SeqCst);
            on_spawn(process);
        };
        let mut signal = |signal: i32| signals.push((signal, held(pgid.load(Ordering::SeqCst))));
        let mut hooks = Hooks {
            spawned: Some(&mut spawned),
            signal: Some(&mut signal),
        };
        spawn_producer(spec, limits, &AtomicBool::new(false), &mut hooks)
    };
    (result, pgid.load(Ordering::SeqCst), signals)
}

#[test]
fn leader_is_held_through_the_last_group_signal_then_reaped() {
    let timeout = Limits {
        timeout: Duration::from_millis(300),
        ..fast()
    };
    for (mode, limits, ending) in [
        (json!({}), fast(), Ending::Exited),
        (json!({"exit": 3}), fast(), Ending::Exited),
        (json!({"sleep": true}), timeout, Ending::TimedOut),
    ] {
        let run_dir = Run::new();
        let (result, pgid, signals) =
            run_recording_signals(&run_dir.spec(&mode), &limits, &mut |_| {});
        let outcome = result.unwrap();
        assert_eq!(outcome.ending, ending, "{mode}");
        assert_eq!(signals.last().map(|s| s.0), Some(libc::SIGKILL), "{mode}");
        assert!(signals.iter().all(|(_, held)| *held), "{mode}: {signals:?}");
        assert!(
            !held(pgid),
            "{mode}: the leader is reaped after the last signal"
        );
        let exit = &outcome.diagnostics.exit;
        assert!(exit.code.is_some() || exit.signal.is_some(), "{mode}");
    }
}

#[test]
fn exited_leader_alone_is_proven_empty_on_every_platform() {
    // Linux accepts the final SIGKILL to the zombie-only group. macOS refuses it with EPERM,
    // and the signal-0 probe after the reap (ESRCH) proves the group empty instead (#624).
    let run_dir = Run::new();
    let outcome = run(&run_dir.spec(&json!({})), &fast());
    assert_eq!(outcome.ending, Ending::Exited);
    assert_eq!(outcome.diagnostics.exit.code, Some(0));
    assert_eq!(outcome.diagnostics.group_status, GroupStatus::Empty);
}

#[test]
fn a_term_ignoring_member_is_killed_and_the_group_proven_empty() {
    // The leader exits at once and leaves a member that ignores SIGTERM, so the final SIGKILL
    // reaches a live member, which every platform accepts.
    let run_dir = Run::new();
    let pidfile = run_dir.path("member.pid");
    let mut reaper = Reaper::new(pidfile.clone());
    let script = format!(
        "trap '' TERM; sleep 60 & echo $! > '{}'; exit 0",
        pidfile.display()
    );
    let spec = ProducerSpec {
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        ..run_dir.spec(&json!({}))
    };
    let outcome = run(&spec, &fast());
    assert_eq!(outcome.ending, Ending::Exited);
    assert_eq!(outcome.diagnostics.exit.code, Some(0));
    assert_eq!(outcome.diagnostics.group_status, GroupStatus::Empty);
    wait_all_gone(&pids(&pidfile));
    reaper.disarm();
}

#[test]
fn a_lost_wait_status_signals_nothing() {
    // Another reaper takes the leader's status: its PID, and so the group ID, may then be
    // reused, so the run fails without signalling the group.
    let run_dir = Run::new();
    let (result, pgid, signals) = run_recording_signals(
        &run_dir.spec(&json!({"sleep": true})),
        &fast(),
        &mut |process| {
            let mut status = 0;
            unsafe {
                libc::kill(process.pgid, libc::SIGKILL);
                assert_eq!(libc::waitpid(process.pgid, &mut status, 0), process.pgid);
            }
        },
    );
    let error = result.unwrap_err();
    assert!(error.to_string().contains("wait for producer"), "{error:#}");
    assert!(signals.is_empty(), "{signals:?}");
    assert!(!held(pgid));
}

#[test]
fn an_unwind_stops_the_held_group_and_reaps_the_leader() {
    let run_dir = Run::new();
    let pidfile = run_dir.path("pids");
    let mut reaper = Reaper::new(pidfile.clone());
    let spec = run_dir.spec(&json!({"tree": pidfile, "sleep": true}));
    let pgid = std::sync::atomic::AtomicI32::new(0);
    let signals = std::sync::Mutex::new(Vec::new());
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut spawned = |process: &trellis::producer::ProcessIdentity| {
            pgid.store(process.pgid, Ordering::SeqCst);
            trellis::testing::wait_until("descendants started", || {
                fs::read_to_string(&pidfile).is_ok_and(|text| text.lines().count() == 3)
            });
            panic!("hook failure");
        };
        let mut signal = |signal: i32| {
            let held = held(pgid.load(Ordering::SeqCst));
            signals.lock().unwrap().push((signal, held));
        };
        let mut hooks = Hooks {
            spawned: Some(&mut spawned),
            signal: Some(&mut signal),
        };
        spawn_producer(&spec, &fast(), &AtomicBool::new(false), &mut hooks)
    }));
    assert!(unwound.is_err(), "the hook panicked");
    assert_eq!(*signals.lock().unwrap(), [(libc::SIGKILL, true)]);
    assert!(
        !held(pgid.load(Ordering::SeqCst)),
        "the leader is reaped once its group is stopped"
    );
    wait_all_gone(&pids(&pidfile));
    reaper.disarm();
}
