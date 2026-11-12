//! Automatic producer re-runs (#660). A manual run that imports records what it was approved
//! with, in the checkout's private producer state, and nothing else writes one. A workspace that
//! opted in through `.trellis/producers.toml` then gets that producer re-run after an edit, one
//! run at a time, never against a head the approval does not cover.
//!
//! The daemons here take their intervals from `TRELLIS_TEST_AUTORUN_INTERVALS`, and the runs are
//! ordered by the run hooks, never by sleeping.
#![cfg(feature = "test-support")]
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

use crate::producer_run::{STUB, World, entries, import_inputs, marker_lines, reply, stderr};
use crate::support::{self, state_dir};

fn approval_path(world: &World) -> PathBuf {
    state_dir(&world.checkout)
        .join("approvals")
        .join(format!("{STUB}.json"))
}

fn mode(path: &std::path::Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn manual_imported_run_writes_approval_with_exact_keys() {
    use trellis::store::topology::WorkspaceIdentity;
    let world = World::new();
    world.emitting();
    assert!(!approval_path(&world).exists());

    let output = world.run(&[]);
    let run = reply(&output);
    assert_eq!(run["outcome"], "imported", "{run}");

    let path = approval_path(&world);
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(path.parent().unwrap()), 0o700);
    let approval: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let mut keys: Vec<&str> = approval
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "approvedAt",
            "producer",
            "request",
            "rootInode",
            "rootRecordId",
            "templateSha256",
            "trustProfile"
        ]
    );
    assert_eq!(approval["producer"], STUB);
    assert_eq!(approval["trustProfile"], run["trustProfile"]);
    let template = approval["templateSha256"].as_str().unwrap();
    assert!(template.len() == 64 && template.bytes().all(|b| b.is_ascii_hexdigit()));
    let mut request: Vec<&str> = approval["request"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    request.sort_unstable();
    assert_eq!(request, ["environment", "executable", "limits"]);
    assert_eq!(approval["request"]["limits"]["timeoutSecs"], 1800);
    assert_eq!(approval["request"]["limits"]["memoryMiB"], 8192);

    let identity =
        WorkspaceIdentity::discover(Some(&world.checkout.root), &world.checkout.root).unwrap();
    assert_eq!(approval["rootRecordId"], identity.record_id);
    assert_eq!(approval["rootInode"], identity.inode);
    let record = world.record(run["runId"].as_str().unwrap());
    assert!(approval["approvedAt"].as_u64().unwrap() >= record["createdAt"].as_u64().unwrap());
}

#[test]
fn failed_manual_run_writes_no_approval() {
    let world = World::new();
    world.mode(serde_json::json!({"stderr": 1, "exit": 3}));
    let output = world.run(&[]);
    assert!(!output.status.success());
    assert_eq!(reply(&output)["outcome"], "producer_failed");
    assert!(!approval_path(&world).exists());
    assert!(!approval_path(&world).parent().unwrap().exists());
}

// --- automatic re-runs ---

/// Quiet, minimum interval and backstop of an automatic-run daemon, in milliseconds.
const FAST: (u64, u64, u64) = (50, 0, 200);

pub(crate) fn auto_world(hooks: &[(&str, &str)], intervals: (u64, u64, u64)) -> World {
    let (name, value) = support::auto_intervals(intervals.0, intervals.1, intervals.2);
    World::with_env(
        hooks,
        &[
            (trellis::testing::NATIVE_WATCHER_ENV, "on"),
            (name.as_str(), value.as_str()),
        ],
    )
}

fn auto_git_world(intervals: (u64, u64, u64)) -> World {
    let (name, value) = support::auto_intervals(intervals.0, intervals.1, intervals.2);
    World::with_git(
        &[],
        &[
            (trellis::testing::NATIVE_WATCHER_ENV, "on"),
            (name.as_str(), value.as_str()),
        ],
    )
}

/// Hold points a run passes in order; a hook name is its socket file.
const PRESLOT: &str = "preslot.sock";
const SPAWNED: &str = "spawned.sock";
const ENQUEUED: &str = "enqueued.sock";
const SUPERSEDE: &str = "supersede.sock";

/// The daemon environment variable that holds `socket`.
fn hook(socket: &'static str) -> (&'static str, &'static str) {
    let variable = match socket {
        PRESLOT => "TRELLIS_TEST_PRODUCER_PRESLOT_SOCKET",
        SPAWNED => "TRELLIS_TEST_PRODUCER_SPAWNED_SOCKET",
        ENQUEUED => "TRELLIS_TEST_PRODUCER_ENQUEUED_SOCKET",
        _ => "TRELLIS_TEST_AUTORUN_SUPERSEDE_SOCKET",
    };
    (variable, socket)
}

