//! Automatic producer re-runs (#660) that need the worker held at a chosen point, so they run
//! in-process: a checkout burst is one trigger, and an edit that lands while a run is importing
//! ends it as `stale_basis` instead of publishing a stale batch.
#![cfg(feature = "test-support")]
use serde_json::{Value, json};
use std::{fs, os::unix::net::UnixStream, path::PathBuf, sync::Arc, thread::JoinHandle};
use tempfile::TempDir;
use trellis::{
    daemon::registry::{CheckoutRegistry, CheckoutRuntime},
    indexer::IndexOptions,
    store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    },
};

use crate::semantic_batch_support::{BatchFixture, edit_scip, uniform_workspace};
use crate::support;

const STUB: &str = "trellis-synthetic-scip";
const PRESLOT: (&str, &str) = ("TRELLIS_TEST_PRODUCER_PRESLOT_SOCKET", "preslot.sock");
const SPAWNED: (&str, &str) = ("TRELLIS_TEST_PRODUCER_SPAWNED_SOCKET", "spawned.sock");
const EXITED: (&str, &str) = ("TRELLIS_TEST_PRODUCER_EXITED_SOCKET", "exited.sock");
const ENQUEUED: (&str, &str) = ("TRELLIS_TEST_PRODUCER_ENQUEUED_SOCKET", "enqueued.sock");
const SUPERSEDE: (&str, &str) = ("TRELLIS_TEST_AUTORUN_SUPERSEDE_SOCKET", "supersede.sock");

/// One in-process checkout with the stub registered as the test producer.
struct Auto {
    _base: TempDir,
    stub_dir: TempDir,
    root: PathBuf,
    topology: TopologyRoots,
    identity: WorkspaceIdentity,
    /// Shared with an MCP session (`Auto::mcp`), as the daemon shares it.
    registry: Arc<tokio::sync::Mutex<CheckoutRegistry>>,
    key: String,
    runtime: Arc<CheckoutRuntime>,
}

/// Set only in the child test process that carries one test's environment.
const CHILD: &str = "TRELLIS_TEST_DAEMON_AUTO_RUN_CHILD";

