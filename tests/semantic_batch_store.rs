#![cfg(feature = "test-support")]
#[path = "support/semantic_batch.rs"]
mod semantic_batch_support;

use semantic_batch_support::{
    BatchFixture, DocSpec, edit_descriptor, edit_page, long_path_workspace, uniform_workspace,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    indexer::{IndexOptions, index_workspace_bundle},
    scip_raw::ScipCaptureError,
    semantic_batch::{
        AdmittedBatch, BatchCounters, BatchLimits, LookupBudget, StageOwner,
        descriptor::Descriptor, limits, stage_batch, testing,
    },
    semantic_evidence::SemanticPublication,
    semantic_import::{DestinationMutations, join_staged, rejoin_inventory},
    store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    },
};

struct World {
    _state: tempfile::TempDir,
    _root: tempfile::TempDir,
    root: PathBuf,
    store: Store,
    index_dir: PathBuf,
    index_db: PathBuf,
    cancel: Arc<AtomicBool>,
    batches: std::cell::Cell<usize>,
}

fn world(populate: impl FnOnce(&Path)) -> World {
    let state = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    populate(&root);
    let store = Store::open_for_tests(state.path(), &root).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let leader = store.publication_for_tests().unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            &leader,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    drop(leader);
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(state.path().join("cache"), state.path().join("data"));
    World {
        index_dir: roots.index_dir(&identity),
        index_db: roots.index_db(&identity),
        _state: state,
        _root: temp,
        root,
        store,
        cancel,
        batches: std::cell::Cell::new(0),
    }
}

impl World {
    fn fixture(&self) -> BatchFixture {
        BatchFixture::from_published(&self.index_db, &self.root).synthetic()
    }

    fn batch_dir(&self) -> PathBuf {
        let number = self.batches.get();
        self.batches.set(number + 1);
        self.root.join(format!(".trellis-semantic-batch-{number}"))
    }

    fn write(&self, fixture: &BatchFixture) -> (PathBuf, String) {
        fixture.write(&self.batch_dir())
    }

    fn stage(
        &self,
        descriptor: &(PathBuf, String),
        limits: &BatchLimits,
        counters: &mut BatchCounters,
    ) -> anyhow::Result<AdmittedBatch> {
        stage_batch(
            &self.store,
            &self.root,
            StageOwner {
                request_id: "request-1",
                incarnation: "claim-1",
            },
            &descriptor.0,
            &descriptor.1,
            limits,
            counters,
            &self.cancel,
        )
    }

    fn admit(&self, descriptor: &(PathBuf, String)) -> anyhow::Result<AdmittedBatch> {
        self.stage(
            descriptor,
            &BatchLimits::PRODUCTION,
            &mut BatchCounters::default(),
        )
    }

    fn stage_dirs(&self) -> Vec<String> {
        fs::read_dir(&self.index_dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("semantic-stage-"))
            .collect()
    }
}

fn detail(error: &anyhow::Error) -> String {
    match error.downcast_ref::<ScipCaptureError>() {
        Some(
            ScipCaptureError::InvalidAdmission(detail)
            | ScipCaptureError::InvalidArtifact(detail)
            | ScipCaptureError::LimitExceeded(detail),
        ) => detail.clone(),
        Some(other) => format!("{other:?}"),
        None => format!("{error:#}"),
    }
}

fn expect_err(result: anyhow::Result<AdmittedBatch>) -> anyhow::Error {
    match result {
        Ok(_) => panic!("admission unexpectedly succeeded"),
        Err(error) => error,
    }
}

fn two_languages(root: &Path) {
    uniform_workspace(root, &["java", "javascript"], 6);
}

fn full_fixture(world: &World) -> BatchFixture {
    let mut fixture = world.fixture().rows_per_page(4);
    for language in ["java", "javascript"] {
        let docs = fixture.paths(language);
        fixture.artifact(language, docs);
    }
    fixture
}

