//! Trellis-written batches (#21): the generated import path reads a private generation
//! directory, binds to its own project-root spellings, records its provenance, and cannot be
//! reached by an external caller. A run's batch is proven in-process (`stage_batch` plus
//! publication under a temporary cache); the daemon-level cases use an owner pickup.
#![cfg(feature = "test-support")]
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::{Arc, atomic::AtomicBool},
};
use tempfile::{Builder, TempDir};
use trellis::{
    indexer::{IndexOptions, index_workspace_bundle},
    model::IndexPin,
    producer::{
        TrustProfile,
        batch::{self, BatchSpec},
        detect::{Resolution, Verification},
        profiles::{self, Target},
        state::StateDir,
    },
    semantic_batch::{
        BatchCounters, BatchLimits, LookupBudget, StageInput, StageOwner,
        descriptor::{Descriptor, GenerationOrigin},
        stage_batch,
    },
    semantic_import::{DestinationMutations, join_staged},
    store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    },
};

#[path = "support/producer.rs"]
mod support;
use support::{Checkout, cache_dir, data_dir, start_checkout, state_dir};

#[path = "support/semantic_batch.rs"]
mod semantic_batch_support;
use semantic_batch_support::{BatchFixture, edit_descriptor, edit_scip, uniform_workspace};

const LANGUAGES: [&str; 4] = ["java", "rust", "python", "javascript"];
const RUN_ID: &str = "0123456789abcdef0123456789abcdef";
const OTHER_ID: &str = "fedcba9876543210fedcba9876543210";

fn workspace(root: &Path) {
    uniform_workspace(root, &LANGUAGES, 2);
    fs::write(root.join(".gitignore"), ".trellis-semantic-batch*/\n").unwrap();
}

/// Documents of `producer` whose coverage is selected and complete at `pin`. A batch whose
/// re-authentication failed on reuse leaves rows selected as `failed` instead.
fn complete_documents(store: &Store, pin: IndexPin, producer: &str) -> usize {
    store
        .semantic_at(pin)
        .unwrap()
        .coverage
        .iter()
        .filter(|row| {
            row.producer_id == producer
                && row.selected
                && row.state == trellis::semantic_evidence::CoverageState::Complete
        })
        .count()
}

fn file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

// ---------------------------------------------------------------------------------------
// In-process: a trellisRun batch under a temporary cache.
// ---------------------------------------------------------------------------------------

struct Inproc {
    _state: TempDir,
    _workspace: TempDir,
    root: PathBuf,
    store: Store,
    cancel: Arc<AtomicBool>,
}

