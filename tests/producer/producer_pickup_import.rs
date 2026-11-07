//! `trellis producer pickup`: an owner-configured artifact is imported through the unchanged
//! #61 path without running a producer, replies with the typed import outcome, survives a
//! daemon restart and is removed by `--clear`.
#![cfg(feature = "test-support")]
use serde_json::Value;
use std::{fs, path::Path, process::Output};

use crate::support;
use support::{
    Checkout, cache_dir, start_checkout, state_dir, stop_daemon, tripwire_dir,
    workspace_tree_digest,
};

use crate::semantic_batch_support;
use semantic_batch_support::{BatchFixture, edit_scip, uniform_workspace};

const LANGUAGES: [&str; 4] = ["java", "rust", "python", "javascript"];

fn workspace(root: &Path) {
    uniform_workspace(root, &LANGUAGES, 2);
}

fn index_db(checkout: &Checkout) -> std::path::PathBuf {
    use trellis::store::topology::{TopologyRoots, WorkspaceIdentity};
    let roots = TopologyRoots::isolated_for_tests(
        cache_dir(&checkout.home),
        support::data_dir(&checkout.home),
    );
    let identity = WorkspaceIdentity::discover(Some(&checkout.root), &checkout.root).unwrap();
    roots.index_db(&identity)
}

/// A valid `scip-java` artifact over the checkout's java documents, written outside it.
fn java_artifact(checkout: &Checkout, name: &str) -> std::path::PathBuf {
    let mut fixture =
        BatchFixture::from_published(&index_db(checkout), &checkout.root).reference("scip-java");
    let docs = fixture.paths("java");
    let bytes = fixture.scip_bytes("java", &docs);
    let _ = &mut fixture;
    let path = checkout.temp.path().join(name);
    fs::write(&path, bytes).unwrap();
    path
}

fn pickup(checkout: &Checkout, tripwire: &Path, args: &[&str]) -> Output {
    checkout
        .cli()
        .env("PATH", format!("{}:/usr/bin:/bin", tripwire.display()))
        .args(["producer", "pickup", "--workspace"])
        .arg(&checkout.root)
        .args(args)
        .output()
        .unwrap()
}

fn status(checkout: &Checkout) -> Value {
    crate::child::run_cli_json(
        checkout
            .cli()
            .args(["producer", "status", "--workspace"])
            .arg(&checkout.root),
        "producer status",
    )
}

