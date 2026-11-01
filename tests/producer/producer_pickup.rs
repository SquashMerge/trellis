//! Automatic pickup (D6): a regenerated artifact is re-imported on a watch event, the
//! activation check or the stat check, without running a producer, following the decision
//! table. Small trigger intervals come from `TRELLIS_TEST_PICKUP_INTERVALS`; negative
//! assertions wait several of those intervals, and every one is paired with a positive
//! control showing the watch was alive.
#![cfg(feature = "test-support")]
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::Duration,
};
use tempfile::{Builder, TempDir};

use crate::support;
use support::{
    Checkout, cache_dir, data_dir, pickup_entry, request_rows, start_checkout, stop_daemon,
    tripwire_dir,
};

use crate::semantic_batch_support;
use semantic_batch_support::{BatchFixture, edit_scip, sha256, uniform_workspace};

const LANGUAGES: [&str; 4] = ["java", "rust", "python", "javascript"];
/// Many multiples of the 100 ms and 200 ms intervals the tests configure.
const SETTLE: Duration = Duration::from_millis(1500);

fn workspace(root: &Path) {
    uniform_workspace(root, &LANGUAGES, 2);
}

fn intervals(value: Value) -> Vec<(String, String)> {
    vec![("TRELLIS_TEST_PICKUP_INTERVALS".into(), value.to_string())]
}

fn fast() -> Vec<(String, String)> {
    intervals(json!({"debounceMs": 100, "stableMs": 100}))
}

fn index_db(checkout: &Checkout) -> PathBuf {
    use trellis::store::topology::{TopologyRoots, WorkspaceIdentity};
    let roots =
        TopologyRoots::isolated_for_tests(cache_dir(&checkout.home), data_dir(&checkout.home));
    let identity = WorkspaceIdentity::discover(Some(&checkout.root), &checkout.root).unwrap();
    roots.index_db(&identity)
}

/// A valid `scip-java` artifact over the first `documents` java documents of the checkout
/// (all of them without a limit). Different counts give different valid bytes.
fn java_bytes(checkout: &Checkout, documents: Option<usize>) -> Vec<u8> {
    let fixture =
        BatchFixture::from_published(&index_db(checkout), &checkout.root).reference("scip-java");
    let docs = fixture.paths("java");
    let take = documents.unwrap_or(docs.len());
    fixture.scip_bytes("java", &docs[..take])
}

fn set_command(checkout: &Checkout, tripwire: &Path, artifact: &Path) -> Command {
    let mut command = checkout.cli();
    command
        .env("PATH", format!("{}:/usr/bin:/bin", tripwire.display()))
        .args(["producer", "pickup", "--workspace"])
        .arg(&checkout.root)
        .args(["--producer", "scip-java", "--artifact"])
        .arg(artifact);
    command
}

fn set(checkout: &Checkout, tripwire: &Path, artifact: &Path) -> Output {
    set_command(checkout, tripwire, artifact).output().unwrap()
}

fn clear(checkout: &Checkout) -> Output {
    checkout
        .cli()
        .args(["producer", "pickup", "--workspace"])
        .arg(&checkout.root)
        .args(["--producer", "scip-java", "--clear"])
        .output()
        .unwrap()
}

fn entry(checkout: &Checkout) -> Value {
    pickup_entry(checkout, "scip-java")
}

fn last(checkout: &Checkout) -> Value {
    entry(checkout)["lastImported"].clone()
}

/// Wait until the entry's last outcome satisfies `ready`, returning it.
fn wait_last(checkout: &Checkout, what: &str, ready: impl Fn(&Value) -> bool) -> Value {
    let mut seen = Value::Null;
    trellis::testing::wait_until(what, || {
        seen = last(checkout);
        ready(&seen)
    });
    seen
}

/// An artifact written under the checkout's temporary directory and imported once by `set`.
struct Configured {
    checkout: Checkout,
    tripwire: PathBuf,
    marker: PathBuf,
    artifact: PathBuf,
    first: Value,
}

fn configured(daemon_env: &[(String, String)]) -> Configured {
    let checkout = start_checkout(workspace, daemon_env);
    let (tripwire, marker) = tripwire_dir(checkout.temp.path());
    let artifact = checkout.temp.path().join("index.scip");
    fs::write(&artifact, java_bytes(&checkout, None)).unwrap();
    let output = set(&checkout, &tripwire, &artifact);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let first = last(&checkout);
    assert_eq!(first["state"], "done");
    Configured {
        checkout,
        tripwire,
        marker,
        artifact,
        first,
    }
}