/// A daemon that holds the named hook points.
fn hooked(sockets: &[&'static str]) -> World {
    let hooks: Vec<_> = sockets.iter().map(|socket| hook(socket)).collect();
    auto_world(&hooks, FAST)
}

/// A hook point is held once per socket path; removing the socket arms it again.
fn rearm(world: &World, hook: &str) {
    let socket = world.hook(hook);
    let _ = fs::remove_file(&socket);
    let _ = fs::remove_file(socket.with_extension("ready"));
}

/// One manual run that imports: the operator's approval of automatic re-runs. The run passes
/// every run hook point the daemon holds, in order, and releases each; the `rearm` ones are held
/// again for the automatic run the test is about.
pub(crate) fn approve(world: &World, rearm_after: &[&str]) -> Value {
    approve_variant(world, rearm_after, 0, &[])
}

fn approve_variant(world: &World, rearm_after: &[&str], variant: u32, extra: &[&str]) -> Value {
    let held: Vec<&str> = [PRESLOT, SPAWNED, ENQUEUED]
        .into_iter()
        .filter(|socket| {
            let (variable, _) = hook(socket);
            world.env.iter().any(|(name, _)| name == variable)
        })
        .collect();
    world.emitting_variant(variant);
    let child = world.spawn_run(extra);
    for socket in &held {
        world.wait_hook(socket);
        world.release_hook(socket);
    }
    let output = child.wait_with_output().unwrap();
    let run = reply(&output);
    assert_eq!(run["outcome"], "imported", "{run} {}", stderr(&output));
    for socket in rearm_after {
        rearm(world, socket);
    }
    run
}

/// Edit a captured file and wait for the daemon's watcher to publish the new head.
pub(crate) fn edit_head(world: &World, n: u32) {
    let before = world.status()["head"]["sourceRevision"].clone();
    fs::write(
        world.checkout.root.join("javascript/u00000.js"),
        format!("function edited{n}() {{ g(); }}\n"),
    )
    .unwrap();
    trellis::testing::wait_until("head moved", || {
        world.status()["head"]["sourceRevision"] != before
    });
}

pub(crate) fn enable(world: &World) {
    support::enable_auto(&world.checkout.root, STUB, "");
}

/// Every readable run record of the checkout, oldest first.
pub(crate) fn records(world: &World) -> Vec<Value> {
    let mut records: Vec<Value> = world
        .run_ids()
        .iter()
        .filter_map(|id| fs::read(world.runs_dir().join(id).join("run.json")).ok())
        .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
        .collect();
    records.sort_by_key(|record| record["createdAt"].as_u64());
    records
}

pub(crate) fn auto_records(world: &World) -> Vec<Value> {
    records(world)
        .into_iter()
        .filter(|record| record["origin"] == "auto")
        .collect()
}

pub(crate) fn auto_state(world: &World) -> Value {
    support::auto_run(&world.checkout, STUB)
}

pub(crate) fn wait_auto(world: &World, state: &str) -> Value {
    let mut seen = Value::Null;
    trellis::testing::wait_until(&format!("autoRun {state}"), || {
        seen = auto_state(world);
        seen["state"] == state
    });
    seen
}

fn wait_records(world: &World, what: &str, ready: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let mut seen = Vec::new();
    trellis::testing::wait_until(what, || {
        seen = auto_records(world);
        ready(&seen)
    });
    seen
}

fn approved_at(world: &World) -> u64 {
    let bytes = fs::read(approval_path(world)).unwrap();
    serde_json::from_slice::<Value>(&bytes).unwrap()["approvedAt"]
        .as_u64()
        .unwrap()
}

fn rewrite_approval(world: &World, change: impl FnOnce(&mut Value)) {
    let path = approval_path(world);
    let mut approval: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    change(&mut approval);
    fs::write(&path, serde_json::to_vec(&approval).unwrap()).unwrap();
}

/// The marker lines of the stub's runs: one per process it started.
fn stub_runs(world: &World) -> usize {
    marker_lines(world)
}

#[test]
fn edit_triggers_one_auto_run_that_imports() {
    let world = auto_world(&[], FAST);
    let manual = approve(&world, &[]);
    edit_head(&world, 1);
    world.emitting_variant(1);
    enable(&world);

    let runs = wait_records(&world, "the automatic run", |runs| {
        runs.iter().any(|run| run["state"] == "done")
    });
    assert_eq!(runs.len(), 1, "{runs:?}");
    let run = &runs[0];
    assert_eq!(run["outcome"], "imported");
    assert_eq!(run["origin"], "auto");
    assert_eq!(run["producer"], STUB);
    assert_ne!(run["sourceRevision"], manual["sourceRevision"]);
    assert_eq!(stub_runs(&world), 2);

    // Imported at the head: nothing is due, and the one run is not repeated.
    let idle = wait_auto(&world, "idle");
    assert_eq!(idle["lastRunId"], run["runId"]);
    assert_eq!(idle["enabled"], true);
    assert_eq!(auto_records(&world).len(), 1);
    assert_eq!(stub_runs(&world), 2);
    assert_eq!(world.stub_entry()["lastRun"]["origin"], "auto");
}

#[test]
fn auto_import_becomes_retention_slot_and_records_origin() {
    let world = auto_world(&[], FAST);
    let manual = approve(&world, &[]);
    edit_head(&world, 1);
    world.emitting_variant(1);
    enable(&world);
    let runs = wait_records(&world, "the automatic run", |runs| {
        runs.iter().any(|run| run["state"] == "done")
    });
    let auto = &runs[0];
    assert_eq!(auto["origin"], "auto");
    assert_eq!(
        world.record(manual["runId"].as_str().unwrap())["origin"],
        "manual"
    );

    // The automatic import is the producer's retained slot: its inputs stay, the manual run's
    // went.
    let dir = world.runs_dir().join(auto["runId"].as_str().unwrap());
    wait_until_entries(&dir, import_inputs(auto));
    let first = world.runs_dir().join(manual["runId"].as_str().unwrap());
    wait_until_entries(&first, vec!["run.json"]);
}

fn wait_until_entries(dir: &std::path::Path, expected: Vec<&'static str>) {
    trellis::testing::wait_until("retained inputs", || entries(dir) == expected);
}

#[test]
fn auto_run_does_not_refresh_approval() {
    let world = auto_world(&[], FAST);
    approve(&world, &[]);
    let before = approved_at(&world);
    let bytes = fs::read(approval_path(&world)).unwrap();
    edit_head(&world, 1);
    world.emitting_variant(1);
    enable(&world);
    wait_records(&world, "the automatic run", |runs| {
        runs.iter().any(|run| run["state"] == "done")
    });
    assert_eq!(approved_at(&world), before);
    assert_eq!(fs::read(approval_path(&world)).unwrap(), bytes);
}

#[test]
fn clone_with_enabled_config_and_no_approval_needs_approval() {
    let world = auto_world(&[], FAST);
    enable(&world);
    let state = wait_auto(&world, "needsApproval");
    assert_eq!(state["reason"], "no approval record");
    assert_eq!(state["enabled"], true);
    assert_eq!(state["dueSince"], Value::Null);
    edit_head(&world, 1);
    assert_eq!(auto_state(&world)["state"], "needsApproval");
    assert!(world.run_ids().is_empty(), "no run record");
    assert_eq!(stub_runs(&world), 0);
}

#[test]
fn template_mismatch_needs_approval_and_trust_comes_from_record() {
    let world = auto_world(&[], FAST);
    // Approved as trusted: the automatic run uses the record's trust, not a default.
    approve_variant(&world, &[], 0, &["--trust", "trusted"]);
    edit_head(&world, 1);
    world.emitting_variant(1);
    enable(&world);
    let runs = wait_records(&world, "the automatic run", |runs| {
        runs.iter().any(|run| run["state"] == "done")
    });
    assert_eq!(runs[0]["trustProfile"], "trusted");

    // The approved template no longer matches what the plan derives.
    rewrite_approval(&world, |approval| {
        approval["templateSha256"] = json!("b".repeat(64));
    });
    world.emitting_variant(2);
    edit_head(&world, 2);
    let state = wait_auto(&world, "needsApproval");
    assert_eq!(state["reason"], "approval template changed");
    assert_eq!(auto_records(&world).len(), 1, "no second automatic run");
}

#[test]
fn replaced_root_drops_approval_until_one_manual_run() {
    let world = auto_world(&[], FAST);
    approve(&world, &[]);
    rewrite_approval(&world, |approval| {
        let inode = approval["rootInode"].as_u64().unwrap();
        approval["rootInode"] = json!(inode + 1);
    });
    edit_head(&world, 1);
    enable(&world);
    let state = wait_auto(&world, "needsApproval");
    assert_eq!(state["reason"], "approval belongs to another root");
    assert!(auto_records(&world).is_empty());

    // One manual run approves the root again.
    let before = approved_at(&world);
    approve_variant(&world, &[], 1, &[]);
    assert!(approved_at(&world) >= before);
    let state = wait_auto(&world, "idle");
    assert_eq!(state["reason"], Value::Null);
    assert!(auto_records(&world).is_empty());
}

#[test]
fn unreadable_approval_needs_approval() {
    let world = auto_world(&[], FAST);
    approve(&world, &[]);
    fs::write(approval_path(&world), b"{not json").unwrap();
    enable(&world);
    let state = wait_auto(&world, "needsApproval");
    assert_eq!(state["reason"], "approval record unreadable");
}

#[test]
fn auto_run_field_shape() {
    let world = auto_world(&[], (3_600_000, 0, 200));
    let status = world.status();
    for producer in status["producers"].as_array().unwrap() {
        let auto = producer["autoRun"].as_object().unwrap();
        let mut keys: Vec<&str> = auto.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "dueSince",
                "enabled",
                "lastRunId",
                "nextEarliestStart",
                "reason",
                "state"
            ]
        );
        assert_eq!(auto["state"], "off");
        assert_eq!(auto["enabled"], false);
        for key in ["reason", "dueSince", "nextEarliestStart", "lastRunId"] {
            assert_eq!(auto[key], Value::Null, "{key}");
        }
    }

    approve(&world, &[]);
    edit_head(&world, 1);
    enable(&world);
    let scheduled = wait_auto(&world, "scheduled");
    assert_eq!(scheduled["enabled"], true);
    assert_eq!(scheduled["reason"], "producer re-run scheduled");
    let due = scheduled["dueSince"].as_u64().unwrap();
    assert!(scheduled["nextEarliestStart"].as_u64().unwrap() >= due);
    assert_eq!(scheduled["lastRunId"], Value::Null);
    assert!(auto_records(&world).is_empty());
}