fn inproc() -> Inproc {
    let state = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    workspace(&root);
    let store = Store::open_for_tests(state.path(), &root).unwrap();
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

impl Inproc {
    fn publish_native(&self) {
        let (graph, native, capture) = index_workspace_bundle(
            &IndexOptions::new(self.root.clone()),
            self.store.root_id(),
            &self.cancel,
            |_| {},
        )
        .unwrap();
        let leader = self.store.publication_for_tests().unwrap();
        self.store
            .publish_native(
                &graph,
                &capture,
                &native,
                &leader,
                self.store.index_baseline().unwrap(),
                &self.cancel,
            )
            .unwrap();
    }

    /// Publishes the current workspace as a native recovery publication, which rejoins every
    /// selected batch over the new revision.
    fn republish_with_rejoin(&self) {
        let (graph, native, capture) = index_workspace_bundle(
            &IndexOptions::new(self.root.clone()),
            self.store.root_id(),
            &self.cancel,
            |_| {},
        )
        .unwrap();
        let leader = self.store.publication_for_tests().unwrap();
        self.store
            .publish_native_recovery_for_tests(&graph, &capture, &native, &leader, &self.cancel)
            .unwrap();
    }

    fn state(&self) -> StateDir {
        let (roots, identity) = self.store.state_topology();
        StateDir::open(roots, identity).unwrap()
    }

    fn index_db(&self) -> PathBuf {
        let (roots, identity) = self.store.state_topology();
        roots.index_db(identity)
    }

    /// The synthetic stub's artifact over the javascript documents, with `project_root`.
    fn artifact(&self, project_root: &str) -> Vec<u8> {
        let mut fixture = BatchFixture::from_published(&self.index_db(), &self.root).synthetic();
        let docs = fixture.paths("javascript");
        fixture.artifact("javascript", docs.clone());
        let bytes = fixture.scip_bytes("javascript", &docs);
        edit_scip(&bytes, |index| {
            index.metadata.as_mut().unwrap().project_root = project_root.into();
        })
    }

    /// Writes a run directory holding `artifact` and the batch written for it. Returns the
    /// canonical run directory, the descriptor and its digest.
    fn write_run(
        &self,
        id: &str,
        artifact: impl FnOnce(&str) -> Vec<u8>,
    ) -> anyhow::Result<(PathBuf, PathBuf, String)> {
        let state = self.state();
        let dir = fs::canonicalize(state.create_run_dir(id).unwrap()).unwrap();
        let worktree = StageInput::generated(&self.root, GenerationOrigin::TrellisRun, id, &dir)?
            .project_roots[0]
            .clone();
        fs::create_dir(dir.join("out")).unwrap();
        fs::write(dir.join(batch::ARTIFACT_PATH), artifact(&worktree)).unwrap();
        let pin = Target::Stub.pin().unwrap();
        let resolution = Resolution {
            launcher: "/stub".into(),
            verification: Verification {
                method: "testStub",
                observed: String::new(),
            },
            node: None,
            java: None,
            java_home: None,
            rustup: None,
            python3: None,
        };
        let table = profiles::run_generation(
            &profiles::row(Target::Stub, TrustProfile::Restricted)?,
            &resolution,
        );
        let head = self.store.index_baseline()?;
        let written = self.store.with_native_reader(head, |reader| {
            batch::write_batch(
                reader,
                &BatchSpec {
                    pin: &pin,
                    language: "javascript",
                    workspace: &self.root,
                    dir: &dir,
                    generation: batch::run_generation(&table, GenerationOrigin::TrellisRun, id),
                },
            )
        })?;
        Ok((dir, written.descriptor, written.descriptor_sha256))
    }

    fn stage(
        &self,
        input: &StageInput,
        descriptor: &Path,
        sha: &str,
    ) -> anyhow::Result<trellis::semantic_batch::AdmittedBatch> {
        stage_batch(
            &self.store,
            input,
            StageOwner {
                request_id: "request-1",
                incarnation: "claim-1",
            },
            descriptor,
            sha,
            &BatchLimits::PRODUCTION,
            &mut BatchCounters::default(),
            &self.cancel,
        )
    }

    /// Admits, joins and publishes a run's batch.
    fn publish_run(&self, id: &str, dir: &Path, descriptor: &Path, sha: &str) -> IndexPin {
        let input =
            StageInput::generated(&self.root, GenerationOrigin::TrellisRun, id, dir).unwrap();
        let mut admitted = self.stage(&input, descriptor, sha).unwrap();
        let limits = BatchLimits::PRODUCTION;
        let joined = join_staged(
            &mut admitted,
            &self.store,
            self.store.index_baseline().unwrap(),
            &limits,
            &mut LookupBudget::new(&limits),
            &mut BatchCounters::default(),
            DestinationMutations::default(),
            &self.cancel,
        )
        .unwrap();
        let leader = self.store.publication_for_tests().unwrap();
        self.store
            .publish_semantic_batch(
                &joined,
                admitted.stage_mut(),
                joined.destination_pin(),
                &leader,
                &self.cancel,
            )
            .unwrap()
    }

    /// The `(generationId, projectRoot)` of every batch selected at `pin`.
    fn selected(&self, pin: IndexPin) -> Vec<(String, String)> {
        self.store
            .selected_raw_inputs_at(pin)
            .unwrap()
            .into_iter()
            .map(|batch| {
                (
                    batch.descriptor.generation.unwrap().generation_id,
                    batch.descriptor.metadata.project_root,
                )
            })
            .collect()
    }
}

#[test]
fn generated_batch_imports_from_private_run_dir() {
    let world = inproc();
    let before = support::workspace_tree_digest(&world.root);
    let (dir, descriptor, sha) = world
        .write_run(RUN_ID, |worktree| world.artifact(worktree))
        .unwrap();
    let state = fs::canonicalize(world.state().path()).unwrap();
    assert!(
        dir.starts_with(&state),
        "the run directory is private state"
    );
    assert!(!dir.starts_with(&world.root), "and outside the workspace");

    // The same descriptor is not an external import: it lies outside the workspace, and it
    // names a generation.
    let external = StageInput::external(&world.root).unwrap();
    let refused = world
        .stage(&external, &descriptor, &sha)
        .err()
        .expect("an external import of a run batch is refused");
    assert!(
        format!("{refused:#}").contains("role outside root"),
        "{refused:#}"
    );

    let published = world.publish_run(RUN_ID, &dir, &descriptor, &sha);
    assert_eq!(world.store.index_baseline().unwrap(), published);
    assert_eq!(world.selected(published).len(), 1);
    assert_eq!(world.selected(published)[0].0, RUN_ID);
    assert!(
        world
            .store
            .semantic_batch_report_at(published, &sha)
            .unwrap()
            .is_some()
    );
    let batches = world.store.selected_raw_inputs_at(published).unwrap();
    assert_eq!(batches[0].producer_id, "trellis-synthetic-scip");
    assert_eq!(
        support::workspace_tree_digest(&world.root),
        before,
        "nothing was written into the workspace"
    );
}

#[test]
fn generated_batch_projectroot_spellings() {
    let world = inproc();
    let mut seen = 0;
    for (n, trailing) in [false, true].into_iter().enumerate() {
        let id = format!("{n:032x}");
        let (dir, descriptor, sha) = world
            .write_run(&id, |worktree| {
                world.artifact(&format!("{worktree}{}", if trailing { "/" } else { "" }))
            })
            .unwrap();
        let retained = Descriptor::parse(&fs::read(&descriptor).unwrap()).unwrap();
        assert_eq!(retained.metadata.project_root.ends_with('/'), trailing);
        let input =
            StageInput::generated(&world.root, GenerationOrigin::TrellisRun, &id, &dir).unwrap();
        assert!(
            input
                .project_roots
                .contains(&retained.metadata.project_root)
        );
        world.stage(&input, &descriptor, &sha).unwrap();
        seen += 1;
    }
    assert_eq!(seen, 2);

    // An empty artifact root is written as the bare worktree spelling.
    let (_, descriptor, _) = world.write_run(OTHER_ID, |_| world.artifact("")).unwrap();
    let retained = Descriptor::parse(&fs::read(&descriptor).unwrap()).unwrap();
    assert!(retained.metadata.project_root.ends_with("/worktree"));

    // Any other spelling fails the writer, and a hand-edited descriptor fails admission.
    let first = format!("{:032x}", 0);
    let error = world
        .write_run("11111111111111111111111111111111", |worktree| {
            world.artifact(&format!("{worktree}//"))
        })
        .unwrap_err();
    assert_eq!(error.to_string(), "producer_output_invalid: project root");
    let dir = fs::canonicalize(world.state().run_dir(&first).unwrap()).unwrap();
    let descriptor = dir.join("batch/descriptor.json");
    let (edited, sha) = edit_descriptor(&descriptor, |value| {
        value["metadata"]["projectRoot"] = json!("file:///elsewhere/worktree");
    });
    let input =
        StageInput::generated(&world.root, GenerationOrigin::TrellisRun, &first, &dir).unwrap();
    let refused = world
        .stage(&input, &edited, &sha)
        .err()
        .expect("an unadmitted project root is refused");
    assert!(
        format!("{refused:#}").contains("metadata binding"),
        "{refused:#}"
    );

    // A pickup binds to the workspace, bare or with a trailing slash; a run spelling is not one.
    let pickup = StageInput::generated(
        &world.root,
        GenerationOrigin::OwnerPickup,
        RUN_ID,
        &world.state().path().join("pickups"),
    )
    .unwrap();
    let uri = file_uri(&world.root);
    assert_eq!(pickup.project_roots, [uri.clone(), format!("{uri}/")]);
}

#[test]
fn generated_run_batch_reuse_rebinds_with_retained_project_root() {
    for (n, trailing) in [false, true].into_iter().enumerate() {
        let world = inproc();
        let id = format!("{:032x}", n + 1);
        let (dir, descriptor, sha) = world
            .write_run(&id, |worktree| {
                world.artifact(&format!("{worktree}{}", if trailing { "/" } else { "" }))
            })
            .unwrap();
        let published = world.publish_run(&id, &dir, &descriptor, &sha);
        let retained = world.selected(published);
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].0, id);
        assert_eq!(retained[0].1.ends_with('/'), trailing);
        assert!(
            world
                .store
                .semantic_batch_report_at(published, &sha)
                .unwrap()
                .is_some()
        );

        // An unrelated native publication re-authenticates the stored batch against its
        // retained project root. The synthetic producer always takes the FULL path.
        fs::write(
            world.root.join("rust/u00000.rs"),
            "fn u00000() { other(); }\n",
        )
        .unwrap();
        world.republish_with_rejoin();
        let head = world.store.index_baseline().unwrap();
        assert!(head.index_revision > published.index_revision);
        assert_eq!(
            world.selected(head),
            retained,
            "the batch with root spelling trailing={trailing} was dropped on reuse"
        );
        assert_eq!(
            complete_documents(&world.store, head, "trellis-synthetic-scip"),
            complete_documents(&world.store, published, "trellis-synthetic-scip"),
            "reuse failed to re-authenticate the batch with root spelling trailing={trailing}"
        );
        assert!(complete_documents(&world.store, head, "trellis-synthetic-scip") > 0);
    }
}

