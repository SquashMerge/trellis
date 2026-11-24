//! Captured producer inputs for Architecture's published incremental builds.
#![allow(dead_code)]

use crate::semantic_batch_support::{BatchFixture, DocSpec};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};
use trellis::{
    http::{
        ArchitectureAttempt, ArchitectureBuildPath, ArchitectureCoreHandle,
        ArchitectureExecutionBranch, DaemonState, FullReason,
    },
    indexer::{IndexOptions, index_workspace_bundle},
    model::IndexPin,
    producer::exec::{Hooks, Limits, ProducerSpec, spawn_producer},
    semantic_batch::{
        BatchCounters, BatchLimits, LookupBudget, StageInput, StageOwner, stage_batch,
    },
    semantic_import::{DestinationMutations, join_staged},
    store::Store,
};

pub const SOURCES: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[package]\nname='fixture'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nrenamed={package='vendor',version='1'}\n",
    ),
    ("src/main.rs", "fn main() { let _value = 1; }\n"),
    (
        "src/lib.rs",
        "pub mod target; pub mod caller; pub mod cargo;\n",
    ),
    ("src/target.rs", "\npub fn target() {}\n"),
    (
        "src/caller.rs",
        "pub fn caller() { crate::target::target(); }\n",
    ),
    ("src/cargo.rs", "pub fn cargo() { renamed::run(); }\n"),
    ("package.json", "{\"dependencies\":{\"shared\":\"1.0.0\"}}"),
    ("npm.js", "import shared from 'shared'; shared();\n"),
    ("a.ts", "export function alpha() { return 1; }\n"),
    ("b.ts", "export function beta() { return 2; }\n"),
];

pub struct CapturedArchitecture {
    pub temp: tempfile::TempDir,
    pub root: PathBuf,
    pub store: Store,
    pub owner: Arc<DaemonState>,
    pub cancel: Arc<AtomicBool>,
    serial: usize,
    workspace: Option<tempfile::TempDir>,
}

pub enum Expected<'a> {
    Full(FullReason),
    Incremental {
        dirty: &'a [&'a str],
        patched: &'a [&'a str],
        native: &'a [&'a str],
        all_b: bool,
    },
}