#[test]
fn invalid_config_reports_invalid_config_daemon_unaffected() {
    let world = auto_git_world(FAST);
    approve(&world, &[]);
    support::write_producers_toml(&world.checkout.root, "[auto.trellis-synthetic-scip\n");
    let state = wait_auto(&world, "invalidConfig");
    assert_eq!(state["reason"], "producers.toml syntax error");
    assert_eq!(state["enabled"], false);
    assert_eq!(state["dueSince"], Value::Null);

    // The daemon and its MCP server are unaffected.
    let mut mcp = Mcp::start(&world);
    let init = mcp.ask(
        "initialize",
        json!({"protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": {"name": "auto", "version": "1"}}),
    );
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    mcp.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let listed = mcp.ask("tools/list", json!({}));
    assert!(!listed["result"]["tools"].as_array().unwrap().is_empty());
    assert!(world.status()["head"].is_object());

    // Fixing the file takes effect without a restart.
    support::write_producers_toml(
        &world.checkout.root,
        &format!("[auto.{STUB}]\nenabled = false\n"),
    );
    let off = wait_auto(&world, "off");
    assert_eq!(off["reason"], Value::Null);
    assert_eq!(off["enabled"], false);
}

/// A raw JSON-RPC session with `trellis mcp` over stdio.
pub(crate) struct Mcp {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    lines: std::sync::mpsc::Receiver<String>,
    stderr: PathBuf,
    next: u64,
}

