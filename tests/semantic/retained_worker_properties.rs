//! Real in-process checkout worker; the fixture never creates a producer process record.
use super::*;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};
use trellis::daemon::registry::{CheckoutRegistry, CheckoutRuntime};
use trellis::store::requests::{GeneratedImportOptions, Request};

const ID: &str = "11111111111111111111111111111111";
const NEXT: &str = "22222222222222222222222222222222";

fn fixture() -> Inproc {
    let state = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    fs::write(root.join("a.js"), "function run(){ target(); }\n").unwrap();
    fs::write(root.join("b.js"), "function target(){ }\n").unwrap();
    let store = Store::open_for_tests(state.path(), &root).unwrap();
    store.mcp_prepare_virgin_queue_for_tests().unwrap();
    let world = Inproc {
        _state: state,
        _workspace: temp,
        root,
        store,
        cancel: Arc::new(AtomicBool::new(false)),
    };
    world.publish_native();
    world
}

fn artifact(world: &Inproc, root: &str) -> Vec<u8> {
    use semantic_batch_support::DocSpec;
    let mut fixture = BatchFixture::from_published(&world.index_db(), &world.root).synthetic();
    let docs = vec![
        DocSpec::new("a.js")
            .occurrence(&[0, 9, 12], &[], "pkg run.", 1)
            .occurrence(&[0, 16, 22], &[0, 16, 24], "pkg target.", 0)
            .symbol("pkg run.", 17, &[]),
        DocSpec::new("b.js")
            .occurrence(&[0, 9, 15], &[], "pkg target.", 1)
            .symbol("pkg target.", 17, &[]),
    ];
    fixture.artifact("javascript", docs.clone());
    edit_scip(&fixture.scip_bytes("javascript", &docs), |index| {
        index.metadata.as_mut().unwrap().project_root = root.into();
    })
}

fn published() -> (Inproc, PathBuf, PathBuf, String) {
    let world = fixture();
    let (dir, desc, sha) = world.write_run(ID, |root| artifact(&world, root)).unwrap();
    let pin = world.publish_run(ID, &dir, &desc, &sha);
    let snapshot = world.store.semantic_at(pin).unwrap();
    assert_eq!(snapshot.declaration_bindings.len(), 2);
    assert_eq!(snapshot.call_bindings.len(), 1);
    assert_eq!(
        snapshot.call_bindings[0].resolution,
        trellis::semantic_evidence::Resolution::Resolved
    );
    (world, dir, desc, sha)
}

fn obsolete(world: Inproc) -> Inproc {
    let Inproc {
        _state,
        _workspace,
        root,
        store,
        cancel,
    } = world;
    let (roots, identity) = store.state_topology();
    let index = roots.index_db(identity);
    drop(store);
    let db = rusqlite::Connection::open(index).unwrap();
    let version: u32 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 36);
    db.pragma_update(None, "user_version", version - 1).unwrap();
    drop(db);
    let store = Store::open_for_tests(_state.path(), &root).unwrap();
    assert!(store.recovery_pending_for_tests());
    Inproc {
        _state,
        _workspace,
        root,
        store,
        cancel,
    }
}

struct Active {
    registry: CheckoutRegistry,
    runtime: Arc<CheckoutRuntime>,
    world: Option<Inproc>,
    key: String,
    startup_release: Option<Sender<()>>,
    claim_release: Option<Sender<()>>,
    closed: bool,
}

impl Active {
    fn start(world: Inproc, hook_claim: bool) -> (Self, Receiver<()>, Option<Receiver<()>>) {
        let state = world.state();
        assert!(!state.path().join("pickup.json").exists());
        for entry in fs::read_dir(state.path().join("runs")).unwrap() {
            assert!(
                !entry.unwrap().path().join("run.json").exists(),
                "sweep must have no process records"
            );
        }
        let identity = WorkspaceIdentity::discover(Some(&world.root), &world.root).unwrap();
        let roots = TopologyRoots::isolated_for_tests(
            world._state.path().join("cache"),
            world._state.path().join("data"),
        );
        let mut registry = CheckoutRegistry::with_roots(roots);
        registry.attach_launch(1, &identity).unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (startup_tx, startup_rx) = mpsc::channel();
        let startup_rx = std::sync::Mutex::new(startup_rx);
        registry.set_next_startup_hook_for_tests(Arc::new(move || {
            let _ = entered_tx.send(());
            let _ = startup_rx
                .lock()
                .unwrap()
                .recv_timeout(trellis::testing::HANG_BACKSTOP);
        }));
        let runtime = registry.activate(&identity.root_key).unwrap();
        entered_rx
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap();
        let (claim_release, claim_entered) = if hook_claim {
            let (hit_tx, hit_rx) = mpsc::channel();
            let (resume_tx, resume_rx) = mpsc::channel();
            runtime.set_after_claim_hook_for_tests(move || {
                let _ = hit_tx.send(());
                let _ = resume_rx.recv_timeout(trellis::testing::HANG_BACKSTOP);
            });
            (Some(resume_tx), Some(hit_rx))
        } else {
            (None, None)
        };
        (
            Self {
                registry,
                runtime,
                world: Some(world),
                key: identity.root_key,
                startup_release: Some(startup_tx),
                claim_release,
                closed: false,
            },
            entered_rx,
            claim_entered,
        )
    }