fn claim_count(batch: &mut AdmittedBatch) -> i64 {
    batch
        .stage_mut()
        .conn()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM claims", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn admission_stages_pages_sources_coverage_and_claims_then_cleans_up() {
    let world = world(two_languages);
    let descriptor = world.write(&full_fixture(&world));
    let mut counters = BatchCounters::default();
    let mut batch = world
        .stage(&descriptor, &BatchLimits::PRODUCTION, &mut counters)
        .unwrap();
    let stage_dir = batch.stage().dir().to_owned();
    assert_eq!(world.stage_dirs().len(), 1);
    assert!(stage_dir.join("artifact-0.scip").exists());
    let (sources, covered, claims, documents): (i64, i64, i64, i64) = {
        let conn = batch.stage_mut().conn().unwrap();
        (
            conn.query_row("SELECT COUNT(*) FROM sources", [], |r| r.get(0))
                .unwrap(),
            conn.query_row(
                "SELECT COUNT(*) FROM coverage WHERE artifact IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap(),
            conn.query_row("SELECT COUNT(*) FROM claims", [], |r| r.get(0))
                .unwrap(),
            conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
                .unwrap(),
        )
    };
    assert_eq!((sources, covered, documents), (12, 12, 12));
    assert_eq!(claims, 24, "one occurrence and one symbol per document");
    assert_eq!(counters.live_page_buffers_high_water, 1);
    assert_eq!(counters.live_unit_buffers_high_water, 1);
    assert_eq!(testing::whole_snapshot_reads(), 0);
    drop(batch);
    assert!(!stage_dir.exists());
    assert!(world.stage_dirs().is_empty());
}

#[test]
fn failed_admission_removes_its_stage() {
    let world = world(two_languages);
    let mut fixture = full_fixture(&world);
    fixture.override_artifact_bytes(0, vec![]);
    let descriptor = world.write(&fixture);
    let error = expect_err(world.admit(&descriptor));
    assert_eq!(detail(&error), "missing artifact document");
    assert!(world.stage_dirs().is_empty());
}

#[test]
fn memory_high_water_does_not_grow_with_document_count() {
    let run = |per_language: usize| {
        let world = world(|root| uniform_workspace(root, &["java", "javascript"], per_language));
        let mut fixture = world.fixture().rows_per_page(25);
        for language in ["java", "javascript"] {
            let docs = fixture.paths(language);
            fixture.artifact(language, docs);
        }
        let descriptor = world.write(&fixture);
        let mut counters = BatchCounters::default();
        let mut batch = world
            .stage(&descriptor, &BatchLimits::PRODUCTION, &mut counters)
            .unwrap();
        let claims = claim_count(&mut batch);
        (counters, claims, testing::whole_snapshot_reads())
    };
    let (small, small_claims, small_reads) = run(25);
    let (large, large_claims, large_reads) = run(100);
    assert_eq!(large_claims, 4 * small_claims);
    for (counters, reads) in [(&small, small_reads), (&large, large_reads)] {
        assert_eq!(counters.live_page_buffers_high_water, 1);
        assert_eq!(counters.live_unit_buffers_high_water, 1);
        assert_eq!(reads, 0);
    }
    assert_eq!(
        small.page_buffer_high_water_bytes,
        large.page_buffer_high_water_bytes
    );
    assert_eq!(
        small.unit_buffer_high_water_bytes,
        large.unit_buffer_high_water_bytes
    );
    assert_eq!(
        small.unit_reservation_high_water_bytes,
        large.unit_reservation_high_water_bytes
    );
}

type Edit = Box<dyn Fn(&mut Value)>;

fn descriptor_value(descriptor: &(PathBuf, String)) -> Value {
    serde_json::from_slice(&fs::read(&descriptor.0).unwrap()).unwrap()
}

#[test]
fn page_faults_fail_typed_through_production_admission() {
    let world = world(two_languages);
    let fixture = full_fixture(&world);
    let cases: Vec<(&str, Edit)> = vec![
        (
            "page ordinals",
            Box::new(|d| {
                let repeated = d["pages"][0].clone();
                d["pages"].as_array_mut().unwrap().insert(1, repeated);
            }),
        ),
        (
            "page ordinals",
            Box::new(|d| {
                d["pages"].as_array_mut().unwrap().remove(1);
            }),
        ),
        (
            "page order",
            Box::new(|d| {
                d["pages"].as_array_mut().unwrap().swap(0, 3);
            }),
        ),
        (
            "duplicate artifact",
            Box::new(|d| {
                let path = d["artifacts"][0]["relativePath"].clone();
                d["artifacts"][1]["relativePath"] = path;
            }),
        ),
        (
            "metadata binding",
            Box::new(|d| d["metadata"]["projectRoot"] = json!("file:///elsewhere")),
        ),
        (
            "producer profile pin",
            Box::new(|d| d["producer"]["version"] = json!("999")),
        ),
        (
            "invalid role path",
            Box::new(|d| d["pages"][0]["relativePath"] = json!("../outside.json")),
        ),
        (
            "selected role absent",
            Box::new(|d| d["pages"][0]["relativePath"] = json!("absent/page.json")),
        ),
    ];
    for (expected, edit) in cases {
        let written = world.write(&fixture);
        let descriptor = edit_descriptor(&written.0, edit);
        let error = expect_err(world.admit(&descriptor));
        assert_eq!(detail(&error), expected, "{error:#}");
        assert!(world.stage_dirs().is_empty());
    }
}

#[test]
fn unretained_revision_is_capture_unavailable() {
    let world = world(two_languages);
    let written = world.write(&full_fixture(&world));
    let descriptor = edit_descriptor(&written.0, |d| {
        d["sourceRevision"] = json!("not-a-retained-revision")
    });
    let error = expect_err(world.admit(&descriptor));
    assert!(
        error
            .to_string()
            .contains("capture_unavailable: captured revision not retained"),
        "{error:#}"
    );
}

#[test]
fn streamed_inventory_mismatch_with_retained_manifest_fails_typed() {
    let world = world(two_languages);
    let fixture = full_fixture(&world);
    let cases: [(&str, Edit); 2] = [
        (
            "captured source mismatch",
            Box::new(|page| page["rows"][0]["contentHash"] = json!("0".repeat(64))),
        ),
        (
            "missing captured source",
            Box::new(|page| page["rows"][0]["relativePath"] = json!("java/u00000.javb")),
        ),
    ];
    for (expected, edit) in cases {
        let written = world.write(&fixture);
        let descriptor = edit_page(&written.0, 0, &world.root, |page| edit(page));
        let error = expect_err(world.admit(&descriptor));
        assert_eq!(detail(&error), expected, "{error:#}");
    }
}

#[test]
fn source_and_coverage_row_faults_fail_typed() {
    let world = world(two_languages);
    let fixture = full_fixture(&world);
    // Pages 0..=2 hold sources (12 rows at 4 per page); pages 3..=5 hold coverage.
    let cases: [(usize, &str, Edit); 5] = [
        (
            0,
            "duplicate source path",
            Box::new(|page| {
                let first = page["rows"][0].clone();
                page["rows"][1] = first;
            }),
        ),
        (
            0,
            "unsorted source rows",
            Box::new(|page| page["rows"].as_array_mut().unwrap().swap(0, 1)),
        ),
        (
            3,
            "unsorted coverage rows",
            Box::new(|page| page["rows"].as_array_mut().unwrap().swap(0, 1)),
        ),
        (
            3,
            "coverage disposition",
            Box::new(|page| page["rows"][0]["exclusion"] = json!("failed")),
        ),
        (
            3,
            "unbound coverage path",
            Box::new(|page| page["rows"][0]["relativePath"] = json!("java/u00000.javb")),
        ),
    ];
    for (entry, expected, edit) in cases {
        let written = world.write(&fixture);
        let descriptor = edit_page(&written.0, entry, &world.root, |page| edit(page));
        let error = expect_err(world.admit(&descriptor));
        assert_eq!(detail(&error), expected, "{error:#}");
    }
}

#[test]
fn artifact_faults_fail_typed() {
    let world = world(two_languages);
    let mut fixture = full_fixture(&world);
    let docs = fixture.paths("java");
    // Bytes carry one document too many, one too few, and a repeat.
    let mut extra = docs.clone();
    extra.push(DocSpec::new("javascript/u00000.js"));
    let mut short = docs.clone();
    short.pop();
    let mut repeated = docs.clone();
    repeated.push(docs[0].clone());
    for (bytes, expected) in [
        (extra, "unlisted document"),
        (short, "missing artifact document"),
        (repeated, "duplicate document path"),
    ] {
        fixture.override_artifact_bytes(0, bytes);
        let error = expect_err(world.admit(&world.write(&fixture)));
        assert_eq!(detail(&error), expected, "{error:#}");
    }
    let written = world.write(&full_fixture(&world));
    let artifact = world.root.join(
        descriptor_value(&written)["artifacts"][0]["relativePath"]
            .as_str()
            .unwrap(),
    );
    let mut bytes = fs::read(&artifact).unwrap();
    *bytes.last_mut().unwrap() ^= 1;
    fs::write(&artifact, bytes).unwrap();
    let error = expect_err(world.admit(&written));
    assert_eq!(detail(&error), "artifact digest");
}

#[test]
fn page_artifact_and_staging_limits_are_typed() {
    let world = world(two_languages);
    let written = world.write(&full_fixture(&world));
    let cases = [
        (
            BatchLimits {
                page_rows: 2,
                ..BatchLimits::PRODUCTION
            },
            "page rows",
        ),
        (
            BatchLimits {
                pages: 2,
                ..BatchLimits::PRODUCTION
            },
            "pages",
        ),
        (
            BatchLimits {
                artifacts: 1,
                ..BatchLimits::PRODUCTION
            },
            "artifacts",
        ),
        (
            BatchLimits {
                artifact_bytes: 16,
                ..BatchLimits::PRODUCTION
            },
            "artifact bytes",
        ),
        (
            BatchLimits {
                source_documents: 5,
                ..BatchLimits::PRODUCTION
            },
            "source documents",
        ),
        (
            BatchLimits {
                staging_bytes: 1,
                ..BatchLimits::PRODUCTION
            },
            "staging bytes",
        ),
        (
            BatchLimits {
                disk_growth_bytes: 1,
                ..BatchLimits::PRODUCTION
            },
            "operation disk growth",
        ),
        (
            BatchLimits {
                unit_expansion_bytes: 8,
                ..BatchLimits::PRODUCTION
            },
            "unit expansion",
        ),
    ];
    for (limits, expected) in cases {
        let error = expect_err(world.stage(&written, &limits, &mut BatchCounters::default()));
        assert_eq!(detail(&error), expected, "{error:#}");
        assert!(world.stage_dirs().is_empty());
    }
}

#[test]
fn partial_coverage_stages_exclusions() {
    let world = world(two_languages);
    let mut fixture = world.fixture().rows_per_page(5);
    let java = fixture.paths("java");
    fixture.artifact("java", java);
    fixture.exclude("javascript", "javascript/u00001.js", "unsupported");
    let mut batch = world.admit(&world.write(&fixture)).unwrap();
    let excluded: Vec<String> = {
        let conn = batch.stage_mut().conn().unwrap();
        let mut statement = conn
            .prepare(
                "SELECT exclusion FROM coverage WHERE exclusion IS NOT NULL ORDER BY source_ord",
            )
            .unwrap();
        statement
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(excluded.len(), 6);
    assert!(excluded.contains(&"unsupported".to_string()));
    assert_eq!(batch.descriptor().totals.excluded_documents, 6);
}

#[test]
fn long_paths_stage_within_page_limits() {
    let world = world(|root| long_path_workspace(root, &["java", "rust"], 4, 120));
    let mut fixture = world.fixture().rows_per_page(3);
    for language in ["java", "rust"] {
        let docs = fixture.paths(language);
        fixture.artifact(language, docs);
    }
    let batch = world.admit(&world.write(&fixture)).unwrap();
    assert_eq!(batch.descriptor().totals.source_documents, 8);
}

#[test]
fn native_reader_streams_the_stored_inventory_in_order() {
    let world = world(two_languages);
    let pin = world.store.index_baseline().unwrap();
    let (artifact, _) = world.store.retained_native_snapshot_at(pin).unwrap();
    let streamed = world
        .store
        .with_native_reader(pin, |reader| {
            let mut stream = reader.inventory()?;
            let mut documents = vec![];
            while let Some(document) = stream.next_document()? {
                documents.push(document);
            }
            Ok(documents)
        })
        .unwrap();
    assert_eq!(streamed, artifact.revision.documents);
}

#[test]
fn cache_budget_fits_sixty_four_mebibytes() {
    let world = world(two_languages);
    let pin = world.store.index_baseline().unwrap();
    let reader_kib = world
        .store
        .with_native_reader(pin, |reader| reader.cache_size_kib())
        .unwrap();
    let kib = |bytes: u64| bytes / 1024;
    let total = limits::STAGE_CACHE_KIB
        + limits::STAGE_ATTACH_CACHE_KIB
        + kib(BatchLimits::PRODUCTION.document_cache_bytes)
        + kib(limits::IO_BUFFER_BYTES * limits::IO_BUFFERS)
        + reader_kib as u64
        + limits::BATCH_WRITER_CACHE_KIB;
    assert!(
        total <= kib(BatchLimits::PRODUCTION.cache_bytes),
        "{total} KiB"
    );
}

impl World {
    fn republish(&self) {
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

    fn join_with(
        &self,
        descriptor: &(PathBuf, String),
        limits: &BatchLimits,
        mutations: DestinationMutations,
    ) -> anyhow::Result<(
        AdmittedBatch,
        trellis::semantic_batch::JoinedBatch,
        BatchCounters,
    )> {
        testing::stage_and_join_admitted(
            &self.store,
            &self.root,
            &descriptor.0,
            &descriptor.1,
            limits,
            mutations,
            &self.cancel,
        )
    }

    /// The publication the in-memory join builds from the same batch bytes.
    fn memory_publication(
        &self,
        descriptor: &(PathBuf, String),
        mutations: DestinationMutations,
    ) -> SemanticPublication {
        let bytes = fs::read(&descriptor.0).unwrap();
        let parsed = Descriptor::parse(&bytes).unwrap();
        let pin = self
            .store
            .retained_native_pin_for(&parsed.source_revision, &parsed.source_set.id)
            .unwrap();
        let (r1, r1_sources) = self.store.retained_native_snapshot_at(pin).unwrap();
        let head = self.store.index_baseline().unwrap();
        let (mut destination, destination_sources) =
            self.store.retained_native_snapshot_at(head).unwrap();
        if r1.revision.id != destination.revision.id {
            mutations.apply(&mut destination.declarations, &mut destination.references);
        }
        let pages: Vec<Vec<u8>> = parsed
            .pages
            .iter()
            .map(|page| fs::read(self.root.join(&page.relative_path)).unwrap())
            .collect();
        let artifacts: Vec<Vec<u8>> = parsed
            .artifacts
            .iter()
            .map(|artifact| fs::read(self.root.join(&artifact.relative_path)).unwrap())
            .collect();
        let page_refs: Vec<&[u8]> = pages.iter().map(Vec::as_slice).collect();
        let artifact_refs: Vec<&[u8]> = artifacts.iter().map(Vec::as_slice).collect();
        let inventories = testing::admit_in_memory(
            &self.root,
            &r1,
            &r1_sources,
            &bytes,
            &page_refs,
            &artifact_refs,
            &self.cancel,
        )
        .unwrap();
        let mut merged = inventories[0].clone();
        merged.claims = inventories
            .iter()
            .flat_map(|inventory| inventory.claims.clone())
            .collect();
        merged.claims.sort_by(|a, b| {
            (
                a.language.as_bytes(),
                a.envelope.relative_path.as_bytes(),
                &a.canonical_bytes,
            )
                .cmp(&(
                    b.language.as_bytes(),
                    b.envelope.relative_path.as_bytes(),
                    &b.canonical_bytes,
                ))
        });
        merged.artifact_hashes = inventories
            .iter()
            .flat_map(|inventory| inventory.artifact_hashes.clone())
            .collect();
        let mut seen = std::collections::BTreeSet::new();
        merged.exclusions = inventories
            .iter()
            .flat_map(|inventory| inventory.exclusions.clone())
            .filter(|e| seen.insert((e.language.clone(), e.relative_path.clone())))
            .collect();
        let producer = parsed.check_producer().unwrap().producer();
        let basis = r1
            .revision
            .documents
            .iter()
            .filter(|document| {
                producer.languages.iter().any(|language| {
                    serde_json::to_value(language).unwrap() == json!(document.key.language)
                })
            })
            .map(|document| {
                let artifact = parsed
                    .artifacts
                    .iter()
                    .find(|a| a.language == document.key.language)
                    .unwrap_or(&parsed.artifacts[0]);
                (
                    document.key.clone(),
                    trellis::semantic_evidence::SemanticBasis {
                        producer_id: producer.id.clone(),
                        producer_version: producer.version.clone(),
                        producer_hash: producer.executable_hash.clone(),
                        artifact_hash: artifact.sha256.clone(),
                        language: serde_json::from_value(json!(document.key.language)).unwrap(),
                        source_set_id: document.key.source_set_id.clone(),
                        revision_id: r1.revision.id.clone(),
                        source_manifest_hash: merged.source_manifest_hash.clone(),
                        toolchain_hash: r1.revision.toolchain_hash.clone(),
                        config_hash: r1.revision.config_hash.clone(),
                        dependency_hash: r1.revision.dependency_hash.clone(),
                        lookup_dependencies: vec![],
                    },
                )
            })
            .collect();
        rejoin_inventory(
            &merged,
            &r1,
            &r1_sources,
            &destination,
            &destination_sources,
            producer,
            &basis,
            parsed.producer.position_encoding,
        )
        .unwrap()
        .publication
    }
}

fn assert_same_publication(staged: &SemanticPublication, memory: &SemanticPublication, case: &str) {
    assert_eq!(staged.coverage, memory.coverage, "{case}: coverage");
    assert_eq!(staged.provenance, memory.provenance, "{case}: provenance");
    assert_eq!(staged.symbols, memory.symbols, "{case}: symbols");
    assert_eq!(
        staged.declaration_bindings, memory.declaration_bindings,
        "{case}: declaration bindings"
    );
    assert_eq!(staged.references, memory.references, "{case}: references");
    assert_eq!(
        staged.call_bindings, memory.call_bindings,
        "{case}: call bindings"
    );
    assert_eq!(
        staged.type_relationships, memory.type_relationships,
        "{case}: type relationships"
    );
}

/// Byte-offset `(line, column)` of every `needle` in `source`.
fn positions(source: &str, needle: &str) -> Vec<(i32, i32)> {
    source
        .match_indices(needle)
        .map(|(start, _)| {
            let prefix = &source[..start];
            let line = prefix.bytes().filter(|b| *b == b'\n').count() as i32;
            let column = prefix.rsplit('\n').next().unwrap().len() as i32;
            (line, column)
        })
        .collect()
}

fn calls_source(count: usize, separator: &str) -> String {
    format!(
        "function run(){{ /* \u{1F600} */{separator}{}{separator}}}",
        format!("f();{separator}").repeat(count)
    )
}

fn calls_doc(source: &str) -> DocSpec {
    let mut doc = DocSpec::new("A.js");
    for (line, column) in positions(source, "f()") {
        doc = doc.occurrence(
            &[line, column, column + 1],
            &[line, column, column + 3],
            "external pkg f.",
            0,
        );
    }
    doc
}

const DEFINITIONS_A: &str = "function run(){ target(); }\n";
const DEFINITIONS_B: &str = "function target(){ }\n";
const JAVA_TYPES: &str = "class B {}\nclass A extends B {}\n";

fn definitions_docs() -> (DocSpec, DocSpec) {
    (
        DocSpec::new("src/A.js")
            .occurrence(&[0, 9, 12], &[], "pkg run.", 1)
            .occurrence(&[0, 16, 22], &[0, 16, 24], "pkg target.", 0)
            .symbol("pkg run.", 17, &[]),
        DocSpec::new("src/B.js")
            .occurrence(&[0, 9, 15], &[], "pkg target.", 1)
            .symbol("pkg target.", 17, &[]),
    )
}

fn java_doc() -> DocSpec {
    DocSpec::new("J.java")
        .occurrence(&[0, 6, 7], &[], "pkg B#", 1)
        .occurrence(&[1, 6, 7], &[], "pkg A#", 1)
        .occurrence(&[1, 16, 17], &[], "pkg B#", 0)
        .symbol("pkg B#", 7, &[])
        .symbol("pkg A#", 7, &["pkg B#"])
}

fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (path, text) in files {
        let file = root.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, text).unwrap();
    }
}

fn one_artifact_per_language(world: &World, docs: Vec<(&str, Vec<DocSpec>)>) -> (PathBuf, String) {
    let mut fixture = world.fixture();
    for (language, specs) in docs {
        fixture.artifact(language, specs);
    }
    world.write(&fixture)
}

fn definitions_world() -> World {
    world(|root| {
        write_files(
            root,
            &[("src/A.js", DEFINITIONS_A), ("src/B.js", DEFINITIONS_B)],
        )
    })
}

#[test]
fn staged_and_memory_join_stores_produce_identical_publications() {
    let check = |world: &World, descriptor: &(PathBuf, String), mutations, case: &str| {
        let (mut admitted, _, _) = world
            .join_with(descriptor, &BatchLimits::PRODUCTION, mutations)
            .unwrap();
        let staged = testing::staged_publication(&mut admitted).unwrap();
        let memory = world.memory_publication(descriptor, mutations);
        assert_same_publication(&staged, &memory, case);
        (staged, memory)
    };
    for (separator, name) in [(" ", "single-line"), ("\r\n", "multi-line")] {
        let source = calls_source(24, separator);
        let world = world(|root| write_files(root, &[("A.js", &source)]));
        let descriptor =
            one_artifact_per_language(&world, vec![("javascript", vec![calls_doc(&source)])]);
        let (staged, _) = check(&world, &descriptor, DestinationMutations::default(), name);
        assert_eq!(staged.references.len(), 24, "{name}");
        assert_eq!(staged.call_bindings.len(), 24, "{name}");
    }
    let hooks = [
        DestinationMutations::default(),
        DestinationMutations {
            duplicate_b_target: true,
            ..Default::default()
        },
        DestinationMutations {
            duplicate_a_reference: true,
            ..Default::default()
        },
        DestinationMutations {
            missing_a_reference: true,
            ..Default::default()
        },
    ];
    let (a, b) = definitions_docs();
    for (changed, label) in [(false, "same revision"), (true, "changed destination")] {
        let world = definitions_world();
        let descriptor =
            one_artifact_per_language(&world, vec![("javascript", vec![a.clone(), b.clone()])]);
        if changed {
            write_files(&world.root, &[("src/C.js", "function other(){ }\n")]);
            world.republish();
        }
        for (index, hook) in hooks.iter().enumerate() {
            let case = format!("{label}, hook {index}");
            let (staged, _) = check(&world, &descriptor, *hook, &case);
            if index == 0 {
                assert!(
                    staged.references.iter().any(|r| r.spelling == "target"),
                    "{case}"
                );
            }
        }
    }
    let world = world(|root| write_files(root, &[("J.java", JAVA_TYPES)]));
    let descriptor = one_artifact_per_language(&world, vec![("java", vec![java_doc()])]);
    let (staged, _) = check(
        &world,
        &descriptor,
        DestinationMutations::default(),
        "java types",
    );
    assert_eq!(staged.type_relationships.len(), 1);
}

/// Adds an occurrence of the `nth` `needle` in `source`. `enclosing` is the byte length of the
/// enclosing call expression starting at the same position.
fn occ_at(
    doc: DocSpec,
    source: &str,
    (needle, nth): (&str, usize),
    enclosing: Option<usize>,
    symbol: &str,
    roles: i32,
) -> DocSpec {
    let (line, column) = positions(source, needle)[nth];
    let width = needle.len() as i32;
    let enclosing = enclosing
        .map(|length| vec![line, column, column + length as i32])
        .unwrap_or_default();
    doc.occurrence(&[line, column, column + width], &enclosing, symbol, roles)
}

const OVERRIDE_JAVA: &str =
    "class Base { void f(int x) {} } class Child extends Base { void f(int x) {} }";

/// Base and Child with an overridden `f`, symbols named by `suffix`.
fn override_doc(path: &str, source: &str, suffix: &str) -> DocSpec {
    let base = format!("pkg Base{suffix}#");
    let child = format!("pkg Child{suffix}#");
    let base_f = format!("pkg Base{suffix}#f(I).");
    let child_f = format!("pkg Child{suffix}#f(I).");
    let mut doc = DocSpec::new(path);
    for (needle, nth, symbol, roles) in [
        (format!("Base{suffix}"), 0, &base, 1),
        (format!("Child{suffix}"), 0, &child, 1),
        (format!("Base{suffix}"), 1, &base, 8),
        ("f(int".to_owned(), 0, &base_f, 1),
        ("f(int".to_owned(), 1, &child_f, 1),
    ] {
        let width = if needle == "f(int" { 1 } else { needle.len() };
        let (line, column) = positions(source, &needle)[nth];
        doc = doc.occurrence(&[line, column, column + width as i32], &[], symbol, roles);
    }
    doc.symbol(&base, 7, &[])
        .symbol(&child, 7, &[&base])
        .symbol(&base_f, 26, &[])
        .symbol(&child_f, 26, &[&base_f])
}

struct Fixture {
    name: &'static str,
    files: Vec<(String, String)>,
    artifacts: Vec<(&'static str, Vec<DocSpec>)>,
}

/// The complexity fixtures of `src/semantic_import_complexity_tests.rs`, as batches: Java
/// override endpoints, a local method in another document, method multiplicity (several
/// documents and a duplicate native method), enum checks, nested invocation ranges and a
/// native gap that makes anchors unsupported.
fn complexity_fixtures() -> Vec<Fixture> {
    let mut fixtures = Vec::new();
    fixtures.push(Fixture {
        name: "java override endpoints",
        files: vec![("O.java".into(), OVERRIDE_JAVA.into())],
        artifacts: vec![("java", vec![override_doc("O.java", OVERRIDE_JAVA, "")])],
    });

    let other = "class Other { void f() {} }";
    let local_a = occ_at(
        DocSpec::new("A.java"),
        OVERRIDE_JAVA,
        ("f(int", 0),
        None,
        "local 7",
        1,
    );
    let local_a = occ_at(
        local_a,
        OVERRIDE_JAVA,
        ("f(int", 1),
        None,
        "pkg Child#f().",
        1,
    )
    .symbol("pkg Child#f().", 26, &["local 7"])
    .symbol("local 7", 26, &[]);
    let local_b = occ_at(
        DocSpec::new("B.java"),
        other,
        ("f()", 0),
        None,
        "local 7",
        1,
    )
    .symbol("local 7", 26, &[]);
    fixtures.push(Fixture {
        name: "local method in another document",
        files: vec![
            ("A.java".into(), OVERRIDE_JAVA.into()),
            ("B.java".into(), other.into()),
        ],
        artifacts: vec![("java", vec![local_a, local_b])],
    });

    let mut files = Vec::new();
    let mut docs = Vec::new();
    for ordinal in 0..3 {
        let source = OVERRIDE_JAVA
            .replace("Base", &format!("Base{ordinal}"))
            .replace("Child", &format!("Child{ordinal}"));
        let path = format!("M{ordinal}.java");
        docs.push(override_doc(&path, &source, &ordinal.to_string()));
        files.push((path, source));
    }
    let duplicate = "class Base9 { void f(int x) {} void f(int x) {} } class Child9 extends Base9 { void f(int x) {} }";
    let mut doc = DocSpec::new("D.java");
    for (needle, nth, symbol, roles) in [
        ("Base9", 0, "pkg Base9#", 1),
        ("Child9", 0, "pkg Child9#", 1),
        ("f(int", 0, "pkg Base9#f(I).", 1),
        ("f(int", 1, "pkg Base9#f(I).", 1),
        ("f(int", 2, "pkg Child9#f(I).", 1),
    ] {
        let (line, column) = positions(duplicate, needle)[nth];
        let width = if needle == "f(int" { 1 } else { needle.len() } as i32;
        doc = doc.occurrence(&[line, column, column + width], &[], symbol, roles);
    }
    docs.push(
        doc.symbol("pkg Base9#", 7, &[])
            .symbol("pkg Child9#", 7, &["pkg Base9#"])
            .symbol("pkg Base9#f(I).", 26, &[])
            .symbol("pkg Child9#f(I).", 26, &["pkg Base9#f(I)."]),
    );
    files.push(("D.java".into(), duplicate.into()));
    fixtures.push(Fixture {
        name: "method multiplicity and duplicate native method",
        files,
        artifacts: vec![("java", docs)],
    });

    let color = "enum Color { RED, GREEN }\nclass Use extends Color {}\n";
    let mut doc = DocSpec::new("E.java");
    for (needle, nth, symbol, roles) in [
        ("Color", 0, "pkg Color#", 1),
        ("RED", 0, "pkg Color#RED.", 1),
        ("Use", 0, "pkg Use#", 1),
        ("Color", 1, "pkg Color#", 8),
    ] {
        doc = occ_at(doc, color, (needle, nth), None, symbol, roles);
    }
    fixtures.push(Fixture {
        name: "enum checks",
        files: vec![("E.java".into(), color.into())],
        artifacts: vec![(
            "java",
            vec![
                doc.symbol("pkg Color#", 11, &[])
                    .symbol("pkg Color#RED.", 12, &[])
                    .symbol("pkg Use#", 7, &["pkg Color#"]),
            ],
        )],
    });

    let nested = "function f(){} function g(){} function h(){} function run(){ f(g(h())); }\n";
    let mut doc = DocSpec::new("N.js");
    for (needle, nth, symbol, roles, enclosing) in [
        ("f", 0, "pkg f.", 1, None),
        ("g", 0, "pkg g.", 1, None),
        ("h", 0, "pkg h.", 1, None),
        ("run", 0, "pkg run.", 1, None),
        ("f(g", 0, "pkg f.", 0, Some("f(g(h()))".len())),
        ("g(h", 0, "pkg g.", 0, Some("g(h())".len())),
        ("h()", 1, "pkg h.", 0, Some("h()".len())),
    ] {
        let (line, column) = positions(nested, needle)[nth];
        let width = if needle.len() > 1 && needle != "run" {
            1
        } else {
            needle.len()
        } as i32;
        let enclosing = enclosing
            .map(|length| vec![line, column, column + length as i32])
            .unwrap_or_default();
        doc = doc.occurrence(&[line, column, column + width], &enclosing, symbol, roles);
    }
    fixtures.push(Fixture {
        name: "nested invocation ranges",
        files: vec![("N.js".into(), nested.into())],
        artifacts: vec![(
            "javascript",
            vec![
                doc.symbol("pkg f.", 17, &[])
                    .symbol("pkg g.", 17, &[])
                    .symbol("pkg h.", 17, &[])
                    .symbol("pkg run.", 17, &[]),
            ],
        )],
    });

    let gap = "fn helper() {}\nfn run() { helper(); println!(\"{}\", helper()); }\n";
    let mut doc = DocSpec::new("G.rs");
    for (needle, nth, symbol, roles, enclosing) in [
        ("helper", 0, "pkg helper().", 1, None),
        ("run", 0, "pkg run().", 1, None),
        ("helper", 1, "pkg helper().", 0, Some("helper()".len())),
        ("helper", 2, "pkg helper().", 0, Some("helper()".len())),
    ] {
        doc = occ_at(doc, gap, (needle, nth), enclosing, symbol, roles);
    }
    fixtures.push(Fixture {
        name: "raw gap and unsupported family",
        files: vec![("G.rs".into(), gap.into())],
        artifacts: vec![(
            "rust",
            vec![
                doc.symbol("pkg helper().", 12, &[])
                    .symbol("pkg run().", 12, &[]),
            ],
        )],
    });
    fixtures
}

#[test]
fn staged_and_memory_join_stores_agree_on_every_complexity_fixture() {
    for fixture in complexity_fixtures() {
        for changed in [false, true] {
            let case = format!(
                "{}, {}",
                fixture.name,
                if changed {
                    "changed destination"
                } else {
                    "same revision"
                }
            );
            let world = world(|root| {
                let files: Vec<_> = fixture
                    .files
                    .iter()
                    .map(|(path, text)| (path.as_str(), text.as_str()))
                    .collect();
                write_files(root, &files)
            });
            let mut batch = world.fixture();
            for (language, docs) in &fixture.artifacts {
                batch.artifact(language, docs.clone());
            }
            let descriptor = world.write(&batch);
            if changed {
                write_files(&world.root, &[("Unrelated.js", "function other(){ }\n")]);
                world.republish();
            }
            let mutations = DestinationMutations::default();
            let codes = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
            let seen = codes.clone();
            testing::before_join_closure(move |conn| {
                let mut statement = conn.prepare("SELECT code FROM f_diag").unwrap();
                let rows = statement
                    .query_map([], |row| row.get::<_, String>(0))
                    .unwrap();
                seen.borrow_mut().extend(rows.map(Result::unwrap));
            });
            let (mut admitted, _, _) = world
                .join_with(&descriptor, &BatchLimits::PRODUCTION, mutations)
                .unwrap_or_else(|error| panic!("{case}: {error:#}"));
            let staged = testing::staged_publication(&mut admitted).unwrap();
            let memory = world.memory_publication(&descriptor, mutations);
            assert_same_publication(&staged, &memory, &case);
            if fixture.name.starts_with("local method") {
                assert!(staged.type_relationships.is_empty(), "{case}");
            } else {
                assert!(!staged.declaration_bindings.is_empty(), "{case}");
                assert!(!staged.references.is_empty(), "{case}");
            }
            if fixture.name.starts_with("method multiplicity") {
                for coverage in [&staged.coverage, &memory.coverage] {
                    let doc = coverage
                        .iter()
                        .find(|c| c.document_path == "D.java")
                        .unwrap();
                    assert_eq!(
                        doc.state,
                        trellis::semantic_evidence::CoverageState::Partial
                    );
                }
                assert!(
                    codes
                        .borrow()
                        .iter()
                        .any(|c| c == "unmeasurable_override_endpoint"),
                    "{case}: {:?}",
                    codes.borrow()
                );
            }
            if fixture.name.starts_with("java override") {
                assert!(
                    staged
                        .type_relationships
                        .iter()
                        .any(|r| r.kind == trellis::semantic_evidence::RelationshipKind::Overrides),
                    "{case}"
                );
            }
        }
    }
}

#[test]
fn batch_memory_high_water_is_one_unit_not_the_corpus() {
    // 50 rows per page keeps every page ordinal one digit at both sizes, so page lengths match.
    let run = |per_language: usize| {
        let world = world(|root| uniform_workspace(root, &["java", "javascript"], per_language));
        let mut fixture = world.fixture().rows_per_page(50);
        for language in ["java", "javascript"] {
            let docs = fixture.paths(language);
            fixture.artifact(language, docs);
        }
        let descriptor = world.write(&fixture);
        let mut limits = BatchLimits::PRODUCTION;
        limits.document_cache_bytes = 65_536;
        let (mut admitted, _, counters) = world
            .join_with(&descriptor, &limits, DestinationMutations::default())
            .unwrap();
        (
            counters,
            claim_count(&mut admitted),
            testing::whole_snapshot_reads(),
        )
    };
    let (small, small_claims, small_reads) = run(50);
    let (large, large_claims, large_reads) = run(200);
    assert_eq!(large_claims, 4 * small_claims);
    for (counters, reads) in [(&small, small_reads), (&large, large_reads)] {
        assert_eq!(counters.live_page_buffers_high_water, 1);
        assert_eq!(counters.live_unit_buffers_high_water, 1);
        assert_eq!(counters.active_document_groups_high_water, 1);
        assert!(counters.candidate_group_high_water <= 2);
        assert!(counters.document_cache_high_water_bytes <= 65_536);
        assert_eq!(reads, 0);
    }
    assert!(large.document_cache_high_water_bytes > 0);
    assert_eq!(
        small.page_buffer_high_water_bytes,
        large.page_buffer_high_water_bytes
    );
    assert_eq!(
        small.unit_buffer_high_water_bytes,
        large.unit_buffer_high_water_bytes
    );
    assert_eq!(
        small.unit_reservation_high_water_bytes,
        large.unit_reservation_high_water_bytes
    );
}

fn split_definitions(world: &World, swap: bool) -> (PathBuf, String) {
    let (a, b) = definitions_docs();
    let mut fixture = world.fixture();
    if swap {
        fixture.artifact("javascript", vec![b]);
        fixture.artifact("javascript", vec![a]);
    } else {
        fixture.artifact("javascript", vec![a]);
        fixture.artifact("javascript", vec![b]);
    }
    world.write(&fixture)
}

fn target_reference(publication: &SemanticPublication) -> trellis::semantic_evidence::Reference {
    publication
        .references
        .iter()
        .find(|r| r.spelling == "target")
        .cloned()
        .expect("target reference")
}

#[test]
fn cross_artifact_reference_resolves_internal_in_any_artifact_order() {
    let world = definitions_world();
    let mut targets = vec![];
    for swap in [false, true] {
        let descriptor = split_definitions(&world, swap);
        let (mut admitted, _, _) = world
            .join_with(
                &descriptor,
                &BatchLimits::PRODUCTION,
                DestinationMutations::default(),
            )
            .unwrap();
        let publication = testing::staged_publication(&mut admitted).unwrap();
        let reference = target_reference(&publication);
        assert_eq!(
            reference.resolution,
            trellis::semantic_evidence::Resolution::Resolved,
            "swap {swap}"
        );
        targets.push(reference.declared_target);
    }
    assert_eq!(targets[0], targets[1]);
    assert!(matches!(
        targets[0],
        Some(trellis::semantic_evidence::Target::Internal { .. })
    ));
}

#[test]
fn competing_definitions_across_artifacts_stay_ambiguous() {
    let world = world(|root| {
        write_files(
            root,
            &[
                ("src/A.js", DEFINITIONS_A),
                ("src/B.js", DEFINITIONS_B),
                ("src/C.js", DEFINITIONS_B),
            ],
        )
    });
    let (a, b) = definitions_docs();
    let c = DocSpec::new("src/C.js")
        .occurrence(&[0, 9, 15], &[], "pkg target.", 1)
        .symbol("pkg target.", 17, &[]);
    let mut fixture = world.fixture();
    fixture.artifact("javascript", vec![a, b]);
    fixture.artifact("javascript", vec![c]);
    let descriptor = world.write(&fixture);
    let (mut admitted, _, _) = world
        .join_with(
            &descriptor,
            &BatchLimits::PRODUCTION,
            DestinationMutations::default(),
        )
        .unwrap();
    let publication = testing::staged_publication(&mut admitted).unwrap();
    let reference = target_reference(&publication);
    assert_eq!(
        reference.resolution,
        trellis::semantic_evidence::Resolution::Ambiguous
    );
    assert_eq!(reference.candidates.len(), 2);
    assert!(reference.declared_target.is_none());
}

#[test]
fn duplicate_symbol_key_within_one_artifact_is_rejected_typed() {
    let world = definitions_world();
    let (a, b) = definitions_docs();
    let b = b.symbol("pkg target.", 12, &[]);
    let descriptor = one_artifact_per_language(&world, vec![("javascript", vec![a, b])]);
    let error = world
        .join_with(
            &descriptor,
            &BatchLimits::PRODUCTION,
            DestinationMutations::default(),
        )
        .map(|_| ())
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("duplicate semantic symbol key"),
        "{error:#}"
    );
}

#[test]
fn forged_derived_from_is_rejected_before_publication() {
    let world = definitions_world();
    let (a, b) = definitions_docs();
    let descriptor = one_artifact_per_language(&world, vec![("javascript", vec![a, b])]);
    testing::before_join_closure(|conn| {
        conn.execute(
            "UPDATE out_provenance SET derived_digest=?1 WHERE ord=(SELECT MIN(ord) FROM out_provenance)",
            ["0".repeat(64)],
        )
        .unwrap();
    });
    let error = world
        .join_with(
            &descriptor,
            &BatchLimits::PRODUCTION,
            DestinationMutations::default(),
        )
        .map(|_| ())
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("derived provenance without staged claim"),
        "{error:#}"
    );
}