// ---------------------------------------------------------------------------------------
// Daemon level: owner pickup, external callers and the strict basis rule.
// ---------------------------------------------------------------------------------------

struct World {
    checkout: Checkout,
    store: Store,
    index_db: PathBuf,
}

fn world(env: &[(String, String)]) -> World {
    let checkout = start_checkout(workspace, env);
    let roots =
        TopologyRoots::isolated_for_tests(cache_dir(&checkout.home), data_dir(&checkout.home));
    let identity = WorkspaceIdentity::discover(Some(&checkout.root), &checkout.root).unwrap();
    let index_db = roots.index_db(&identity);
    let store = Store::open(roots, identity).unwrap();
    World {
        checkout,
        store,
        index_db,
    }
}

impl World {
    fn head(&self) -> IndexPin {
        self.store.index_baseline().unwrap()
    }

    fn java_artifact(&self, project_root: Option<String>) -> PathBuf {
        self.reference_artifact("scip-java", "java", project_root)
    }

    fn reference_artifact(
        &self,
        producer: &str,
        language: &str,
        project_root: Option<String>,
    ) -> PathBuf {
        let fixture =
            BatchFixture::from_published(&self.index_db, &self.checkout.root).reference(producer);
        let docs = fixture.paths(language);
        let bytes = fixture.scip_bytes(language, &docs);
        let bytes = match project_root {
            Some(root) => edit_scip(&bytes, |index| {
                index.metadata.as_mut().unwrap().project_root = root;
            }),
            None => bytes,
        };
        let path = self
            .checkout
            .temp
            .path()
            .join(format!("artifact-{}.scip", fixture.source_count()));
        fs::write(&path, bytes).unwrap();
        path
    }