#[test]
fn pickup_reimports_on_watch_event() {
    let world = configured(&fast());
    let Configured {
        checkout,
        marker,
        artifact,
        first,
        ..
    } = &world;
    assert_eq!(entry(checkout)["watch"], "active");
    let rows = request_rows(checkout).len();

    let regenerated = java_bytes(checkout, Some(1));
    let digest = sha256(&regenerated);
    assert_ne!(digest, first["sha256"].as_str().unwrap());
    fs::write(artifact, &regenerated).unwrap();

    let imported = wait_last(checkout, "the regenerated artifact to import", |last| {
        last["sha256"] == digest.as_str() && last["state"] == "done"
    });
    assert_ne!(imported["requestId"], first["requestId"]);
    assert_eq!(request_rows(checkout).len(), rows + 1);
    assert!(!marker.exists(), "a pickup watch event ran a producer");
}

#[test]
fn pickup_skips_unchanged_artifact_after_done() {
    let world = configured(&fast());
    let Configured {
        checkout,
        artifact,
        first,
        ..
    } = &world;
    let rows = request_rows(checkout).len();

    // The same bytes with a new mtime: the watch sees a change, the decision table skips it.
    let same = fs::read(artifact).unwrap();
    fs::write(artifact, &same).unwrap();
    std::thread::sleep(SETTLE);
    assert_eq!(
        request_rows(checkout).len(),
        rows,
        "unchanged bytes imported"
    );
    assert_eq!(last(checkout), *first);

    // Positive control: the watch is alive and imports different bytes.
    let regenerated = java_bytes(checkout, Some(1));
    let digest = sha256(&regenerated);
    fs::write(artifact, &regenerated).unwrap();
    wait_last(checkout, "the regenerated artifact to import", |last| {
        last["sha256"] == digest.as_str() && last["state"] == "done"
    });
    assert_eq!(request_rows(checkout).len(), rows + 1);
}

#[test]
fn pickup_retries_after_stale_basis() {
    let sockets = Builder::new()
        .prefix("pickup-stale-")
        .tempdir_in("/tmp")
        .unwrap();
    let staging = sockets.path().join("stage.sock");
    let mut env = intervals(json!({
        "debounceMs": 100, "stableMs": 100, "checkMs": 200, "retryMs": 200
    }));
    env.push((
        "TRELLIS_TEST_SEMANTIC_STAGING_SOCKET".into(),
        staging.display().to_string(),
    ));
    let checkout = start_checkout(workspace, &env);
    let (tripwire, marker) = tripwire_dir(checkout.temp.path());
    let artifact = checkout.temp.path().join("index.scip");
    fs::write(&artifact, java_bytes(&checkout, None)).unwrap();

    // The set import pauses while staged; a source edit moves the head under it.
    let child = set_command(&checkout, &tripwire, &artifact)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    trellis::testing::wait_until("the import to stage", || {
        staging.with_extension("ready").exists()
    });
    fs::write(
        checkout.root.join("rust/u00000.rs"),
        "fn u00000() { moved(); }\n",
    )
    .unwrap();
    // Later imports must not pause: the marker is written before the first is released.
    fs::write(staging.with_extension("done"), []).unwrap();
    let mut stream = UnixStream::connect(&staging).unwrap();
    let _ = stream.read(&mut [0_u8; 1]);
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());

    // The reply is the stale outcome; polling status could miss it, because the retry
    // follows within a few hundred milliseconds.
    let stale: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stale["state"], "failed", "{stale}");
    assert!(
        stale["errorCode"]
            .as_str()
            .unwrap()
            .starts_with("stale_basis"),
        "{stale}"
    );
    // With no further event, the stat check re-imports the same bytes once the retry
    // interval has passed, under a new request.
    let retried = wait_last(&checkout, "the retry to import", |last| {
        last["state"] == "done"
    });
    assert_eq!(retried["sha256"], stale["sha256"]);
    assert_ne!(retried["requestId"], stale["requestId"]);
    assert!(!marker.exists());
}