type RowEdit = fn(&mut Value);

/// Runs a staged join whose rows of `kind` are edited just before their row-local validation.
fn join_with_row_edit(
    world: &World,
    descriptor: &(PathBuf, String),
    kind: &'static str,
    edit: RowEdit,
) -> anyhow::Error {
    testing::before_row_validation(move |row_kind, row| {
        if row_kind == kind {
            edit(row);
        }
    });
    let result = world.join_with(
        descriptor,
        &BatchLimits::PRODUCTION,
        DestinationMutations::default(),
    );
    testing::clear_row_hook();
    result.map(|_| ()).unwrap_err()
}

#[test]
fn staged_rows_run_the_row_local_validators() {
    let js = definitions_world();
    let (a, b) = definitions_docs();
    let descriptor = one_artifact_per_language(&js, vec![("javascript", vec![a, b])]);
    let cases: [(&str, &str, RowEdit, &str); 9] = [
        (
            "reference spelling against the native reference candidate",
            "reference",
            |row| {
                if row["spelling"] == "target" {
                    row["spelling"] = json!("targex");
                }
            },
            "reference differs from native reference candidate",
        ),
        (
            "call role needs a measured invocation",
            "reference",
            |row| {
                if row["spelling"] == "run" {
                    row["roles"] = json!(["definition", "call"]);
                }
            },
            "call role without measured invocation",
        ),
        (
            "declaration-site reference must be a measured declaration name",
            "reference",
            |row| {
                if row["spelling"] == "target" {
                    row["site"] = json!("declaration");
                    row["roles"] = json!(["definition"]);
                }
            },
            "declaration reference site not measured",
        ),
        (
            "declaration binding join against measured candidates",
            "declarationBinding",
            |row| row["join"]["status"] = json!("unmatched"),
            "join differs from measured candidates",
        ),
        (
            "declaration binding document against its provenance",
            "declarationBinding",
            |row| {
                if row["join"]["anchor"]["document"]["path"] == "src/A.js" {
                    row["join"]["anchor"]["document"]["path"] = json!("src/B.js");
                }
            },
            "semantic fact/provenance tuple mismatch",
        ),
        (
            "call binding join against measured candidates",
            "callBinding",
            |row| row["join"]["status"] = json!("unmatched"),
            "join differs from measured candidates",
        ),
        (
            "symbol display-name text",
            "symbol",
            |row| row["displayName"] = json!(""),
            "empty semantic text",
        ),
        (
            "duplicate symbol targets",
            "symbol",
            |row| {
                let first = row["declarations"][0].clone();
                row["declarations"].as_array_mut().unwrap().push(first);
            },
            "duplicate semantic target",
        ),
        (
            "symbol document against its provenance",
            "symbol",
            |row| {
                row["key"]["scope"] = json!("document");
                row["key"]["document"] = json!({
                    "sourceSetId": row["declarations"][0]["document"]["sourceSetId"],
                    "language": "javascript",
                    "path": "src/Z.js",
                });
            },
            "symbol evidence document mismatch",
        ),
    ];
    for (name, kind, edit, message) in cases {
        let error = join_with_row_edit(&js, &descriptor, kind, edit);
        assert!(format!("{error:#}").contains(message), "{name}: {error:#}");
    }

    let java = world(|root| write_files(root, &[("J.java", JAVA_TYPES)]));
    let descriptor = one_artifact_per_language(&java, vec![("java", vec![java_doc()])]);
    let error = join_with_row_edit(&java, &descriptor, "typeRelationship", |row| {
        row["source"]["document"]["path"] = json!("Z.java");
    });
    assert!(
        format!("{error:#}").contains("relationship source/evidence document mismatch"),
        "{error:#}"
    );
}