/// These tests arm the stub, the intervals and the hook sockets through process environment, which
/// the daemon's threads read, so they must not share a process with other tests (#869). The
/// parent run re-executes this test binary for the one test, with the marker set only on the
/// child, and returns `false`; the child returns `true` and runs the body.
fn in_own_process(test: &str) -> bool {
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let path = format!("daemon_auto_run::{test}");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &path, "--test-threads=1", "--nocapture"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{test} child: {}\n{stdout}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

fn set_env(name: &str, value: impl AsRef<std::ffi::OsStr>) {
    // SAFETY: only the child process of `in_own_process` gets here, and it runs this one test.
    unsafe { std::env::set_var(name, value) };
}

impl Auto {
    /// An indexed checkout whose owner uses `intervals` (quiet, minimum interval, backstop).
    fn start(intervals: (u64, u64, u64)) -> Self {
        let base = tempfile::tempdir().unwrap();
        let stub_dir = tempfile::Builder::new()
            .prefix("auto-run-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = base.path().join("work");
        fs::create_dir(&root).unwrap();
        uniform_workspace(&root, &["javascript"], 3);
        let root = fs::canonicalize(root).unwrap();
        let mode = support::write_mode(stub_dir.path(), "mode.json", &json!({}));
        set_env("TRELLIS_TEST_PRODUCER_STUB", support::stub_path());
        set_env(
            "TRELLIS_TEST_PRODUCER_STUB_ARGS",
            json!(["--mode-file", mode]).to_string(),
        );
        set_env(
            "TRELLIS_TEST_PRODUCER_LIMITS",
            json!({"graceMs": 100}).to_string(),
        );
        let (name, value) = support::auto_intervals(intervals.0, intervals.1, intervals.2);
        set_env(&name, value);
        // No probe file is ever written: the gates read unknown, whatever the host does.
        set_env(
            support::AUTORUN_PROBES_ENV,
            support::probes_file(stub_dir.path()),
        );

        let identity = WorkspaceIdentity::discover(Some(&root), &root)
            .unwrap()
            .attach_marker()
            .unwrap();
        let key = identity.root_key.clone();
        let activation_key = key.clone();
        let topology =
            TopologyRoots::isolated_for_tests(base.path().join("cache"), base.path().join("data"));
        let mut registry = CheckoutRegistry::with_roots(topology.clone());
        registry.attach_launch(1, &identity).unwrap();
        let runtime = registry.activate(&activation_key).unwrap();
        let auto = Self {
            _base: base,
            stub_dir,
            root,
            topology,
            identity,
            registry: Arc::new(tokio::sync::Mutex::new(registry)),
            key,
            runtime,
        };
        auto.settle();
        auto
    }

    fn settle(&self) -> Value {
        let pin = self
            .runtime
            .await_settled_for_tests(None)
            .unwrap_or_else(|error| panic!("the checkout did not settle: {error}"));
        serde_json::to_value(pin).unwrap()
    }

    fn payload(&self) -> Value {
        json!({
            "version": 1,
            "producer": STUB,
            "trustProfile": "restricted",
            "limits": {"timeoutSecs": 1800, "memoryMiB": 8192},
            "executable": null,
            "environment": {"PATH": "/usr/bin:/bin", "HOME": self.root.parent().unwrap()},
        })
    }

    /// A manual run on its own thread: plan, then run the plan it was shown.
    fn manual_run(&self) -> JoinHandle<anyhow::Result<Value>> {
        let (runtime, mut request) = (self.runtime.clone(), self.payload());
        std::thread::spawn(move || {
            let plan = runtime.cli_producer_plan(&request)?;
            request["planSha256"] = plan["planSha256"].clone();
            runtime.cli_producer_run(&request)
        })
    }

    fn approve(&self, variant: u32) {
        self.emit(variant);
        let run = self.manual_run().join().unwrap().unwrap();
        assert_eq!(run["outcome"], "imported", "{run}");
    }

    /// The stub emits a valid artifact over the published documents, different per `variant`.
    fn emit(&self, variant: u32) {
        let mut fixture =
            BatchFixture::from_published(&self.topology.index_db(&self.identity), &self.root)
                .synthetic();
        let docs = fixture.paths("javascript");
        fixture.artifact("javascript", docs.clone());
        let bytes = edit_scip(&fixture.scip_bytes("javascript", &docs), |index| {
            let info = index.metadata.as_mut().unwrap();
            info.project_root = String::new();
            info.tool_info
                .as_mut()
                .unwrap()
                .arguments
                .push(format!("variant-{variant}"));
        });
        let artifact = self.stub_dir.path().join(format!("index-{variant}.scip"));
        fs::write(&artifact, bytes).unwrap();
        support::write_mode(
            self.stub_dir.path(),
            "mode.json",
            &json!({"emit": artifact}),
        );
    }

    fn hold(&self, hooks: &[(&str, &str)]) {
        for (variable, name) in hooks {
            set_env(variable, self.stub_dir.path().join(name));
        }
    }

    fn hook(&self, hook: (&str, &str)) -> PathBuf {
        self.stub_dir.path().join(hook.1)
    }

    fn wait_hook(&self, hook: (&str, &str)) {
        let ready = self.hook(hook).with_extension("ready");
        trellis::testing::wait_until("hook announced", || ready.exists());
    }

    fn release_hook(&self, hook: (&str, &str)) {
        UnixStream::connect(self.hook(hook)).unwrap();
    }

    fn enable(&self) {
        support::enable_auto(&self.root, STUB, "");
    }

    fn write_javascript(&self, document: usize, body: &str) {
        fs::write(
            self.root.join(format!("javascript/u{document:05}.js")),
            body,
        )
        .unwrap();
    }

    /// An index request on its own thread: it is accepted at once and completes with the worker.
    fn index(&self) -> JoinHandle<anyhow::Result<Value>> {
        let (runtime, root) = (self.runtime.clone(), self.root.clone());
        std::thread::spawn(move || runtime.cli_index(&IndexOptions::new(root)))
    }

    /// What the head's selected raw batches are: `(producer, batch, captured revision)`.
    fn selected_batches(&self) -> Vec<(String, i64, String)> {
        let store = trellis::testing::retry_admission(|| {
            let identity =
                WorkspaceIdentity::discover(Some(&self.root), &self.root)?.attach_marker()?;
            Store::open(self.topology.clone(), identity)
        });
        let pin = store.index_baseline().unwrap();
        store
            .selected_raw_inputs_at(pin)
            .unwrap()
            .into_iter()
            .map(|batch| (batch.producer_id, batch.batch_id, batch.captured_revision))
            .collect()
    }

    fn accepted_requests(&self) -> usize {
        let Ok(db) = rusqlite::Connection::open_with_flags(
            self.topology.requests_db(&self.identity),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) else {
            return 0;
        };
        db.query_row("SELECT count(*) FROM requests", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_or(0, |count| count as usize)
    }

    /// The checkout has no client: it releases once nothing keeps it active.
    fn try_release(&mut self) -> bool {
        let mut registry = self.registry.try_lock().unwrap();
        registry.disconnect(1);
        registry.release(&self.key).unwrap()
    }

    fn auto_state(&self) -> Value {
        self.stub_entry()["autoRun"]["state"].clone()
    }

    fn status(&self) -> Value {
        self.runtime
            .cli_producer_status(&json!({
                "version": 1,
                "environment": {"PATH": "/usr/bin:/bin", "HOME": self.root.parent().unwrap()},
            }))
            .unwrap()
    }

    fn head(&self) -> Value {
        self.status()["head"]["sourceRevision"].clone()
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
        self.topology
            .producer_state_dir(&self.identity)
            .join("runs")
    }

    fn auto_records(&self) -> Vec<Value> {
        let mut records: Vec<Value> = fs::read_dir(self.runs_dir())
            .map(|dir| {
                dir.filter_map(|entry| fs::read(entry.ok()?.path().join("run.json")).ok())
                    .filter_map(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    .filter(|record| record["origin"] == "auto")
                    .collect()
            })
            .unwrap_or_default();
        records.sort_by_key(|record| record["createdAt"].as_u64());
        records
    }

    fn wait_auto(&self, what: &str, ready: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let mut seen = Vec::new();
        trellis::testing::wait_until(what, || {
            seen = self.auto_records();
            ready(&seen)
        });
        seen
    }
}

/// A checkout switch that rewrites hundreds of files, and three index requests that land with
/// it, publish several heads while the worker is held. The owner waits for the queue to drain
/// and the head to be quiet, and starts one run, at the final head.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkout_burst_is_one_trigger() {
    if !in_own_process("checkout_burst_is_one_trigger") {
        return;
    }
    let auto = Auto::start((200, 0, 200));
    auto.approve(0);
    auto.hold(&[PRESLOT]);
    auto.enable();

    let scheduler = auto.runtime.browser_scheduler().unwrap();
    let held = scheduler.hold_worker_for_tests();
    let before = auto.accepted_requests();
    fs::create_dir(auto.root.join("burst")).unwrap();
    let mut indexes = Vec::new();
    for round in 0..3 {
        for file in 0..400 {
            if round == 0 || file % 100 == 0 {
                fs::write(
                    auto.root.join(format!("burst/b{file:03}.js")),
                    format!("function burst{file}_{round}() {{}}\n"),
                )
                .unwrap();
            }
        }
        indexes.push(auto.index());
    }
    trellis::testing::wait_until("three accepted requests", || {
        auto.accepted_requests() == before + 3
    });
    drop(held);
    for index in indexes {
        index.join().unwrap().unwrap();
    }

    // The run waits at its slot, after its plan check, at the final head.
    auto.wait_hook(PRESLOT);
    auto.settle();
    auto.emit(1);
    auto.release_hook(PRESLOT);
    let runs = auto.wait_auto("the automatic run", |runs| {
        runs.iter().any(|run| run["state"] == "done")
    });
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["outcome"], "imported");
    assert_eq!(runs[0]["sourceRevision"], auto.head());
    // Nothing else is due, so a later settled head has still seen exactly one run.
    auto.settle();
    assert_eq!(auto.auto_records().len(), 1);
}

/// An edit published while an automatic run's import is queued cannot stop the run any more: the
/// import's strict basis check ends it, nothing partial is published, and the next run at the new
/// head imports.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_during_importing_ends_stale_basis() {
    if !in_own_process("edit_during_importing_ends_stale_basis") {
        return;
    }
    let auto = Auto::start((50, 0, 200));
    auto.approve(0);
    auto.write_javascript(0, "function edited1() { g(); }\n");
    auto.index().join().unwrap().unwrap();
    auto.settle();
    auto.emit(1);
    let before = auto.selected_batches();
    assert!(!before.is_empty());
    auto.hold(&[EXITED, ENQUEUED, SUPERSEDE]);
    auto.enable();

    // The producer has exited and the import is not yet queued.
    auto.wait_hook(EXITED);
    // The run that follows waits at its slot, so the stale run's effect can be read first.
    auto.hold(&[PRESLOT]);
    let scheduler = auto.runtime.browser_scheduler().unwrap();
    let held = scheduler.hold_worker_for_tests();
    auto.write_javascript(0, "function edited2() { g(); }\n");
    let requests = auto.accepted_requests();
    let index = auto.index();
    trellis::testing::wait_until("the edit's index request", || {
        auto.accepted_requests() == requests + 1
    });
    // Its import joins the queue behind the edit and pauses once it is accepted.
    auto.release_hook(EXITED);
    auto.wait_hook(ENQUEUED);
    assert_eq!(auto.status()["activeRun"]["origin"], "auto");
    drop(held);

    // The worker publishes the edit; the owner asks the run to stop and is told it cannot.
    auto.wait_hook(SUPERSEDE);
    index.join().unwrap().unwrap();
    auto.settle();
    auto.emit(2);
    auto.release_hook(SUPERSEDE);
    auto.release_hook(ENQUEUED);

    let runs = auto.wait_auto("the stale run", |runs| {
        runs.first().is_some_and(|run| run["state"] == "failed")
    });
    let stale = &runs[0];
    assert_eq!(stale["outcome"], "stale_basis");
    let code = stale["errorCode"].as_str().unwrap();
    assert!(code.starts_with("stale_basis"), "{code}");
    assert!(code.contains("head revision changed"), "{code}");
    // Nothing partial was published: the producer's selected batch is the one it had before.
    auto.wait_hook(PRESLOT);
    assert_eq!(auto.selected_batches(), before);

    // The next run, at the new head, imports and replaces it.
    auto.release_hook(PRESLOT);
    let runs = auto.wait_auto("the next run", |runs| {
        runs.len() == 2 && runs[1]["state"] == "done"
    });
    assert_eq!(runs[1]["outcome"], "imported");
    assert_ne!(runs[1]["sourceRevision"], stale["sourceRevision"]);
    assert_ne!(auto.selected_batches(), before);
}

/// A schedule that is only waiting out its quiet period holds no lease, so the idle checkout
/// retires, and its owner goes with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_schedule_does_not_hold_retirement() {
    if !in_own_process("idle_schedule_does_not_hold_retirement") {
        return;
    }
    let mut auto = Auto::start((3_600_000, 0, 200));
    auto.approve(0);
    auto.write_javascript(0, "function edited() { g(); }\n");
    auto.index().join().unwrap().unwrap();
    auto.settle();
    auto.enable();
    trellis::testing::wait_until("the schedule", || auto.auto_state() == "scheduled");

    trellis::testing::wait_until("the idle checkout to release", || auto.try_release());
    assert!(!auto.runtime.has_active_resources());
    assert!(auto.auto_records().is_empty());
}

