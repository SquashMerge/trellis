//! `trellis producer run` end to end with the test stub as the producer: an explicit,
//! plan-hash-bound run generates in its private directory from the controlled worktree and
//! publishes through the generated import; its bounds, cancel, crash diagnostics, basis check,
//! retirement and cleanup are observable from the CLI, `run.json` and the run directory.
#![cfg(feature = "test-support")]
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Output, Stdio},
};
use tempfile::TempDir;

use crate::support;
use support::{
    Checkout, cache_dir, start_checkout, state_dir, stub_env, workspace_tree_digest, write_mode,
};

use crate::semantic_batch_support;
use semantic_batch_support::{BatchFixture, edit_scip, uniform_workspace};

const STUB: &str = "trellis-synthetic-scip";

fn workspace(root: &Path) {
    uniform_workspace(root, &["javascript"], 3);
}

struct World {
    checkout: Checkout,
    stub_dir: TempDir,
    marker: PathBuf,
}

impl World {
    fn new() -> Self {
        Self::with(&[])
    }

    /// `hooks` maps hook environment variables to socket file names inside the stub
    /// directory. The stub's mode starts empty and is set per test with `mode`. The daemon
    /// shortens the termination grace so a stopped group is gone quickly.
    fn with(hooks: &[(&str, &str)]) -> Self {
        Self::with_env(hooks, &[])
    }

    /// Like `with`, and `extra` is also set in the daemon's own environment, the process that
    /// launches the producer.
    fn with_env(hooks: &[(&str, &str)], extra: &[(&str, &str)]) -> Self {
        let stub_dir = tempfile::Builder::new()
            .prefix("producer-stub-")
            .tempdir_in("/tmp")
            .unwrap();
        let marker = stub_dir.path().join("stub.marker");
        let mut env = stub_env(stub_dir.path(), &json!({"marker": marker}));
        env.push((
            "TRELLIS_TEST_PRODUCER_LIMITS".to_owned(),
            json!({"graceMs": 100}).to_string(),
        ));
        for (variable, name) in hooks {
            env.push((
                (*variable).to_owned(),
                stub_dir.path().join(name).display().to_string(),
            ));
        }
        env.extend(
            extra
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
        );
        let checkout = start_checkout(workspace, &env);
        Self {
            checkout,
            stub_dir,
            marker,
        }
    }

    fn index_db(&self) -> PathBuf {
        use trellis::store::topology::{TopologyRoots, WorkspaceIdentity};
        let roots = TopologyRoots::isolated_for_tests(
            cache_dir(&self.checkout.home),
            support::data_dir(&self.checkout.home),
        );
        let identity =
            WorkspaceIdentity::discover(Some(&self.checkout.root), &self.checkout.root).unwrap();
        roots.index_db(&identity)
    }

    /// A valid synthetic artifact over the javascript documents, bound to the run worktree.
    fn artifact(&self, edit: impl FnOnce(&mut scip::types::Index)) -> PathBuf {
        let mut fixture =
            BatchFixture::from_published(&self.index_db(), &self.checkout.root).synthetic();
        let docs = fixture.paths("javascript");
        fixture.artifact("javascript", docs.clone());
        let bytes = edit_scip(&fixture.scip_bytes("javascript", &docs), |index| {
            index.metadata.as_mut().unwrap().project_root = String::new();
            edit(index);
        });
        let path = self.stub_dir.path().join("index.scip");
        fs::write(&path, bytes).unwrap();
        path
    }

    /// The stub's behavior: it records its argv, then does `extra`.
    fn mode(&self, extra: Value) {
        let mut mode = json!({"marker": self.marker});
        mode.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        write_mode(self.stub_dir.path(), "mode.json", &mode);
    }

    /// A stub that emits a valid artifact and exits.
    fn emitting(&self) {
        self.emitting_variant(0);
    }

    /// Like `emitting`, with bytes that differ per `variant`: the same artifact cannot be
    /// imported twice.
    fn emitting_variant(&self, variant: u32) {
        let artifact = self.artifact(|index| {
            index
                .metadata
                .as_mut()
                .unwrap()
                .tool_info
                .as_mut()
                .unwrap()
                .arguments
                .push(format!("variant-{variant}"));
        });
        self.mode(json!({"emit": artifact}));
    }

    fn hook(&self, name: &str) -> PathBuf {
        self.stub_dir.path().join(name)
    }

