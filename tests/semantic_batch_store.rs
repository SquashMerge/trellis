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
        AdmittedBatch, BatchCounters, BatchLimits, StageOwner, limits, stage_batch, testing,
    },
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
