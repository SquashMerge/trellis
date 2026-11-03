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
    language: &'static str,
}

impl Drop for World {
    fn drop(&mut self) {
        let marker = self.checkout.home.join("daemon.pid");
        if let Some(identity) = trellis::testing::daemon_marker_identity(&marker)
            && identity.signal(libc::SIGTERM).unwrap_or(false)
        {
            let pid = fs::read_to_string(&marker)
                .ok()
                .and_then(|s| s.parse().ok());
            let deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
            while identity.matches() && pid.is_none_or(trellis::testing::process_running) {
                if std::time::Instant::now() >= deadline {
                    eprintln!("fixture daemon did not exit after identity-validated SIGTERM");
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
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
        Self::with_named_hook("TRELLIS_TEST_SEMANTIC_CRASH_AFTER_PUBLISH_SOCKET", hook)
    }

    fn with_named_hook(name: &str, hook: Option<&Path>) -> Self {
        Self::for_language("javascript", name, hook)
    }

    fn for_language(language: &'static str, name: &str, hook: Option<&Path>) -> Self {
        Self::workspace(language, name, hook, false)
    }

    fn with_workspace(name: &str, hook: Option<&Path>, cross_document: bool) -> Self {
        Self::workspace("javascript", name, hook, cross_document)
    }

    fn workspace(
        language: &'static str,
        name: &str,
        hook: Option<&Path>,
        cross_document: bool,
    ) -> Self {
        let stub = tempfile::tempdir().unwrap();
        let marker = stub.path().join("invocations");
        let mut env = support::stub_env(stub.path(), &json!({"marker":marker}));
        env.push((
            "TRELLIS_TEST_PRODUCER_STUB_LANGUAGE".into(),
            language.into(),
        ));
        if let Some(hook) = hook {
            env.push((name.into(), hook.to_string_lossy().into_owned()));
        }
        let checkout = start_checkout(
            |root| {
                uniform_workspace(root, &[language], if cross_document { 2 } else { 1 });
                let (path, source) = if language == "rust" {
                    ("rust/u00000.rs", "fn u00000() { g(); }\nfn g() {}\n")
                } else {
                    (
                        "javascript/u00000.js",
                        "function u00000() { g(); }\nfunction g() {}\n",
                    )
                };
                fs::write(root.join(path), source).unwrap();
                if name == "multi-producer" && language == "javascript" {
                    fs::write(root.join("other.rs"), "fn other() {}\n").unwrap();
                    fs::create_dir(root.join(".trellis-semantic-batch-2")).unwrap();
                }
                if cross_document {
                    fs::write(
                        root.join("javascript/u00000.js"),
                        "function run() { target(); }\n",
                    )
                    .unwrap();
                    fs::write(root.join("javascript/u00001.js"), "function target() {}\n").unwrap();
                }
            },
            &env,
        );
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
            language,
        }
    }

    fn spawn_run(&self) -> Child {
        self.spawn_run_variant(0)
    }

    fn spawn_run_variant(&self, variant: u32) -> Child {
        let mut fixture =
            BatchFixture::from_published(&self.index, &self.checkout.root).synthetic();
        let (definition, call, target) = if self.language == "rust" {
            ([0, 3, 9], [0, 14, 15], [1, 3, 4])
        } else {
            ([0, 9, 15], [0, 20, 21], [1, 9, 10])
        };
        let paths = fixture.paths(self.language);
        let docs = if paths.len() == 2 {
            vec![
                semantic_batch_support::DocSpec::new("javascript/u00000.js")
                    .occurrence(&[0, 9, 12], &[], "pkg run.", 1)
                    .occurrence(&[0, 17, 23], &[0, 17, 25], "pkg target.", 0)
                    .symbol("pkg run.", 17, &[]),
                semantic_batch_support::DocSpec::new("javascript/u00001.js")
                    .occurrence(&[0, 9, 15], &[], "pkg target.", 1)
                    .symbol("pkg target.", 17, &[]),
            ]
        } else {
            paths
                .into_iter()
                .map(|doc| {
                    doc.occurrence(&definition, &[], "pkg u00000().", 1)
                        .occurrence(&call, &[], "pkg g().", 0)
                        .occurrence(&target, &[], "pkg g().", 1)
                        .symbol("pkg u00000().", 17, &[])
                        .symbol("pkg g().", 17, &[])
                })
                .collect::<Vec<_>>()
        };
        fixture.artifact(self.language, docs.clone());
        let bytes = edit_scip(&fixture.scip_bytes(self.language, &docs), |index| {
            let metadata = index.metadata.as_mut().unwrap();
            metadata.project_root = String::new();
            metadata
                .tool_info
                .as_mut()
                .unwrap()
                .arguments
                .push(format!("variant-{variant}"));
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
        self.run_once_variant(0)
    }

    fn run_once_variant(&self, variant: u32) -> (Value, Vec<u8>, Value) {
        let run = reply(
            "producer run",
            self.spawn_run_variant(variant).wait_with_output().unwrap(),
        );
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
        assert_eq!(version, 37);
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

    fn assert_semantic_sequence(&self, status: &Value, original: &Value) {
        use axum::{
            body::{Body, to_bytes},
            http::Request,
        };
        use tower::ServiceExt;

        let roots = TopologyRoots::isolated_for_tests(
            cache_dir(&self.checkout.home),
            data_dir(&self.checkout.home),
        );
        let identity =
            WorkspaceIdentity::discover(Some(&self.checkout.root), &self.checkout.root).unwrap();
        let store = trellis::store::Store::open(roots, identity).unwrap();
        let pin = serde_json::from_value(status["revision"].clone()).unwrap();
        let (seed, target) = store
            .with_native_reader(pin, |reader| {
                let mut inventory = reader.inventory()?;
                let document = loop {
                    let document = inventory.next_document()?.unwrap();
                    if document.key.language == self.language {
                        break document;
                    }
                };
                let rows = reader.document(&document)?;
                Ok((
                    rows.declarations
                        .iter()
                        .find(|d| d.name.as_deref() == Some("u00000"))
                        .unwrap()
                        .syntax_id
                        .clone(),
                    rows.declarations
                        .iter()
                        .find(|d| d.name.as_deref() == Some("g"))
                        .unwrap()
                        .syntax_id
                        .clone(),
                ))
            })
            .unwrap();
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let app = trellis::http::router(
            trellis::http::new(
                store,
                trellis::indexer::IndexOptions::new(self.checkout.root.clone()),
                token.into(),
                "127.0.0.1:7331".parse().unwrap(),
            )
            .unwrap(),
        );
        let view: Value = tokio::runtime::Runtime::new().unwrap().block_on(async {
            let response = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/sequence")
                        .header("host", "127.0.0.1:7331")
                        .header("authorization", format!("Bearer {token}"))
                        .header("content-type", "application/json")
                        .body(Body::from(
                            json!({"seed":seed,"expectedRevision":pin,"showAll":true}).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                .unwrap()
        });
        assert_eq!(view["revision"], status["revision"]);
        assert_eq!(view["semantic"]["tier"], "semantic", "{view}");
        assert_eq!(view["semantic"]["producerId"], PRODUCER);
        let step = view["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|step| step["target"] == target)
            .unwrap();
        assert_eq!(step["callEvidence"]["freshness"], "fresh");
        assert_eq!(step["callEvidence"]["boundaryReason"], "none");
        let basis = &step["callEvidence"]["semanticBasis"];
        assert_eq!(basis["revisionId"], original["sourceRevision"]);
        assert_eq!(
            basis["toolchainHash"],
            original["components"]["toolchainHash"]
        );
        assert_eq!(basis["artifactHash"], original["artifacts"][0]["sha256"]);
    }

    fn assert_recovered_basis(&self, status: &Value, original: &Value, fresh: bool) {
        let js = status["semanticLanguages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["language"] == "javascript")
            .unwrap();
        assert_eq!(js["reimport"], Value::Null);
        assert_eq!(js["semanticProducerId"], PRODUCER);
        assert_eq!(
            js["coverage"]["complete"],
            original["totals"]["coveredDocuments"]
        );
        assert_eq!(js["coverage"]["failed"], 0);
        assert!(
            js["internal"].as_u64().unwrap() > 0,
            "recovered semantic call target must remain usable"
        );
        assert_eq!(js["semanticState"], if fresh { "fresh" } else { "stale" });
        let key = format!(
            "pin:v1:{}:{}",
            status["revision"]["indexGeneration"].as_str().unwrap(),
            status["revision"]["indexRevision"].as_u64().unwrap()
        );
        let db =
            Connection::open_with_flags(&self.index, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let (proofs, expected): (i64, i64) = db.query_row(
            "SELECT COUNT(*),SUM(p.freshness=?3) FROM semantic_provenance p JOIN semantic_coverage c USING(tuple_id) WHERE c.revision_id=?1 AND c.producer_id=?2 AND c.selected=1",
            rusqlite::params![key, PRODUCER, if fresh { "fresh" } else { "possiblyStale" }], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert!(
            proofs > 0,
            "recovery must select nonempty semantic evidence"
        );
        assert_eq!(proofs, expected);
        let basis_revisions: Vec<String> = db.prepare(
            "SELECT DISTINCT b.captured_native_revision_id FROM semantic_basis b JOIN semantic_coverage c USING(tuple_id) WHERE c.revision_id=?1 AND c.producer_id=?2 AND c.selected=1",
        ).unwrap().query_map(rusqlite::params![key, PRODUCER], |row| row.get(0)).unwrap().map(Result::unwrap).collect();
        assert_eq!(
            basis_revisions,
            vec![original["sourceRevision"].as_str().unwrap().to_owned()]
        );
    }
}

#[test]
fn binary_only_retained_recovery_is_fresh_at_head() {
    binary_only_recovery("javascript");
}

#[test]
fn rust_binary_only_retained_recovery_is_fresh_at_head() {
    binary_only_recovery("rust");
}

#[test]
fn recovered_freshness_does_not_survive_real_context_changes() {
    for (path, bytes) in [
        ("Cargo.lock", "version = 4\n"),
        ("rust-toolchain.toml", "[toolchain]\nchannel = \"stable\"\n"),
        (".gitignore", "ignored-directory/\n"),
    ] {
        binary_only_recovery_with_drift("javascript", Some((path, bytes)));
    }
}

fn binary_only_recovery(language: &'static str) {
    binary_only_recovery_with_drift(language, None);
}

fn binary_only_recovery_with_drift(language: &'static str, drift: Option<(&str, &str)>) {
    use std::io::Write;
    let world = World::for_language(language, "multi-producer", None);
    let (_, original, options) = world.run_once();
    let descriptor: Value = serde_json::from_slice(&original).unwrap();
    let old = world.status()["revision"].clone();
    let read_context = || {
        let db =
            Connection::open_with_flags(&world.index, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        db.query_row("SELECT n.retained_toolchain,n.retained_config,n.retained_dependencies FROM native_revisions n JOIN index_metadata m ON n.id='pin:v1:'||m.index_generation||':'||m.index_revision", [], |row| Ok((row.get::<_, String>(0)?,row.get::<_, String>(1)?,row.get::<_, String>(2)?))).unwrap()
    };
    let original_context = read_context();
    let original_key = format!(
        "pin:v1:{}:{}",
        old["indexGeneration"].as_str().unwrap(),
        old["indexRevision"].as_u64().unwrap()
    );
    let original_proofs = || {
        let db =
            Connection::open_with_flags(&world.index, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        db.prepare("SELECT p.provenance_id,p.freshness,p.derived_from_provenance_id,p.derived_from_fact_digest FROM semantic_provenance p JOIN semantic_coverage c USING(tuple_id) WHERE c.revision_id=?1 AND c.selected=1 ORDER BY p.provenance_id")
            .unwrap().query_map([&original_key], |row| Ok((row.get::<_,String>(0)?, row.get::<_,String>(1)?, row.get::<_,Option<String>>(2)?,row.get::<_,Option<String>>(3)?)))
            .unwrap().map(Result::unwrap).collect::<Vec<_>>()
    };
    let captured_proofs = original_proofs();
    assert!(!captured_proofs.is_empty());
    let run_dir = Path::new(options["descriptor"].as_str().unwrap())
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let pages = descriptor["pages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|page| {
            let path = run_dir.join(page["relativePath"].as_str().unwrap());
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect::<Vec<_>>();
    support::stop_daemon(&world.checkout.home);
    let binary = world._stub.path().join("trellis-freshness");
    fs::copy(env!("CARGO_BIN_EXE_trellis"), &binary).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(&binary)
        .unwrap()
        .write_all(b"TRELLIS-RETAINED-FRESHNESS")
        .unwrap();
    let rebuilt_cli = || {
        let mut cli = trellis::testing::owned(std::process::Command::new(&binary));
        cli.env("HOME", &world.checkout.home)
            .env(
                "TRELLIS_TEST_DAEMON_PID_FILE",
                world.checkout.home.join("daemon.pid"),
            )
            .env_remove("XDG_CACHE_HOME")
            .env_remove("XDG_DATA_HOME");
        cli
    };
    reply(
        "binary-only index",
        rebuilt_cli()
            .args(["index", "--workspace"])
            .arg(&world.checkout.root)
            .output()
            .unwrap(),
    );
    trellis::testing::wait_until("binary-only retained input completes", || {
        let rows = world.generated_rows();
        rows.len() == 2 && matches!(rows[1].0.as_str(), "done" | "failed")
    });
    let rows = world.generated_rows();
    assert_eq!(rows[1].0, "done", "{rows:?}");
    assert_eq!(rows[1].2, options);
    let status = reply(
        "binary-only status",
        rebuilt_cli()
            .args(["status", "--workspace"])
            .arg(&world.checkout.root)
            .output()
            .unwrap(),
    );
    assert_eq!(
        status["revision"]["indexGeneration"],
        old["indexGeneration"]
    );
    let recovered_context = read_context();
    let without_binary = |text: &str| {
        let mut value: Value = serde_json::from_str(text).unwrap();
        value.as_object_mut().unwrap().remove("nativeExecutable");
        value
    };
    assert_eq!(
        without_binary(&original_context.0),
        without_binary(&recovered_context.0)
    );
    assert_eq!(original_context.1, recovered_context.1);
    assert_eq!(original_context.2, recovered_context.2);
    world.assert_semantic_sequence(&status, &descriptor);
    let language_row = status["semanticLanguages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["language"] == language)
        .unwrap();
    assert_eq!(language_row["semanticState"], "fresh");
    if language == "javascript" && drift.is_none() {
        let mut fixture = BatchFixture::from_published(&world.index, &world.checkout.root)
            .reference("rust-analyzer");
        let docs = fixture
            .paths("rust")
            .into_iter()
            .map(|doc| {
                doc.occurrence(
                    &[0, 3, 8],
                    &[],
                    "rust-analyzer cargo fixture 0.1.0 other().",
                    1,
                )
                .symbol("rust-analyzer cargo fixture 0.1.0 other().", 17, &[])
            })
            .collect::<Vec<_>>();
        fixture.artifact("rust", docs);
        let (batch, sha) = fixture.write(&world.checkout.root.join(".trellis-semantic-batch-2"));
        reply(
            "second producer staged publication",
            rebuilt_cli()
                .args(["semantic", "import", "--workspace"])
                .arg(&world.checkout.root)
                .arg("--batch")
                .arg(batch)
                .args(["--batch-sha256", &sha])
                .output()
                .unwrap(),
        );
        let later = reply(
            "later semantic-only status",
            rebuilt_cli()
                .args(["status", "--workspace"])
                .arg(&world.checkout.root)
                .output()
                .unwrap(),
        );
        assert!(
            later["revision"]["indexRevision"].as_u64().unwrap()
                > status["revision"]["indexRevision"].as_u64().unwrap()
        );
        world.assert_semantic_sequence(&later, &descriptor);
        let js = later["semanticLanguages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["language"] == "javascript")
            .unwrap();
        assert_eq!(js["semanticState"], "fresh");
        let db =
            Connection::open_with_flags(&world.index, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let selected: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM semantic_coverage c JOIN index_metadata m ON c.revision_id='pin:v1:'||m.index_generation||':'||m.index_revision WHERE c.selected=1 AND c.producer_id='rust-analyzer' AND c.language='rust')", [], |row| row.get(0)).unwrap();
        assert!(selected);
    }
    assert_eq!(
        original_proofs(),
        captured_proofs,
        "historical proofs must remain immutable"
    );
    for (path, bytes) in pages {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
    if let Some((path, bytes)) = drift {
        fs::write(world.checkout.root.join(path), bytes).unwrap();
        reply(
            "context drift index",
            rebuilt_cli()
                .args(["index", "--workspace"])
                .arg(&world.checkout.root)
                .output()
                .unwrap(),
        );
        let changed = reply(
            "context drift status",
            rebuilt_cli()
                .args(["status", "--workspace"])
                .arg(&world.checkout.root)
                .output()
                .unwrap(),
        );
        let language_row = changed["semanticLanguages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["language"] == language)
            .unwrap();
        assert_ne!(language_row["semanticState"], "fresh", "{path}: {changed}");
        assert_eq!(
            original_proofs(),
            captured_proofs,
            "historical proofs changed after {path}"
        );
    }
    assert_eq!(world.invocation_count(), 1);
    assert_eq!(
        fs::read(options["descriptor"].as_str().unwrap()).unwrap(),
        original
    );
    assert_eq!(descriptor["sourceRevision"].as_str().unwrap(), {
        let db =
            Connection::open_with_flags(&world.index, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        db.query_row("SELECT b.captured_native_revision_id FROM semantic_basis b JOIN semantic_coverage c USING(tuple_id) JOIN index_metadata m ON c.revision_id='pin:v1:'||m.index_generation||':'||m.index_revision WHERE c.selected=1 AND c.producer_id='trellis-synthetic-scip' LIMIT 1", [], |row| row.get::<_, String>(0)).unwrap()
    });
}

#[test]
fn changed_executable_readmits_retained_run_again_within_the_same_generation() {
    changed_executable_race(true, true);
}

#[test]
fn changed_executable_recovery_survives_restart_and_original_binary_return() {
    changed_executable_race(false, true);
}

#[test]
fn changed_executable_binary_only_compares_fresh_import() {
    changed_executable_race(true, false);
}

fn changed_executable_race(compare_fresh: bool, config_change: bool) {
    use protobuf::Message;
    use std::io::Write;

    let world = World::with_workspace("unused", None, true);
    let (_, original, options) = world.run_once();
    let descriptor: Value = serde_json::from_slice(&original).unwrap();
    let run_dir = Path::new(options["descriptor"].as_str().unwrap())
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let original_pages: Vec<_> = descriptor["pages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|page| {
            let path = run_dir.join(page["relativePath"].as_str().unwrap());
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    let artifact_path = run_dir.join(descriptor["artifacts"][0]["relativePath"].as_str().unwrap());
    let original_artifact = fs::read(&artifact_path).unwrap();
    let producer_out = fs::read(run_dir.join("out/index.scip")).unwrap();
    let emitted = fs::read(world._stub.path().join("index.scip")).unwrap();
    let parsed = scip::types::Index::parse_from_bytes(&producer_out).unwrap();
    let project_root = &parsed
        .metadata
        .as_ref()
        .expect("SCIP metadata")
        .project_root;
    eprintln!(
        "657 actual producer output: selected_path={} out_sha={} selected_sha={} emitted_sha={} metadata_project_root={project_root:?} descriptor_project_root={}",
        artifact_path.display(),
        semantic_batch_support::sha256(&producer_out),
        semantic_batch_support::sha256(&original_artifact),
        semantic_batch_support::sha256(&emitted),
        descriptor["metadata"]["projectRoot"]
    );
    assert_eq!(
        producer_out, emitted,
        "inspect actual producer output bytes"
    );
    assert_eq!(
        producer_out, original_artifact,
        "selected retained artifact must match producer output"
    );
    assert!(
        project_root.is_empty(),
        "actual producer output has a nonempty project root"
    );
    assert_eq!(
        descriptor["artifacts"][0]["sha256"],
        semantic_batch_support::sha256(&original_artifact)
    );
    let original_inventory: String = Connection::open_with_flags(&world.index, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap().query_row(
        "SELECT n.source_inventory FROM native_revisions n JOIN index_metadata m ON n.id='pin:v1:'||m.index_generation||':'||m.index_revision", [], |r| r.get(0)).unwrap();
    let context = || {
        Connection::open_with_flags(&world.index, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap().query_row(
        "SELECT n.retained_toolchain,n.retained_config,n.retained_dependencies FROM native_revisions n JOIN index_metadata m ON n.id='pin:v1:'||m.index_generation||':'||m.index_revision", [], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))).unwrap()
    };
    let original_context = context();
    let original_schema: u32 =
        Connection::open_with_flags(&world.index, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
    let old = world.status()["revision"].clone();
    support::stop_daemon(&world.checkout.home);
    // A previous attempt for this generation must not suppress a new capture identity.
    let catalog = Connection::open(state_dir(&world.checkout).join("retention.sqlite")).unwrap();
    catalog
        .execute(
            "UPDATE retained_runs SET attempted_generation=?1",
            [old["indexGeneration"].as_str().unwrap()],
        )
        .unwrap();
    drop(catalog);
    let binary = world._stub.path().join("trellis-rebuilt");
    fs::copy(env!("CARGO_BIN_EXE_trellis"), &binary).unwrap();
    let mut image = fs::OpenOptions::new().append(true).open(&binary).unwrap();
    image
        .write_all(b"TRELLIS-RETAINED-CAPTURE-IDENTITY-DRIFT")
        .unwrap();
    image.sync_all().unwrap();
    drop(image);
    let rebuilt_cli = || {
        let mut command = trellis::testing::owned(std::process::Command::new(&binary));
        command
            .env("HOME", &world.checkout.home)
            .env(
                "TRELLIS_TEST_DAEMON_PID_FILE",
                world.checkout.home.join("daemon.pid"),
            )
            .env_remove("XDG_CACHE_HOME")
            .env_remove("XDG_DATA_HOME");
        command
    };
    let native_identity = || {
        let db =
            Connection::open_with_flags(&world.index, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        db.query_row(
            "SELECT n.native_revision_id FROM native_revisions n JOIN index_metadata m
            ON n.id='pin:v1:'||m.index_generation||':'||m.index_revision",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
    };
    let old_native = native_identity();
    assert_ne!(
        trellis::native_ids::digest(b"", &fs::read(&binary).unwrap()),
        trellis::native_ids::digest(b"", &fs::read(env!("CARGO_BIN_EXE_trellis")).unwrap())
    );
    let socket = world._stub.path().join("after-startup.sock");
    let claim_socket = world._stub.path().join("retained-claim.sock");
    let mut command = rebuilt_cli();
    command
        .env("TRELLIS_TEST_RETAINED_AFTER_STARTUP_SOCKET", &socket)
        .env(
            "TRELLIS_TEST_SEMANTIC_CRASH_AFTER_CLAIM_SOCKET",
            &claim_socket,
        )
        .args(["index", "--workspace"])
        .arg(&world.checkout.root);
    if config_change {
        command.args(["--max-file-bytes", "1048576"]);
    }
    let index = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    trellis::testing::wait_until("startup capture held before retained admission", || {
        socket.with_extension("ready").exists()
    });
    let startup_native = native_identity();
    assert_ne!(startup_native, old_native);
    trellis::testing::wait_until(
        "post-start index accepted ahead of retained recovery",
        || {
            let db = Connection::open_with_flags(&world.requests, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
            db.query_row("SELECT EXISTS(SELECT 1 FROM requests WHERE request_kind='index' AND state='queued')", [], |row| row.get::<_, bool>(0)).unwrap()
        },
    );
    let queued_index: (i64, String) =
        Connection::open_with_flags(&world.requests, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap()
            .query_row(
                "SELECT seq,id FROM requests WHERE request_kind='index' AND state='queued'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
    assert_eq!(
        world.generated_rows().len(),
        1,
        "retained input not enqueued before hook release"
    );
    std::os::unix::net::UnixStream::connect(&socket).unwrap();
    trellis::testing::wait_until("retained claim held after earlier native request", || {
        claim_socket.exists()
    });
    let at_claim = native_identity();
    if config_change {
        assert_ne!(
            at_claim, startup_native,
            "post-start native head must advance before retained claim"
        );
    } else {
        assert_eq!(at_claim, startup_native);
    }
    let destination_context = context();
    let without_binary = |s: &str| {
        let mut v: Value = serde_json::from_str(s).unwrap();
        v.as_object_mut().unwrap().remove("nativeExecutable");
        v
    };
    assert_eq!(
        without_binary(&original_context.0),
        without_binary(&destination_context.0)
    );
    assert_eq!(original_context.2, destination_context.2);
    if config_change {
        assert_ne!(original_context.1, destination_context.1);
    } else {
        assert_eq!(original_context.1, destination_context.1);
    }
    let db =
        Connection::open_with_flags(&world.requests, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let index_row: (String, i64) = db
        .query_row(
            "SELECT state,result_revision FROM requests WHERE id=?1",
            [&queued_index.1],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let retained_row: (i64, String, String) = db.query_row("SELECT seq,id,state FROM requests WHERE request_kind='semanticImport' ORDER BY seq DESC LIMIT 1", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(index_row.0, "done");
    assert!(queued_index.0 < retained_row.0);
    assert_eq!(retained_row.2, "running");
    let db_index =
        Connection::open_with_flags(&world.index, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let (revision, inventory, producer_sha): (String, String, String) = db_index.query_row("SELECT n.native_revision_id,n.source_inventory,b.producer_sha FROM native_revisions n JOIN revision_producer_bindings b ON b.revision_id=n.id WHERE n.published_index_revision=?1", [index_row.1], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(revision, at_claim);
    let old_inventory: Vec<Value> = serde_json::from_str(&original_inventory).unwrap();
    let new_inventory: Vec<Value> = serde_json::from_str(&inventory).unwrap();
    assert_eq!(old_inventory.len(), 2);
    assert_eq!(old_inventory.len(), new_inventory.len());
    for (old_doc, new_doc) in old_inventory.iter().zip(&new_inventory) {
        assert_eq!(old_doc["revisionId"], descriptor["sourceRevision"]);
        assert_eq!(new_doc["revisionId"], at_claim);
        assert_eq!(
            (
                &old_doc["key"],
                &old_doc["contentHash"],
                &old_doc["byteLength"]
            ),
            (
                &new_doc["key"],
                &new_doc["contentHash"],
                &new_doc["byteLength"]
            )
        );
    }
    let source_rows: Vec<Value> = descriptor["pages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|page| page["kind"] == "source")
        .flat_map(|page| {
            let bytes = fs::read(run_dir.join(page["relativePath"].as_str().unwrap())).unwrap();
            serde_json::from_slice::<Value>(&bytes).unwrap()["rows"]
                .as_array()
                .unwrap()
                .clone()
        })
        .collect();
    assert_eq!(source_rows.len(), new_inventory.len());
    for (source, destination) in source_rows.iter().zip(&new_inventory) {
        assert_eq!(source["language"], destination["key"]["language"]);
        assert_eq!(source["relativePath"], destination["key"]["path"]);
        assert_eq!(source["contentHash"], destination["contentHash"]);
        assert_eq!(source["byteLength"], destination["byteLength"]);
        assert_eq!(
            descriptor["sourceSet"]["id"],
            destination["key"]["sourceSetId"]
        );
    }
    let binary_sha = trellis::native_ids::digest(b"", &fs::read(&binary).unwrap());
    assert_eq!(producer_sha, binary_sha);
    let schema: u32 = db_index
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(schema, original_schema);
    eprintln!(
        "656 startup={startup_native} destination={at_claim} index_seq={} index_id={} index_pin={} retained_seq={} retained_id={} state={} binary_sha={} schema={schema} inventory={inventory}",
        queued_index.0,
        queued_index.1,
        index_row.1,
        retained_row.0,
        retained_row.1,
        retained_row.2,
        producer_sha
    );
    drop(db_index);
    drop(db);
    fs::write(claim_socket.with_extension("done"), []).unwrap();
    std::os::unix::net::UnixStream::connect(&claim_socket).unwrap();
    reply(
        "index under changed executable",
        index.wait_with_output().unwrap(),
    );
    assert_eq!(native_identity(), at_claim);
    trellis::testing::wait_until(
        "new native capture identity readmits retained input",
        || {
            let rows = world.generated_rows();
            rows.len() == 2 && matches!(rows[1].0.as_str(), "done" | "failed")
        },
    );
    let rows = world.generated_rows();
    assert_eq!(
        rows[1].2, options,
        "readmission must keep original capture identity"
    );
    // Use the running rebuilt binary, not the original CLI's executable pin.
    let status = reply(
        "rebuilt status",
        rebuilt_cli()
            .args(["status", "--workspace"])
            .arg(&world.checkout.root)
            .output()
            .unwrap(),
    );
    assert_eq!(
        status["revision"]["indexGeneration"],
        old["indexGeneration"]
    );
    let js = status["semanticLanguages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["language"] == "javascript")
        .unwrap();
    assert_eq!(
        rows[1].0, "done",
        "retained recovery failed after own post-start capture: {rows:?}"
    );
    let terminal_db =
        Connection::open_with_flags(&world.requests, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let terminal: (String, Option<String>, String, i64) = terminal_db
        .query_row(
            "SELECT state,error_code,result_generation,result_revision FROM requests WHERE id=?1",
            [&retained_row.1],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(terminal.0, "done");
    assert!(terminal.1.is_none());
    assert_eq!(
        terminal.2,
        status["revision"]["indexGeneration"].as_str().unwrap()
    );
    assert_eq!(
        terminal.3,
        status["revision"]["indexRevision"].as_i64().unwrap()
    );
    eprintln!(
        "656 terminal retained_id={} state={} error={:?} pin={}:{} old_binary_sha={} new_binary_sha={}",
        retained_row.1,
        terminal.0,
        terminal.1,
        terminal.2,
        terminal.3,
        trellis::native_ids::digest(b"", &fs::read(env!("CARGO_BIN_EXE_trellis")).unwrap()),
        binary_sha
    );
    drop(terminal_db);
    assert_eq!(js["reimport"], Value::Null);
    world.assert_recovered_basis(&status, &descriptor, !config_change);
    let proof_derivations = |pin: &Value| {
        let key = format!(
            "pin:v1:{}:{}",
            pin["indexGeneration"].as_str().unwrap(),
            pin["indexRevision"].as_u64().unwrap()
        );
        let db =
            Connection::open_with_flags(&world.index, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let total: i64 = db.query_row("SELECT COUNT(*) FROM semantic_provenance p JOIN semantic_coverage c USING(tuple_id) WHERE c.revision_id=?1 AND c.producer_id=?2 AND c.selected=1", rusqlite::params![key, PRODUCER], |r| r.get(0)).unwrap();
        let mut stmt = db.prepare("SELECT c.language,c.path,p.evidence_kind,p.derived_from_provenance_id,p.derived_from_record_kind,p.derived_from_fact_digest FROM semantic_provenance p JOIN semantic_coverage c USING(tuple_id) JOIN semantic_raw_proofs r ON r.capture_id=p.capture_id AND r.proof_id=p.derived_from_provenance_id AND r.record_kind=p.derived_from_record_kind AND r.fact_digest=p.derived_from_fact_digest AND r.language=c.language AND r.path=c.path WHERE c.revision_id=?1 AND c.producer_id=?2 AND c.selected=1 ORDER BY 1,2,3,4,5,6").unwrap();
        let rows = stmt
            .query_map(rusqlite::params![key, PRODUCER], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        assert!(total > 0);
        assert_eq!(
            total as usize,
            rows.len(),
            "every selected proof must uniquely resolve its raw derivation"
        );
        rows
    };
    assert_eq!(
        proof_derivations(&old),
        proof_derivations(&status["revision"]),
        "recovery must preserve original raw proof derivations"
    );
    assert!(rows[1].1.is_empty());
    assert_eq!(fs::read(&artifact_path).unwrap(), original_artifact);

    if !compare_fresh {
        support::stop_daemon(&world.checkout.home);
        reply(
            "unchanged rebuilt restart",
            rebuilt_cli()
                .args(["index", "--workspace"])
                .arg(&world.checkout.root)
                .args(["--max-file-bytes", "1048576"])
                .output()
                .unwrap(),
        );
        assert_eq!(world.generated_rows().len(), 2);
        assert_eq!(native_identity(), at_claim);
        support::stop_daemon(&world.checkout.home);
        reply(
            "return to original executable",
            world
                .checkout
                .cli()
                .args(["index", "--workspace"])
                .arg(&world.checkout.root)
                .args(["--max-file-bytes", "1048576"])
                .output()
                .unwrap(),
        );
        trellis::testing::wait_until("next distinct native identity retries once", || {
            let rows = world.generated_rows();
            rows.len() == 3 && matches!(rows[2].0.as_str(), "done" | "failed")
        });
        let returned = world.generated_rows();
        assert_eq!(
            returned[2].0, "done",
            "returning executable recovery failed: {returned:?}"
        );
        assert!(returned[2].1.is_empty());
        assert_eq!(returned[2].2, options);
        let returned_status = world.status();
        world.assert_recovered_basis(&returned_status, &descriptor, false);
        assert_eq!(
            returned_status["revision"]["indexGeneration"],
            old["indexGeneration"]
        );
        assert_eq!(
            fs::read(options["descriptor"].as_str().unwrap()).unwrap(),
            original
        );
        assert_eq!(fs::read(&artifact_path).unwrap(), original_artifact);
        assert_eq!(world.invocation_count(), 1);
        for (path, bytes) in original_pages {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
        eprintln!(
            "656 restart control: unchanged identity admitted no new request; return-original produced one done retained request; same generation and original basis"
        );
        return;
    }

    // A fresh, normally queued external import at the very same native head,
    // using the exact retained producer artifact, including its empty SCIP root.
    let roots = TopologyRoots::isolated_for_tests(
        cache_dir(&world.checkout.home),
        data_dir(&world.checkout.home),
    );
    let identity =
        WorkspaceIdentity::discover(Some(&world.checkout.root), &world.checkout.root).unwrap();
    let observer = trellis::store::Store::open(roots.clone(), identity).unwrap();
    let retained_pin: trellis::model::IndexPin =
        serde_json::from_value(status["revision"].clone()).unwrap();
    let retained = observer.semantic_at(retained_pin).unwrap();
    assert_eq!(native_identity(), at_claim);
    assert_eq!(retained.coverage.iter().filter(|c| c.selected && c.state == trellis::semantic_evidence::CoverageState::Complete).count(), 2);
    assert_eq!(retained.declaration_bindings.len(), 2);
    assert_eq!(retained.call_bindings.len(), 1);
    assert_eq!(
        retained.call_bindings[0].resolution,
        trellis::semantic_evidence::Resolution::Resolved
    );
    assert!(
        matches!(&retained.call_bindings[0].declared_target, Some(trellis::semantic_evidence::Target::Internal { document, .. }) if document.path == "javascript/u00001.js")
    );
    assert_eq!(
        retained.call_bindings[0].join.anchor.document.path,
        "javascript/u00000.js"
    );
    drop(observer);
    let mut fixture = BatchFixture::from_published(&world.index, &world.checkout.root).synthetic();
    let docs = fixture.paths("javascript");
    let ordinal = fixture.artifact("javascript", docs);
    let fresh_artifact = original_artifact.clone();
    fixture.override_artifact_raw(ordinal, fresh_artifact.clone());
    let (fresh_desc, fresh_sha) =
        fixture.write(&world.checkout.root.join(".trellis-semantic-batch-656"));
    let fresh_json: Value = serde_json::from_slice(&fs::read(&fresh_desc).unwrap()).unwrap();
    let written_artifact = fs::read(
        world
            .checkout
            .root
            .join(fresh_json["artifacts"][0]["relativePath"].as_str().unwrap()),
    )
    .unwrap();
    assert_eq!(written_artifact, original_artifact);
    assert_eq!(
        fresh_json["artifacts"][0]["sha256"],
        descriptor["artifacts"][0]["sha256"]
    );
    eprintln!(
        "657 same-artifact external attempt config_change={config_change} bytes={} sha={} external_descriptor_root={}",
        written_artifact.len(),
        semantic_batch_support::sha256(&written_artifact),
        fresh_json["metadata"]["projectRoot"]
    );
    assert_eq!(fresh_json["sourceRevision"], at_claim);
    assert_ne!(fresh_json["sourceRevision"], descriptor["sourceRevision"]);
    let response = reply(
        "fresh normal import",
        rebuilt_cli()
            .args(["semantic", "import", "--workspace"])
            .arg(&world.checkout.root)
            .arg("--batch")
            .arg(&fresh_desc)
            .args(["--batch-sha256", &fresh_sha])
            .output()
            .unwrap(),
    );
    let fresh_pin: trellis::model::IndexPin =
        serde_json::from_value(response["publishedRevision"].clone()).unwrap();
    let db = Connection::open_with_flags(&world.index, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let fresh_key = format!(
        "pin:v1:{}:{}",
        fresh_pin.index_generation, fresh_pin.index_revision
    );
    let fresh_native: String = db
        .query_row(
            "SELECT native_revision_id FROM native_revisions WHERE id=?1",
            [&fresh_key],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        fresh_native, at_claim,
        "fresh import must publish over the same new native head"
    );
    let (proofs, fresh_proofs): (i64, i64) = db.query_row("SELECT COUNT(*), SUM(p.freshness='fresh') FROM semantic_provenance p JOIN semantic_coverage c USING(tuple_id) WHERE c.revision_id=?1 AND c.selected=1", [&fresh_key], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert!(proofs > 0);
    assert_eq!(proofs, fresh_proofs);
    drop(db);
    let identity =
        WorkspaceIdentity::discover(Some(&world.checkout.root), &world.checkout.root).unwrap();
    let observer = trellis::store::Store::open(roots, identity).unwrap();
    let fresh = observer.semantic_at(fresh_pin).unwrap();
    let original_artifact_sha = semantic_batch_support::sha256(&original_artifact);
    let fresh_artifact_sha = semantic_batch_support::sha256(&fresh_artifact);
    assert_eq!(original_artifact_sha, fresh_artifact_sha);
    let retained_key = format!(
        "pin:v1:{}:{}",
        retained_pin.index_generation, retained_pin.index_revision
    );
    let raw_proof = |key: &str, proof_id: &str| {
        let db =
            Connection::open_with_flags(&world.index, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut stmt = db.prepare("SELECT p.derived_from_provenance_id,p.derived_from_record_kind,p.derived_from_fact_digest,r.canonical_bytes FROM semantic_provenance p JOIN semantic_coverage c USING(tuple_id) JOIN semantic_raw_proofs r ON r.capture_id=p.capture_id AND r.proof_id=p.derived_from_provenance_id AND r.record_kind=p.derived_from_record_kind AND r.fact_digest=p.derived_from_fact_digest AND r.language=c.language AND r.path=c.path WHERE c.revision_id=?1 AND c.selected=1 AND c.producer_id=?2 AND p.provenance_id=?3").unwrap();
        let matches = stmt
            .query_map(rusqlite::params![key, PRODUCER, proof_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Vec<u8>>(3)?,
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        assert_eq!(
            matches.len(),
            1,
            "each claim must uniquely resolve its raw derivation"
        );
        matches.into_iter().next().unwrap()
    };
    let check_claim_provenance = |old_id: &str, fresh_id: &str, document_path: &str| {
        let old_matches: Vec<_> = retained
            .provenance
            .iter()
            .filter(|p| p.id == old_id)
            .collect();
        let fresh_matches: Vec<_> = fresh
            .provenance
            .iter()
            .filter(|p| p.id == fresh_id)
            .collect();
        assert_eq!(
            old_matches.len(),
            1,
            "retained claim provenance must resolve uniquely"
        );
        assert_eq!(
            fresh_matches.len(),
            1,
            "fresh claim provenance must resolve uniquely"
        );
        let (old, new) = (old_matches[0], fresh_matches[0]);
        assert_eq!(old.producer_id, PRODUCER);
        assert_eq!(old.producer_id, new.producer_id);
        assert_eq!(old.document, new.document);
        assert_eq!(old.document.path, document_path);
        assert_eq!(old.evidence_kind, new.evidence_kind);
        let old_raw = raw_proof(&retained_key, old_id);
        let new_raw = raw_proof(&fresh_key, fresh_id);
        assert_eq!(
            (&old_raw.1, &old_raw.2, &old_raw.3),
            (&new_raw.1, &new_raw.2, &new_raw.3),
            "corresponding raw kind, fact digest and canonical bytes"
        );
        assert_eq!(
            old.freshness,
            if config_change {
                trellis::semantic_evidence::Freshness::PossiblyStale
            } else {
                trellis::semantic_evidence::Freshness::Fresh
            }
        );
        assert_eq!(new.freshness, trellis::semantic_evidence::Freshness::Fresh);
        let old_basis = old.basis.as_ref().expect("retained claim basis");
        let new_basis = new.basis.as_ref().expect("fresh claim basis");
        assert_eq!(
            old_basis.revision_id,
            descriptor["sourceRevision"].as_str().unwrap()
        );
        assert_eq!(new_basis.revision_id, at_claim);
        assert_eq!(old_basis.artifact_hash, original_artifact_sha);
        assert_eq!(new_basis.artifact_hash, fresh_artifact_sha);
        assert_eq!(old_basis.producer_id, PRODUCER);
        assert_eq!(new_basis.producer_id, PRODUCER);
        assert_eq!(old_basis.source_set_id, old.document.source_set_id);
        assert_eq!(new_basis.source_set_id, new.document.source_set_id);
        eprintln!(
            "657 claim provenance document={} kind={:?} retained_id={} fresh_id={} retained_raw_id={} fresh_raw_id={} raw_kind={} fact_digest={} original_basis={} fresh_basis={}",
            document_path,
            old.evidence_kind,
            old.id,
            new.id,
            old_raw.0,
            new_raw.0,
            old_raw.1,
            old_raw.2,
            old_basis.revision_id,
            new_basis.revision_id
        );
    };
    assert_eq!(retained.producers, fresh.producers);
    assert_eq!(retained.coverage, fresh.coverage);
    assert_eq!(retained.symbols.len(), 2);
    assert_eq!(retained.symbols.len(), fresh.symbols.len());
    for (a, b) in retained.symbols.iter().zip(&fresh.symbols) {
        assert_eq!(
            (&a.key, &a.display_name, &a.declarations),
            (&b.key, &b.display_name, &b.declarations)
        );
        assert_eq!(a.declarations.len(), 1);
        let trellis::semantic_evidence::Target::Internal { document, .. } = &a.declarations[0]
        else {
            panic!("fixture symbol must have an internal declaration");
        };
        check_claim_provenance(&a.provenance_id, &b.provenance_id, &document.path);
    }
    assert_eq!(
        retained.declaration_bindings.len(),
        fresh.declaration_bindings.len()
    );
    for (a, b) in retained
        .declaration_bindings
        .iter()
        .zip(&fresh.declaration_bindings)
    {
        assert_eq!(
            (&a.syntax_id, &a.symbols, &a.join),
            (&b.syntax_id, &b.symbols, &b.join)
        );
        check_claim_provenance(
            &a.provenance_id,
            &b.provenance_id,
            &a.join.anchor.document.path,
        );
    }
    assert_eq!(retained.references.len(), 3);
    assert_eq!(retained.references.len(), fresh.references.len());
    for (a, b) in retained.references.iter().zip(&fresh.references) {
        assert_eq!(
            (
                &a.id,
                &a.owner_syntax_id,
                a.ordinal,
                &a.document,
                &a.revision_id,
                &a.range,
                &a.spelling,
                &a.lookup_key
            ),
            (
                &b.id,
                &b.owner_syntax_id,
                b.ordinal,
                &b.document,
                &b.revision_id,
                &b.range,
                &b.spelling,
                &b.lookup_key
            )
        );
        check_claim_provenance(&a.provenance_id, &b.provenance_id, &a.document.path);
        assert_eq!(
            (
                &a.site,
                &a.roles,
                &a.resolution,
                &a.declared_target,
                &a.candidates
            ),
            (
                &b.site,
                &b.roles,
                &b.resolution,
                &b.declared_target,
                &b.candidates
            )
        );
    }
    assert!(retained.type_relationships.is_empty());
    assert_eq!(
        retained.type_relationships.len(),
        fresh.type_relationships.len()
    );
    for (a, b) in retained
        .type_relationships
        .iter()
        .zip(&fresh.type_relationships)
    {
        assert_eq!(
            (&a.kind, &a.source, &a.target),
            (&b.kind, &b.source, &b.target)
        );
        let trellis::semantic_evidence::Target::Internal { document, .. } = &a.source else {
            panic!("fixture relationship source must be internal");
        };
        check_claim_provenance(&a.provenance_id, &b.provenance_id, &document.path);
    }
    assert_eq!(fresh.call_bindings.len(), 1);
    let a = &retained.call_bindings[0];
    let b = &fresh.call_bindings[0];
    check_claim_provenance(
        &a.provenance_id,
        &b.provenance_id,
        &a.join.anchor.document.path,
    );
    assert_eq!(
        (
            &a.call_id,
            &a.join,
            &a.resolution,
            &a.declared_target,
            &a.candidates,
            &a.dispatch,
            &a.possible_dispatch,
            a.possible_dispatch_complete,
            a.stale_target
        ),
        (
            &b.call_id,
            &b.join,
            &b.resolution,
            &b.declared_target,
            &b.candidates,
            &b.dispatch,
            &b.possible_dispatch,
            b.possible_dispatch_complete,
            b.stale_target
        )
    );
    eprintln!(
        "656 fresh comparator config_change={config_change} retained_pin={retained_pin:?} fresh_pin={fresh_pin:?} original_artifact_sha={} fresh_artifact_sha={} retained_basis={} fresh_basis={} symbols={} declarations={} calls={} references={} type_relationships={}; claim provenance producer/document/kind and each capture basis checked",
        semantic_batch_support::sha256(&original_artifact),
        semantic_batch_support::sha256(&fresh_artifact),
        descriptor["sourceRevision"],
        fresh_json["sourceRevision"],
        retained.symbols.len(),
        retained.declaration_bindings.len(),
        retained.call_bindings.len(),
        retained.references.len(),
        retained.type_relationships.len()
    );
    drop(observer);
    assert_eq!(
        fs::read(options["descriptor"].as_str().unwrap()).unwrap(),
        original
    );
    assert_eq!(world.invocation_count(), 1);
    for (path, bytes) in original_pages {
        assert_eq!(
            fs::read(path).unwrap(),
            bytes,
            "recovery must not remint source or coverage pages"
        );
    }
}

#[test]
fn retained_recovery_refuses_changed_source_pages_before_binding_destination() {
    use std::io::Write;

    let world = World::new();
    let (_, original, options) = world.run_once();
    let old_generation = world.status()["revision"]["indexGeneration"].clone();
    support::stop_daemon(&world.checkout.home);
    let binary = world._stub.path().join("trellis-changed-source");
    fs::copy(env!("CARGO_BIN_EXE_trellis"), &binary).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(&binary)
        .unwrap()
        .write_all(b"TRELLIS-RETAINED-SOURCE-PARITY")
        .unwrap();
    let socket = world._stub.path().join("source-before-bind.sock");
    let mut cli = trellis::testing::owned(std::process::Command::new(&binary));
    let index = cli
        .env("HOME", &world.checkout.home)
        .env(
            "TRELLIS_TEST_DAEMON_PID_FILE",
            world.checkout.home.join("daemon.pid"),
        )
        .env("TRELLIS_TEST_RETAINED_AFTER_STARTUP_SOCKET", &socket)
        .env_remove("XDG_CACHE_HOME")
        .env_remove("XDG_DATA_HOME")
        .args(["index", "--workspace"])
        .arg(&world.checkout.root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    trellis::testing::wait_until("new binary H held before recovery enqueue", || {
        socket.with_extension("ready").exists()
    });
    trellis::testing::wait_until("own native request accepted before source edit", || {
        let db =
            Connection::open_with_flags(&world.requests, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM requests WHERE request_kind='index' AND state='queued')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .unwrap()
    });
    fs::write(
        world.checkout.root.join("javascript/u00000.js"),
        "function changed() { g(); }\nfunction g() {}\n",
    )
    .unwrap();
    std::os::unix::net::UnixStream::connect(&socket).unwrap();
    reply(
        "changed-source rebuilt index",
        index.wait_with_output().unwrap(),
    );
    trellis::testing::wait_until("source parity refusal reaches terminal row", || {
        let rows = world.generated_rows();
        rows.len() == 2 && matches!(rows[1].0.as_str(), "done" | "failed")
    });
    let rows = world.generated_rows();
    assert_eq!(rows[1].0, "failed");
    assert_eq!(
        rows[1].1,
        "stale_basis: retained source pages no longer match"
    );
    assert_eq!(rows[1].2, options);
    assert_eq!(
        fs::read(options["descriptor"].as_str().unwrap()).unwrap(),
        original
    );
    let db = Connection::open_with_flags(&world.index, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let generation: String = db
        .query_row("SELECT index_generation FROM index_metadata", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(generation, old_generation.as_str().unwrap());
    let selected: i64 = db.query_row("SELECT COUNT(*) FROM semantic_coverage c JOIN index_metadata m ON c.revision_id='pin:v1:'||m.index_generation||':'||m.index_revision WHERE selected=1", [], |row| row.get(0)).unwrap();
    assert_eq!(
        selected, 0,
        "source mismatch must not publish a retained overlay"
    );
    let reasons: i64 = db.query_row("SELECT COUNT(*) FROM semantic_reimport r JOIN index_metadata m ON r.revision_id='pin:v1:'||m.index_generation||':'||m.index_revision", [], |row| row.get(0)).unwrap();
    assert!(reasons > 0);
    assert_eq!(world.invocation_count(), 1);
}

#[test]
fn fresh_trellis_run_keeps_strict_basis_without_a_successful_retention_slot() {
    let hooks = tempfile::tempdir().unwrap();
    let socket = hooks.path().join("claim.sock");
    let world = World::with_named_hook(
        "TRELLIS_TEST_SEMANTIC_CRASH_AFTER_CLAIM_SOCKET",
        Some(&socket),
    );
    let run = world.spawn_run();
    trellis::testing::wait_until("fresh generated request claimed before admission", || {
        socket.exists()
    });
    assert_eq!(world.generated_rows()[0].0, "running");
    fs::write(
        world.checkout.root.join("javascript/u00000.js"),
        "function changed() { other(); }\n",
    )
    .unwrap();
    fs::write(socket.with_extension("done"), []).unwrap();
    std::os::unix::net::UnixStream::connect(&socket).unwrap();
    let output = run.wait_with_output().unwrap();
    assert!(!output.status.success());
    let rows = world.generated_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "failed");
    assert_eq!(rows[0].1, "stale_basis: head revision changed");
    assert_eq!(rows[0].2["origin"], "trellisRun");
    assert_eq!(world.invocation_count(), 1);
}

#[test]
fn retained_recovery_does_not_follow_a_later_native_change_at_cutoff() {
    let hooks = tempfile::tempdir().unwrap();
    let socket = hooks.path().join("cutoff.sock");
    let after_startup = hooks.path().join("after-startup.sock");
    let world = World::with_named_hook("TRELLIS_TEST_SEMANTIC_CUTOFF_SOCKET", Some(&socket));
    let (_, original, options) = world.run_once();
    support::stop_daemon(&world.checkout.home);
    use std::io::Write;
    let binary = world._stub.path().join("trellis-cutoff");
    fs::copy(env!("CARGO_BIN_EXE_trellis"), &binary).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(&binary)
        .unwrap()
        .write_all(b"TRELLIS-RETAINED-CUTOFF")
        .unwrap();
    let rebuilt_cli = || {
        let mut cli = trellis::testing::owned(std::process::Command::new(&binary));
        cli.env("HOME", &world.checkout.home)
            .env(
                "TRELLIS_TEST_DAEMON_PID_FILE",
                world.checkout.home.join("daemon.pid"),
            )
            .env_remove("XDG_CACHE_HOME")
            .env_remove("XDG_DATA_HOME");
        cli
    };
    let index = rebuilt_cli()
        .env("TRELLIS_TEST_SEMANTIC_CUTOFF_SOCKET", &socket)
        .env("TRELLIS_TEST_RETAINED_AFTER_STARTUP_SOCKET", &after_startup)
        .args(["index", "--workspace"])
        .arg(&world.checkout.root)
        .args(["--max-file-bytes", "1048576"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    trellis::testing::wait_until("startup held before retained recovery enqueue", || {
        after_startup.with_extension("ready").exists()
    });
    trellis::testing::wait_until("index accepted ahead of retained cutoff", || {
        let db =
            Connection::open_with_flags(&world.requests, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM requests WHERE request_kind='index' AND state='queued')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .unwrap()
    });
    let descriptor: Value = serde_json::from_slice(&original).unwrap();
    let db = Connection::open_with_flags(&world.index, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let captured: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM native_revisions r WHERE native_revision_id=?1 AND NOT EXISTS(SELECT 1 FROM revision_capture_inputs WHERE revision_id=r.id AND input_key IN ('__released:v1','__pending_release:v1')))",
        [descriptor["sourceRevision"].as_str().unwrap()], |row| row.get(0),
    ).unwrap();
    assert!(
        captured,
        "original capture must still be retained before recovery admission"
    );
    drop(db);
    std::os::unix::net::UnixStream::connect(&after_startup).unwrap();
    trellis::testing::wait_until("retained recovery bound before publication cutoff", || {
        let rows = world.generated_rows();
        assert!(
            !rows
                .get(1)
                .is_some_and(|row| matches!(row.0.as_str(), "done" | "failed")),
            "recovery terminalized before cutoff hook: {rows:?}"
        );
        socket.with_extension("ready").exists()
    });
    assert_eq!(world.generated_rows()[1].0, "running");
    fs::write(
        world.checkout.root.join("javascript/u00000.js"),
        "function changed() { later(); }\n",
    )
    .unwrap();
    std::os::unix::net::UnixStream::connect(&socket).unwrap();
    reply(
        "index during retained cutoff",
        index.wait_with_output().unwrap(),
    );
    trellis::testing::wait_until("later native change refuses retained request", || {
        world.generated_rows()[1].0 == "failed"
    });
    // The later native identity can trigger one separate retained attempt under the
    // existing owner policy; it cannot move this terminal request backwards.
    trellis::testing::wait_until(
        "new native identity gets its one source-parity refusal",
        || {
            let rows = world.generated_rows();
            rows.len() == 3 && rows[2].0 == "failed"
        },
    );
    let rows = world.generated_rows();
    assert_eq!(rows[1].1, "stale_basis: head revision changed");
    assert_eq!(
        rows[2].1,
        "stale_basis: retained source pages no longer match"
    );
    assert_eq!(rows[1].2, options);
    assert_eq!(rows[2].2, options);
    support::stop_daemon(&world.checkout.home);
    reply(
        "unchanged native identity restart after refusal",
        rebuilt_cli()
            .args(["index", "--workspace"])
            .arg(&world.checkout.root)
            .args(["--max-file-bytes", "1048576"])
            .output()
            .unwrap(),
    );
    assert_eq!(
        world.generated_rows(),
        rows,
        "unchanged identity cannot repeat or reverse either refusal"
    );
    assert_eq!(
        fs::read(options["descriptor"].as_str().unwrap()).unwrap(),
        original
    );
    assert_eq!(world.invocation_count(), 1);
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
    assert_eq!(
        rows[1].1,
        "capture_unavailable: captured revision not retained"
    );
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

#[test]
fn second_successful_run_releases_previous_bundle_before_any_later_sweep() {
    let world = World::new();
    let (first, _, first_options) = world.run_once_variant(0);
    let (second, second_bytes, second_options) = world.run_once_variant(1);
    let first_dir = Path::new(first_options["descriptor"].as_str().unwrap())
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let second_dir = Path::new(second_options["descriptor"].as_str().unwrap())
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    // No status, plan, third run or restart may trigger a later sweep before this check.
    assert!(
        !first_dir.join("out").exists(),
        "previous run output survived replacement publication"
    );
    assert!(
        !first_dir.join("batch").exists(),
        "previous run batch survived replacement publication"
    );
    assert!(second_dir.join("out").is_dir());
    assert_eq!(
        fs::read(second_options["descriptor"].as_str().unwrap()).unwrap(),
        second_bytes
    );
    for (dir, run) in [(first_dir, &first), (second_dir, &second)] {
        let record: Value =
            serde_json::from_slice(&fs::read(dir.join("run.json")).unwrap()).unwrap();
        assert_eq!(record["state"], "done");
        assert_eq!(record["outcome"], "imported");
        assert_eq!(record["requestId"], run["import"]["requestId"]);
    }
    let rows = world.generated_rows();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].2, second_options);
    assert_eq!(world.invocation_count(), 2);
}

#[test]
fn structurally_corrupt_retention_identity_does_not_block_native_index_fifo() {
    let world = World::new();
    let (_, original, options) = world.run_once();
    let retention = state_dir(&world.checkout).join("retention.sqlite");
    world.upgrade(|_| {
        let db = Connection::open(&retention).unwrap();
        assert_eq!(db.execute("DELETE FROM identity", []).unwrap(), 1);
    });
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
        original
    );
    reply(
        "second index with structurally corrupt retention catalog",
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