    fn producer(&self, verb: &str, extra: &[&str]) -> Output {
        self.command(verb, extra).output().unwrap()
    }

    fn command(&self, verb: &str, extra: &[&str]) -> std::process::Command {
        let mut command = self.checkout.cli();
        command
            .args(["producer", verb, "--workspace"])
            .arg(&self.checkout.root)
            .args(extra);
        command
    }

    fn plan(&self, extra: &[&str]) -> Value {
        let mut args = vec!["--producer", STUB];
        args.extend_from_slice(extra);
        let output = self.producer("plan", &args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// The `producer run` arguments for the plan of `extra`.
    fn run_args(&self, extra: &[&str]) -> Vec<String> {
        let plan = self.plan(extra);
        let mut args = vec!["--producer".to_owned(), STUB.to_owned()];
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        args.extend([
            "--plan-sha256".to_owned(),
            plan["planSha256"].as_str().unwrap().to_owned(),
        ]);
        args
    }

    fn spawn_run(&self, extra: &[&str]) -> Child {
        let args = self.run_args(extra);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.command("run", &args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn run(&self, extra: &[&str]) -> Output {
        self.spawn_run(extra).wait_with_output().unwrap()
    }

    fn status(&self) -> Value {
        let output = self.producer("status", &[]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn stub_entry(&self) -> Value {
        self.status()["producers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == STUB)
            .unwrap()
            .clone()
    }

    fn runs_dir(&self) -> PathBuf {
        state_dir(&self.checkout).join("runs")
    }

    fn run_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = fs::read_dir(self.runs_dir())
            .map(|dir| {
                dir.map(|entry| entry.unwrap().file_name().into_string().unwrap())
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        ids
    }

    fn record(&self, run_id: &str) -> Value {
        serde_json::from_slice(&fs::read(self.runs_dir().join(run_id).join("run.json")).unwrap())
            .unwrap()
    }

    /// Wait for a hook socket to be announced, hold the run there, and release it later.
    fn wait_hook(&self, name: &str) {
        let ready = self.hook(name).with_extension("ready");
        trellis::testing::wait_until("hook announced", || ready.exists());
    }

    fn release_hook(&self, name: &str) {
        UnixStream::connect(self.hook(name)).unwrap();
    }

    fn active_run(&self) -> Value {
        let mut active = Value::Null;
        trellis::testing::wait_until("active run", || {
            active = self.status()["activeRun"].clone();
            !active.is_null()
        });
        active
    }
}

fn reply(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "no JSON on stdout; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn marker_lines(world: &World) -> usize {
    fs::read_to_string(&world.marker).map_or(0, |text| text.lines().count())
}

fn only_record_remains(dir: &Path) -> bool {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names == ["run.json"]
}

#[test]
fn explicit_run_imports_stub_artifact() {
    let world = World::new();
    world.emitting();
    // Planning and status execute nothing.
    let args = world.run_args(&[]);
    world.status();
    assert_eq!(marker_lines(&world), 0);
    assert!(world.run_ids().is_empty());

    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = world.producer("run", &args);
    let reply = reply(&output);
    assert!(output.status.success(), "{reply} {}", stderr(&output));
    assert_eq!(reply["state"], "done");
    assert_eq!(reply["outcome"], "imported");
    assert_eq!(reply["errorCode"], Value::Null);
    assert_eq!(reply["producer"], STUB);
    assert_eq!(reply["import"]["requestId"].as_str().unwrap().len(), 36);
    assert!(reply["import"]["publishedRevision"].is_object());
    assert!(reply["import"]["report"].is_object());
    assert_eq!(reply["diagnostics"]["exit"]["code"], 0);

    assert_eq!(marker_lines(&world), 1, "the stub ran exactly once");
    assert_eq!(world.run_ids(), [reply["runId"].as_str().unwrap()]);
    let record = world.record(reply["runId"].as_str().unwrap());
    assert_eq!(record["state"], "done");
    assert_eq!(record["outcome"], "imported");
    assert_eq!(record["requestId"], reply["import"]["requestId"]);
}

#[test]
fn run_refuses_mismatched_plan_hash() {
    let world = World::new();
    world.emitting();
    let output = world.producer(
        "run",
        &["--producer", STUB, "--plan-sha256", &"0".repeat(64)],
    );
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("plan_changed"),
        "{}",
        stderr(&output)
    );
    assert_eq!(marker_lines(&world), 0, "a refused run starts nothing");
    assert!(world.run_ids().is_empty());

    // A plan inspected under other bounds is not the plan of this run.
    let inspected = world.plan(&["--timeout-secs", "60"]);
    let output = world.producer(
        "run",
        &[
            "--producer",
            STUB,
            "--plan-sha256",
            inspected["planSha256"].as_str().unwrap(),
        ],
    );
    assert!(
        stderr(&output).contains("plan_changed"),
        "{}",
        stderr(&output)
    );
    assert_eq!(marker_lines(&world), 0);
}

#[test]
fn run_times_out_and_reports() {
    let world = World::new();
    let pids = world.hook("tree.pids");
    world.mode(json!({"tree": pids, "sleep": true}));
    let output = world.run(&["--timeout-secs", "1"]);
    assert!(!output.status.success());
    let reply = reply(&output);
    assert_eq!(reply["state"], "failed");
    assert_eq!(reply["outcome"], "timed_out");
    assert_eq!(reply["errorCode"], "timed_out");
    assert!(stderr(&output).contains("timed_out"), "{}", stderr(&output));
    assert!(reply["diagnostics"]["durationMs"].as_u64().unwrap() >= 1000);
    assert_eq!(reply["diagnostics"]["groupSurvived"], false);
    let record = world.record(reply["runId"].as_str().unwrap());
    assert_eq!(record["outcome"], "timed_out");
    assert_eq!(record["diagnostics"]["groupSurvived"], false);
    assert_descendants_gone(&pids);
}

fn assert_descendants_gone(pids: &Path) {
    let text = fs::read_to_string(pids).unwrap();
    assert_eq!(text.lines().count(), 3);
    for pid in text.lines() {
        let pid: i32 = pid.parse().unwrap();
        trellis::testing::wait_until("descendant gone", || {
            !trellis::testing::process_running(pid)
        });
    }
}

#[test]
fn cancel_command_terminates_tree() {
    let world = World::with(&[("TRELLIS_TEST_PRODUCER_SPAWNED_SOCKET", "spawned.sock")]);
    let pids = world.hook("tree.pids");
    world.mode(json!({"tree": pids, "sleep": true}));
    let child = world.spawn_run(&[]);
    world.wait_hook("spawned.sock");
    let active = world.active_run();
    assert_eq!(active["producer"], STUB);
    assert_eq!(active["state"], "running");
    assert!(active["process"]["pgid"].is_i64());
    // The monitor is held at the hook while the stub runs: wait for its whole tree.
    trellis::testing::wait_until("descendants started", || {
        fs::read_to_string(&pids).is_ok_and(|text| text.lines().count() == 3)
    });
    let run_id = active["runId"].as_str().unwrap().to_owned();
    let output = world.producer("cancel", &["--run", &run_id]);
    assert!(output.status.success(), "{}", stderr(&output));
    world.release_hook("spawned.sock");

    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let reply = reply(&output);
    assert_eq!(reply["runId"], run_id);
    assert_eq!(reply["state"], "cancelled");
    assert_eq!(reply["outcome"], "cancelled");
    assert_descendants_gone(&pids);
    assert!(world.status()["activeRun"].is_null());
    assert_eq!(world.record(&run_id)["state"], "cancelled");

    let unknown = world.producer("cancel", &["--run", &"1".repeat(32)]);
    assert!(
        stderr(&unknown).contains("not_found"),
        "{}",
        stderr(&unknown)
    );
    let finished = world.producer("cancel", &["--run", &run_id]);
    assert!(
        stderr(&finished).contains("not_cancellable"),
        "{}",
        stderr(&finished)
    );
}

#[test]
fn cancel_after_enqueue_is_not_cancellable() {
    let world = World::with(&[("TRELLIS_TEST_PRODUCER_ENQUEUED_SOCKET", "enqueued.sock")]);
    world.emitting();
    let child = world.spawn_run(&[]);
    world.wait_hook("enqueued.sock");
    let active = world.active_run();
    assert_eq!(active["state"], "importing");
    let request_id = active["requestId"].as_str().unwrap().to_owned();
    let output = world.producer("cancel", &["--run", active["runId"].as_str().unwrap()]);
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(message.contains("not_cancellable"), "{message}");
    assert!(message.contains(&request_id), "{message}");
    world.release_hook("enqueued.sock");

    let output = child.wait_with_output().unwrap();
    let reply = reply(&output);
    assert!(output.status.success(), "{reply} {}", stderr(&output));
    assert_eq!(reply["outcome"], "imported");
    assert_eq!(reply["import"]["requestId"], request_id);
}

#[test]
fn crash_diagnostics_in_run_record() {
    let world = World::new();
    world.mode(json!({"stderr": 3, "abort": true}));
    let output = world.run(&[]);
    assert!(!output.status.success());
    let reply = reply(&output);
    assert_eq!(reply["state"], "failed");
    assert_eq!(reply["outcome"], "producer_failed");
    let run_id = reply["runId"].as_str().unwrap();
    for diagnostics in [&reply["diagnostics"], &world.record(run_id)["diagnostics"]] {
        assert_eq!(diagnostics["exit"]["signalName"], "SIGABRT");
        assert_eq!(diagnostics["exit"]["code"], Value::Null);
        assert!(
            diagnostics["stderrTail"]
                .as_str()
                .unwrap()
                .contains("stub stderr line 2")
        );
        assert_eq!(diagnostics["memoryEnforcement"], "sampled");
    }

    world.mode(json!({"stderr": 1, "exit": 3}));
    let reply = reply_of(&world.run(&[]));
    assert_eq!(reply["outcome"], "producer_failed");
    assert_eq!(reply["diagnostics"]["exit"]["code"], 3);

    world.mode(json!({}));
    let reply = reply_of(&world.run(&[]));
    assert_eq!(reply["outcome"], "producer_output_missing");
}

fn reply_of(output: &Output) -> Value {
    reply(output)
}

#[test]
fn basis_change_during_run_rejects_result() {
    let world = World::with(&[("TRELLIS_TEST_PRODUCER_EXITED_SOCKET", "exited.sock")]);
    world.emitting();
    let before = world.status()["head"]["sourceRevision"].clone();
    let child = world.spawn_run(&[]);
    world.wait_hook("exited.sock");
    fs::write(
        world.checkout.root.join("javascript/u00000.js"),
        "function changed() { g(); }\n",
    )
    .unwrap();
    trellis::testing::wait_until("head moved", || {
        world.status()["head"]["sourceRevision"] != before
    });
    world.release_hook("exited.sock");

    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let reply = reply(&output);
    assert_eq!(reply["state"], "failed");
    assert_eq!(reply["outcome"], "stale_basis");
    assert!(
        reply["errorCode"]
            .as_str()
            .unwrap()
            .starts_with("stale_basis"),
        "{reply}"
    );
    assert_eq!(reply["import"], Value::Null, "nothing was submitted");
    assert!(stderr(&output).contains("stale_basis"));
    // The rejected run publishes nothing: no revision carries its batch.
    assert_eq!(world.stub_entry()["mode"], "importedOnly");
}

#[test]
fn env_canaries_absent_in_daemon_launched_producer() {
    let canaries = [
        "NODE_OPTIONS",
        "JAVA_TOOL_OPTIONS",
        "RUSTC_WRAPPER",
        "PYTHONPATH",
        "AWS_SECRET_ACCESS_KEY",
        "GITHUB_TOKEN",
        "SSH_AUTH_SOCK",
        "TRELLIS_TEST_PRODUCER_STUB",
    ];
    // The daemon launches the producer, so its own environment carries the canaries.
    let daemon_canaries: Vec<(&str, &str)> = canaries
        .iter()
        .filter(|name| **name != "TRELLIS_TEST_PRODUCER_STUB")
        .map(|name| (*name, "canary"))
        .collect();
    let world = World::with_env(&[], &daemon_canaries);
    let dump = world.hook("env.json");
    let artifact = world.artifact(|_| {});
    world.mode(json!({"emit": artifact, "envdump": dump}));
    let args = world.run_args(&[]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut command = world.command("run", &args);
    for name in canaries {
        command.env(name, "canary");
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let dumped: Value = serde_json::from_slice(&fs::read(&dump).unwrap()).unwrap();
    let env = dumped["env"].as_object().unwrap();
    for name in canaries {
        assert!(!env.contains_key(name), "{name} reached the producer");
    }
    assert!(env.keys().all(|key| !key.starts_with("TRELLIS_")));
    // The run's own directory is its HOME, and the worktree its cwd.
    let run_dir = fs::canonicalize(world.runs_dir()).unwrap();
    let cwd = dumped["cwd"].as_str().unwrap();
    assert!(
        cwd.starts_with(run_dir.to_str().unwrap()) && cwd.ends_with("/worktree"),
        "{cwd}"
    );
    assert!(env["HOME"].as_str().unwrap().ends_with("/home"));
}

#[test]
fn workspace_unmodified_by_run() {
    let world = World::new();
    world.emitting();
    let before = workspace_tree_digest(&world.checkout.root);
    let output = world.run(&[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(workspace_tree_digest(&world.checkout.root), before);
    // The producer saw the retained bytes in its own worktree, not the workspace.
    let mode = json!({"envdump": world.hook("cwd.json")});
    world.mode(mode);
    let _ = world.run(&[]);
    let dumped: Value = serde_json::from_slice(&fs::read(world.hook("cwd.json")).unwrap()).unwrap();
    assert!(
        !dumped["cwd"]
            .as_str()
            .unwrap()
            .starts_with(world.checkout.root.to_str().unwrap())
    );
    assert_eq!(workspace_tree_digest(&world.checkout.root), before);
}

#[test]
fn retirement_cancels_active_run() {
    let world = World::with(&[("TRELLIS_TEST_PRODUCER_SPAWNED_SOCKET", "spawned.sock")]);
    let pids = world.hook("tree.pids");
    world.mode(json!({"tree": pids, "sleep": true}));
    let child = world.spawn_run(&[]);
    world.wait_hook("spawned.sock");
    trellis::testing::wait_until("descendants started", || {
        fs::read_to_string(&pids).is_ok_and(|text| text.lines().count() == 3)
    });
    // The captured root is replaced under the active run.
    let root = world.checkout.root.clone();
    fs::rename(&root, world.checkout.temp.path().join("old-checkout")).unwrap();
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function g(){}\n").unwrap();
    let nudge = world
        .checkout
        .cli()
        .args(["status", "--workspace"])
        .arg(&root)
        .output()
        .unwrap();
    let _ = nudge;
    world.release_hook("spawned.sock");

    let output = child.wait_with_output().unwrap();
    let reply = reply(&output);
    assert_eq!(reply["state"], "failed", "{reply} {}", stderr(&output));
    assert_eq!(reply["outcome"], "root_changed");
    assert!(
        reply["errorCode"]
            .as_str()
            .unwrap()
            .starts_with("root_changed")
    );
    assert_descendants_gone(&pids);
}

#[test]
fn run_dir_cleaned_and_record_retained() {
    let world = World::new();
    world.emitting();
    let first = reply_of(&world.run(&[]));
    world.emitting_variant(1);
    let second = reply_of(&world.run(&["--trust", "trusted"]));
    assert_eq!(world.run_ids().len(), 2);
    for reply in [&first, &second] {
        let id = reply["runId"].as_str().unwrap();
        // Only the record survives a finished, imported run.
        assert!(only_record_remains(&world.runs_dir().join(id)), "{id}");
        assert_eq!(world.record(id)["outcome"], "imported");
    }

    // A failed producer is cleaned by phase too, and its record is kept.
    world.mode(json!({"exit": 1}));
    let failed = reply_of(&world.run(&[]));
    let id = failed["runId"].as_str().unwrap();
    assert!(only_record_remains(&world.runs_dir().join(id)));
    assert_eq!(world.run_ids().len(), 3);
}

#[test]
fn status_reports_restricted_and_trusted_modes() {
    let world = World::new();
    world.emitting();
    let entry = world.stub_entry();
    assert_eq!(entry["mode"], "importedOnly");
    assert_eq!(entry["lastRun"], Value::Null);
    assert!(world.status()["activeRun"].is_null());

    let restricted = reply_of(&world.run(&[]));
    let entry = world.stub_entry();
    assert_eq!(entry["mode"], "restricted");
    assert_eq!(entry["lastRun"]["runId"], restricted["runId"]);
    assert_eq!(entry["lastRun"]["outcome"], "imported");

    world.emitting_variant(1);
    let trusted = reply_of(&world.run(&["--trust", "trusted"]));
    let entry = world.stub_entry();
    assert_eq!(entry["mode"], "trusted");
    assert_eq!(entry["lastRun"]["runId"], trusted["runId"]);
    assert_eq!(entry["lastRun"]["trustProfile"], "trusted");
    let other = world.status()["producers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "scip-java")
        .unwrap()
        .clone();
    assert_eq!(other["mode"], "importedOnly");
    assert_eq!(other["lastRun"], Value::Null);

    // One run by ID.
    let id = restricted["runId"].as_str().unwrap();
    let output = world.producer("status", &["--run", id]);
    assert!(output.status.success(), "{}", stderr(&output));
    let one = reply_of(&output);
    assert_eq!(one["run"]["runId"], id);
    assert_eq!(one["run"]["trustProfile"], "restricted");
    let missing = world.producer("status", &["--run", &"2".repeat(32)]);
    assert!(
        stderr(&missing).contains("not_found"),
        "{}",
        stderr(&missing)
    );
}

#[test]
fn run_reply_carries_typed_import_error() {
    let world = World::new();
    let artifact = world.artifact(|index| {
        index
            .metadata
            .as_mut()
            .unwrap()
            .tool_info
            .as_mut()
            .unwrap()
            .version = "0.0.0-not-the-pin".into();
    });
    world.mode(json!({"emit": artifact}));
    let output = world.run(&[]);
    assert!(!output.status.success());
    let reply = reply(&output);
    assert_eq!(reply["state"], "failed");
    assert_eq!(reply["outcome"], "import_failed");
    let code = reply["errorCode"].as_str().unwrap();
    assert!(code.starts_with("invalid_artifact"), "{code}");
    assert!(stderr(&output).contains(code), "{}", stderr(&output));
    assert_eq!(reply["import"]["publishedRevision"], Value::Null);
    let run_id = reply["runId"].as_str().unwrap();
    assert_eq!(world.record(run_id)["errorCode"], code);
    assert_eq!(world.stub_entry()["lastRun"]["errorCode"], code);
    assert!(only_record_remains(&world.runs_dir().join(run_id)));
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn index_revision(world: &World) -> u64 {
    world.status()["head"]["indexRevision"].as_u64().unwrap()
}

/// A run record another daemon left unfinished, with leftovers in its directory.
fn plant_foreign_run(world: &World, id: &str) -> PathBuf {
    let dir = world.runs_dir().join(id);
    for name in ["worktree", "out", "batch"] {
        fs::create_dir_all(dir.join(name)).unwrap();
        fs::write(dir.join(name).join("file"), b"x").unwrap();
    }
    let record = json!({
        "version": 1,
        "runId": id,
        "producer": STUB,
        "trustProfile": "restricted",
        "planSha256": "0".repeat(64),
        "sourceRevision": "revision",
        "state": "importing",
        "outcome": null,
        "errorCode": null,
        "daemon": {"pid": 1, "startTime": 0},
        "process": null,
        "requestId": null,
        "createdAt": 1,
        "finishedAt": null,
        "diagnostics": null,
    });
    fs::write(dir.join("run.json"), record.to_string()).unwrap();
    dir
}

fn assert_abandoned_and_stripped(world: &World, id: &str) {
    let record = world.record(id);
    assert_eq!(record["state"], "failed");
    assert_eq!(record["outcome"], "abandoned");
    assert_eq!(record["errorCode"], "abandoned");
    assert!(only_record_remains(&world.runs_dir().join(id)));
}

#[test]
fn root_loss_before_the_run_takes_its_slot_cancels_it() {
    let world = World::with(&[("TRELLIS_TEST_PRODUCER_PRESLOT_SOCKET", "preslot.sock")]);
    world.emitting();
    let child = world.spawn_run(&[]);
    world.wait_hook("preslot.sock");
    // The captured root is replaced while the run is admitted but has no slot yet.
    let root = world.checkout.root.clone();
    fs::rename(&root, world.checkout.temp.path().join("old-checkout")).unwrap();
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function g(){}\n").unwrap();
    let _ = world
        .checkout
        .cli()
        .args(["status", "--workspace"])
        .arg(&root)
        .output()
        .unwrap();
    world.release_hook("preslot.sock");

    let output = child.wait_with_output().unwrap();
    let reply = reply(&output);
    assert_eq!(
        reply["outcome"],
        "root_changed",
        "{reply} {}",
        stderr(&output)
    );
    assert_eq!(reply["state"], "failed");
    assert_eq!(marker_lines(&world), 0, "no producer was started");
    assert_eq!(reply["import"], Value::Null);
}

#[test]
fn import_inputs_survive_until_the_request_is_terminal() {
    let world = World::with(&[("TRELLIS_TEST_PRODUCER_ENQUEUED_SOCKET", "enqueued.sock")]);
    world.emitting();
    let child = world.spawn_run(&[]);
    world.wait_hook("enqueued.sock");
    let active = world.active_run();
    let dir = world.runs_dir().join(active["runId"].as_str().unwrap());
    // The producer phase is over; only the import's inputs and the record remain.
    assert_eq!(entries(&dir), ["batch", "out", "run.json"]);
    assert_eq!(
        world.record(active["runId"].as_str().unwrap())["state"],
        "importing"
    );
    world.release_hook("enqueued.sock");
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(only_record_remains(&dir));
}

#[test]
fn unwritable_importing_record_does_not_fail_the_accepted_import() {
    let world = World::with(&[
        ("TRELLIS_TEST_PRODUCER_ENQUEUED_SOCKET", "enqueued.sock"),
        ("TRELLIS_TEST_PRODUCER_FAIL_IMPORTING_RECORD", "unused"),
    ]);
    world.emitting();
    let child = world.spawn_run(&[]);
    world.wait_hook("enqueued.sock");
    let active = world.active_run();
    let run_id = active["runId"].as_str().unwrap().to_owned();
    let dir = world.runs_dir().join(&run_id);
    assert_eq!(entries(&dir), ["batch", "out", "run.json"]);
    assert_eq!(world.record(&run_id)["state"], "writingBatch");
    world.release_hook("enqueued.sock");

    let output = child.wait_with_output().unwrap();
    let reply = reply(&output);
    assert!(output.status.success(), "{reply} {}", stderr(&output));
    assert_eq!(reply["outcome"], "imported");
    let record = world.record(&run_id);
    assert_eq!(record["state"], "done");
    assert_eq!(record["requestId"], reply["import"]["requestId"]);
    assert!(only_record_remains(&dir));
}

#[test]
fn lost_import_wait_keeps_inputs_and_an_importing_record() {
    let world = World::with(&[
        ("TRELLIS_TEST_PRODUCER_ENQUEUED_SOCKET", "enqueued.sock"),
        ("TRELLIS_TEST_PRODUCER_FAIL_WAIT", "unused"),
    ]);
    world.emitting();
    let before = index_revision(&world);
    let child = world.spawn_run(&[]);
    world.wait_hook("enqueued.sock");
    let active = world.active_run();
    let run_id = active["runId"].as_str().unwrap().to_owned();
    let request_id = active["requestId"].as_str().unwrap().to_owned();
    // The accepted import publishes whatever the run's wait does.
    trellis::testing::wait_until("import published", || index_revision(&world) != before);
    world.release_hook("enqueued.sock");

    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("storage_busy"),
        "{}",
        stderr(&output)
    );
    let dir = world.runs_dir().join(&run_id);
    assert_eq!(entries(&dir), ["batch", "out", "run.json"]);
    let record = world.record(&run_id);
    assert_eq!(record["state"], "importing");
    assert_eq!(record["requestId"], request_id);
    assert_eq!(record["outcome"], Value::Null);
    assert!(world.status()["activeRun"].is_null());
}

#[test]
fn sweep_before_a_run_abandons_another_daemons_unfinished_run() {
    let world = World::new();
    world.emitting();
    let first = reply_of(&world.run(&[]));
    assert_eq!(first["outcome"], "imported");
    let foreign = run_id_text(7);
    plant_foreign_run(&world, &foreign);

    world.emitting_variant(1);
    let second = reply_of(&world.run(&[]));
    assert_eq!(second["outcome"], "imported");
    assert_abandoned_and_stripped(&world, &foreign);
}

#[test]
fn daemon_restart_sweep_abandons_another_daemons_unfinished_run() {
    let world = World::new();
    world.emitting();
    let first = reply_of(&world.run(&[]));
    assert_eq!(first["outcome"], "imported");
    support::stop_daemon(&world.checkout.home);
    let foreign = run_id_text(8);
    plant_foreign_run(&world, &foreign);

    // Any command starts a daemon, whose serving session sweeps before it serves.
    let output = world.producer("status", &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    trellis::testing::wait_until("restart sweep", || {
        world.record(&foreign)["state"] == "failed"
    });
    assert_abandoned_and_stripped(&world, &foreign);
}

fn run_id_text(n: u32) -> String {
    format!("{n:032x}")
}