impl Mcp {
    fn start(world: &World) -> Self {
        Self::start_in(
            &world.checkout,
            &world.env,
            world.stub_dir.path().join("mcp.stderr"),
        )
    }

    /// The server of `checkout`, with `env` added to its environment; its stderr goes to
    /// `stderr`.
    pub(crate) fn start_in(
        checkout: &support::Checkout,
        env: &[(String, String)],
        stderr: PathBuf,
    ) -> Self {
        use std::io::BufRead;
        let mut child = checkout
            .cli()
            .envs(env.iter().map(|(name, value)| (name, value)))
            .arg("mcp")
            .arg("--workspace")
            .arg(&checkout.root)
            .current_dir(&checkout.root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let stdin = child.stdin.take();
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
            stderr,
            next: 1,
        }
    }

    pub(crate) fn send(&mut self, value: &Value) {
        use std::io::Write;
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{value}").unwrap();
        stdin.flush().unwrap();
    }

    pub(crate) fn ask(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let line = self
            .lines
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap_or_else(|error| {
                panic!(
                    "a bounded MCP reply: {error}; stderr: {}",
                    fs::read_to_string(&self.stderr).unwrap_or_default()
                )
            });
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["id"], id, "{reply}");
        reply
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn failed_auto_run_reports_reason_and_waits_for_head_change() {
    let world = hooked_git(&[PRESLOT]);
    approve(&world, &[PRESLOT]);
    edit_head(&world, 1);
    world.mode(json!({"stderr": 1, "exit": 1}));
    enable(&world);
    world.wait_hook(PRESLOT);
    world.release_hook(PRESLOT);
    let failed = wait_auto(&world, "failed");
    assert_eq!(failed["reason"], "automatic re-run failed: producer_failed");
    assert!(failed["dueSince"].is_u64());
    let runs = auto_records(&world);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["outcome"], "producer_failed");
    assert_eq!(failed["lastRunId"], runs[0]["runId"]);

