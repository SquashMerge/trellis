//! AC1 runtime proof: launch and activation (including the startup sweep and the pickup
//! activation check), native watcher events, pickup watcher events and each of the 11 MCP
//! tools start no producer. Tripwire executables named like every producer sit on both the
//! daemon `PATH` and the `PATH` every CLI declares; a registered stub writes its own marker.
#![cfg(feature = "test-support")]
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
};
use tempfile::{Builder, TempDir};
use trellis::store::{
    Store,
    topology::{TopologyRoots, WorkspaceIdentity},
};

#[path = "support/producer.rs"]
mod support;
use support::{
    Checkout, cache_dir, data_dir, start_checkout, state_dir, stop_daemon, stub_env, tripwire_dir,
    write_mode,
};

#[path = "support/semantic_batch.rs"]
mod semantic_batch_support;
use semantic_batch_support::{BatchFixture, sha256, uniform_workspace};

const LANGUAGES: [&str; 4] = ["java", "rust", "python", "javascript"];
const RESIDUE: &str = "0123456789abcdef0123456789abcdef";

fn workspace(root: &Path) {
    // `trellis mcp` only serves a Git checkout.
    fs::create_dir_all(root.join(".git")).unwrap();
    uniform_workspace(root, &LANGUAGES, 2);
}

struct Fixture {
    aux: TempDir,
    checkout: Checkout,
    tripwire_marker: PathBuf,
    stub_marker: PathBuf,
    path: String,
    stub: Vec<(String, String)>,
    artifact: PathBuf,
}

impl Fixture {
    /// A CLI whose declared environment carries the tripwire `PATH` and the stub registration.
    fn cli(&self) -> Command {
        let mut command = self.checkout.cli();
        command.env("PATH", &self.path);
        for (key, value) in &self.stub {
            command.env(key, value);
        }
        command.env("TRELLIS_TEST_PICKUP_INTERVALS", intervals());
        command
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.cli()
            .args(args)
            .arg("--workspace")
            .arg(&self.checkout.root)
            .output()
            .unwrap()
    }