    fn pickup_command(&self, artifact: &Path) -> std::process::Command {
        self.pickup_command_for("scip-java", artifact)
    }

    fn pickup_command_for(&self, producer: &str, artifact: &Path) -> std::process::Command {
        let mut command = self.checkout.cli();
        command
            .args(["producer", "pickup", "--producer", producer, "--workspace"])
            .arg(&self.checkout.root)
            .arg("--artifact")
            .arg(artifact);
        command
    }

    fn pickup(&self, artifact: &Path) -> (Output, Value) {
        let output = self.pickup_command(artifact).output().unwrap();
        let reply = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
        (output, reply)
    }

    fn batches(&self) -> i64 {
        self.store
            .with_index_sql_for_tests(|db| {
                Ok(
                    db.query_row("SELECT COUNT(*) FROM semantic_raw_batches", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .unwrap()
    }

    fn queue(&self) -> Vec<(String, String, Option<String>)> {
        let db = rusqlite::Connection::open_with_flags(
            self.store.request_db_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        db.prepare("SELECT id,state,error_code FROM requests ORDER BY seq")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }
}

#[test]
fn generation_recorded_in_retained_descriptor() {
    let world = world(&[]);
    let artifact = world.java_artifact(None);
    let (output, reply) = world.pickup(&artifact);
    assert!(output.status.success(), "{reply}");
    let head = world.head();
    let descriptor_bytes: Vec<u8> = world
        .store
        .with_index_sql_for_tests(|db| {
            Ok(db.query_row(
                "SELECT descriptor_bytes FROM semantic_raw_batches",
                [],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    let descriptor = Descriptor::parse(&descriptor_bytes).unwrap();
    let generation = descriptor.generation.clone().expect("generation recorded");
    assert_eq!(generation.origin, GenerationOrigin::OwnerPickup);
    assert_eq!(generation.generation_id.len(), 32);
    assert_eq!(
        generation,
        batch::pickup_generation(&generation.generation_id),
        "a pickup observes nothing and asserts every component"
    );
    assert_eq!(generation.trust_profile, None);
    assert!(generation.runtime.is_empty());
    let sha = reply["report"]["descriptorSha256"].as_str().unwrap();
    assert!(
        world
            .store
            .semantic_batch_report_at(head, sha)
            .unwrap()
            .is_some()
    );
}

fn external_batch(world: &World) -> (PathBuf, String) {
    let mut fixture =
        BatchFixture::from_published(&world.index_db, &world.checkout.root).synthetic();
    for language in LANGUAGES {
        let docs = fixture.paths(language);
        fixture.artifact(language, docs);
    }
    fixture.write(&world.checkout.root.join(".trellis-semantic-batch-0"))
}

fn import(world: &World, descriptor: &Path, sha: &str) -> Output {
    world
        .checkout
        .cli()
        .args(["semantic", "import", "--workspace"])
        .arg(&world.checkout.root)
        .arg("--batch")
        .arg(descriptor)
        .args(["--batch-sha256", sha])
        .output()
        .unwrap()
}

#[test]
fn external_import_cannot_claim_generation() {
    let world = world(&[]);
    let (descriptor, _) = external_batch(&world);
    let before = world.head();
    for (origin, id) in [("trellisRun", RUN_ID), ("ownerPickup", RUN_ID)] {
        let (edited, sha) = edit_descriptor(&descriptor, |value| {
            value["generation"] = json!({
                "origin": origin, "generationId": id,
                "trustProfile": null, "runtime": [],
                "observed": [],
                "asserted": ["source","config","dependencyManifests","producerVersion",
                    "producerExecutable","runtime","externalInputs"],
            });
        });
        let output = import(&world, &edited, &sha);
        assert!(!output.status.success(), "{origin} was accepted");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("invalid_request: request ")
                && stderr.contains(" failed: generation origin"),
            "{stderr}"
        );
    }
    assert_eq!(world.batches(), 0);
    assert_eq!(world.head().index_generation, before.index_generation);
    // The key is required (owner ruling 2026-10-30, #21): without it the descriptor is not
    // closed, and nothing is published.
    let (absent, sha) = edit_descriptor(&descriptor, |value| {
        value.as_object_mut().unwrap().remove("generation");
    });
    let output = import(&world, &absent, &sha);
    assert!(!output.status.success(), "a descriptor without generation");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("closed descriptor"), "{stderr}");
    assert_eq!(world.batches(), 0);
    // The same batch with a null generation imports.
    let (plain, sha) = edit_descriptor(&descriptor, |value| {
        value["generation"] = Value::Null;
    });
    let output = import(&world, &plain, &sha);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(world.batches(), 1);
}

#[test]
fn external_import_outside_workspace_still_refused() {
    let world = world(&[]);
    let (descriptor, sha) = external_batch(&world);
    let batch_dir = descriptor.parent().unwrap().to_owned();
    let state = state_dir(&world.checkout);
    let private = state.join("runs").join(RUN_ID);
    fs::create_dir_all(&private).unwrap();
    let moved = private.join("batch");
    fs::rename(&batch_dir, &moved).unwrap();
    let before = world.queue();
    let output = import(&world, &moved.join("descriptor.json"), &sha);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid_request: role outside root"),
        "{stderr}"
    );
    assert_eq!(world.queue(), before, "nothing was queued");
    assert_eq!(world.batches(), 0);
}

#[test]
fn external_op_cannot_set_origin() {
    use trellis::daemon::protocol;
    let world = world(&[]);
    let (descriptor, sha) = external_batch(&world);
    let before = world.queue();
    let descriptor = descriptor.display().to_string();
    for extra in [
        json!({"origin": "trellisRun", "generationId": RUN_ID}),
        json!({"origin": "ownerPickup", "generationId": RUN_ID}),
        json!({"origin": "external"}),
        json!({"generationId": RUN_ID}),
    ] {
        let mut args = json!({"version": 2, "descriptor": descriptor, "descriptorSha256": sha});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let socket = trellis::daemon::SocketPaths::new(&data_dir(&world.checkout.home)).socket;
        let mut stream = UnixStream::connect(socket).unwrap();
        protocol::write_frame(
            &mut stream,
            &protocol::Request {
                id: 91,
                operation: "semantic_import".into(),
                payload: json!({"workspace": world.checkout.root, "args": args}),
            },
        )
        .unwrap();
        let reply: protocol::Reply = protocol::read_frame(&mut stream).unwrap();
        assert!(
            reply.payload["error"]
                .as_str()
                .is_some_and(|error| error.contains("invalid_request")),
            "{args}: {reply:?}"
        );
    }
    assert_eq!(
        world.queue(),
        before,
        "no refused frame created a queue row"
    );
    // The command line cannot express an origin either.
    let output = world
        .checkout
        .cli()
        .args(["semantic", "import", "--origin", "trellisRun"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(world.queue(), before);
}

#[test]
fn generated_import_failure_publishes_nothing() {
    let world = world(&[]);
    let good = world.java_artifact(None);
    let bad = world.checkout.temp.path().join("bad.scip");
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
                .name = "someone-else".into();
        }),
    )
    .unwrap();
    let before = world.head();
    let (output, reply) = world.pickup(&bad);
    assert!(!output.status.success());
    assert_eq!(reply["state"], "failed");
    assert!(
        reply["errorCode"]
            .as_str()
            .unwrap()
            .starts_with("invalid_artifact"),
        "{reply}"
    );
    assert_eq!(world.head(), before, "the head did not move");
    assert_eq!(world.batches(), 0);
    assert!(
        world
            .store
            .selected_raw_inputs_at(before)
            .unwrap()
            .is_empty()
    );
    let queue = world.queue();
    let last = queue.last().unwrap();
    assert_eq!(last.1, "failed");
    assert_eq!(last.2.as_deref(), reply["errorCode"].as_str());
}

fn socket_env(name: &str, path: &Path) -> (String, String) {
    (name.to_owned(), path.display().to_string())
}

fn wait_for(path: &Path, what: &str) {
    trellis::testing::wait_until(what, || path.exists());
}

fn edit_rust(world: &World, text: &str) {
    fs::write(world.checkout.root.join("rust/u00000.rs"), text).unwrap();
}

/// Runs a pickup whose import pauses while staged; `at_pause` edits the workspace and may
/// release further pauses. Returns the reply once the pickup has finished.
fn stale_pickup(world: &World, staging: &Path, at_stage: impl FnOnce(&World)) -> (Output, Value) {
    let artifact = world.java_artifact(None);
    let child = world
        .pickup_command(&artifact)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for(&staging.with_extension("ready"), "the import to stage");
    at_stage(world);
    drop(UnixStream::connect(staging).unwrap());
    let output = child.wait_with_output().unwrap();
    let reply = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    (output, reply)
}

fn assert_stale(world: &World, output: &Output, reply: &Value, advanced: IndexPin) {
    assert!(!output.status.success());
    assert_eq!(reply["state"], "failed", "{reply}");
    assert_eq!(reply["errorCode"], "stale_basis: head revision changed");
    assert_eq!(world.batches(), 0, "a late batch published nothing");
    let queue = world.queue();
    assert_eq!(
        queue.last().unwrap().2.as_deref(),
        Some("stale_basis: head revision changed")
    );
    // The drain published the edit natively; only the batch was refused.
    trellis::testing::wait_until("the edit published natively", || {
        world.head().index_revision > advanced.index_revision
    });
    assert!(
        world
            .store
            .selected_raw_inputs_at(world.head())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn generated_import_rejected_when_head_moved() {
    let sockets = Builder::new()
        .prefix("generated-stale-")
        .tempdir_in("/tmp")
        .unwrap();
    let staging = sockets.path().join("stage.sock");
    let world = world(&[socket_env("TRELLIS_TEST_SEMANTIC_STAGING_SOCKET", &staging)]);
    let advanced = world.head();
    let (output, reply) = stale_pickup(&world, &staging, |world| {
        edit_rust(world, "fn u00000() { moved(); }\n");
    });
    assert_stale(&world, &output, &reply, advanced);
}

#[test]
fn generated_import_rejected_when_head_moves_at_cutoff() {
    let sockets = Builder::new()
        .prefix("generated-cutoff-")
        .tempdir_in("/tmp")
        .unwrap();
    let staging = sockets.path().join("stage.sock");
    let cutoff = sockets.path().join("cutoff.sock");
    let world = world(&[
        socket_env("TRELLIS_TEST_SEMANTIC_STAGING_SOCKET", &staging),
        socket_env("TRELLIS_TEST_SEMANTIC_CUTOFF_SOCKET", &cutoff),
    ]);
    let advanced = world.head();
    let artifact = world.java_artifact(None);
    let child = world
        .pickup_command(&artifact)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for(&staging.with_extension("ready"), "the import to stage");
    edit_rust(&world, "fn u00000() { moved(); }\n");
    drop(UnixStream::connect(&staging).unwrap());
    // The first drain published the edit; the retry joined against the moved head and now
    // waits at the cutoff. A second edit lands there.
    wait_for(&cutoff.with_extension("ready"), "the cutoff pause");
    edit_rust(&world, "fn u00000() { moved_again(); }\n");
    drop(UnixStream::connect(&cutoff).unwrap());
    let output = child.wait_with_output().unwrap();
    let reply = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    assert_stale(&world, &output, &reply, advanced);
}

#[test]
fn pickup_batch_with_trailing_slash_survives_full_reuse() {
    let world = world(&[]);
    let root_uri = file_uri(&world.checkout.root);
    // scip-java always takes the FULL path on reuse.
    let artifact = world.java_artifact(Some(format!("{root_uri}/")));
    let (output, reply) = world.pickup(&artifact);
    assert!(
        output.status.success(),
        "{reply} {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let imported = world.head();
    assert_eq!(
        world.store.selected_raw_inputs_at(imported).unwrap().len(),
        1
    );
    let documents = complete_documents(&world.store, imported, "scip-java");
    assert!(documents > 0);

    edit_rust(&world, "fn u00000() { unrelated(); }\n");
    trellis::testing::wait_until("the watcher to publish the edit", || {
        world.head().index_revision > imported.index_revision
    });
    let head = world.head();
    assert_eq!(
        world.store.selected_raw_inputs_at(head).unwrap().len(),
        1,
        "the batch was dropped when the head moved"
    );
    assert_eq!(
        complete_documents(&world.store, head, "scip-java"),
        documents,
        "reuse failed to re-authenticate the pickup batch"
    );
    let retained = &world.store.selected_raw_inputs_at(head).unwrap()[0];
    assert_eq!(
        retained.descriptor.metadata.project_root,
        format!("{root_uri}/")
    );
    assert_eq!(
        retained.descriptor.generation.as_ref().unwrap().origin,
        GenerationOrigin::OwnerPickup
    );
}

/// A Rust (rust-analyzer) pickup batch is retained and re-authenticated when an unrelated
/// publication moves the head and the store reuses the stored batch.
#[test]
fn rust_pickup_batch_survives_store_reuse() {
    let world = world(&[]);
    let root_uri = file_uri(&world.checkout.root);
    let artifact = world.reference_artifact("rust-analyzer", "rust", Some(format!("{root_uri}/")));
    let output = world
        .pickup_command_for("rust-analyzer", &artifact)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let imported = world.head();
    assert_eq!(
        world.store.selected_raw_inputs_at(imported).unwrap().len(),
        1
    );
    let documents = complete_documents(&world.store, imported, "rust-analyzer");
    assert!(documents > 0);

    fs::write(
        world.checkout.root.join("java/u00000.java"),
        "class u00000 { void unrelated() {} }\n",
    )
    .unwrap();
    trellis::testing::wait_until("the watcher to publish the edit", || {
        world.head().index_revision > imported.index_revision
    });
    let head = world.head();
    let retained = world.store.selected_raw_inputs_at(head).unwrap();
    assert_eq!(retained.len(), 1, "the Rust batch was dropped on reuse");
    assert_eq!(
        complete_documents(&world.store, head, "rust-analyzer"),
        documents,
        "reuse failed to re-authenticate the Rust pickup batch"
    );
    assert_eq!(
        retained[0].descriptor.generation.as_ref().unwrap().origin,
        GenerationOrigin::OwnerPickup
    );
}