/// A run holds a lease for as long as it is active: the checkout is not idle, and releases only
/// after the run is over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_auto_run_holds_retirement() {
    if !in_own_process("active_auto_run_holds_retirement") {
        return;
    }
    let mut auto = Auto::start((50, 0, 200));
    auto.approve(0);
    auto.write_javascript(0, "function edited() { g(); }\n");
    auto.index().join().unwrap().unwrap();
    auto.settle();
    support::write_mode(auto.stub_dir.path(), "mode.json", &json!({"sleep": true}));
    auto.hold(&[SPAWNED]);
    auto.enable();
    auto.wait_hook(SPAWNED);
    let run_id = auto.status()["activeRun"]["runId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(auto.status()["activeRun"]["origin"], "auto");

    assert!(!auto.try_release(), "an active run keeps the checkout");
    assert!(auto.runtime.has_active_resources());

    auto.release_hook(SPAWNED);
    auto.runtime
        .cli_producer_cancel(&json!({"version": 1, "runId": run_id}))
        .unwrap();
    trellis::testing::wait_until("the checkout to release after the run", || {
        auto.try_release()
    });
    assert!(!auto.runtime.has_active_resources());
}

/// One in-process MCP session over the fixture's registry, as the daemon serves a client.
struct Mcp {
    client: UnixStream,
    reader: std::io::BufReader<UnixStream>,
    session: Option<JoinHandle<std::io::Result<()>>>,
    next_id: i64,
}