    fn world(&self) -> &Inproc {
        self.world.as_ref().unwrap()
    }
    fn resume_startup(&mut self) {
        self.startup_release.take().unwrap().send(()).unwrap();
    }

    fn wait_startup_step(&self) {
        let scheduler = self.runtime.browser_scheduler().unwrap();
        let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
        loop {
            let seen = self.runtime.state_version();
            if scheduler.runtime_phase() == trellis::http::RuntimePhase::Ready {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "startup did not become ready: {:?}",
                self.runtime.reconciliation_error()
            );
            self.runtime
                .wait_state_change(seen, Duration::from_millis(250));
        }
        // Ready is installed within the worker step. Acquiring its serial boundary
        // proves that step, including retained enqueue and FIFO drain, has ended.
        self.runtime.hold_worker_step_for_tests(|| {});
    }

    async fn terminal(&mut self, claimed: Receiver<()>, pending_reason: Option<&str>) -> Request {
        claimed
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap();
        self.terminal_started(pending_reason).await
    }

    async fn terminal_started(&mut self, pending_reason: Option<&str>) -> Request {
        let row = self
            .world()
            .store
            .earliest_unfinished_request()
            .unwrap()
            .unwrap();
        assert_eq!(row.state, "running");
        if let Some(reason) = pending_reason {
            assert_unavailable(self, reason);
        }
        let notify = Arc::new(tokio::sync::Notify::new());
        self.runtime
            .notify_on_terminal_request_for_tests(&row.id, notify.clone())
            .unwrap();
        self.claim_release.take().unwrap().send(()).unwrap();
        tokio::time::timeout(trellis::testing::HANG_BACKSTOP, notify.notified())
            .await
            .expect("terminal commit signal");
        self.world().store.request_by_id(&row.id).unwrap().unwrap()
    }