    // The failure is the language's re-import reason in every view.
    let mut mcp = Mcp::initialized(&world);
    let (status, described, row) = javascript_views(&world, &mut mcp, 1);
    assert_eq!(
        status,
        pending_status("needsReimport", "automatic re-run failed: producer_failed")
    );
    let reimport =
        json!({"state": "needsReimport", "reason": "automatic re-run failed: producer_failed"});
    assert_eq!(described["reimport"], reimport);
    assert_eq!(row["reimport"], reimport);

    // At the same head nothing retries: a request that changes nothing wakes the daemon, and a
    // later reply shows no second run started.
    crate::child::run_cli(
        world
            .checkout
            .cli()
            .args(["index", "--workspace"])
            .arg(&world.checkout.root),
        "index at the same head",
    );
    assert_eq!(wait_auto(&world, "failed")["reason"], failed["reason"]);
    assert_eq!(auto_records(&world).len(), 1);

    // A new head ends the failure. The run waits at its slot while the stub learns the head.
    rearm(&world, PRESLOT);
    edit_head(&world, 2);
    world.wait_hook(PRESLOT);
    world.emitting_variant(2);
    world.release_hook(PRESLOT);
    wait_records(&world, "the retry", |runs| {
        runs.iter().any(|run| run["state"] == "done")
    });
    assert_eq!(wait_auto(&world, "idle")["reason"], Value::Null);
    let (status, described, _) = javascript_views(&world, &mut mcp, 2);
    assert_eq!(status, json!([]));
    assert_eq!(described["reimport"], Value::Null);
}

/// Retire the daemon and bring a new one up through the armed environment: its owner is
/// seeded from the run records.
fn restart(world: &World) {
    support::stop_daemon(&world.checkout.home);
    let output = world.producer("status", &[]);
    assert!(output.status.success(), "{}", stderr(&output));
}

#[test]
fn reactivation_keeps_failure_backoff() {
    let world = auto_world(&[], FAST);
    approve(&world, &[]);
    edit_head(&world, 1);
    world.mode(json!({"stderr": 1, "exit": 1}));
    enable(&world);
    let failed = wait_auto(&world, "failed");
    let before = auto_records(&world);
    assert_eq!(before.len(), 1);

    restart(&world);
    let again = wait_auto(&world, "failed");
    assert_eq!(again["reason"], failed["reason"]);
    assert_eq!(auto_records(&world).len(), 1, "no new run record");
    assert_eq!(stub_runs(&world), 2);
}

#[test]
fn reactivation_keeps_min_interval() {
    let hour = 3_600_000;
    let world = auto_world(&[], (50, hour, 200));
    approve(&world, &[]);
    edit_head(&world, 1);
    world.emitting_variant(1);
    enable(&world);
    let runs = wait_records(&world, "the first automatic run", |runs| {
        runs.iter().any(|run| run["state"] == "done")
    });
    let created = runs[0]["createdAt"].as_u64().unwrap();

    restart(&world);
    edit_head(&world, 2);
    let state = wait_auto(&world, "scheduled");
    assert_eq!(state["nextEarliestStart"].as_u64().unwrap(), created + hour);
    assert_eq!(auto_records(&world).len(), 1, "no new run record");
}

#[test]
fn manual_run_preempts_auto_run() {
    let world = hooked(&[SPAWNED]);
    approve(&world, &[SPAWNED]);
    let approved = approved_at(&world);
    edit_head(&world, 1);
    world.mode(json!({"sleep": true}));
    enable(&world);
    world.wait_hook(SPAWNED);
    let active = world.active_run();
    assert_eq!(active["origin"], "auto");
    assert_eq!(auto_state(&world)["state"], "running");
    assert_eq!(auto_state(&world)["lastRunId"], active["runId"]);
    // The automatic stub has read its mode before the manual run changes it.
    trellis::testing::wait_until("the automatic stub started", || stub_runs(&world) == 2);

    world.emitting_variant(2);
    let manual = world.spawn_run(&[]);
    world.release_hook(SPAWNED);
    let output = manual.wait_with_output().unwrap();
    let manual = reply(&output);
    assert_eq!(
        manual["outcome"],
        "imported",
        "{manual} {}",
        stderr(&output)
    );
    assert_eq!(manual["origin"], "manual");

    let preempted = world.record(active["runId"].as_str().unwrap());
    assert_eq!(preempted["origin"], "auto");
    assert_eq!(preempted["state"], "cancelled");
    assert_eq!(preempted["outcome"], "cancelled");
    assert_eq!(preempted["errorCode"], "cancelled: preempted");
    assert!(approved_at(&world) > approved);
    // Preemption is not a failure.
    assert_ne!(wait_auto_not_running(&world)["state"], "failed");
}