impl Mcp {
    fn start(auto: &Auto) -> Self {
        let (client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
            .unwrap();
        let identity = WorkspaceIdentity::discover(Some(&auto.root), &auto.root)
            .unwrap()
            .attach_marker()
            .unwrap();
        let workspace = trellis::mcp::OpenedWorkspace::new(identity)
            .with_call_workers_for_tests(Default::default());
        let (registry, handle) = (auto.registry.clone(), tokio::runtime::Handle::current());
        let session = std::thread::spawn(move || {
            trellis::mcp::run_socket(workspace, server, registry, 1, handle)
        });
        let reader = std::io::BufReader::new(client.try_clone().unwrap());
        Self {
            client,
            reader,
            session: Some(session),
            next_id: 0,
        }
    }

    /// One tool call's structured envelope.
    fn call(&mut self, name: &str, arguments: Value) -> Value {
        use std::io::{BufRead, Write};
        self.next_id += 1;
        let request = json!({"jsonrpc":"2.0","id":self.next_id,"method":"tools/call","params":{
            "name":name,"arguments":arguments,"_meta":{
                "io.modelcontextprotocol/protocolVersion":"2026-07-28",
                "io.modelcontextprotocol/clientCapabilities":{}}}});
        writeln!(self.client, "{request}").unwrap();
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["id"], self.next_id, "{reply}");
        reply["result"]["structuredContent"].clone()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.client.shutdown(std::net::Shutdown::Both);
        if let Some(session) = self.session.take() {
            let _ = session.join();
        }
    }
}

