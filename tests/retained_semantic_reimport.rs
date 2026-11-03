//! Real generated-run retention and automatic upgrade recovery. No manual import after restart.
#![cfg(feature = "test-support")]
#[path = "support/semantic_batch.rs"]
mod semantic_batch_support;
#[path = "support/producer.rs"]
mod support;

use rusqlite::{Connection, OpenFlags};
use semantic_batch_support::{BatchFixture, edit_scip, uniform_workspace};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Output, Stdio},
};
use support::{Checkout, cache_dir, data_dir, start_checkout, state_dir};
use trellis::store::topology::{TopologyRoots, WorkspaceIdentity};

const PRODUCER: &str = "trellis-synthetic-scip";

struct World {
    checkout: Checkout,
    _stub: tempfile::TempDir,
    marker: PathBuf,
    index: PathBuf,
    requests: PathBuf,
}

fn reply(operation: &str, output: Output) -> Value {
    assert!(
        output.status.success(),
        "{operation} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

impl World {
    fn new() -> Self {
        Self::with_hook(None)
    }

    fn with_hook(hook: Option<&Path>) -> Self {
        let stub = tempfile::tempdir().unwrap();
        let marker = stub.path().join("invocations");
        let mut env = support::stub_env(stub.path(), &json!({"marker":marker}));
        if let Some(hook) = hook {
            env.push((
                "TRELLIS_TEST_SEMANTIC_CRASH_AFTER_PUBLISH_SOCKET".into(),
                hook.to_string_lossy().into_owned(),
            ));
        }
        let checkout = start_checkout(|root| uniform_workspace(root, &["javascript"], 1), &env);
        let roots =
            TopologyRoots::isolated_for_tests(cache_dir(&checkout.home), data_dir(&checkout.home));
        let identity = WorkspaceIdentity::discover(Some(&checkout.root), &checkout.root).unwrap();
        let index = roots.index_db(&identity);
        let requests = roots.requests_db(&identity);
        Self {
            checkout,
            _stub: stub,
            marker,
            index,
            requests,
        }
    }

    fn spawn_run(&self) -> Child {
        let mut fixture =
            BatchFixture::from_published(&self.index, &self.checkout.root).synthetic();
        let docs = fixture.paths("javascript");
        fixture.artifact("javascript", docs.clone());
        let bytes = edit_scip(&fixture.scip_bytes("javascript", &docs), |index| {
            index.metadata.as_mut().unwrap().project_root = String::new();
        });
        let artifact = self._stub.path().join("index.scip");
        fs::write(&artifact, bytes).unwrap();
        support::write_mode(
            self._stub.path(),
            "mode.json",
            &json!({"marker":self.marker,"emit":artifact}),
        );
        let plan = reply(
            "producer plan",
            self.checkout
                .cli()
                .args(["producer", "plan", "--workspace"])
                .arg(&self.checkout.root)
                .args(["--producer", PRODUCER])
                .output()
                .unwrap(),
        );
        self.checkout
            .cli()
            .args(["producer", "run", "--workspace"])
            .arg(&self.checkout.root)
            .args([
                "--producer",
                PRODUCER,
                "--plan-sha256",
                plan["planSha256"].as_str().unwrap(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn run_once(&self) -> (Value, Vec<u8>, Value) {
        let run = reply("producer run", self.spawn_run().wait_with_output().unwrap());
        assert_eq!(run["outcome"], "imported");
        let run_id = run["runId"].as_str().unwrap();
        let descriptor = state_dir(&self.checkout)
            .join("runs")
            .join(run_id)
            .join("batch/descriptor.json");
        let original = fs::read(&descriptor).unwrap();
        let original_json: Value = serde_json::from_slice(&original).unwrap();
        assert_eq!(original_json["generation"]["origin"], "trellisRun");
        let request = run["import"]["requestId"].as_str().unwrap();
        let db =
            Connection::open_with_flags(&self.requests, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let options: String = db
            .query_row(
                "SELECT options_json FROM requests WHERE id=?1",
                [request],
                |row| row.get(0),
            )
            .unwrap();
        (run, original, serde_json::from_str(&options).unwrap())
    }

    fn status(&self) -> Value {
        reply(
            "status",
            self.checkout
                .cli()
                .args(["status", "--workspace"])
                .arg(&self.checkout.root)
                .output()
                .unwrap(),
        )
    }

    fn upgrade(&self, edit: impl FnOnce(&Path)) {
        support::stop_daemon(&self.checkout.home);
        edit(&self.checkout.root);
        let db = Connection::open(&self.index).unwrap();
        let version: u32 = db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 35);
        db.pragma_update(None, "user_version", version - 1).unwrap();
        drop(db);
        // The real daemon establishes the new native head and owns retained FIFO work.
        reply(
            "index after upgrade",
            self.checkout
                .cli()
                .args(["index", "--workspace"])
                .arg(&self.checkout.root)
                .output()
                .unwrap(),
        );
    }

    fn generated_rows(&self) -> Vec<(String, String, Value)> {
        let db =
            Connection::open_with_flags(&self.requests, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        db.prepare("SELECT state,COALESCE(error_code,''),options_json FROM requests WHERE request_kind='semanticImport' ORDER BY seq")
            .unwrap().query_map([], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?)))
            .unwrap().map(|row| { let (state,error,options) = row.unwrap(); (state,error,serde_json::from_str(&options).unwrap()) }).collect()
    }

    fn invocation_count(&self) -> usize {
        fs::read_to_string(&self.marker).map_or(0, |text| text.lines().count())
    }
}

#[test]
fn upgrade_automatically_readmits_original_generated_run_without_rerunning_producer() {
    let world = World::new();
    let (run, original, options) = world.run_once();
    let old = world.status()["revision"].clone();
    world.upgrade(|_| {});
    trellis::testing::wait_until("retained run published by worker FIFO", || {
        let rows = world.generated_rows();
        rows.len() == 2 && matches!(rows[1].0.as_str(), "done" | "failed")
    });
    let rows = world.generated_rows();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].0, "done", "automatic re-import refused: {rows:?}");
    assert_eq!(
        rows[1].2, options,
        "recovery must not rewrite descriptor identity or paths"
    );
    let status = world.status();
    assert_ne!(
        status["revision"]["indexGeneration"],
        old["indexGeneration"]
    );
    let js = status["semanticLanguages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["language"] == "javascript")
        .unwrap();
    assert_eq!(js["reimport"], Value::Null);
    assert_eq!(js["semanticProducerId"], PRODUCER);
    assert_eq!(js["semanticState"], "fresh");
    let descriptor = Path::new(options["descriptor"].as_str().unwrap());
    assert_eq!(fs::read(descriptor).unwrap(), original);
    assert!(
        descriptor
            .to_string_lossy()
            .contains(run["runId"].as_str().unwrap())
    );
    let db = Connection::open_with_flags(&world.index, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let retained: Vec<u8> = db
        .query_row(
            "SELECT descriptor_bytes FROM semantic_raw_batches",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, original);
    assert_eq!(world.invocation_count(), 1);
}

#[test]
fn upgraded_changed_sources_keep_syntax_and_report_reimport_without_rerunning_producer() {
    let world = World::new();
    let (_, original, options) = world.run_once();
    world.upgrade(|root| {
        fs::write(
            root.join("javascript/u00000.js"),
            "function changed() { other(); }\n",
        )
        .unwrap()
    });
    trellis::testing::wait_until("retained mismatch reaches terminal refusal", || {
        let rows = world.generated_rows();
        rows.len() == 2 && matches!(rows[1].0.as_str(), "done" | "failed")
    });
    let rows = world.generated_rows();
    assert_eq!(
        rows[1].0, "failed",
        "mismatched input was published: {rows:?}"
    );
    assert!(!rows[1].1.is_empty());
    let status = world.status();
    assert!(status["stats"]["symbols"].as_u64().unwrap() > 0);
    let js = status["semanticLanguages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["language"] == "javascript")
        .unwrap();
    assert_eq!(js["semanticState"], "unavailable");
    assert_eq!(js["semanticProducerId"], Value::Null);
    assert_eq!(js["reimport"]["state"], "needsReimport");
    assert!(
        js["reimport"]["reason"]
            .as_str()
            .unwrap()
            .contains("no longer matches")
    );
    assert_eq!(world.generated_rows()[1].2, options);
    assert_eq!(
        fs::read(options["descriptor"].as_str().unwrap()).unwrap(),
        original
    );
    assert_eq!(world.invocation_count(), 1);
}

#[test]
fn first_upgrade_reports_known_native_language_but_clean_start_has_no_false_reimport() {
    let world = World::new();
    assert!(world.status().get("semanticLanguages").is_none());
    world.upgrade(|_| {});
    let status = world.status();
    let rows = status["semanticLanguages"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["language"], "javascript");
    assert_eq!(rows[0]["semanticState"], "unavailable");
    assert_eq!(rows[0]["reimport"]["state"], "needsReimport");
    assert!(
        rows[0]["reimport"]["reason"]
            .as_str()
            .unwrap()
            .contains("not retained before this upgrade")
    );
    assert!(world.generated_rows().is_empty());
    assert_eq!(world.invocation_count(), 0);
}

#[test]
fn crash_after_generated_publication_before_ack_keeps_original_inputs_for_upgrade() {
    let marker = tempfile::tempdir().unwrap();
    let socket = marker.path().join("published.sock");
    let world = World::with_hook(Some(&socket));
    let mut run = world.spawn_run();
    trellis::testing::wait_until("generated publication before terminal ACK", || {
        socket.exists()
    });
    let rows = world.generated_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "running");
    let options = rows[0].2.clone();
    let descriptor = Path::new(options["descriptor"].as_str().unwrap());
    let original = fs::read(descriptor).unwrap();
    let db = Connection::open_with_flags(&world.index, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let admitted: Vec<u8> = db
        .query_row(
            "SELECT descriptor_bytes FROM semantic_raw_batches",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(admitted, original, "publication committed before the pause");
    drop(db);
    let daemon = world.checkout.home.join("daemon.pid");
    let pid: i32 = fs::read_to_string(&daemon).unwrap().parse().unwrap();
    assert!(trellis::testing::signal_daemon_marker(&daemon, libc::SIGKILL).unwrap());
    trellis::testing::wait_until("daemon crash", || !trellis::testing::process_running(pid));
    let _ = run.wait();
    fs::write(socket.with_extension("done"), []).unwrap();
    fs::remove_file(&socket).unwrap();
    // Rebuild before restart so neither a terminal request nor run-record ACK can be
    // mistaken for the authority that retained this last successfully published input.
    let db = Connection::open(&world.index).unwrap();
    db.pragma_update(None, "user_version", 34).unwrap();
    drop(db);
    reply(
        "index after publication crash",
        world
            .checkout
            .cli()
            .args(["index", "--workspace"])
            .arg(&world.checkout.root)
            .output()
            .unwrap(),
    );
    trellis::testing::wait_until("crashed generated import settles", || {
        let rows = world.generated_rows();
        rows.iter()
            .all(|row| matches!(row.0.as_str(), "done" | "failed"))
    });
    let rows = world.generated_rows();
    assert_eq!(
        rows[0].0, "done",
        "crashed request did not recover: {rows:?}"
    );
    assert!(rows.iter().all(|row| row.2 == options));
    let status = world.status();
    let js = status["semanticLanguages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["language"] == "javascript")
        .unwrap();
    assert_eq!(js["semanticState"], "fresh");
    assert_eq!(js["reimport"], Value::Null);
    assert_eq!(fs::read(descriptor).unwrap(), original);
    assert_eq!(
        world.invocation_count(),
        1,
        "recovery must not execute a producer"
    );
}

#[test]
fn missing_retained_bundle_leaves_native_available_and_reports_reimport() {
    let world = World::new();
    let (_, _, options) = world.run_once();
    let descriptor = PathBuf::from(options["descriptor"].as_str().unwrap());
    world.upgrade(|_| fs::remove_file(&descriptor).unwrap());
    let rows = world.generated_rows();
    assert_eq!(
        rows.len(),
        1,
        "missing input must be refused before queue acceptance: {rows:?}"
    );
    assert_eq!(rows[0].0, "done");
    let status = world.status();
    assert!(status["stats"]["symbols"].as_u64().unwrap() > 0);
    let js = status["semanticLanguages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["language"] == "javascript")
        .unwrap();
    assert_eq!(js["semanticState"], "unavailable");
    assert_eq!(js["reimport"]["state"], "needsReimport");
    assert!(
        js["reimport"]["reason"]
            .as_str()
            .unwrap()
            .contains("missing")
    );
    reply(
        "second index with missing retained bundle",
        world
            .checkout
            .cli()
            .args(["index", "--workspace"])
            .arg(&world.checkout.root)
            .output()
            .unwrap(),
    );
    assert_eq!(world.generated_rows().len(), 1);
    assert_eq!(world.invocation_count(), 1);
}

#[test]
fn corrupt_retention_catalog_does_not_block_native_index_fifo() {
    let world = World::new();
    let (_, original, options) = world.run_once();
    let retention = state_dir(&world.checkout).join("retention.sqlite");
    world.upgrade(|_| fs::write(&retention, b"not a SQLite database").unwrap());
    let status = world.status();
    assert!(status["stats"]["symbols"].as_u64().unwrap() > 0);
    let js = status["semanticLanguages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["language"] == "javascript")
        .unwrap();
    assert_eq!(js["semanticState"], "unavailable");
    assert_eq!(js["reimport"]["state"], "needsReimport");
    assert!(!js["reimport"]["reason"].as_str().unwrap().is_empty());
    assert_eq!(
        fs::read(options["descriptor"].as_str().unwrap()).unwrap(),
        original,
        "bad retention state must not delete unenumerated originals"
    );
    // A second explicit native request must still drain with the bad optional catalog.
    reply(
        "second index with corrupt retention catalog",
        world
            .checkout
            .cli()
            .args(["index", "--workspace"])
            .arg(&world.checkout.root)
            .output()
            .unwrap(),
    );
    assert_eq!(world.invocation_count(), 1);
}
