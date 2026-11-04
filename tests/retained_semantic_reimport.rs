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
        Self::for_workspace(language, name, hook, false)
    }

    fn for_workspace(language: &'static str, name: &str, hook: Option<&Path>, git: bool) -> Self {
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
                if git {
                    let output = std::process::Command::new("git")
                        .args(["init", "--quiet"])
                        .arg(root)
                        .output()
                        .unwrap();
                    assert!(output.status.success(), "git init failed: {output:?}");
                }
                uniform_workspace(root, &[language], 1);
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
        let docs = fixture
            .paths(self.language)
            .into_iter()
            .map(|doc| {
                doc.occurrence(&definition, &[], "pkg u00000().", 1)
                    .occurrence(&call, &[], "pkg g().", 0)
                    .occurrence(&target, &[], "pkg g().", 1)
                    .symbol("pkg u00000().", 17, &[])
                    .symbol("pkg g().", 17, &[])
            })
            .collect::<Vec<_>>();
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

    fn assert_recovered_basis(&self, status: &Value, original: &Value) {
        let js = status["semanticLanguages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["language"] == "javascript")
            .unwrap();
        assert_eq!(js["reimport"], Value::Null);
        assert_eq!(js["semanticProducerId"], PRODUCER);
        assert_eq!(js["coverage"]["complete"], 1);
        assert_eq!(js["coverage"]["failed"], 0);
        assert!(
            js["internal"].as_u64().unwrap() > 0,
            "recovered semantic call target must remain usable"
        );
        // This existing race also changes maxFileBytes, so it remains conservative.
        assert_eq!(js["semanticState"], "stale");
        let key = format!(
            "pin:v1:{}:{}",
            status["revision"]["indexGeneration"].as_str().unwrap(),
            status["revision"]["indexRevision"].as_u64().unwrap()
        );
        let db =
            Connection::open_with_flags(&self.index, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let (proofs, conservative, changed): (i64, i64, i64) = db.query_row(
            "SELECT COUNT(*),SUM(p.freshness='possiblyStale'),SUM(p.freshness='stale') FROM semantic_provenance p JOIN semantic_coverage c USING(tuple_id) WHERE c.revision_id=?1 AND c.producer_id=?2 AND c.selected=1",
            rusqlite::params![key, PRODUCER], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        assert!(
            proofs > 0,
            "recovery must select nonempty semantic evidence"
        );
        assert_eq!(proofs, conservative);
        assert_eq!(changed, 0);
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
    use std::io::Write;

    let world = World::for_workspace("javascript", "git-status", None, true);
    let workspace_marker = world.checkout.root.join(".git/trellis/workspace-id");
    let original_marker = fs::read(&workspace_marker).unwrap();
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
    let claim_socket = world._stub.path().join("after-claim.sock");
    let index = rebuilt_cli()
        .env("TRELLIS_TEST_RETAINED_AFTER_STARTUP_SOCKET", &socket)
        .env(
            "TRELLIS_TEST_SEMANTIC_CRASH_AFTER_CLAIM_SOCKET",
            &claim_socket,
        )
        .args(["index", "--workspace"])
        .arg(&world.checkout.root)
        .args(["--max-file-bytes", "1048576"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    trellis::testing::wait_until("startup capture held before retained admission", || {
        socket.with_extension("ready").exists()
    });
    let startup_native = native_identity();
    assert_ne!(startup_native, old_native);
    let pending = reply(
        "Git checkout status before retained admission",
        rebuilt_cli()
            .args(["status", "--workspace"])
            .arg(&world.checkout.root)
            .output()
            .unwrap(),
    );
    assert_eq!(
        pending["workspaceRoot"],
        world.checkout.root.to_str().unwrap()
    );
    assert_eq!(
        pending["revision"]["indexGeneration"],
        old["indexGeneration"]
    );
    assert_eq!(pending["catchingUp"], true);
    assert_eq!(fs::read(&workspace_marker).unwrap(), original_marker);
    trellis::testing::wait_until(
        "post-start index accepted ahead of retained recovery",
        || {
            let db = Connection::open_with_flags(&world.requests, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
            db.query_row("SELECT EXISTS(SELECT 1 FROM requests WHERE request_kind='index' AND state='queued')", [], |row| row.get::<_, bool>(0)).unwrap()
        },
    );
    assert_eq!(
        world.generated_rows().len(),
        1,
        "retained input not enqueued before hook release"
    );
    std::os::unix::net::UnixStream::connect(&socket).unwrap();
    reply(
        "index under changed executable",
        index.wait_with_output().unwrap(),
    );
    assert_ne!(
        native_identity(),
        startup_native,
        "own post-start native capture must supersede startup before retained import"
    );
    trellis::testing::wait_until("retained import claimed", || claim_socket.exists());
    let claimed = reply(
        "Git checkout status during retained admission",
        rebuilt_cli()
            .args(["status", "--workspace"])
            .arg(&world.checkout.root)
            .output()
            .unwrap(),
    );
    assert_eq!(
        claimed["workspaceRoot"],
        world.checkout.root.to_str().unwrap()
    );
    assert_eq!(
        claimed["revision"]["indexGeneration"],
        old["indexGeneration"]
    );
    assert_eq!(claimed["catchingUp"], true);
    assert_eq!(fs::read(&workspace_marker).unwrap(), original_marker);
    fs::write(claim_socket.with_extension("done"), []).unwrap();
    std::os::unix::net::UnixStream::connect(&claim_socket).unwrap();
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
    assert_eq!(js["reimport"], Value::Null);
    assert_eq!(
        status["workspaceRoot"],
        world.checkout.root.to_str().unwrap()
    );
    assert_eq!(fs::read(&workspace_marker).unwrap(), original_marker);
    world.assert_recovered_basis(&status, &descriptor);
    assert_eq!(
        fs::read(options["descriptor"].as_str().unwrap()).unwrap(),
        original
    );
    // Restarting the same binary/capture identity must not repeat successful recovery.
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
    assert_eq!(native_identity(), {
        let db =
            Connection::open_with_flags(&world.index, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        db.query_row(
            "SELECT native_revision_id FROM native_revisions WHERE id=?1",
            [format!(
                "pin:v1:{}:{}",
                status["revision"]["indexGeneration"].as_str().unwrap(),
                status["revision"]["indexRevision"].as_u64().unwrap()
            )],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
    });
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
    assert_eq!(returned[2].2, options);
    let returned_status = world.status();
    let returned_js = returned_status["semanticLanguages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["language"] == "javascript")
        .unwrap();
    assert_eq!(returned_js["reimport"], Value::Null);
    world.assert_recovered_basis(&returned_status, &descriptor);
    assert_eq!(
        returned_status["revision"]["indexGeneration"],
        old["indexGeneration"]
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
fn unreadable_successful_run_does_not_erase_published_upgrade_recovery() {
    use std::os::unix::fs::PermissionsExt;

    let world = World::new();
    let (run, _, _) = world.run_once();
    let state = state_dir(&world.checkout);
    world.upgrade(|root| {
        let db = Connection::open(state.join("retention.sqlite")).unwrap();
        db.execute("DELETE FROM retained_runs", []).unwrap();
        drop(db);
        fs::write(root.join("tool.py"), "def tool():\n    pass\n").unwrap();
    });
    let needs_reimport = |status: &Value| {
        status["semanticLanguages"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["language"] == "javascript"))
            .is_some_and(|row| row["reimport"]["state"] == "needsReimport")
    };
    let before = world.status();
    assert!(needs_reimport(&before), "before fault: {before}");

    // Model an ordinary permission fault on the existing successful record.
    let record = state
        .join("runs")
        .join(run["runId"].as_str().unwrap())
        .join("run.json");
    let permissions = fs::metadata(&record).unwrap().permissions();
    fs::set_permissions(&record, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(
        fs::read(&record).is_err(),
        "fault must make run.json unreadable"
    );
    fs::write(
        world.checkout.root.join("extra.js"),
        "function extra() {}\n",
    )
    .unwrap();
    let indexed = world
        .checkout
        .cli()
        .args(["index", "--workspace"])
        .arg(&world.checkout.root)
        .output()
        .unwrap();
    // Restore before checking the result so even an indexing failure cleans up the fault.
    fs::set_permissions(&record, permissions).unwrap();
    reply("index with unreadable successful run", indexed);
    let during = world.status();

    // Readability returning must not require a second upgrade to recover the warning.
    fs::write(
        world.checkout.root.join("extra.js"),
        "function extra() { return 1; }\n",
    )
    .unwrap();
    reply(
        "index after successful record becomes readable",
        world
            .checkout
            .cli()
            .args(["index", "--workspace"])
            .arg(&world.checkout.root)
            .output()
            .unwrap(),
    );
    let after = world.status();
    assert!(
        needs_reimport(&during),
        "after unreadable history: {during}; after readability restored: {after}"
    );
    assert!(
        needs_reimport(&after),
        "after readability restored: {after}"
    );
    assert_eq!(world.invocation_count(), 1);
}

/// #664: a language that never had a producer is not configured, before and after an
/// upgrade. Nothing was lost, so nothing needs re-import.
#[test]
fn first_upgrade_reports_no_reimport_for_languages_without_producer_history() {
    let world = World::new();
    assert!(world.status().get("semanticLanguages").is_none());
    world.upgrade(|root| fs::write(root.join("tool.py"), "def tool():\n    pass\n").unwrap());
    let status = world.status();
    assert!(
        status["semanticLanguages"]
            .as_array()
            .is_none_or(|rows| rows.iter().all(|row| row["reimport"].is_null())),
        "{status}"
    );
    assert!(
        !status
            .to_string()
            .contains("not retained before this upgrade"),
        "{status}"
    );
    assert!(world.generated_rows().is_empty());
    assert_eq!(world.invocation_count(), 0);
    // A legacy row an earlier version wrote for a never-configured language is dropped at
    // the next publication.
    support::stop_daemon(&world.checkout.home);
    let catalog = Connection::open(state_dir(&world.checkout).join("retention.sqlite")).unwrap();
    catalog
        .execute("INSERT INTO legacy_languages VALUES('python')", [])
        .unwrap();
    drop(catalog);
    fs::write(
        world.checkout.root.join("tool.py"),
        "def tool():\n    return 1\n",
    )
    .unwrap();
    reply(
        "index after an earlier legacy row",
        world
            .checkout
            .cli()
            .args(["index", "--workspace"])
            .arg(&world.checkout.root)
            .output()
            .unwrap(),
    );
    let status = world.status();
    assert!(
        !status
            .to_string()
            .contains("not retained before this upgrade"),
        "{status}"
    );
}

/// #664: a language whose producer published evidence before its inputs were retained
/// needs re-import after the upgrade; a language no producer ever covered does not.
#[test]
fn first_upgrade_reports_reimport_only_for_languages_with_producer_history() {
    let world = World::new();
    world.run_once();
    let catalog = state_dir(&world.checkout).join("retention.sqlite");
    world.upgrade(|root| {
        // Evidence published before retention: its run inputs were never retained.
        let db = Connection::open(&catalog).unwrap();
        db.execute("DELETE FROM retained_runs", []).unwrap();
        drop(db);
        fs::write(root.join("tool.py"), "def tool():\n    pass\n").unwrap();
    });
    let status = world.status();
    let rows = status["semanticLanguages"].as_array().unwrap();
    let row = |language: &str| rows.iter().find(|row| row["language"] == language);
    let js = row("javascript").unwrap();
    assert_eq!(js["reimport"]["state"], "needsReimport", "{status}");
    assert!(
        js["reimport"]["reason"]
            .as_str()
            .unwrap()
            .contains("not retained before this upgrade"),
        "{status}"
    );
    let python = row("python").unwrap();
    assert_eq!(python["reimport"], Value::Null, "{status}");
    assert_eq!(python["semanticProducerId"], Value::Null, "{status}");
    assert_eq!(world.generated_rows().len(), 1);
    assert_eq!(world.invocation_count(), 1);
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
