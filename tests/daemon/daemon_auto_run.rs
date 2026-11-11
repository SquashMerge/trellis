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
    registry: CheckoutRegistry,
    key: String,
    runtime: Arc<CheckoutRuntime>,
}

fn set_env(name: &str, value: impl AsRef<std::ffi::OsStr>) {
    // Each test is its own process, and the daemon's threads only read these once armed.
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
            registry,
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
        self.registry.disconnect(1);
        self.registry.release(&self.key).unwrap()
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