fn wait_auto_not_running(world: &World) -> Value {
    let mut seen = Value::Null;
    trellis::testing::wait_until("the automatic run to end", || {
        seen = auto_state(world);
        seen["state"] != "running"
    });
    seen
}

#[test]
fn manual_run_against_importing_auto_run_is_busy() {
    let world = hooked(&[ENQUEUED]);
    approve(&world, &[ENQUEUED]);
    edit_head(&world, 1);
    world.emitting_variant(1);
    enable(&world);
    world.wait_hook(ENQUEUED);
    assert_eq!(world.active_run()["origin"], "auto");
    assert_eq!(world.active_run()["state"], "importing");

    let args = world.run_args(&[]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = world.producer("run", &args);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("producer_busy"),
        "{}",
        stderr(&output)
    );

    world.release_hook(ENQUEUED);
    let runs = wait_records(&world, "the import", |runs| {
        runs.iter().any(|run| run["state"] == "done")
    });
    assert_eq!(runs[0]["outcome"], "imported");
}

#[test]
fn edit_during_running_supersedes() {
    let world = hooked(&[SPAWNED, SUPERSEDE]);
    approve(&world, &[SPAWNED]);
    edit_head(&world, 1);
    world.mode(json!({"sleep": true}));
    enable(&world);
    world.wait_hook(SPAWNED);
    let active = world.active_run();
    assert_eq!(active["origin"], "auto");
    trellis::testing::wait_until("the automatic stub started", || stub_runs(&world) == 2);

    // A newer head: the owner asks the run to stop, once, and the hook marks the call.
    edit_head(&world, 2);
    world.wait_hook(SUPERSEDE);
    // The next run starts as soon as this one ends, and it reads the head just published.
    world.emitting_variant(2);
    world.release_hook(SUPERSEDE);
    world.release_hook(SPAWNED);

    let runs = wait_records(&world, "the rerun", |runs| {
        runs.len() == 2 && runs[0]["state"] == "cancelled" && runs[1]["state"] == "done"
    });
    let superseded = &runs[0];
    assert_eq!(superseded["runId"], active["runId"]);
    assert_eq!(superseded["state"], "cancelled");
    assert_eq!(superseded["outcome"], "cancelled");
    assert_eq!(superseded["errorCode"], "cancelled: superseded");
    assert_eq!(runs[1]["outcome"], "imported");
    assert_ne!(runs[1]["sourceRevision"], superseded["sourceRevision"]);
    // Superseding is not a failure.
    assert_eq!(wait_auto(&world, "idle")["reason"], Value::Null);
}

#[test]
fn supersede_before_install_installs_cancelled() {
    let world = hooked(&[PRESLOT, SUPERSEDE]);
    approve(&world, &[PRESLOT]);
    edit_head(&world, 1);
    enable(&world);
    // The run has passed its plan check and has no slot yet.
    world.wait_hook(PRESLOT);

    edit_head(&world, 2);
    // The owner's first request finds no installed run, and the token is raised.
    world.wait_hook(SUPERSEDE);
    world.emitting_variant(2);
    world.release_hook(SUPERSEDE);
    world.release_hook(PRESLOT);

    let runs = wait_records(&world, "the rerun", |runs| {
        runs.len() == 2 && runs[1]["state"] == "done"
    });
    assert_eq!(runs[0]["state"], "cancelled");
    assert_eq!(runs[0]["errorCode"], "cancelled: superseded");
    assert_eq!(runs[1]["outcome"], "imported");
    // Only the manual run and the second automatic run started the stub.
    assert_eq!(stub_runs(&world), 2);
}

#[test]
fn root_loss_cancels_auto_run() {
    let world = hooked(&[SPAWNED]);
    approve(&world, &[SPAWNED]);
    edit_head(&world, 1);
    world.mode(json!({"sleep": true}));
    enable(&world);
    world.wait_hook(SPAWNED);
    let active = world.active_run();
    assert_eq!(active["origin"], "auto");

    // The run directory is found through the root's identity, which the replacement changes.
    let record_path = world
        .runs_dir()
        .join(active["runId"].as_str().unwrap())
        .join("run.json");
    let read = || -> Value { serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap() };
    let root = world.checkout.root.clone();
    fs::rename(&root, world.checkout.temp.path().join("old-checkout")).unwrap();
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function g(){}\n").unwrap();
    let _ = world
        .checkout
        .cli()
        .args(["status", "--workspace"])
        .arg(&root)
        .output();
    world.release_hook(SPAWNED);

    trellis::testing::wait_until("the run ends", || read()["state"] == "failed");
    let record = read();
    assert_eq!(record["origin"], "auto");
    assert_eq!(record["outcome"], "root_changed");
}