/// The read surface the issue names, against the fixture's one edited function.
fn read_surface(mcp: &mut Mcp, lookup: &str) -> Vec<(&'static str, Value)> {
    let symbol = json!({"schemaVersion":1,"lookupKey":lookup});
    vec![
        (
            "trellis_workspace_describe",
            mcp.call("trellis_workspace_describe", json!({"schemaVersion":1})),
        ),
        (
            "trellis_find_symbols",
            mcp.call(
                "trellis_find_symbols",
                json!({"schemaVersion":1,"query":"edited","compact":true}),
            ),
        ),
        (
            "trellis_declaration",
            mcp.call("trellis_declaration", symbol.clone()),
        ),
        (
            "trellis_outgoing_calls",
            mcp.call("trellis_outgoing_calls", symbol),
        ),
    ]
}

/// MCP reads keep answering while an automatic run runs its producer and while its import's
/// publication holds the index (#905). During the producer phase they answer from the prior
/// validated head. While the publication has spilled to `index.db` and holds every reader out,
/// they answer the typed, retryable `rebuild_in_progress` within the call's deadline, never a
/// plain `deadline_exceeded`. After COMMIT they answer from the new head.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads_answer_while_an_auto_run_runs_and_imports() {
    if !in_own_process("reads_answer_while_an_auto_run_runs_and_imports") {
        return;
    }
    let auto = Auto::start((50, 0, 200));
    auto.approve(0);
    auto.write_javascript(0, "function edited() { g(); }\nfunction g() {}\n");
    auto.index().join().unwrap().unwrap();
    let prior = auto.settle();
    auto.emit(1);
    let mut mcp = Mcp::start(&auto);
    let found = mcp.call(
        "trellis_find_symbols",
        json!({"schemaVersion":1,"query":"edited","compact":true}),
    );
    let lookup = found["data"]["symbols"][0]["stableLookupKey"]
        .as_str()
        .unwrap_or_else(|| panic!("{found}"))
        .to_owned();

    // The producer has started.
    auto.hold(&[SPAWNED]);
    auto.enable();
    auto.wait_hook(SPAWNED);
    assert_eq!(auto.status()["activeRun"]["origin"], "auto");
    for (tool, envelope) in read_surface(&mut mcp, &lookup) {
        assert!(envelope.get("error").is_none(), "{tool}: {envelope}");
        assert_eq!(
            envelope["evidenceBasis"]["indexRevision"], prior["indexRevision"],
            "{tool}: {envelope}"
        );
    }

    // Its import's publication has spilled and waits before COMMIT.
    let pause = auto.stub_dir.path().join("commit.sock");
    set_env("TRELLIS_TEST_BATCH_WRITER_CACHE_KIB", "64");
    set_env("TRELLIS_TEST_SEMANTIC_PAUSE_BEFORE_COMMIT_SOCKET", &pause);
    auto.release_hook(SPAWNED);
    let ready = pause.with_extension("ready");
    trellis::testing::wait_until("the publication paused before COMMIT", || ready.exists());
    for (tool, envelope) in read_surface(&mut mcp, &lookup) {
        assert_eq!(
            envelope["error"]["code"], "rebuild_in_progress",
            "{tool}: {envelope}"
        );
        assert_eq!(envelope["error"]["retryable"], true, "{tool}: {envelope}");
    }

    UnixStream::connect(&pause).unwrap();
    let runs = auto.wait_auto("the automatic run", |runs| {
        runs.iter().any(|run| run["state"] == "done")
    });
    assert_eq!(runs[0]["outcome"], "imported");
    let published = auto.settle();
    assert_ne!(published, prior);
    for (tool, envelope) in read_surface(&mut mcp, &lookup) {
        assert!(envelope.get("error").is_none(), "{tool}: {envelope}");
        assert_eq!(
            envelope["evidenceBasis"]["indexRevision"], published["indexRevision"],
            "{tool}: {envelope}"
        );
    }
}