fn pickup_status(checkout: &Checkout, producer: &str) -> Value {
    let status = status(checkout);
    let entry = status["producers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == producer)
        .unwrap();
    entry["pickup"].clone()
}

/// Daemon settings that keep the pickup watch from triggering an import of its own while a
/// test runs (#197). A watcher import is legitimate, and the pickup directory it holds is
/// present until its verdict, so a test that asserts on `pickups` after a CLI pickup must not
/// race one. The watch still arms; only its debounce, stability and stat-check intervals are
/// longer than any test.
fn quiet_watch() -> Vec<(String, String)> {
    vec![(
        "TRELLIS_TEST_PICKUP_INTERVALS".to_owned(),
        r#"{"debounceMs":3600000,"stableMs":3600000,"checkMs":3600000}"#.to_owned(),
    )]
}

fn json_out(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "no JSON on stdout; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn pickup_set_imports_artifact_without_producer() {
    let checkout = start_checkout(workspace, &quiet_watch());
    let (tripwire, marker) = tripwire_dir(checkout.temp.path());
    let artifact = java_artifact(&checkout, "index.scip");
    let before = workspace_tree_digest(&checkout.root);
    let output = pickup(
        &checkout,
        &tripwire,
        &[
            "--producer",
            "scip-java",
            "--artifact",
            artifact.to_str().unwrap(),
        ],
    );
    let reply = json_out(&output);
    crate::child::assert_cli_success(&output, format_args!("{reply}"));
    assert_eq!(reply["state"], "done");
    assert_eq!(reply["errorCode"], Value::Null);
    assert_eq!(reply["producer"], "scip-java");
    assert!(reply["publishedRevision"].is_object());
    assert!(reply["report"].is_object());
    assert!(reply["requestId"].is_string());
    assert_eq!(reply["sha256"].as_str().unwrap().len(), 64);

    assert!(!marker.exists(), "pickup ran a producer");
    assert_eq!(workspace_tree_digest(&checkout.root), before);
    let state = state_dir(&checkout);
    assert!(!state.join("runs").exists(), "pickup created a run");
    let leftovers = fs::read_dir(state.join("pickups"))
        .map(|dir| dir.count())
        .unwrap_or(0);
    assert_eq!(leftovers, 0, "the pickup directory outlived its import");

    let entry = pickup_status(&checkout, "scip-java");
    assert_eq!(
        entry["artifact"],
        fs::canonicalize(&artifact).unwrap().to_str().unwrap()
    );
    assert_eq!(entry["lastImported"]["state"], "done");
    assert_eq!(entry["lastImported"]["sha256"], reply["sha256"]);
    assert_eq!(entry["lastImported"]["requestId"], reply["requestId"]);
    assert_eq!(entry["lastImported"]["errorCode"], Value::Null);
    assert_eq!(pickup_status(&checkout, "scip-typescript"), Value::Null);
}

#[test]
fn pickup_set_reports_typed_import_error() {
    let checkout = start_checkout(workspace, &quiet_watch());
    let (tripwire, marker) = tripwire_dir(checkout.temp.path());
    let good = java_artifact(&checkout, "good.scip");
    let bad = checkout.temp.path().join("bad.scip");
    fs::write(
        &bad,
        edit_scip(&fs::read(&good).unwrap(), |index| {
            index
                .metadata
                .as_mut()
                .unwrap()
                .tool_info
                .as_mut()
                .unwrap()
                .version = "0.0.0-not-the-pin".into();
        }),
    )
    .unwrap();
    let output = pickup(
        &checkout,
        &tripwire,
        &[
            "--producer",
            "scip-java",
            "--artifact",
            bad.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    let reply = json_out(&output);
    assert_eq!(reply["state"], "failed");
    let code = reply["errorCode"].as_str().unwrap();
    assert!(code.starts_with("invalid_artifact"), "{code}");
    assert_eq!(reply["publishedRevision"], Value::Null);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(code), "{stderr}");
    assert!(!marker.exists());

    let entry = pickup_status(&checkout, "scip-java");
    assert_eq!(entry["lastImported"]["state"], "failed");
    assert_eq!(entry["lastImported"]["errorCode"], code);
    assert_eq!(
        fs::read_dir(state_dir(&checkout).join("pickups"))
            .unwrap()
            .count(),
        0
    );

    // A pickup that fails before its import is queued is typed too, and nothing is published.
    let missing = checkout.temp.path().join("missing.scip");
    let output = pickup(
        &checkout,
        &tripwire,
        &[
            "--producer",
            "scip-java",
            "--artifact",
            missing.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    let reply = json_out(&output);
    assert!(
        reply["errorCode"]
            .as_str()
            .unwrap()
            .starts_with("producer_output_missing"),
        "{reply}"
    );
}

#[test]
fn pickup_rejects_path_inside_topology() {
    let checkout = start_checkout(workspace, &[]);
    let (tripwire, _) = tripwire_dir(checkout.temp.path());
    let artifact = java_artifact(&checkout, "index.scip");
    let cache = cache_dir(&checkout.home);
    fs::create_dir_all(&cache).unwrap();
    let inside = cache.join("index.scip");
    fs::copy(&artifact, &inside).unwrap();
    for (path, why) in [
        (inside.display().to_string(), "inside the cache directory"),
        (
            checkout
                .temp
                .path()
                .join("no-such-dir/index.scip")
                .display()
                .to_string(),
            "parent directory does not exist",
        ),
        (
            format!("{}/", checkout.temp.path().display()),
            "a directory spelling",
        ),
    ] {
        let output = pickup(
            &checkout,
            &tripwire,
            &["--producer", "scip-java", "--artifact", &path],
        );
        assert!(!output.status.success(), "{why} was accepted");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("invalid_request"), "{why}: {stderr}");
    }
    assert_eq!(pickup_status(&checkout, "scip-java"), Value::Null);
    assert!(!state_dir(&checkout).join("pickup.json").exists());
}

#[test]
fn pickup_clear_removes_entry() {
    let checkout = start_checkout(workspace, &[]);
    let (tripwire, _) = tripwire_dir(checkout.temp.path());
    let artifact = java_artifact(&checkout, "index.scip");
    let set = pickup(
        &checkout,
        &tripwire,
        &[
            "--producer",
            "scip-java",
            "--artifact",
            artifact.to_str().unwrap(),
        ],
    );
    crate::child::assert_cli_success(&set, "set");
    assert_ne!(pickup_status(&checkout, "scip-java"), Value::Null);
    assert!(state_dir(&checkout).join("pickup.json").exists());

    let cleared = pickup(
        &checkout,
        &tripwire,
        &["--producer", "scip-java", "--clear"],
    );
    crate::child::assert_cli_success(&cleared, "cleared");
    assert_eq!(json_out(&cleared)["cleared"], true);
    assert_eq!(pickup_status(&checkout, "scip-java"), Value::Null);
    assert!(!state_dir(&checkout).join("pickup.json").exists());
    let again = pickup(
        &checkout,
        &tripwire,
        &["--producer", "scip-java", "--clear"],
    );
    crate::child::assert_cli_success(&again, "again");
    assert_eq!(json_out(&again)["cleared"], false);

    // Neither action alone nor both together is a command.
    for args in [
        &["--producer", "scip-java"][..],
        &["--producer", "scip-java", "--clear", "--artifact", "/x"][..],
    ] {
        assert!(!pickup(&checkout, &tripwire, args).status.success());
    }
}

#[test]
fn pickup_config_survives_daemon_restart() {
    let checkout = start_checkout(workspace, &[]);
    let (tripwire, _) = tripwire_dir(checkout.temp.path());
    let artifact = java_artifact(&checkout, "index.scip");
    let set = pickup(
        &checkout,
        &tripwire,
        &[
            "--producer",
            "scip-java",
            "--artifact",
            artifact.to_str().unwrap(),
        ],
    );
    crate::child::assert_cli_success(&set, "set");
    let before = pickup_status(&checkout, "scip-java");
    assert_eq!(before["lastImported"]["state"], "done");

    // A restarted daemon refuses reads while startup reconciliation runs, and a loaded host can
    // keep it there past a read's own wait (#736). Read only once the checkout has settled.
    stop_daemon(&checkout.home);
    settle_through_held_startup(&checkout);
    assert_eq!(pickup_status(&checkout, "scip-java"), before);

    let cleared = pickup(
        &checkout,
        &tripwire,
        &["--producer", "scip-java", "--clear"],
    );
    crate::child::assert_cli_success(&cleared, "cleared");
    stop_daemon(&checkout.home);
    crate::child::settled(checkout.cli(), &checkout.root, None);
    assert_eq!(pickup_status(&checkout, "scip-java"), Value::Null);
}

/// Restart the stopped daemon through a settled wait, holding its startup reconciliation where
/// reads are refused (after it latched them, before it publishes) until the wait is under way.
/// A loaded host delays startup there; the settled wait returns only once startup has committed.
fn settle_through_held_startup(checkout: &Checkout) -> Value {
    use std::io::{Read, Write};
    let gate = tempfile::Builder::new()
        .prefix("startup-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = gate.path().join("latched.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut command = checkout.cli();
    command.env("TRELLIS_TEST_STARTUP_LATCHED_SOCKET", &socket);
    let root = checkout.root.clone();
    let settling = std::thread::spawn(move || crate::child::settled(command, &root, None));
    let mut held = None;
    trellis::testing::wait_until("the restarted startup to reach its latched hold", || {
        held = listener.accept().ok().map(|(stream, _)| stream);
        held.is_some() || settling.is_finished()
    });
    let Some(mut held) = held else {
        let settled = settling.join().unwrap();
        panic!("the settled wait ended before startup reached its hold: {settled}");
    };
    held.set_nonblocking(false).unwrap();
    held.read_exact(&mut [0]).unwrap();
    assert!(
        !settling.is_finished(),
        "the settled wait returned while startup reconciliation was held"
    );
    held.write_all(&[1]).unwrap();
    drop(listener);
    settling.join().unwrap()
}

#[test]
fn lost_pickup_wait_keeps_the_batch_until_the_request_settles() {
    let mut env = quiet_watch();
    env.push((
        "TRELLIS_TEST_PRODUCER_FAIL_WAIT".to_owned(),
        "unused".to_owned(),
    ));
    let checkout = start_checkout(workspace, &env);
    let (tripwire, _marker) = tripwire_dir(checkout.temp.path());
    let artifact = java_artifact(&checkout, "index.scip");
    let before = status(&checkout)["head"]["indexRevision"].as_u64().unwrap();
    let output = pickup(
        &checkout,
        &tripwire,
        &[
            "--producer",
            "scip-java",
            "--artifact",
            artifact.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("storage_busy"), "{stderr}");

    // The accepted request still publishes, and its inputs are still there.
    trellis::testing::wait_until("import published", || {
        status(&checkout)["head"]["indexRevision"].as_u64().unwrap() != before
    });
    let pickups = state_dir(&checkout).join("pickups");
    let dirs: Vec<_> = fs::read_dir(&pickups)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(dirs.len(), 1, "{dirs:?}");
    assert!(dirs[0].join("out").exists(), "{dirs:?}");
    assert!(dirs[0].join("batch").exists(), "{dirs:?}");

    // With the default retry interval nothing has reconciled the lost wait yet.
    let entry = pickup_status(&checkout, "scip-java");
    assert_eq!(entry["lastImported"]["state"], "failed");
    assert_eq!(entry["lastImported"]["errorCode"], "storage_busy");
    assert!(entry["lastImported"]["requestId"].is_string());
}

/// V1: a second identical `--artifact` pickup at an unchanged head is an idempotent success,
/// and the published evidence's status stays `done`.
#[test]
fn duplicate_pickup_at_same_head_is_idempotent() {
    let checkout = start_checkout(workspace, &quiet_watch());
    let (tripwire, _) = tripwire_dir(checkout.temp.path());
    let artifact = java_artifact(&checkout, "index.scip");
    let args = [
        "--producer",
        "scip-java",
        "--artifact",
        artifact.to_str().unwrap(),
    ];
    let first = pickup(&checkout, &tripwire, &args);
    crate::child::assert_cli_success(&first, "first");
    let first = json_out(&first);
    assert_eq!(first["state"], "done");
    let head = status(&checkout)["head"]["indexRevision"].as_u64().unwrap();

    let second = pickup(&checkout, &tripwire, &args);
    let reply = json_out(&second);
    crate::child::assert_cli_success(&second, format_args!("{reply}"));
    assert_eq!(reply["state"], "done", "{reply}");
    assert_eq!(reply["unchanged"], true, "{reply}");
    assert_eq!(reply["sha256"], first["sha256"]);
    assert_eq!(reply["errorCode"], Value::Null);
    assert_eq!(reply["requestId"], first["requestId"]);
    assert!(reply["publishedRevision"].is_object(), "{reply}");
    assert_eq!(reply["report"], first["report"]);
    let entry = pickup_status(&checkout, "scip-java");
    assert_eq!(entry["lastImported"]["state"], "done");
    assert_eq!(entry["lastImported"]["errorCode"], Value::Null);
    assert_eq!(
        status(&checkout)["head"]["indexRevision"].as_u64().unwrap(),
        head,
        "an idempotent pickup published a new revision"
    );
    assert_eq!(
        fs::read_dir(state_dir(&checkout).join("pickups"))
            .map(|dir| dir.count())
            .unwrap_or(0),
        0
    );

    // A real change still imports: different bytes at the same head publish a new revision.
    let fixture =
        BatchFixture::from_published(&index_db(&checkout), &checkout.root).reference("scip-java");
    let docs = fixture.paths("java");
    fs::write(&artifact, fixture.scip_bytes("java", &docs[..1])).unwrap();
    let changed = pickup(&checkout, &tripwire, &args);
    let changed_reply = json_out(&changed);
    crate::child::assert_cli_success(&changed, format_args!("{changed_reply}"));
    assert_eq!(changed_reply["state"], "done");
    assert_eq!(changed_reply.get("unchanged"), None, "{changed_reply}");
    assert_ne!(changed_reply["sha256"], first["sha256"]);
    assert_ne!(changed_reply["requestId"], first["requestId"]);
    assert_ne!(
        status(&checkout)["head"]["indexRevision"].as_u64().unwrap(),
        head
    );
}