#[test]
fn staged_closure_rejects_cross_row_defects() {
    let world = definitions_world();
    let (a, b) = definitions_docs();
    let descriptor = one_artifact_per_language(&world, vec![("javascript", vec![a, b])]);
    let cases: [(&str, &str, &str); 3] = [
        (
            "INSERT INTO out_declaration_bindings SELECT (SELECT MAX(ord)+1 FROM out_declaration_bindings), provenance_id, body \
             FROM out_declaration_bindings WHERE ord=(SELECT MIN(ord) FROM out_declaration_bindings)",
            "duplicate declaration binding fact",
            "duplicate binding",
        ),
        (
            "UPDATE out_coverage SET body=json_set(body,'$.state','failed')",
            "proof on non-fact-bearing coverage",
            "proof on failed coverage",
        ),
        (
            "UPDATE out_provenance SET body=json_set(body,'$.contentHash',?1)",
            "captured proof document hash mismatch",
            "hash mismatch",
        ),
    ];
    for (sql, message, name) in cases {
        testing::before_join_closure(move |conn| {
            let hash = "0".repeat(64);
            let params: &[&dyn rusqlite::ToSql] = if sql.contains("?1") { &[&hash] } else { &[] };
            conn.execute(sql, params).unwrap();
        });
        let error = world
            .join_with(
                &descriptor,
                &BatchLimits::PRODUCTION,
                DestinationMutations::default(),
            )
            .map(|_| ())
            .unwrap_err();
        assert!(format!("{error:#}").contains(message), "{name}: {error:#}");
    }
}