// --- read-path labels (#660 D10) ---

/// Like `hooked`, for a Git checkout: the MCP server serves only those.
fn hooked_git(sockets: &[&'static str]) -> World {
    let hooks: Vec<_> = sockets.iter().map(|socket| hook(socket)).collect();
    let (name, value) = support::auto_intervals(FAST.0, FAST.1, FAST.2);
    World::with_git(
        &hooks,
        &[
            (trellis::testing::NATIVE_WATCHER_ENV, "on"),
            (name.as_str(), value.as_str()),
        ],
    )
}

impl Mcp {
    fn initialized(world: &World) -> Self {
        Self::handshake(Self::start(world))
    }

    pub(crate) fn handshake(mut mcp: Self) -> Self {
        let init = mcp.ask(
            "initialize",
            json!({"protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "auto", "version": "1"}}),
        );
        assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
        mcp.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        mcp
    }

    /// The structured content of a tool call; a tool-level error fails the test.
    pub(crate) fn tool(&mut self, name: &str, arguments: Value) -> Value {
        let reply = self.ask("tools/call", json!({"name": name, "arguments": arguments}));
        assert!(reply["result"]["isError"] != true, "{name}: {reply}");
        reply["result"]["structuredContent"].clone()
    }
}

/// The `semanticStatus` of `trellis_declaration` for the first symbol matching `query`, once the
/// head indexes one.
pub(crate) fn declaration_semantic_status(mcp: &mut Mcp, query: &str) -> Value {
    let mut symbol = Value::Null;
    trellis::testing::wait_until("the symbol is indexed", || {
        let found = mcp.tool(
            "trellis_find_symbols",
            json!({"schemaVersion": 1, "query": query, "limit": 5}),
        );
        symbol = found["data"]["symbols"][0]["symbolId"].clone();
        symbol.is_string()
    });
    mcp.tool(
        "trellis_declaration",
        json!({"schemaVersion": 1, "symbolId": symbol}),
    )["semanticStatus"]
        .clone()
}

/// What the stub's language (javascript) reports through the MCP tools and the CLI at the head
/// the test just edited to: `(semanticStatus of a declaration answer, describe's javascript
/// semantic block, the status reply's javascript row)`.
fn javascript_views(world: &World, mcp: &mut Mcp, edit: u32) -> (Value, Value, Value) {
    let semantic_status = declaration_semantic_status(mcp, &format!("edited{edit}"));
    let described = mcp.tool("trellis_workspace_describe", json!({"schemaVersion": 1}));
    let language = described["data"]["languages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["language"] == "javascript")
        .unwrap()["semantic"]
        .clone();
    let status = crate::child::run_cli_json(
        world
            .checkout
            .cli()
            .args(["status", "--workspace"])
            .arg(&world.checkout.root),
        "status",
    );
    let row = status["semanticLanguages"]
        .as_array()
        .map(|rows| rows.iter().find(|row| row["language"] == "javascript"))
        .and_then(|row| row.cloned())
        .unwrap_or(Value::Null);
    (semantic_status, language, row)
}

/// Like `javascript_views`, once the head's semantic catch-up has published `semanticStatus ==
/// []`: right after an edit the answer can still read `catchingUp` or `re-import pending`.
fn settled_views(world: &World, mcp: &mut Mcp, edit: u32) -> (Value, Value, Value) {
    trellis::testing::wait_until("semantic catch-up published", || {
        declaration_semantic_status(mcp, &format!("edited{edit}")) == json!([])
    });
    javascript_views(world, mcp, edit)
}

const SCHEDULED_REASON: &str = "producer re-run scheduled";

fn pending_status(state: &str, reason: &str) -> Value {
    json!([{"language": "javascript", "state": state, "reason": reason}])
}

/// While the automatic run is in flight the language is catching up with `producer re-run
/// scheduled` in `semanticStatus`, and the committed evidence is untouched. Without the run the
/// same head reads exactly as committed.
#[test]
fn running_auto_run_labels_language_catching_up_and_nothing_else() {
    let world = hooked_git(&[SPAWNED]);
    approve(&world, &[SPAWNED]);
    edit_head(&world, 1);
    let mut mcp = Mcp::initialized(&world);

    // Auto-run off: the committed projection, with no overlay.
    assert_eq!(auto_state(&world)["state"], "off");
    let (off_status, off_described, off_row) = settled_views(&world, &mut mcp, 1);
    assert_eq!(off_status, json!([]));

    world.mode(json!({"sleep": true}));
    enable(&world);
    world.wait_hook(SPAWNED);
    let running = auto_state(&world);
    assert_eq!(running["state"], "running");
    assert_eq!(world.active_run()["origin"], "auto");

    let (status, described, row) = javascript_views(&world, &mut mcp, 1);
    assert_eq!(status, pending_status("catchingUp", SCHEDULED_REASON));
    // Evidence, coverage and freshness come from the committed revision.
    assert_eq!(described, off_described);
    assert_eq!(row, off_row);
    // #930: the row counts its documents by status; a scheduled run labels none needsReimport.
    assert_eq!(
        described["documentStatus"]["needsReimport"], 0,
        "{described}"
    );
    // The stub sleeps until it is stopped: end the run so no stub outlives the test.
    let run_id = world.active_run()["runId"].as_str().unwrap().to_owned();
    let output = world.producer("cancel", &["--run", &run_id]);
    assert!(output.status.success(), "{}", stderr(&output));
    world.release_hook(SPAWNED);
    wait_records(&world, "the cancelled run", |runs| {
        runs.iter().all(|run| run["state"] == "cancelled")
    });
}

/// With auto-run off (default, then explicitly disabled) the daemon adds no label and starts no
/// run: the views are the same before and after, and no automatic run record appears. That the
/// documents read `needsReimport` without a label is asserted in
/// `tests/semantic/auto_run_labels.rs::empty_labels_change_nothing`.
#[test]
fn auto_off_adds_no_label_and_starts_no_run() {
    let world = hooked_git(&[SPAWNED]);
    approve(&world, &[SPAWNED]);
    edit_head(&world, 1);
    let mut mcp = Mcp::initialized(&world);
    assert_eq!(auto_state(&world)["state"], "off");
    let committed = settled_views(&world, &mut mcp, 1);
    assert_eq!(committed.0, json!([]));
    assert_eq!(committed.1["reimport"], Value::Null);
    assert_eq!(committed.2["reimport"], Value::Null);
    assert_eq!(javascript_views(&world, &mut mcp, 1), committed);

    // Explicitly disabled: still no label.
    support::write_producers_toml(
        &world.checkout.root,
        &format!("[auto.{STUB}]\nenabled = false\n"),
    );
    wait_auto(&world, "off");
    assert_eq!(javascript_views(&world, &mut mcp, 1), committed);
    assert!(auto_records(&world).is_empty());
}

// --- gates (#660 AC10) ---

fn wait_deferred(world: &World, reason: &str) {
    trellis::testing::wait_until(&format!("autoRun {reason}"), || {
        let state = auto_state(world);
        state["state"] == "deferred" && state["reason"] == reason
    });
}

/// A gate defers a due producer, and the deferral is rechecked rather than permanent. A gate
/// that closes while the run is in flight does not stop it.
#[test]
fn battery_and_load_gates_defer_and_never_stop_a_run() {
    let world = hooked(&[SPAWNED]);
    approve(&world, &[SPAWNED]);
    let home = world.checkout.home.clone();
    support::set_probes(&home, Some(true), Some(false));
    edit_head(&world, 1);
    world.emitting_variant(1);
    enable(&world);

    wait_deferred(&world, "deferred: battery");
    let deferred = auto_state(&world);
    assert!(deferred["nextEarliestStart"].is_u64());
    assert!(deferred["dueSince"].is_u64());
    assert!(auto_records(&world).is_empty());

    support::set_probes(&home, Some(false), Some(true));
    wait_deferred(&world, "deferred: load");
    assert!(auto_records(&world).is_empty());

    support::set_probes(&home, Some(false), Some(false));
    world.wait_hook(SPAWNED);
    assert_eq!(auto_state(&world)["state"], "running");

    // Both gates close during the run: it is not stopped, and no second run starts.
    support::set_probes(&home, Some(true), Some(true));
    assert_eq!(auto_state(&world)["state"], "running");
    assert_eq!(world.active_run()["origin"], "auto");
    world.release_hook(SPAWNED);
    let runs = wait_records(&world, "the automatic run", |runs| {
        runs.iter().any(|run| run["state"] == "done")
    });
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["outcome"], "imported");
}

/// Each switch turns off its own gate; a host that reports nothing never gates.
#[test]
fn gate_switches_off() {
    let world = auto_world(&[], FAST);
    approve(&world, &[]);
    support::set_probes(&world.checkout.home, Some(true), Some(true));
    edit_head(&world, 1);
    world.emitting_variant(1);

    support::enable_auto(&world.checkout.root, STUB, "batteryGate = false");
    wait_deferred(&world, "deferred: load");

    support::enable_auto(
        &world.checkout.root,
        STUB,
        "batteryGate = false\nloadGate = false",
    );
    let runs = wait_records(&world, "the automatic run", |runs| {
        runs.iter().any(|run| run["state"] == "done")
    });
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["outcome"], "imported");
}
