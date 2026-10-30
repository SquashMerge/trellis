//! `trellis producer pickup`: an owner-configured artifact is imported through the unchanged
//! #61 path without running a producer, replies with the typed import outcome, survives a
//! daemon restart and is removed by `--clear`.
#![cfg(feature = "test-support")]
use serde_json::Value;
use std::{fs, path::Path, process::Output};

#[path = "support/producer.rs"]
mod support;
use support::{
    Checkout, cache_dir, start_checkout, state_dir, stop_daemon, tripwire_dir,
    workspace_tree_digest,
};

#[path = "support/semantic_batch.rs"]
mod semantic_batch_support;
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
    let output = checkout
        .cli()
        .args(["producer", "status", "--workspace"])
        .arg(&checkout.root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
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
    let checkout = start_checkout(workspace, &[]);
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
    assert!(
        output.status.success(),
        "{reply} {}",
        String::from_utf8_lossy(&output.stderr)
    );
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
    let checkout = start_checkout(workspace, &[]);
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
    assert!(set.status.success());
    assert_ne!(pickup_status(&checkout, "scip-java"), Value::Null);
    assert!(state_dir(&checkout).join("pickup.json").exists());

    let cleared = pickup(
        &checkout,
        &tripwire,
        &["--producer", "scip-java", "--clear"],
    );
    assert!(cleared.status.success());
    assert_eq!(json_out(&cleared)["cleared"], true);
    assert_eq!(pickup_status(&checkout, "scip-java"), Value::Null);
    assert!(!state_dir(&checkout).join("pickup.json").exists());
    let again = pickup(
        &checkout,
        &tripwire,
        &["--producer", "scip-java", "--clear"],
    );
    assert!(again.status.success());
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
    assert!(set.status.success());
    let before = pickup_status(&checkout, "scip-java");
    assert_eq!(before["lastImported"]["state"], "done");

    stop_daemon(&checkout.home);
    assert_eq!(pickup_status(&checkout, "scip-java"), before);

    let cleared = pickup(
        &checkout,
        &tripwire,
        &["--producer", "scip-java", "--clear"],
    );
    assert!(cleared.status.success());
    stop_daemon(&checkout.home);
    assert_eq!(pickup_status(&checkout, "scip-java"), Value::Null);
}

#[test]
fn lost_pickup_wait_keeps_the_batch_until_the_request_settles() {
    let checkout = start_checkout(
        workspace,
        &[(
            "TRELLIS_TEST_PRODUCER_FAIL_WAIT".to_owned(),
            "unused".to_owned(),
        )],
    );
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