#[test]
fn lookup_staged_row_and_output_limits_are_typed() {
    let world = definitions_world();
    let (a, b) = definitions_docs();
    let descriptor = one_artifact_per_language(&world, vec![("javascript", vec![a, b])]);
    for (name, edit) in [
        (
            "lookup steps",
            (|l: &mut BatchLimits| l.lookup_steps = 3) as fn(&mut BatchLimits),
        ),
        ("staged rows", |l| l.staged_rows = 5),
        ("output bytes", |l| l.output_bytes = 10),
    ] {
        let mut limits = BatchLimits::PRODUCTION;
        edit(&mut limits);
        let error = world
            .join_with(&descriptor, &limits, DestinationMutations::default())
            .map(|_| ())
            .unwrap_err();
        assert_eq!(detail(&error), name, "{error:#}");
    }
    let mut counters = BatchCounters::default();
    let mut admitted = world
        .stage(&descriptor, &BatchLimits::PRODUCTION, &mut counters)
        .unwrap();
    let head = world.store.index_baseline().unwrap();
    let mut join = |limits: &BatchLimits, budget: &mut LookupBudget| {
        join_staged(
            &mut admitted,
            &world.store,
            head,
            limits,
            budget,
            &mut counters,
            DestinationMutations::default(),
            &world.cancel,
        )
    };
    let mut probe = LookupBudget::new(&BatchLimits::PRODUCTION);
    join(&BatchLimits::PRODUCTION, &mut probe).unwrap();
    let one_attempt = probe.used();
    assert!(one_attempt > 0);
    let mut limits = BatchLimits::PRODUCTION;
    limits.lookup_steps = one_attempt + one_attempt / 2;
    let mut budget = LookupBudget::new(&limits);
    join(&limits, &mut budget).unwrap();
    let error = join(&limits, &mut budget).map(|_| ()).unwrap_err();
    assert_eq!(detail(&error), "lookup steps");
}