    /// `trellis producer status`, through the armed environment: a daemon this call has to
    /// launch carries the tripwire `PATH`, the stub registration and the test intervals.
    fn status(&self) -> Value {
        let output = self.run(&["producer", "status"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// The pid of the serving daemon.
    fn daemon_pid(&self) -> i32 {
        fs::read_to_string(self.checkout.home.join("daemon.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    /// Launch the daemon again after `stop_daemon`, through the armed environment, so no
    /// other call can launch an unarmed one. Returns the new pid.
    fn restart_daemon_after_stop(&self) -> i32 {
        let old = self.daemon_pid();
        self.status();
        let new = self.daemon_pid();
        assert_ne!(old, new, "the daemon was relaunched");
        assert!(trellis::testing::process_running(new));
        new
    }

    fn index_db(&self) -> PathBuf {
        roots(&self.checkout).index_db(&identity(&self.checkout))
    }

    fn javascript_bytes(&self, documents: usize) -> Vec<u8> {
        let fixture = BatchFixture::from_published(&self.index_db(), &self.checkout.root)
            .reference("scip-typescript");
        let docs = fixture.paths("javascript");
        fixture.scip_bytes("javascript", &docs[..documents])
    }

    fn pickup_state(&self) -> Value {
        let status = self.status();
        status["producers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == "scip-typescript")
            .unwrap()["pickup"]["lastImported"]
            .clone()
    }

    /// No producer ran: neither marker exists, no run directory exists and status reports no
    /// active or last run for any producer.
    fn assert_no_producer_started(&self, phase: &str) {
        assert!(
            !self.tripwire_marker.exists(),
            "{phase}: a tripwire executable ran: {}",
            fs::read_to_string(&self.tripwire_marker).unwrap_or_default()
        );
        assert!(!self.stub_marker.exists(), "{phase}: the stub ran");
        let runs = state_dir(&self.checkout).join("runs");
        let entries = fs::read_dir(&runs).map_or(0, |dir| dir.count());
        assert_eq!(entries, 0, "{phase}: a run directory exists");
        let status = self.status();
        assert_eq!(status["activeRun"], Value::Null, "{phase}");
        let producers = status["producers"].as_array().unwrap();
        // The serving daemon is the armed one: it registered the stub.
        let stub = producers
            .iter()
            .find(|producer| producer["name"] == "trellis-synthetic-scip")
            .unwrap_or_else(|| panic!("{phase}: the stub is not registered: {status}"));
        assert_eq!(stub["detection"]["result"], "available", "{phase}: {stub}");
        for producer in producers {
            assert_eq!(
                producer["lastRun"],
                Value::Null,
                "{phase}: {}",
                producer["name"]
            );
        }
    }
}

fn intervals() -> String {
    json!({"debounceMs": 100, "stableMs": 100}).to_string()
}

fn roots(checkout: &Checkout) -> TopologyRoots {
    TopologyRoots::isolated_for_tests(cache_dir(&checkout.home), data_dir(&checkout.home))
}

fn identity(checkout: &Checkout) -> WorkspaceIdentity {
    WorkspaceIdentity::discover(Some(&checkout.root), &checkout.root).unwrap()
}

fn fixture() -> Fixture {
    let aux = Builder::new()
        .prefix("triggers-")
        .tempdir_in("/tmp")
        .unwrap();
    let (tripwire, tripwire_marker) = tripwire_dir(aux.path());
    let stub_marker = aux.path().join("stub.marker");
    let stub = stub_env(aux.path(), &json!({"marker": stub_marker}));
    let path = format!(
        "{}:{}",
        tripwire.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut daemon_env = stub.clone();
    daemon_env.push(("PATH".into(), path.clone()));
    daemon_env.push(("TRELLIS_TEST_PICKUP_INTERVALS".into(), intervals()));
    let checkout = start_checkout(workspace, &daemon_env);
    let artifact = checkout.temp.path().join("index.scip");
    let fixture = Fixture {
        aux,
        checkout,
        tripwire_marker,
        stub_marker: stub_marker.clone(),
        path,
        stub,
        artifact,
    };
    // A run of the stub would emit a real artifact and leave a marker.
    write_mode(
        fixture.aux.path(),
        "mode.json",
        &json!({"marker": stub_marker, "emit": fixture.artifact}),
    );
    fs::write(&fixture.artifact, fixture.javascript_bytes(2)).unwrap();
    fixture
}

/// Run `read` until it stops failing with the typed `storage_busy` that the daemon's own writes
/// cause; any other error fails at once, and a wait that never ends is a hang.
fn retry_busy<T>(what: &str, mut read: impl FnMut() -> anyhow::Result<T>) -> T {
    let mut value = None;
    trellis::testing::wait_until(what, || match read() {
        Ok(read) => {
            value = Some(read);
            true
        }
        Err(error) if format!("{error:#}").contains("storage_busy") => false,
        Err(error) => panic!("{what} failed: {error:#}"),
    });
    value.unwrap()
}

fn head(checkout: &Checkout) -> u64 {
    retry_busy("the index head read", || {
        let store = Store::open(roots(checkout), identity(checkout))?;
        Ok(store.index_baseline()?.index_revision)
    })
}

#[test]
fn head_reads_retry_storage_busy_until_the_writer_lets_go() {
    let mut busy = 3;
    let value = retry_busy("the retried read", || {
        if busy == 0 {
            return Ok(7);
        }
        busy -= 1;
        Err(anyhow::anyhow!("storage_busy: SQLite lock contention"))
    });
    assert_eq!((value, busy), (7, 0));
}

#[test]
#[should_panic(expected = "the failing read failed")]
fn head_reads_do_not_retry_other_errors() {
    retry_busy::<u64>("the failing read", || Err(anyhow::anyhow!("corrupt")));
}

/// A raw JSON-RPC session with `trellis mcp` over stdio.
struct Mcp {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    lines: mpsc::Receiver<String>,
    stderr: PathBuf,
    next: u64,
}

impl Mcp {
    fn start(fixture: &Fixture) -> Self {
        let mut child = fixture
            .cli()
            .arg("mcp")
            .arg("--workspace")
            .arg(&fixture.checkout.root)
            .current_dir(&fixture.checkout.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(fs::File::create(fixture.aux.path().join("mcp.stderr")).unwrap())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let stdin = child.stdin.take();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
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
            stderr: fixture.aux.path().join("mcp.stderr"),
            next: 1,
        }
    }

    fn send(&mut self, value: &Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{value}").unwrap();
        stdin.flush().unwrap();
    }

    fn ask(&mut self, method: &str, params: Value) -> Value {
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

    /// A tool call: a tool-level error is acceptable, a transport error is not.
    fn call(&mut self, name: &str, arguments: Value) -> Value {
        let reply = self.ask("tools/call", json!({"name": name, "arguments": arguments}));
        assert!(
            reply.get("error").is_none(),
            "{name}: transport error {reply}"
        );
        assert!(reply["result"].is_object(), "{name}: {reply}");
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

fn fake_symbol() -> String {
    format!("sid:v1:{}", "a".repeat(32))
}

#[test]
fn no_trigger_starts_a_producer() {
    let fx = fixture();

    // Launch and activation: the first launch indexed; index again.
    fx.assert_no_producer_started("first launch");
    assert!(fx.run(&["index"]).status.success());
    fx.assert_no_producer_started("index");

    // A pickup import is an explicit owner action and still runs no producer.
    let set = fx.run(&[
        "producer",
        "pickup",
        "--producer",
        "scip-typescript",
        "--artifact",
        fx.artifact.to_str().unwrap(),
    ]);
    assert!(
        set.status.success(),
        "{}",
        String::from_utf8_lossy(&set.stderr)
    );
    fx.assert_no_producer_started("pickup set");

    // A residue directory no request names, then a restart: the startup sweep removes it and
    // the pickup activation check runs.
    let pickups = state_dir(&fx.checkout).join("pickups");
    let _ = fs::DirBuilder::new().mode(0o700).create(&pickups);
    let residue = pickups.join(RESIDUE);
    fs::DirBuilder::new().mode(0o700).create(&residue).unwrap();
    // The artifact changes while the daemon is down, so the activation check has something
    // observable to import.
    let before_restart = fx.pickup_state()["sha256"].clone();
    let offline = fx.javascript_bytes(1);
    assert_ne!(before_restart, sha256(&offline).as_str());
    stop_daemon(&fx.checkout.home);
    fs::write(&fx.artifact, &offline).unwrap();
    let armed = fx.restart_daemon_after_stop();
    fx.assert_no_producer_started("restart");
    trellis::testing::wait_until("the sweep to remove the residue", || !residue.exists());
    trellis::testing::wait_until("the activation check to import the offline bytes", || {
        fx.pickup_state()["sha256"] == sha256(&offline).as_str()
    });
    assert_eq!(fx.daemon_pid(), armed, "the armed daemon is still serving");
    fx.assert_no_producer_started("startup sweep and activation check");

    // Native watcher: an edit publishes a new head.
    let before = head(&fx.checkout);
    fs::write(
        fx.checkout.root.join("javascript/u00000.js"),
        "function u00000() { return 2; }\n",
    )
    .unwrap();
    trellis::testing::wait_until("the native watcher to publish the edit", || {
        head(&fx.checkout) > before
    });
    fx.assert_no_producer_started("native watcher event");

    // Pickup watcher: different valid bytes are re-imported.
    let regenerated = fx.javascript_bytes(2);
    let digest = sha256(&regenerated);
    assert_ne!(fx.pickup_state()["sha256"], digest.as_str());
    fs::write(&fx.artifact, &regenerated).unwrap();
    trellis::testing::wait_until("the pickup watcher to re-import", || {
        let last = fx.pickup_state();
        last["sha256"] == digest.as_str() && last["state"] == "done"
    });
    fx.assert_no_producer_started("pickup watcher event");

    // The eleven MCP tools.
    let mut mcp = Mcp::start(&fx);
    let init = mcp.ask(
        "initialize",
        json!({"protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": {"name": "triggers", "version": "1"}}),
    );
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    mcp.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let listed = mcp.ask("tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 11);
    fx.assert_no_producer_started("tools/list");

    let found = mcp.call(
        "trellis_find_symbols",
        json!({"schemaVersion": 1, "query": "u00000", "limit": 5}),
    );
    let symbol = found["result"]["structuredContent"]["data"]["symbols"][0]["symbolId"]
        .as_str()
        .map_or_else(fake_symbol, str::to_owned);
    fx.assert_no_producer_started("trellis_find_symbols");
    let by_symbol = json!({"schemaVersion": 1, "symbolId": symbol});
    let calls: [(&str, Value); 10] = [
        ("trellis_workspace_describe", json!({"schemaVersion": 1})),
        (
            "trellis_search_symbols",
            json!({"schemaVersion": 1, "query": "u00000"}),
        ),
        ("trellis_declaration", by_symbol.clone()),
        ("trellis_outgoing_calls", by_symbol.clone()),
        ("trellis_incoming_calls", by_symbol.clone()),
        (
            "trellis_read_source",
            json!({"schemaVersion": 1, "path": "javascript/u00000.js",
                "startLine": 1, "endLine": 1}),
        ),
        ("trellis_call_graph", by_symbol),
        ("trellis_references", json!({"schemaVersion": 1})),
        ("trellis_rules_check", json!({"schemaVersion": 1})),
        (
            "trellis_rules_explain",
            json!({"schemaVersion": 1, "rule": "none", "target": "javascript"}),
        ),
    ];
    for (name, arguments) in calls {
        mcp.call(name, arguments);
        fx.assert_no_producer_started(name);
    }
    drop(mcp);

    // Positive controls: the detector fires for each marker and for a run directory.
    let fires = |setup: &dyn Fn(), undo: &dyn Fn()| {
        setup();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fx.assert_no_producer_started("control");
        }));
        undo();
        assert!(caught.is_err(), "the detector missed a started producer");
    };
    fires(
        &|| {
            let tripwire = fx.aux.path().join("tripwire/scip-typescript");
            let status = Command::new(tripwire).arg("--control").status().unwrap();
            assert_eq!(status.code(), Some(97));
        },
        &|| fs::remove_file(&fx.tripwire_marker).unwrap(),
    );
    fires(
        &|| fs::write(&fx.stub_marker, "stub --control\n").unwrap(),
        &|| fs::remove_file(&fx.stub_marker).unwrap(),
    );
    let runs = state_dir(&fx.checkout).join("runs");
    fires(&|| fs::create_dir_all(runs.join("r0")).unwrap(), &|| {
        fs::remove_dir_all(runs.join("r0")).unwrap()
    });
    fx.assert_no_producer_started("after the controls");
}