    /// Install the next claim barrier while the first claim is still held, so
    /// FIFO cannot finish the second request before the test observes it.
    async fn terminal_then_hold_next(&mut self, claimed: Receiver<()>) -> (Request, Receiver<()>) {
        claimed
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap();
        let row = self
            .world()
            .store
            .earliest_unfinished_request()
            .unwrap()
            .unwrap();
        assert_eq!(row.state, "running");
        let (hit_tx, hit_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        self.runtime.set_after_claim_hook_for_tests(move || {
            let _ = hit_tx.send(());
            let _ = resume_rx.recv_timeout(trellis::testing::HANG_BACKSTOP);
        });
        let notify = Arc::new(tokio::sync::Notify::new());
        self.runtime
            .notify_on_terminal_request_for_tests(&row.id, notify.clone())
            .unwrap();
        self.claim_release.take().unwrap().send(()).unwrap();
        tokio::time::timeout(trellis::testing::HANG_BACKSTOP, notify.notified())
            .await
            .expect("first terminal commit signal");
        let row = self.world().store.request_by_id(&row.id).unwrap().unwrap();
        self.claim_release = Some(resume_tx);
        (row, hit_rx)
    }

    fn close(&mut self) {
        if self.closed {
            return;
        }
        let scheduler = self.runtime.browser_scheduler().unwrap();
        let scheduler_lifetime = Arc::downgrade(&scheduler);
        drop(scheduler);
        self.startup_release.take();
        self.claim_release.take();
        self.registry.disconnect(1);
        let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
        loop {
            let seen = self.runtime.state_version();
            if self.registry.release(&self.key).unwrap_or(false) {
                break;
            }
            assert!(Instant::now() < deadline, "fixture runtime did not retire");
            self.runtime
                .wait_state_change(seen, Duration::from_millis(250));
        }
        assert!(!self.runtime.has_active_resources());
        // PickupWatch has no join/completion signal. Its host owns the scheduler
        // until its bounded shutdown poll exits; observe that ownership ending
        // before deleting any fixture roots.
        while scheduler_lifetime.strong_count() != 0 {
            assert!(
                Instant::now() < deadline,
                "fixture scheduler still has owners"
            );
            std::thread::park_timeout(Duration::from_millis(10));
        }
        self.closed = true;
    }

    fn finish(mut self) -> Inproc {
        self.close();
        self.world.take().unwrap()
    }
}

fn rebind_catalog_digest(world: &Inproc, digest: &str) {
    let path = world.state().path().join("retention.sqlite");
    let db = rusqlite::Connection::open(path).unwrap();
    let options: String = db
        .query_row(
            "SELECT options FROM retained_runs WHERE producer='trellis-synthetic-scip'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let mut options: Value = serde_json::from_str(&options).unwrap();
    options["descriptorSha256"] = json!(digest);
    assert_eq!(db.execute("UPDATE retained_runs SET options=?1, descriptor_sha256=?2 WHERE producer='trellis-synthetic-scip'", rusqlite::params![options.to_string(), digest]).unwrap(), 1);
}

fn assert_unavailable(active: &Active, expected_reason: &str) {
    let status = active.runtime.cli_status().unwrap();
    let js = status["semanticLanguages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["reimport"]["state"] == "needsReimport")
        .unwrap();
    assert_eq!(js["reimport"]["state"], "needsReimport");
    assert!(
        js["reimport"]["reason"]
            .as_str()
            .unwrap()
            .contains(expected_reason),
        "{js}"
    );
    assert_eq!(js["semanticProducerId"], Value::Null);
    assert_eq!(js["semanticState"], "unavailable");
    let observer =
        Store::open_for_tests(active.world()._state.path(), &active.world().root).unwrap();
    let pin = observer.index_baseline().unwrap();
    let semantic = observer.semantic_at(pin).unwrap();
    assert!(semantic.producers.is_empty());
    assert!(semantic.coverage.is_empty());
    assert!(semantic.provenance.is_empty());
    assert!(semantic.symbols.is_empty());
    assert!(semantic.declaration_bindings.is_empty());
    assert!(semantic.references.is_empty());
    assert!(semantic.call_bindings.is_empty());
    assert!(semantic.type_relationships.is_empty());
}

impl Drop for Active {
    fn drop(&mut self) {
        if !self.closed {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.close()));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_fifo_reimports_selected_retained_run_after_rebuild() {
    let (world, dir, desc, sha) = published();
    let before = world.store.index_baseline().unwrap();
    let world = obsolete(world);
    let (mut active, _, claim) = Active::start(world, true);
    active.resume_startup();
    let row = active
        .terminal(claim.unwrap(), Some("normal validation"))
        .await;
    eprintln!(
        "automatic recovery terminal: state={} error={:?}",
        row.state, row.error_code
    );
    assert_eq!(row.state, "done");
    assert!(row.error_code.is_none());
    let options: Value = serde_json::from_str(&row.options_json).unwrap();
    assert_eq!(options["generationId"], ID);
    assert_eq!(options["descriptor"], desc.to_str().unwrap());
    assert_eq!(options["descriptorSha256"], sha);
    let runs = fs::read_dir(active.world().state().path().join("runs"))
        .unwrap()
        .map(|entry| fs::canonicalize(entry.unwrap().path()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(runs, vec![dir.clone()]);
    assert!(
        !dir.join("run.json").exists(),
        "automatic recovery must not start a producer run"
    );
    assert!(dir.join("out").is_dir());
    assert!(dir.join("batch").is_dir());
    let pin = row.revision.unwrap();
    assert_ne!(pin.index_generation, before.index_generation);
    // The pre-activation Store retained its own obsolete-disposition flag.
    let observer =
        Store::open_for_tests(active.world()._state.path(), &active.world().root).unwrap();
    let recovered = observer.semantic_at(pin).unwrap();
    assert_eq!(recovered.declaration_bindings.len(), 2);
    assert_eq!(recovered.call_bindings.len(), 1);
    assert_eq!(
        recovered.call_bindings[0].resolution,
        trellis::semantic_evidence::Resolution::Resolved
    );
    match &recovered.call_bindings[0].declared_target {
        Some(trellis::semantic_evidence::Target::Internal { document, .. }) => {
            assert_eq!(document.path, "b.js")
        }
        other => panic!("expected b.js target, got {other:?}"),
    }
    let status = active.runtime.cli_status().unwrap();
    let js = status["semanticLanguages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["language"] == "javascript")
        .unwrap();
    assert_eq!(js["reimport"], Value::Null);
    assert_eq!(js["semanticState"], "fresh");
    assert_eq!(
        observer.selected_raw_inputs_at(pin).unwrap()[0]
            .descriptor
            .generation
            .as_ref()
            .unwrap()
            .generation_id,
        ID
    );
    drop(observer);
    let world = active.finish().rebuild();
    let fresh = world.publish_run(ID, &dir, &desc, &sha);
    assert_eq!(world.store.semantic_at(fresh).unwrap(), recovered);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_generated_failure_preserves_previous_successful_slot() {
    let (world, original_dir, _, _) = published();
    let original_pin = world.store.index_baseline().unwrap();
    let original = world.store.semantic_at(original_pin).unwrap();
    let (dir, desc, sha) = world
        .write_run(NEXT, |root| artifact(&world, root))
        .unwrap();
    fs::write(dir.join(batch::ARTIFACT_PATH), b"broken").unwrap();
    let accepted = world
        .store
        .enqueue_generated_semantic_import(
            &world.root,
            &GeneratedImportOptions {
                version: 2,
                descriptor: desc.to_string_lossy().into_owned(),
                descriptor_sha256: sha,
                origin: GenerationOrigin::TrellisRun,
                generation_id: NEXT.into(),
            },
        )
        .unwrap();
    let (mut active, _, claim) = Active::start(world, true);
    active.resume_startup();
    let row = active.terminal(claim.unwrap(), None).await;
    eprintln!(
        "generated failure terminal: state={} error={:?}",
        row.state, row.error_code
    );
    assert_eq!(row.id, accepted.id);
    assert_eq!(row.state, "failed");
    assert!(
        row.error_code
            .as_deref()
            .unwrap()
            .starts_with("invalid_request")
    );
    let head = active.world().store.index_baseline().unwrap();
    assert_eq!(active.world().store.semantic_at(head).unwrap(), original);
    assert_eq!(active.world().selected(head)[0].0, ID);
    assert!(original_dir.join("out").is_dir());
    assert!(original_dir.join("batch").is_dir());
    let catalog = active.world().state().path().join("retention.sqlite");
    let db =
        rusqlite::Connection::open_with_flags(catalog, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let options: String = db
        .query_row(
            "SELECT options FROM retained_runs WHERE producer='trellis-synthetic-scip'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&options).unwrap()["generationId"],
        ID
    );
    drop(db);
    let world = obsolete(active.finish());
    let (mut restarted, _, claim) = Active::start(world, true);
    restarted.resume_startup();
    let row = restarted
        .terminal(claim.unwrap(), Some("normal validation"))
        .await;
    assert_eq!(row.state, "done");
    let observer =
        Store::open_for_tests(restarted.world()._state.path(), &restarted.world().root).unwrap();
    let head = row.revision.unwrap();
    assert_eq!(observer.semantic_at(head).unwrap(), original);
    assert_eq!(
        observer.selected_raw_inputs_at(head).unwrap()[0]
            .descriptor
            .generation
            .as_ref()
            .unwrap()
            .generation_id,
        ID
    );
    drop(observer);
    let _ = restarted.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_producer_slots_survive_terminal_failure_and_rebuild_fifo() {
    use semantic_batch_support::DocSpec;
    use std::collections::{BTreeMap, BTreeSet};
    use trellis::producer::ProducerName;
    use trellis::semantic_evidence::{Resolution, Target as SemanticTarget};

    const A1: &str = "11111111111111111111111111111111";
    const B1: &str = "22222222222222222222222222222222";
    const A2: &str = "33333333333333333333333333333333";
    const A3: &str = "44444444444444444444444444444444";
    const A: &str = "trellis-synthetic-scip";
    const B: &str = "scip-java";

    let state = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    fs::write(root.join("a.js"), "function run(){ target(); }\n").unwrap();
    fs::write(root.join("b.js"), "function target(){ }\n").unwrap();
    fs::write(root.join("C.java"), "class C { void f() {} }\n").unwrap();
    let store = Store::open_for_tests(state.path(), &root).unwrap();
    store.mcp_prepare_virgin_queue_for_tests().unwrap();
    let world = Inproc {
        _state: state,
        _workspace: temp,
        root,
        store,
        cancel: Arc::new(AtomicBool::new(false)),
    };
    world.publish_native();

    let (a1_dir, a1_desc, a1_sha) = world.write_run(A1, |root| artifact(&world, root)).unwrap();
    let pin_a1 = world.publish_run(A1, &a1_dir, &a1_desc, &a1_sha);
    let a1_snapshot = world.store.semantic_at(pin_a1).unwrap();
    assert!(
        a1_snapshot
            .symbols
            .iter()
            .any(|s| s.key.symbol == "pkg run.")
    );
    let (b1_dir, b1_desc, b1_sha) = world
        .write_run_for(
            B1,
            Target::Real(ProducerName::ScipJava),
            "java",
            |worktree| {
                let mut fixture = BatchFixture::from_published(&world.index_db(), &world.root)
                    .reference("scip-java");
                let docs = vec![
                    DocSpec::new("C.java")
                        .occurrence(&[0, 6, 7], &[], "pkg C#", 1)
                        .symbol("pkg C#", 7, &[]),
                ];
                fixture.artifact("java", docs.clone());
                edit_scip(&fixture.scip_bytes("java", &docs), |index| {
                    index.metadata.as_mut().unwrap().project_root = worktree.into()
                })
            },
        )
        .unwrap();
    world.publish_run(B1, &b1_dir, &b1_desc, &b1_sha);
    let (a2_dir, a2_desc, a2_sha) = world
        .write_run(A2, |root| {
            edit_scip(&artifact(&world, root), |index| {
                for document in &mut index.documents {
                    for occurrence in &mut document.occurrences {
                        occurrence.symbol = occurrence.symbol.replace("pkg ", "pkg latest_");
                    }
                    for symbol in &mut document.symbols {
                        symbol.symbol = symbol.symbol.replace("pkg ", "pkg latest_");
                    }
                }
            })
        })
        .unwrap();
    let expected_pin = world.publish_run(A2, &a2_dir, &a2_desc, &a2_sha);
    let expected = world.store.semantic_at(expected_pin).unwrap();
    assert!(
        expected
            .symbols
            .iter()
            .any(|s| s.key.symbol == "pkg latest_run.")
    );
    assert!(!expected.symbols.iter().any(|s| s.key.symbol == "pkg run."));
    assert!(
        expected
            .declaration_bindings
            .iter()
            .any(|b| b.join.anchor.document.path == "C.java" && b.syntax_id.is_some())
    );
    assert!(expected.provenance.iter().any(|p| p.producer_id == A));
    assert!(expected.provenance.iter().any(|p| p.producer_id == B));
    assert!(expected.coverage.iter().any(|c| c.producer_id == A
        && c.document_path == "a.js"
        && c.selected
        && c.state == trellis::semantic_evidence::CoverageState::Complete));
    assert!(expected.coverage.iter().any(|c| c.producer_id == B
        && c.document_path == "C.java"
        && c.selected
        && c.state == trellis::semantic_evidence::CoverageState::Complete));
    let call = expected
        .call_bindings
        .iter()
        .find(|b| b.join.anchor.document.path == "a.js")
        .unwrap();
    assert_eq!(call.resolution, Resolution::Resolved);
    assert!(
        matches!(&call.declared_target, Some(SemanticTarget::Internal { document, .. }) if document.path == "b.js")
    );

    let slots = |world: &Inproc| -> BTreeMap<String, String> {
        let db = rusqlite::Connection::open_with_flags(
            world.state().path().join("retention.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut query = db
            .prepare("SELECT producer, options FROM retained_runs")
            .unwrap();
        query
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .map(|row| {
                let (producer, options) = row.unwrap();
                (
                    producer,
                    serde_json::from_str::<Value>(&options).unwrap()["generationId"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                )
            })
            .collect()
    };
    let wanted = BTreeMap::from([(A.to_owned(), A2.to_owned()), (B.to_owned(), B1.to_owned())]);
    assert_eq!(slots(&world), wanted);

    let (a3_dir, a3_desc, a3_sha) = world.write_run(A3, |root| artifact(&world, root)).unwrap();
    fs::write(a3_dir.join(batch::ARTIFACT_PATH), b"broken").unwrap();
    let accepted = world
        .store
        .enqueue_generated_semantic_import(
            &world.root,
            &GeneratedImportOptions {
                version: 2,
                descriptor: a3_desc.to_string_lossy().into_owned(),
                descriptor_sha256: a3_sha,
                origin: GenerationOrigin::TrellisRun,
                generation_id: A3.into(),
            },
        )
        .unwrap();
    let (mut active, _, claim) = Active::start(world, true);
    active.resume_startup();
    let failed = active.terminal(claim.unwrap(), None).await;
    eprintln!(
        "multi-producer A3 terminal: state={} error={:?}",
        failed.state, failed.error_code
    );
    assert_eq!(failed.id, accepted.id);
    assert_eq!(failed.state, "failed");
    assert_eq!(
        failed.error_code.as_deref(),
        Some("invalid_request: artifact length")
    );
    assert_eq!(slots(active.world()), wanted);
    assert!(
        !a1_dir.join("batch").exists(),
        "superseded run must no longer be held after startup sweep"
    );
    let head = active.world().store.index_baseline().unwrap();
    assert_eq!(active.world().store.semantic_at(head).unwrap(), expected);
    let before = head.index_generation;
    let world = obsolete(active.finish());

    let (mut active, _, claim) = Active::start(world, true);
    active.resume_startup();
    let (first, second_claim) = active.terminal_then_hold_next(claim.unwrap()).await;
    assert_eq!(
        first.state, "done",
        "first recovered row: {:?}",
        first.error_code
    );
    let first_options: Value = serde_json::from_str(&first.options_json).unwrap();
    let first_id = first_options["generationId"].as_str().unwrap().to_owned();
    assert!(first_id == A2 || first_id == B1);
    second_claim
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    let observer =
        Store::open_for_tests(active.world()._state.path(), &active.world().root).unwrap();
    let interim = observer.semantic_at(first.revision.unwrap()).unwrap();
    let first_producer = if first_id == A2 { A } else { B };
    assert_eq!(interim.producers.len(), 1);
    assert_eq!(interim.producers[0].id, first_producer);
    assert!(
        interim
            .provenance
            .iter()
            .any(|p| p.producer_id == first_producer)
    );
    let expected_outstanding_language = if first_id == A2 { "java" } else { "javascript" };
    let status = active.runtime.cli_status().unwrap();
    let pending = status["semanticLanguages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["language"] == expected_outstanding_language)
        .unwrap();
    assert_eq!(pending["reimport"]["state"], "needsReimport");
    drop(observer);
    let second = active.terminal_started(None).await;
    assert_eq!(
        second.state, "done",
        "second recovered row: {:?}",
        second.error_code
    );
    assert!(first.seq < second.seq);
    let second_options: Value = serde_json::from_str(&second.options_json).unwrap();
    let second_id = second_options["generationId"].as_str().unwrap().to_owned();
    assert_eq!(
        BTreeSet::from([first_id.clone(), second_id.clone()]),
        BTreeSet::from([A2.to_owned(), B1.to_owned()])
    );
    assert_eq!(
        first_options["descriptorSha256"].as_str(),
        Some(if first_id == A2 {
            a2_sha.as_str()
        } else {
            b1_sha.as_str()
        })
    );
    assert_eq!(
        second_options["descriptorSha256"].as_str(),
        Some(if second_id == A2 {
            a2_sha.as_str()
        } else {
            b1_sha.as_str()
        })
    );
    let head = second.revision.unwrap();
    assert_ne!(head.index_generation, before);
    let observer =
        Store::open_for_tests(active.world()._state.path(), &active.world().root).unwrap();
    let actual = observer.semantic_at(head).unwrap();
    assert_eq!(
        actual, expected,
        "complete selected evidence of both producers"
    );
    let selected = observer
        .selected_raw_inputs_at(head)
        .unwrap()
        .into_iter()
        .map(|entry| {
            (
                entry.producer_id,
                entry.descriptor.generation.unwrap().generation_id,
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(selected, wanted);
    assert_eq!(slots(active.world()), wanted);
    let status = active.runtime.cli_status().unwrap();
    let rows = status["semanticLanguages"].as_array().unwrap();
    let js = rows.iter().find(|r| r["language"] == "javascript").unwrap();
    assert_eq!(js["semanticProducerId"], A);
    assert_eq!(js["semanticState"], "fresh");
    assert_eq!(js["reimport"], Value::Null);
    let java = rows.iter().find(|r| r["language"] == "java").unwrap();
    // The synthetic profile covers Java too, so #483 requires syntax-only
    // default selection despite both published contributions being retained.
    assert_eq!(java["semanticProducerId"], Value::Null);
    assert_eq!(java["semanticState"], "unavailable");
    assert_eq!(
        java["semanticNote"],
        "multiple semantic producers for java; showing syntax"
    );
    assert_eq!(java["reimport"], Value::Null);
    assert!(a2_dir.join("batch").is_dir() && a2_dir.join("out").is_dir());
    assert!(b1_dir.join("batch").is_dir() && b1_dir.join("out").is_dir());
    eprintln!(
        "multi-producer FIFO: A1/B1/A2 -> failed A3 -> done {first_id} -> done {second_id}; selected slots {wanted:?}"
    );
    drop(observer);
    let _ = active.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_fifo_reports_failures_without_publishing_evidence() {
    for damage in [
        "missing_descriptor",
        "changed_source",
        "truncated_descriptor",
        "corrupt_descriptor",
        "missing_artifact",
        "truncated_artifact",
        "corrupt_artifact",
        "wrong_source_hash",
        "wrong_producer",
        "unassigned_document",
    ] {
        let (world, dir, desc, _) = published();
        let descriptor_bytes = fs::read(&desc).unwrap();
        let parsed: Value = serde_json::from_slice(&descriptor_bytes).unwrap();
        let artifact_path = dir.join(batch::ARTIFACT_PATH);
        match damage {
            "missing_descriptor" => fs::remove_file(&desc).unwrap(),
            "changed_source" => {
                fs::write(world.root.join("a.js"), "function changed() {}\n").unwrap()
            }
            "truncated_descriptor" => {
                fs::write(&desc, &descriptor_bytes[..descriptor_bytes.len() / 2]).unwrap()
            }
            "corrupt_descriptor" => fs::write(&desc, b"not json").unwrap(),
            "missing_artifact" => fs::remove_file(&artifact_path).unwrap(),
            "truncated_artifact" => {
                let bytes = fs::read(&artifact_path).unwrap();
                fs::write(&artifact_path, &bytes[..bytes.len() / 2]).unwrap();
            }
            "corrupt_artifact" => {
                let mut bytes = fs::read(&artifact_path).unwrap();
                bytes[0] ^= 1;
                fs::write(&artifact_path, bytes).unwrap();
            }
            "wrong_producer" => {
                let (_, digest) =
                    edit_descriptor(&desc, |v| v["producer"]["name"] = json!("wrong-producer"));
                rebind_catalog_digest(&world, &digest);
            }
            "wrong_source_hash" => {
                let entry = parsed["pages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .position(|p| p["kind"] == "source")
                    .unwrap();
                let (_, digest) = semantic_batch_support::edit_page(&desc, entry, &dir, |page| {
                    page["rows"][0]["contentHash"] = json!("0".repeat(64))
                });
                rebind_catalog_digest(&world, &digest);
            }
            "unassigned_document" => {
                let entry = parsed["pages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .position(|p| p["kind"] == "coverage")
                    .unwrap();
                semantic_batch_support::edit_page(&desc, entry, &dir, |page| {
                    page["rows"].as_array_mut().unwrap().pop().unwrap();
                });
                let (_, digest) = edit_descriptor(&desc, |v| {
                    let count = v["pages"][entry]["rowCount"].as_u64().unwrap();
                    v["pages"][entry]["rowCount"] = json!(count - 1);
                    let count = v["totals"]["coveredDocuments"].as_u64().unwrap();
                    v["totals"]["coveredDocuments"] = json!(count - 1);
                });
                rebind_catalog_digest(&world, &digest);
            }
            _ => unreachable!(),
        }
        let world = obsolete(world);
        let (mut active, _, claim) = Active::start(world, damage != "missing_descriptor");
        active.resume_startup();
        let reason = if damage == "missing_descriptor" {
            "missing"
        } else if damage == "changed_source" {
            "no longer matches"
        } else {
            "normal validation"
        };
        if let Some(claim) = claim {
            let row = active.terminal(claim, Some(reason)).await;
            eprintln!(
                "retained damage={damage}: terminal state={} error={:?}",
                row.state, row.error_code
            );
            assert_eq!(row.state, "failed", "{damage}");
            let expected = match damage {
                "changed_source" => "capture_unavailable: captured revision not retained",
                "truncated_descriptor" | "corrupt_descriptor" => {
                    "invalid_request: descriptor digest"
                }
                "missing_artifact" => "invalid_request: selected role absent",
                "truncated_artifact" => "invalid_request: artifact length",
                "corrupt_artifact" => "invalid_request: artifact digest",
                "wrong_source_hash" => "invalid_request: unlisted detail",
                "wrong_producer" => "invalid_request: producer profile pin",
                "unassigned_document" => "invalid_request: coverage completion",
                _ => unreachable!(),
            };
            assert_eq!(row.error_code.as_deref(), Some(expected), "{damage}");
        } else {
            active.wait_startup_step();
            assert!(
                active
                    .world()
                    .store
                    .earliest_unfinished_request()
                    .unwrap()
                    .is_none()
            );
            let db = rusqlite::Connection::open_with_flags(
                active.world().store.request_db_path(),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let count: i64 = db
                .query_row(
                    "SELECT COUNT(*) FROM requests WHERE request_kind='semanticImport'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 0, "missing descriptor must not enqueue");
        }
        assert_unavailable(&active, reason);
        let _ = active.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intentionally_incomplete_batch_is_readmitted_when_coverage_is_assigned() {
    use semantic_batch_support::DocSpec;
    let world = fixture();
    let (dir, desc, sha) = world
        .write_run(ID, |root| {
            let mut fixture =
                BatchFixture::from_published(&world.index_db(), &world.root).synthetic();
            let docs = vec![
                DocSpec::new("a.js")
                    .occurrence(&[0, 9, 12], &[], "pkg run.", 1)
                    .symbol("pkg run.", 17, &[]),
            ];
            fixture.artifact("javascript", docs.clone());
            edit_scip(&fixture.scip_bytes("javascript", &docs), |index| {
                index.metadata.as_mut().unwrap().project_root = root.into()
            })
        })
        .unwrap();
    let descriptor: Value = serde_json::from_slice(&fs::read(&desc).unwrap()).unwrap();
    assert_eq!(descriptor["completion"], "partial");
    assert_eq!(descriptor["totals"]["coveredDocuments"], 1);
    assert_eq!(descriptor["totals"]["excludedDocuments"], 1);
    let pin = world.publish_run(ID, &dir, &desc, &sha);
    let expected = world.store.semantic_at(pin).unwrap();
    assert_eq!(expected.declaration_bindings.len(), 1);
    assert_eq!(expected.coverage.len(), 2);
    assert!(expected.coverage.iter().any(|c| c.document_path == "b.js"
        && c.state == trellis::semantic_evidence::CoverageState::Failed));
    let (mut active, _, claim) = Active::start(obsolete(world), true);
    active.resume_startup();
    let row = active
        .terminal(claim.unwrap(), Some("normal validation"))
        .await;
    eprintln!(
        "intentional partial batch: terminal state={} error={:?}",
        row.state, row.error_code
    );
    assert_eq!(row.state, "done");
    let observer =
        Store::open_for_tests(active.world()._state.path(), &active.world().root).unwrap();
    assert_eq!(
        observer.semantic_at(row.revision.unwrap()).unwrap(),
        expected
    );
    drop(observer);
    let _ = active.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn genuinely_partial_semantic_tuple_is_automatically_readmitted() {
    use semantic_batch_support::DocSpec;
    use trellis::producer::ProducerName;
    use trellis::semantic_evidence::CoverageState;
    let state = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    fs::write(root.join("lib.rs"), "#[derive(Debug)]\npub struct Point;\n\nmy_macro! { fn hidden() {} }\n\npub fn run() { helper(); }\npub fn helper() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), &root).unwrap();
    store.mcp_prepare_virgin_queue_for_tests().unwrap();
    let world = Inproc {
        _state: state,
        _workspace: temp,
        root,
        store,
        cancel: Arc::new(AtomicBool::new(false)),
    };
    world.publish_native();
    let (dir, desc, sha) = world
        .write_run_with_trust(
            ID,
            Target::Real(ProducerName::RustAnalyzer),
            "rust",
            TrustProfile::Trusted,
            |worktree| {
                let mut fixture = BatchFixture::from_published(&world.index_db(), &world.root)
                    .reference("rust-analyzer");
                let docs = vec![
                    DocSpec::new("lib.rs")
                        .occurrence(&[0, 9, 14], &[], "external pkg Debug#", 0)
                        .occurrence(&[1, 11, 16], &[], "pkg Point#", 1)
                        .occurrence(&[3, 0, 8], &[], "external pkg my_macro.", 0)
                        .occurrence(&[3, 15, 21], &[], "pkg hidden.", 1)
                        .occurrence(&[5, 7, 10], &[], "pkg run.", 1)
                        .occurrence(&[5, 15, 21], &[5, 15, 23], "pkg helper.", 0)
                        .occurrence(&[6, 7, 13], &[], "pkg helper.", 1)
                        .symbol("pkg run.", 17, &[])
                        .symbol("pkg helper.", 17, &[]),
                ];
                fixture.artifact("rust", docs.clone());
                edit_scip(&fixture.scip_bytes("rust", &docs), |index| {
                    index.metadata.as_mut().unwrap().project_root = worktree.into()
                })
            },
        )
        .unwrap();
    let pin = world.publish_run(ID, &dir, &desc, &sha);
    let expected = world.store.semantic_at(pin).unwrap();
    let tuple = expected
        .coverage
        .iter()
        .find(|c| c.document_path == "lib.rs")
        .unwrap();
    assert_eq!(tuple.state, CoverageState::Partial);
    assert_eq!(tuple.diagnostic.as_deref(), Some("unsupported_native_gap"));
    assert!(!expected.declaration_bindings.is_empty());
    let (mut active, _, claim) = Active::start(obsolete(world), true);
    active.resume_startup();
    let row = active
        .terminal(claim.unwrap(), Some("normal validation"))
        .await;
    eprintln!(
        "partial tuple: terminal state={} error={:?}",
        row.state, row.error_code
    );
    assert_eq!(row.state, "done");
    let observer =
        Store::open_for_tests(active.world()._state.path(), &active.world().root).unwrap();
    assert_eq!(
        observer.semantic_at(row.revision.unwrap()).unwrap(),
        expected
    );
    drop(observer);
    let _ = active.finish();
}