#[test]
fn pickup_does_not_retry_deterministic_failure_until_head_changes() {
    let env = intervals(json!({
        "debounceMs": 100, "stableMs": 100, "checkMs": 200, "retryMs": 200
    }));
    let checkout = start_checkout(workspace, &env);
    let (tripwire, _) = tripwire_dir(checkout.temp.path());
    let bad = checkout.temp.path().join("bad.scip");
    fs::write(
        &bad,
        edit_scip(&java_bytes(&checkout, None), |index| {
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
    assert!(!set(&checkout, &tripwire, &bad).status.success());
    let failed = last(&checkout);
    assert!(
        failed["errorCode"]
            .as_str()
            .unwrap()
            .starts_with("invalid_artifact"),
        "{failed}"
    );
    let rows = request_rows(&checkout).len();

    // Many stat checks later, the same bytes against the same head were not tried again.
    std::thread::sleep(SETTLE);
    assert_eq!(request_rows(&checkout).len(), rows);
    assert_eq!(last(&checkout), failed);

    // A new head makes the failure worth another attempt.
    fs::write(
        checkout.root.join("rust/u00000.rs"),
        "fn u00000() { moved(); }\n",
    )
    .unwrap();
    let again = wait_last(&checkout, "the retry against the new head", |last| {
        last["sourceRevision"] != failed["sourceRevision"]
    });
    assert_eq!(again["sha256"], failed["sha256"]);
    assert_eq!(again["state"], "failed");
    assert!(request_rows(&checkout).len() > rows);
}

#[test]
fn pickup_activation_check_imports_changed_artifact() {
    let world = configured(&fast());
    let Configured {
        checkout,
        artifact,
        first,
        marker,
        ..
    } = &world;
    stop_daemon(&checkout.home);

    // Changed while no daemon watched: only the activation check can notice.
    let regenerated = java_bytes(checkout, Some(1));
    let digest = sha256(&regenerated);
    fs::write(artifact, &regenerated).unwrap();

    let mut status = checkout.cli();
    for (key, value) in fast() {
        status.env(key, value);
    }
    let output = status
        .args(["producer", "status", "--workspace"])
        .arg(&checkout.root)
        .output()
        .unwrap();
    assert!(output.status.success());
    let imported = wait_last(checkout, "the activation check to import", |last| {
        last["sha256"] == digest.as_str() && last["state"] == "done"
    });
    assert_ne!(imported["requestId"], first["requestId"]);
    assert!(!marker.exists());
}

#[test]
fn pickup_clear_stops_watch() {
    let world = configured(&fast());
    let Configured {
        checkout,
        tripwire,
        artifact,
        ..
    } = &world;
    assert_eq!(entry(checkout)["watch"], "active");
    assert!(clear(checkout).status.success());
    let rows = request_rows(checkout).len();

    let regenerated = java_bytes(checkout, Some(1));
    fs::write(artifact, &regenerated).unwrap();
    std::thread::sleep(SETTLE);
    assert_eq!(
        request_rows(checkout).len(),
        rows,
        "a cleared pickup imported"
    );
    assert_eq!(entry(checkout), Value::Null);

    // Positive control: configuring it again imports the new bytes at once and re-arms.
    assert!(set(checkout, tripwire, artifact).status.success());
    assert_eq!(request_rows(checkout).len(), rows + 1);
    assert_eq!(last(checkout)["sha256"], sha256(&regenerated).as_str());
    assert_eq!(entry(checkout)["watch"], "active");
}

#[test]
fn pickup_status_reports_degraded_watch_when_directory_is_missing() {
    let checkout = start_checkout(workspace, &fast());
    let (tripwire, _) = tripwire_dir(checkout.temp.path());
    let directory = checkout.temp.path().join("generated");
    fs::create_dir(&directory).unwrap();
    let artifact = directory.join("index.scip");
    fs::write(&artifact, java_bytes(&checkout, None)).unwrap();
    assert!(set(&checkout, &tripwire, &artifact).status.success());
    assert_eq!(entry(&checkout)["watch"], "active");

    stop_daemon(&checkout.home);
    fs::remove_dir_all(&directory).unwrap();
    let degraded = entry(&checkout);
    assert_eq!(degraded["watch"], "degraded", "{degraded}");
    assert_eq!(degraded["lastImported"]["state"], "done");
}

/// Daemon environment whose pickup wait is lost right after the queue accepts the import.
fn lost_wait(extra: Option<(String, String)>) -> Vec<(String, String)> {
    let mut env = intervals(json!({
        "debounceMs": 100, "stableMs": 100, "checkMs": 200, "retryMs": 200
    }));
    env.push(("TRELLIS_TEST_PRODUCER_FAIL_WAIT".into(), "unused".into()));
    env.extend(extra);
    env
}

#[test]
fn lost_wait_reconciles_with_the_accepted_request_instead_of_importing_again() {
    let checkout = start_checkout(workspace, &lost_wait(None));
    let (tripwire, _marker) = tripwire_dir(checkout.temp.path());
    let artifact = checkout.temp.path().join("index.scip");
    fs::write(&artifact, java_bytes(&checkout, None)).unwrap();
    let output = set(&checkout, &tripwire, &artifact);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("storage_busy"));
    let accepted = last(&checkout)["requestId"].clone();
    assert!(accepted.is_string());
    let rows = request_rows(&checkout).len();

    let settled = wait_last(&checkout, "the lost wait to reconcile as done", |last| {
        last["state"] == "done"
    });
    assert_eq!(settled["requestId"], accepted);
    assert_eq!(settled["errorCode"], Value::Null);

    // Several retry intervals later no second import has been queued or failed.
    std::thread::sleep(SETTLE);
    let all = request_rows(&checkout);
    assert_eq!(all.len(), rows, "{all:?}");
    assert!(all.iter().all(|row| row.1 != "failed"), "{all:?}");
    assert_eq!(last(&checkout)["requestId"], accepted);
}

#[test]
fn pending_request_past_the_retry_interval_starts_no_second_import() {
    let sockets = Builder::new()
        .prefix("pickup-pending-")
        .tempdir_in("/tmp")
        .unwrap();
    let staging = sockets.path().join("stage.sock");
    let env = lost_wait(Some((
        "TRELLIS_TEST_SEMANTIC_STAGING_SOCKET".into(),
        staging.display().to_string(),
    )));
    let checkout = start_checkout(workspace, &env);
    let (tripwire, _marker) = tripwire_dir(checkout.temp.path());
    let artifact = checkout.temp.path().join("index.scip");
    fs::write(&artifact, java_bytes(&checkout, None)).unwrap();

    // The accepted request pauses while staged; the pickup's wait is lost meanwhile.
    let output = set(&checkout, &tripwire, &artifact);
    assert!(!output.status.success());
    trellis::testing::wait_until("the import to stage", || {
        staging.with_extension("ready").exists()
    });
    let accepted = last(&checkout)["requestId"].clone();
    let rows = request_rows(&checkout).len();

    std::thread::sleep(SETTLE);
    assert_eq!(request_rows(&checkout).len(), rows, "a second import began");
    assert_eq!(last(&checkout)["requestId"], accepted);

    // Regenerated bytes during the hold are not judged and start nothing yet.
    let regenerated = java_bytes(&checkout, Some(1));
    assert_ne!(regenerated, fs::read(&artifact).unwrap());
    fs::write(&artifact, &regenerated).unwrap();
    std::thread::sleep(SETTLE);
    assert_eq!(request_rows(&checkout).len(), rows, "a second import began");
    assert_eq!(last(&checkout)["requestId"], accepted);

    // Released, the held request settles and the regenerated bytes are then imported.
    fs::write(staging.with_extension("done"), []).unwrap();
    let mut stream = UnixStream::connect(&staging).unwrap();
    let _ = stream.read(&mut [0_u8; 1]);
    let sha = sha256(&regenerated);
    let settled = wait_last(&checkout, "the regenerated bytes to be imported", |last| {
        last["state"] == "done" && last["sha256"] == sha.as_str()
    });
    assert_ne!(settled["requestId"], accepted);
    assert!(
        request_rows(&checkout).iter().all(|row| row.1 != "failed"),
        "a request failed"
    );
}

/// V7: a `done` outcome does not outlive the evidence. After a rebuild drops the index, the
/// activation check re-imports the unchanged artifact instead of skipping it.
#[test]
fn pickup_reimports_unchanged_artifact_after_rebuild() {
    let world = configured(&fast());
    let Configured {
        checkout,
        first,
        marker,
        ..
    } = &world;
    stop_daemon(&checkout.home);

    // Rebuild: the disposable index goes; the producer state with `pickup.json` stays.
    let db = index_db(checkout);
    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{suffix}", db.display()));
        if path.exists() {
            fs::remove_file(&path).unwrap();
        }
    }

    let mut status = checkout.cli();
    for (key, value) in fast() {
        status.env(key, value);
    }
    let output = status
        .args(["producer", "status", "--workspace"])
        .arg(&checkout.root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let imported = wait_last(checkout, "the rebuilt index to re-import", |last| {
        last["requestId"] != first["requestId"] && last["state"] == "done"
    });
    assert_eq!(imported["sha256"], first["sha256"]);
    assert!(!marker.exists());
}

/// Holds the daemon's next repair of a corrupt head before it can publish, so a test can
/// order that repair against a read. The stat check and a read can each detect the corruption.
struct RecoveryGate {
    _dir: TempDir,
    socket: PathBuf,
}

impl RecoveryGate {
    fn new() -> Self {
        let dir = Builder::new()
            .prefix("recovery-")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = dir.path().join("recovery.sock");
        Self { _dir: dir, socket }
    }

    /// A configured checkout whose stat check runs every 100 ms.
    fn configured(&self) -> Configured {
        let mut env = intervals(json!({"debounceMs": 100, "stableMs": 100, "checkMs": 100}));
        env.push((
            "TRELLIS_TEST_LIVE_RECOVERY_SOCKET".into(),
            self.socket.display().to_string(),
        ));
        configured(&env)
    }

    /// Wait for the recovery to reach the gate, then let it publish.
    fn release(&self) {
        let ready = self.socket.with_extension("ready");
        trellis::testing::wait_until("the live recovery to reach its gate", || ready.exists());
        drop(UnixStream::connect(&self.socket).unwrap());
    }
}

/// The published pin, as a read reports it.
fn head(checkout: &Checkout) -> Value {
    let db = rusqlite::Connection::open_with_flags(
        index_db(checkout),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let (generation, revision): (String, i64) = db
        .query_row(
            "SELECT index_generation,index_revision FROM index_metadata",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    json!({"indexGeneration": generation, "indexRevision": revision})
}

/// Corrupt the published head under the running daemon, returning the corrupted pin.
fn corrupt_head(checkout: &Checkout) -> Value {
    let pin = head(checkout);
    let db = rusqlite::Connection::open(index_db(checkout)).unwrap();
    db.execute("UPDATE index_metadata SET stats='not-json'", [])
        .unwrap();
    pin
}

fn read_symbols(checkout: &Checkout) -> Output {
    checkout
        .cli()
        .args(["symbols", "--search", "a", "--workspace"])
        .arg(&checkout.root)
        .output()
        .unwrap()
}

/// V7's outcome: the same daemon re-imported the same bytes without running a producer.
fn assert_reimported(world: &Configured, pid: &str) {
    let imported = wait_last(
        &world.checkout,
        "the recovered index to re-import",
        |last| last["requestId"] != world.first["requestId"] && last["state"] == "done",
    );
    assert_eq!(imported["sha256"], world.first["sha256"]);
    assert_eq!(
        fs::read_to_string(world.checkout.home.join("daemon.pid")).unwrap(),
        pid,
        "the daemon restarted"
    );
    assert!(!world.marker.exists());
}

/// V7 without a restart: a live corruption recovery in the same daemon drops the semantic
/// selection while the artifact and its stat stay unchanged. The stat check's retry asks
/// whether the `done` evidence is still selected and re-imports the bytes.
///
/// Read first: while the repair is held, a read of the corrupt head is refused, whichever of
/// the read and the stat check detected the corruption.
#[test]
fn pickup_reimports_unchanged_artifact_after_live_recovery() {
    let gate = RecoveryGate::new();
    let world = gate.configured();
    let checkout = &world.checkout;
    let pid = fs::read_to_string(checkout.home.join("daemon.pid")).unwrap();

    corrupt_head(checkout);
    let read = read_symbols(checkout);
    let stderr = String::from_utf8_lossy(&read.stderr);
    assert!(
        !read.status.success() && stderr.contains("incompatible_index"),
        "the corrupt head was served: {} {stderr}",
        String::from_utf8_lossy(&read.stdout)
    );
    gate.release();

    let index = checkout
        .cli()
        .args(["index", "--workspace"])
        .arg(&checkout.root)
        .output()
        .unwrap();
    assert!(
        index.status.success(),
        "{}",
        String::from_utf8_lossy(&index.stderr)
    );
    assert_reimported(&world, &pid);
}

/// Repair first: with no read and no index request, the stat check alone detects the corrupt
/// head and the daemon recovers. A later read then succeeds, on the repaired head and not on
/// the corrupted pin.
#[test]
fn stat_check_repairs_a_corrupt_head_before_any_read() {
    let gate = RecoveryGate::new();
    let world = gate.configured();
    let checkout = &world.checkout;
    let pid = fs::read_to_string(checkout.home.join("daemon.pid")).unwrap();

    let corrupted = corrupt_head(checkout);
    gate.release();

    // Until the repair is ready a read gets a typed refusal, never the corrupted pin.
    let mut revision = Value::Null;
    trellis::testing::wait_until("a read to succeed on the repaired head", || {
        let read = read_symbols(checkout);
        if !read.status.success() {
            let stderr = String::from_utf8_lossy(&read.stderr);
            assert!(
                ["incompatible_index", "storage_busy", "rebuild_in_progress"]
                    .iter()
                    .any(|code| stderr.contains(code)),
                "{stderr}"
            );
            return false;
        }
        let reply: Value = serde_json::from_slice(&read.stdout).unwrap();
        revision = reply["data"]["revision"].clone();
        true
    });
    assert_ne!(revision, corrupted);
    assert_reimported(&world, &pid);
}