#[test]
fn a_new_destination_clears_the_destination_tables() {
    let world = definitions_world();
    let (a, b) = definitions_docs();
    let descriptor = one_artifact_per_language(&world, vec![("javascript", vec![a, b])]);
    let mut counters = BatchCounters::default();
    let mut admitted = world
        .stage(&descriptor, &BatchLimits::PRODUCTION, &mut counters)
        .unwrap();
    let limits = BatchLimits::PRODUCTION;
    let mut budget = LookupBudget::new(&limits);
    let mut join = |admitted: &mut AdmittedBatch| {
        join_staged(
            admitted,
            &world.store,
            world.store.index_baseline().unwrap(),
            &limits,
            &mut budget,
            &mut counters,
            DestinationMutations::default(),
            &world.cancel,
        )
        .unwrap()
    };
    let first = join(&mut admitted);
    let rows = |admitted: &mut AdmittedBatch| -> (i64, i64) {
        let conn = admitted.stage_mut().conn().unwrap();
        (
            conn.query_row("SELECT COUNT(*) FROM out_coverage", [], |r| r.get(0))
                .unwrap(),
            conn.query_row("SELECT COUNT(*) FROM out_provenance", [], |r| r.get(0))
                .unwrap(),
        )
    };
    let (coverage, provenance) = rows(&mut admitted);
    write_files(&world.root, &[("src/C.js", "function other(){ }\n")]);
    world.republish();
    let second = join(&mut admitted);
    assert_ne!(
        first.destination_revision_id(),
        second.destination_revision_id()
    );
    let (coverage_after, provenance_after) = rows(&mut admitted);
    assert_eq!(coverage_after, coverage + 1);
    assert_eq!(provenance_after, provenance);
}

#[test]
fn join_cursor_plans_use_no_temp_btree() {
    let world = definitions_world();
    let (a, b) = definitions_docs();
    let descriptor = one_artifact_per_language(&world, vec![("javascript", vec![a, b])]);
    let (mut admitted, _, _) = world
        .join_with(
            &descriptor,
            &BatchLimits::PRODUCTION,
            DestinationMutations::default(),
        )
        .unwrap();
    let conn = admitted.stage_mut().conn().unwrap();
    for sql in testing::join_cursor_sql() {
        let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let params = vec![rusqlite::types::Value::Null; statement.parameter_count()];
        let plan: Vec<String> = statement
            .query_map(rusqlite::params_from_iter(params), |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(
            plan.iter().all(|step| !step.contains("TEMP B-TREE")),
            "{sql}: {plan:?}"
        );
    }
}