impl CapturedArchitecture {
    pub fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        fs::create_dir(&root).unwrap();
        for (path, text) in SOURCES {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        let root = fs::canonicalize(root).unwrap();
        let store = Store::open_for_tests(&temp.path().join("state"), &root).unwrap();
        let owner = trellis::http::new(
            store.clone(),
            IndexOptions::new(root.clone()),
            "0123456789abcdef".repeat(4),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        Self {
            temp,
            root,
            store,
            owner,
            cancel: Arc::new(AtomicBool::new(false)),
            serial: 0,
            workspace: None,
        }
    }

    pub fn published(state: tempfile::TempDir, workspace: tempfile::TempDir, store: Store) -> Self {
        let root = fs::canonicalize(workspace.path()).unwrap();
        let owner = trellis::http::new(
            store.clone(),
            IndexOptions::new(root.clone()),
            "0123456789abcdef".repeat(4),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        Self {
            temp: state,
            root,
            store,
            owner,
            cancel: Arc::new(AtomicBool::new(false)),
            serial: 0,
            workspace: Some(workspace),
        }
    }

    pub fn producers() -> Self {
        let mut fixture = Self::new();
        fixture.package_users();
        let pin = fixture.native();
        fixture.expect(pin, Expected::Full(FullReason::NoBase));
        let pin = fixture.import(true);
        fixture.expect(pin, Expected::Full(FullReason::ProducerSetChanged));
        let pin = fixture.import(false);
        fixture.expect(pin, Expected::Full(FullReason::ProducerSetChanged));
        fixture
    }

    pub fn stabilize(&self) -> ArchitectureCoreHandle {
        fs::write(
            self.root.join("src/main.rs"),
            "fn main() { let _value = 2; }\n",
        )
        .unwrap();
        self.native();
        let s = self.catch_up();
        self.expect(
            s,
            Expected::Incremental {
                dirty: &[
                    "src/main.rs",
                    "src/caller.rs",
                    "src/target.rs",
                    "a.ts",
                    "b.ts",
                ],
                patched: &[],
                native: &["src/main.rs"],
                all_b: false,
            },
        )
        .1
    }

    pub fn native(&self) -> IndexPin {
        let (graph, native, capture) = index_workspace_bundle(
            &IndexOptions::new(self.root.clone()),
            self.store.root_id(),
            &self.cancel,
            |_| {},
        )
        .unwrap();
        let writer = self.store.publication_for_tests().unwrap();
        self.store
            .publish_native(
                &graph,
                &capture,
                &native,
                &writer,
                self.store.index_baseline().unwrap(),
                &self.cancel,
            )
            .unwrap()
    }

    pub fn index_db(&self) -> PathBuf {
        let (roots, identity) = self.store.state_topology();
        roots.index_db(identity)
    }

    pub fn package_users(&self) {
        for i in 0..7 {
            fs::write(
                self.root.join(format!("npm{i}.js")),
                "import shared from 'shared'; shared();\n",
            )
            .unwrap();
        }
    }

    /// Recompute both occurrence and enclosing positions from this artifact's current bytes.
    fn definition(&self, path: &str, name: &str) -> DocSpec {
        let text = fs::read_to_string(self.root.join(path)).unwrap();
        let start = text.find(name).unwrap();
        let line = i32::try_from(text[..start].bytes().filter(|b| *b == b'\n').count()).unwrap();
        let column = i32::try_from(start - text[..start].rfind('\n').map_or(0, |i| i + 1)).unwrap();
        let declaration = text
            .find(if path.ends_with(".rs") {
                "pub fn"
            } else {
                "export function"
            })
            .unwrap();
        let end = text.rfind('}').unwrap() + 1;
        let position = |offset: usize| {
            let line = text[..offset].bytes().filter(|b| *b == b'\n').count();
            let column = offset - text[..offset].rfind('\n').map_or(0, |i| i + 1);
            [i32::try_from(line).unwrap(), i32::try_from(column).unwrap()]
        };
        let [begin_line, begin_column] = position(declaration);
        let [end_line, end_column] = position(end);
        let symbol = format!("scip fixture architecture 1 {name}().");
        DocSpec::new(path)
            .occurrence(
                &[line, column, column + i32::try_from(name.len()).unwrap()],
                &[begin_line, begin_column, end_line, end_column],
                &symbol,
                1,
            )
            .symbol(&symbol, 17, &[])
    }

    pub fn import(&mut self, rust: bool) -> IndexPin {
        self.serial += 1;
        let mut batch = BatchFixture::from_published(&self.index_db(), &self.root);
        let (language, docs) = if rust {
            let text = fs::read_to_string(self.root.join("src/caller.rs")).unwrap();
            let snapshot = self
                .store
                .architecture_reference_snapshot_for_tests()
                .unwrap();
            let source = snapshot
                .documents
                .iter()
                .find(|doc| doc.key.path == "src/caller.rs")
                .unwrap();
            let call = &source.calls[0];
            let callee = call.callee_range.as_ref().unwrap();
            let position = |offset: usize| {
                let line = text[..offset].bytes().filter(|b| *b == b'\n').count();
                let column = offset - text[..offset].rfind('\n').map_or(0, |i| i + 1);
                [i32::try_from(line).unwrap(), i32::try_from(column).unwrap()]
            };
            let [line, column] = position(callee.start);
            let [end_line, end_column] = position(callee.end);
            let [call_start_line, call_start_column] = position(call.range.start);
            let [call_line, call_column] = position(call.range.end);
            let caller = self.definition("src/caller.rs", "caller").occurrence(
                &[line, column, end_line, end_column],
                &[call_start_line, call_start_column, call_line, call_column],
                "scip fixture architecture 1 target().",
                0,
            );
            (
                "rust",
                vec![self.definition("src/target.rs", "target"), caller],
            )
        } else {
            batch = batch.reference("scip-typescript");
            (
                "typescript",
                vec![
                    self.definition("a.ts", "alpha"),
                    self.definition("b.ts", "beta"),
                ],
            )
        };
        let ordinal = batch.artifact(language, docs.clone());
        let input = self.temp.path().join(format!("input-{}.scip", self.serial));
        let output = self
            .temp
            .path()
            .join(format!("output-{}.scip", self.serial));
        fs::write(&input, batch.scip_bytes(language, &docs)).unwrap();
        let mode = crate::support::write_mode(
            self.temp.path(),
            &format!("mode-{}.json", self.serial),
            &json!({"emit": input}),
        );
        let outcome = spawn_producer(
            &ProducerSpec {
                argv: vec![
                    crate::support::stub_path().into_os_string(),
                    "--mode-file".into(),
                    mode.into_os_string(),
                    "--output".into(),
                    output.clone().into_os_string(),
                ],
                cwd: self.root.clone(),
                env: BTreeMap::new(),
                artifact: output.clone(),
                log_dir: None,
            },
            &Limits::default(),
            &self.cancel,
            &mut Hooks::default(),
        )
        .unwrap();
        assert_eq!(outcome.ending, trellis::producer::exec::Ending::Exited);
        assert_eq!(fs::read(&input).unwrap(), fs::read(&output).unwrap());
        batch.override_artifact_raw(ordinal, fs::read(output).unwrap());
        let (descriptor, sha) = batch.write(
            &self
                .root
                .join(format!(".trellis-semantic-batch-{}", self.serial)),
        );
        let limits = BatchLimits::PRODUCTION;
        let mut admitted = stage_batch(
            &self.store,
            &StageInput::external(&self.root).unwrap(),
            StageOwner {
                request_id: "architecture",
                incarnation: "captured",
            },
            &descriptor,
            &sha,
            &limits,
            &mut BatchCounters::default(),
            &self.cancel,
        )
        .unwrap();
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
        let writer = self.store.publication_for_tests().unwrap();
        self.store
            .publish_semantic_batch(
                &joined,
                admitted.stage_mut(),
                joined.destination_pin(),
                &writer,
                &self.cancel,
            )
            .unwrap()
    }

    pub fn catch_up(&self) -> IndexPin {
        let pin = self
            .store
            .catch_up_semantic_for_tests(&self.cancel)
            .unwrap();
        let semantic = self.store.semantic_at(pin).unwrap();
        // A changed source can legitimately cease to join its captured offsets. That is
        // distinct from losing an authenticated raw capture for every selected document.
        assert!(
            semantic
                .coverage
                .iter()
                .all(|row| !row.diagnostic.as_deref().is_some_and(
                    |diagnostic| diagnostic.starts_with("selected raw capture unavailable:")
                )),
            "catch-up lost an authenticated raw capture: {:?}",
            semantic.coverage
        );
        pin
    }

    pub fn build(&self, pin: IndexPin) -> (ArchitectureAttempt, ArchitectureCoreHandle) {
        let before = self.owner.architecture_verified_for_tests();
        self.owner.architecture_hint();
        let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
        loop {
            let seen = self.owner.state_version();
            if let Some(core) = self.owner.architecture_core_for_tests(pin) {
                let attempt = self
                    .owner
                    .architecture_attempts_for_tests()
                    .into_iter()
                    .find(|attempt| attempt.pin == pin && attempt.published)
                    .unwrap();
                assert!(attempt.compared, "{attempt:?}");
                assert_eq!(self.owner.architecture_verified_for_tests(), before + 1);
                assert_eq!(
                    self.owner.architecture_differences_for_tests(),
                    Vec::<String>::new(),
                    "the oracle must not mask a candidate mismatch"
                );
                let read = self.store.evidence_response().unwrap();
                let full = read.architecture_full_for_tests(pin).unwrap();
                read.finish(()).unwrap();
                assert_eq!(core.canonical().unwrap(), full);
                return (attempt, core);
            }
            assert!(
                Instant::now() < deadline,
                "owner did not publish {pin:?}: {:?}",
                self.owner.architecture_attempts_for_tests()
            );
            self.owner
                .wait_state_change(seen, deadline.saturating_duration_since(Instant::now()));
        }
    }

    pub fn canonical(&self, pin: IndexPin) -> Value {
        self.owner
            .architecture_core_for_tests(pin)
            .unwrap()
            .canonical()
            .unwrap()
    }

    pub fn packages(&self, core: &ArchitectureCoreHandle) -> Value {
        let canonical = core.canonical().unwrap();
        let packages = &canonical["references"]["selected"]["data"]["externalPackages"];
        let response = self.store.evidence_response().unwrap();
        let admitted = response
            .architecture_manifests_for_tests(core.pin())
            .unwrap();
        response.finish(()).unwrap();
        let mut manifests = 0;
        for package in packages.as_array().unwrap() {
            let evidence = package["evidence"].as_array().unwrap();
            let unique: std::collections::BTreeSet<_> =
                evidence.iter().map(Value::to_string).collect();
            assert_eq!(unique.len(), evidence.len());
            for proof in evidence {
                if proof["kind"] != "manifest" {
                    continue;
                }
                manifests += 1;
                assert_eq!(proof.as_object().unwrap().len(), 10);
                let captured = admitted
                    .iter()
                    .find(|entry| {
                        proof["sourceSetId"] == entry.source_set_id && proof["path"] == entry.path
                    })
                    .unwrap();
                assert_eq!(proof["nativeRevisionId"], captured.native_revision_id);
                assert_eq!(proof["dependencyHash"], captured.dependency_hash);
                assert_eq!(
                    proof["contentHash"],
                    captured.content_hash.as_ref().unwrap().as_str()
                );
                assert!(
                    proof["importRoot"].is_string()
                        && proof["key"].is_string()
                        && proof["section"].is_string()
                        && proof.get("versionConstraint").is_some()
                );
            }
        }
        assert!(manifests >= 2, "{packages}");
        packages.clone()
    }

    pub fn expect(
        &self,
        pin: IndexPin,
        expected: Expected<'_>,
    ) -> (ArchitectureAttempt, ArchitectureCoreHandle) {
        let (attempt, core) = self.build(pin);
        let order: Vec<trellis::native_records::DocumentKey> =
            serde_json::from_value(core.canonical().unwrap()["dependencies"]["order"].clone())
                .unwrap();
        let selected = |paths: &[&str]| {
            let keys: Vec<_> = order
                .iter()
                .filter(|key| paths.contains(&key.path.as_str()))
                .cloned()
                .collect();
            assert_eq!(keys.len(), paths.len(), "every expected path is admitted");
            keys
        };
        let (references, semantic, metrics) = match expected {
            Expected::Full(reason) => {
                assert_eq!(attempt.path, ArchitectureBuildPath::Full(reason));
                (order.clone(), order.clone(), order.clone())
            }
            Expected::Incremental {
                dirty,
                patched,
                native,
                all_b,
            } => {
                let dirty = selected(dirty);
                let patched = selected(patched);
                let reused = order
                    .iter()
                    .filter(|key| !dirty.contains(key) && !patched.contains(key))
                    .cloned()
                    .collect();
                assert_eq!(
                    attempt.path,
                    ArchitectureBuildPath::Incremental {
                        dirty: dirty.clone(),
                        patched,
                        reused
                    }
                );
                let semantic = if all_b { order.clone() } else { dirty.clone() };
                (dirty, semantic, selected(native))
            }
        };
        assert_eq!(attempt.full_retries, 0);
        assert_eq!(attempt.semantic_sources, semantic);
        assert_eq!(attempt.measured_sources, metrics);
        let candidate = &attempt.executions[&ArchitectureExecutionBranch::Candidate];
        assert_eq!(candidate.file_references, references);
        assert!(candidate.references.is_some());
        assert_eq!(candidate.measured_sources, metrics);
        let oracle = &attempt.executions[&ArchitectureExecutionBranch::Oracle];
        assert_eq!(oracle.file_references, order);
        assert!(oracle.references.is_some());
        assert_eq!(oracle.measured_sources, order);
        assert_eq!(
            self.owner.architecture_build_paths_for_tests().last(),
            Some(&(pin, attempt.path.clone()))
        );
        (attempt, core)
    }

    pub fn bounded_failure(&self, pin: IndexPin, message: &str) {
        let before = self.owner.architecture_verified_for_tests();
        self.owner.architecture_hint();
        let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
        loop {
            let seen = self.owner.state_version();
            if self
                .owner
                .architecture_build_paths_for_tests()
                .iter()
                .any(|(at, _)| *at == pin)
            {
                assert!(self.owner.architecture_core_for_tests(pin).is_none());
                let attempt = self
                    .owner
                    .architecture_attempts_for_tests()
                    .into_iter()
                    .find(|attempt| attempt.pin == pin && attempt.published)
                    .unwrap();
                assert!(attempt.compared);
                assert_eq!(self.owner.architecture_verified_for_tests(), before + 1);
                assert_eq!(
                    self.owner.architecture_differences_for_tests(),
                    Vec::<String>::new(),
                    "the oracle must not mask a candidate mismatch"
                );
                assert_eq!(
                    attempt.path,
                    ArchitectureBuildPath::Full(FullReason::IncrementalFailure)
                );
                assert_eq!(attempt.full_retries, 1);
                for trace in attempt.executions.values() {
                    assert!(trace.file_references.is_empty() && trace.measured_sources.is_empty());
                    assert!(trace.references.is_none());
                }
                let read = self.store.evidence_response().unwrap();
                let error = read.architecture_full_for_tests(pin).unwrap_err();
                read.finish(()).unwrap();
                assert_eq!(error.to_string(), message);
                return;
            }
            assert!(
                Instant::now() < deadline,
                "owner did not keep bounded failure"
            );
            self.owner
                .wait_state_change(seen, deadline.saturating_duration_since(Instant::now()));
        }
    }
}
