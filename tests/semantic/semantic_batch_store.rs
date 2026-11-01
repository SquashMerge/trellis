#![cfg(feature = "test-support")]
use crate::semantic_batch_support;

use protobuf::Message;
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
    model::IndexPin,
    scip_raw::ScipCaptureError,
    semantic_batch::{
        AdmittedBatch, BatchCounters, BatchLimits, LookupBudget, StageInput, StageOwner,
        descriptor::Descriptor, limits, stage_batch, testing,
    },
    semantic_evidence::{Freshness, Resolution, SemanticPublication},
    semantic_import::{DestinationMutations, join_staged, rejoin_inventory},
    store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    },
};

/// Counts the bytes each thread requests and frees (layout sizes), so a test can compare the
/// join's live reservations with the Rust storage it really holds. SQLite and tree-sitter
/// allocate through the C allocator and are not counted, as the live budget excludes them.
mod counting {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    struct Counting;

    thread_local! {
        static NET: Cell<i64> = const { Cell::new(0) };
        static PEAK: Cell<i64> = const { Cell::new(0) };
    }

    fn add(bytes: i64) {
        let _ = NET.try_with(|net| {
            let value = net.get() + bytes;
            net.set(value);
            let _ = PEAK.try_with(|peak| peak.set(peak.get().max(value)));
        });
    }

    // SAFETY: every call forwards to `System` with the caller's arguments unchanged.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            add(layout.size() as i64);
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            add(-(layout.size() as i64));
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            add(layout.size() as i64);
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            add(new_size as i64 - layout.size() as i64);
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    #[global_allocator]
    static COUNTING: Counting = Counting;

    /// Runs `f`; returns its result and the most bytes this thread held at once during `f`,
    /// above what it held before.
    pub fn peak<R>(f: impl FnOnce() -> R) -> (R, u64) {
        let before = NET.with(Cell::get);
        PEAK.with(|peak| peak.set(before));
        let result = f();
        (
            result,
            u64::try_from(PEAK.with(Cell::get) - before).unwrap_or(0),
        )
    }
}

struct MemoryAdmission {
    pin: IndexPin,
    parsed: Descriptor,
    r1: trellis::native_evidence::Artifact,
    r1_sources: std::collections::BTreeMap<trellis::native_evidence::DocumentKey, Vec<u8>>,
    artifacts: Vec<Vec<u8>>,
    merged: trellis::scip_raw::CapturedScipInventory,
}

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
    world_with(populate, false)
}

/// `mcp` stages the virgin request queue before the first publication, as a daemon
/// would, so an MCP session can be served over this index.
fn world_with(populate: impl FnOnce(&Path), mcp: bool) -> World {
    let state = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    populate(&root);
    let store = Store::open_for_tests(state.path(), &root).unwrap();
    if mcp {
        store.mcp_prepare_virgin_queue_for_tests().unwrap();
    }
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
            &StageInput::external(&self.root)?,
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

/// The real cap, not a test override: the first byte past 256 MiB is refused typed before any
/// artifact byte is read, and the last byte within it is not.
#[test]
fn production_artifact_cap_refuses_one_byte_over_256_mib() {
    let world = world(two_languages);
    let written = world.write(&full_fixture(&world));
    let declare = |byte_length: u64| {
        edit_descriptor(&written.0, |descriptor| {
            let others: u64 = descriptor["artifacts"].as_array().unwrap()[1..]
                .iter()
                .map(|artifact| artifact["byteLength"].as_u64().unwrap())
                .sum();
            descriptor["artifacts"][0]["byteLength"] = json!(byte_length);
            descriptor["totals"]["artifactBytes"] = json!(byte_length + others);
        })
    };
    let cap = 256 * 1024 * 1024;
    assert_eq!(BatchLimits::PRODUCTION.artifact_bytes, cap);
    let error = expect_err(world.admit(&declare(cap + 1)));
    assert!(
        matches!(error.downcast_ref::<ScipCaptureError>(),
            Some(ScipCaptureError::LimitExceeded(name)) if name == "artifact bytes"),
        "{error:#}"
    );
    assert!(world.stage_dirs().is_empty());
    // At the cap the descriptor passes and only the actual bytes disagree with it.
    let error = expect_err(world.admit(&declare(cap)));
    assert_ne!(detail(&error), "artifact bytes", "{error:#}");
    assert!(world.stage_dirs().is_empty());
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
    // The publication writer's own page cache, as the connection reports it just before commit.
    let descriptor = world.write(&full_fixture(&world));
    let (mut admitted, joined, _) = world
        .join_with(
            &descriptor,
            &BatchLimits::PRODUCTION,
            DestinationMutations::default(),
        )
        .unwrap();
    let leader = world.store.publication_for_tests().unwrap();
    let mut caches = (0i64, 0i64);
    world
        .store
        .publish_semantic_batch_observing_for_tests(
            &joined,
            admitted.stage_mut(),
            joined.destination_pin(),
            &leader,
            &world.cancel,
            |db| {
                caches = (
                    db.pragma_query_value(None, "cache_size", |row| row.get(0))?,
                    db.pragma_query_value(Some("stage"), "cache_size", |row| row.get(0))?,
                );
                Ok(())
            },
        )
        .unwrap();
    let (writer_kib, attached_kib) = (-caches.0, -caches.1);
    assert_eq!(writer_kib as u64, limits::BATCH_WRITER_CACHE_KIB);
    assert_eq!(attached_kib as u64, limits::STAGE_ATTACH_CACHE_KIB);
    let kib = |bytes: u64| bytes / 1024;
    let total = limits::STAGE_CACHE_KIB
        + attached_kib as u64
        + kib(BatchLimits::PRODUCTION.document_cache_bytes)
        + kib(limits::IO_BUFFER_BYTES * limits::IO_BUFFERS)
        + reader_kib as u64
        + writer_kib as u64;
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

    /// The whole-publication validator the memory path runs, over `publication`: it recomputes
    /// freshness independently and runs the three cross-row checks the staged join's closure
    /// queries do not repeat (contradictory resolved call targets, coverage observed roles).
    fn full_validation(
        &self,
        descriptor: &(PathBuf, String),
        mutations: DestinationMutations,
        publication: &SemanticPublication,
    ) -> anyhow::Result<()> {
        let MemoryAdmission {
            pin,
            parsed,
            r1,
            r1_sources,
            artifacts,
            merged,
        } = self.memory_admission(descriptor);
        let (toolchain, config, dependencies) =
            self.store.retained_native_components_at(pin).unwrap();
        let head = self.store.index_baseline().unwrap();
        let (mut destination, _) = self.store.retained_native_snapshot_at(head).unwrap();
        if r1.revision.id != destination.revision.id {
            mutations.apply(&mut destination.declarations, &mut destination.references);
        }
        let producer = parsed.check_producer().unwrap().producer();
        let captured = trellis::semantic_evidence::CapturedComponents {
            producer_executables: Default::default(),
            artifacts: artifacts
                .into_iter()
                .map(|bytes| {
                    (
                        (
                            producer.id.clone(),
                            trellis::native_ids::digest(b"", &bytes),
                        ),
                        bytes,
                    )
                })
                .collect(),
            sources: r1_sources,
            captured_revision: r1.revision.clone(),
            native_toolchain: toolchain.clone(),
            native_config: config.clone(),
            native_dependencies: dependencies.clone(),
            requested_producers: vec![producer],
            toolchain: trellis::native_ids::canonical(&toolchain),
            config: trellis::native_ids::canonical(&json!(config)),
            dependency: trellis::native_ids::canonical(&json!(dependencies)),
            native_extraction_components: Default::default(),
        };
        trellis::semantic_evidence::validate_evidence_profile_pinned_for_tests(
            publication,
            &destination,
            &captured,
            &merged,
        )
    }

    /// The batch decoded in memory: its descriptor, the captured native revision, the original
    /// artifact bytes and the merged inventory of every artifact.
    fn memory_admission(&self, descriptor: &(PathBuf, String)) -> MemoryAdmission {
        self.try_memory_admission(descriptor).unwrap()
    }

    fn try_memory_admission(
        &self,
        descriptor: &(PathBuf, String),
    ) -> Result<MemoryAdmission, ScipCaptureError> {
        self.try_memory_admission_with(descriptor, &BatchLimits::PRODUCTION)
    }

    fn try_memory_admission_with(
        &self,
        descriptor: &(PathBuf, String),
        limits: &BatchLimits,
    ) -> Result<MemoryAdmission, ScipCaptureError> {
        let bytes = fs::read(&descriptor.0).unwrap();
        let parsed = Descriptor::parse(&bytes).unwrap();
        let pin = self
            .store
            .retained_native_pin_for(&parsed.source_revision, &parsed.source_set.id)
            .unwrap();
        let (r1, r1_sources) = self.store.retained_native_snapshot_at(pin).unwrap();
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
        let inventories = testing::admit_in_memory_with_limits(
            &self.root,
            &r1,
            &r1_sources,
            &self.store.retained_native_components_at(pin).unwrap().1,
            &bytes,
            &page_refs,
            &artifact_refs,
            limits,
            &self.cancel,
        )?;
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
        merged.capped_documents = inventories
            .iter()
            .flat_map(|inventory| inventory.capped_documents.clone())
            .collect();
        Ok(MemoryAdmission {
            pin,
            parsed,
            r1,
            r1_sources,
            artifacts,
            merged,
        })
    }

    /// The publication the in-memory join builds from the same batch bytes.
    fn memory_publication(
        &self,
        descriptor: &(PathBuf, String),
        mutations: DestinationMutations,
    ) -> SemanticPublication {
        self.try_memory_publication(descriptor, mutations).unwrap()
    }

    fn try_memory_publication(
        &self,
        descriptor: &(PathBuf, String),
        mutations: DestinationMutations,
    ) -> anyhow::Result<SemanticPublication> {
        self.memory_rejoin(descriptor, mutations)
            .map(|result| result.publication)
    }

    fn memory_rejoin(
        &self,
        descriptor: &(PathBuf, String),
        mutations: DestinationMutations,
    ) -> anyhow::Result<trellis::semantic_import::RejoinResult> {
        self.memory_rejoin_with(descriptor, mutations, &BatchLimits::PRODUCTION)
    }

    fn memory_rejoin_with(
        &self,
        descriptor: &(PathBuf, String),
        mutations: DestinationMutations,
        limits: &BatchLimits,
    ) -> anyhow::Result<trellis::semantic_import::RejoinResult> {
        let MemoryAdmission {
            parsed,
            r1,
            r1_sources,
            merged,
            ..
        } = self.try_memory_admission_with(descriptor, limits)?;
        let head = self.store.index_baseline().unwrap();
        let (mut destination, destination_sources) =
            self.store.retained_native_snapshot_at(head).unwrap();
        if r1.revision.id != destination.revision.id {
            mutations.apply(&mut destination.declarations, &mut destination.references);
        }
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
    }
}

/// The per-language claim tallies of a report, by language name.
fn report_tallies(report: &Value) -> std::collections::BTreeMap<String, [u64; 5]> {
    report["languages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            let count = |name: &str| row[name].as_u64().unwrap();
            (
                row["language"].as_str().unwrap().to_owned(),
                [
                    count("requested"),
                    count("matched"),
                    count("skipped"),
                    count("ambiguous"),
                    count("unsupported"),
                ],
            )
        })
        .collect()
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
        let (mut admitted, sealed, _) = world
            .join_with(descriptor, &BatchLimits::PRODUCTION, mutations)
            .unwrap();
        let staged = testing::staged_publication(&mut admitted).unwrap();
        let rejoined = world.memory_rejoin(descriptor, mutations).unwrap();
        let memory = rejoined.publication;
        assert_same_publication(&staged, &memory, case);
        assert_eq!(
            report_tallies(sealed.report()),
            report_tallies(&rejoined.report),
            "{case}: report tallies"
        );
        let full = world.full_validation(descriptor, mutations, &staged);
        let reference = world.full_validation(descriptor, mutations, &memory);
        assert_eq!(
            full.is_ok(),
            reference.is_ok(),
            "{case}: {full:?} {reference:?}"
        );
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
                assert!(!staged.call_bindings.is_empty(), "{case}");
            }
            // `target()` is a plain call of a free function (#470): direct exactly while it
            // settles resolved, on both paths (`check` compared them), also under rejoin and
            // a destination that makes the target ambiguous.
            for binding in &staged.call_bindings {
                assert_eq!(
                    binding.dispatch == trellis::semantic_evidence::Dispatch::Direct,
                    binding.resolution == Resolution::Resolved,
                    "{case}: {binding:?}"
                );
            }
        }
    }
    let java_world = world(|root| write_files(root, &[("J.java", JAVA_TYPES)]));
    let descriptor = one_artifact_per_language(&java_world, vec![("java", vec![java_doc()])]);
    let (staged, _) = check(
        &java_world,
        &descriptor,
        DestinationMutations::default(),
        "java types",
    );
    assert_eq!(staged.type_relationships.len(), 1);
    // Dispatch shapes (#470) on both paths: `new` of a class by identifier and by dotted name
    // is constructor, a plain call of a free function is direct; a class called without `new`
    // and a member call of the same free function stay unknown.
    let source =
        "class W {}\nfunction m(o){ new W(); new o.W(); W(); h(); o.h(); }\nfunction h(){}\n";
    let world = world(|root| write_files(root, &[("D.js", source)]));
    let mut doc = DocSpec::new("D.js")
        .symbol("pkg W#", 7, &[])
        .symbol("pkg h().", 17, &[]);
    for (needle, nth, symbol, roles) in [
        ("W", 0, "pkg W#", 1),
        ("h", 2, "pkg h().", 1),
        ("W", 1, "pkg W#", 0),
        ("W", 2, "pkg W#", 0),
        ("W", 3, "pkg W#", 0),
        ("h", 0, "pkg h().", 0),
        ("h", 1, "pkg h().", 0),
    ] {
        doc = occ_at(doc, source, (needle, nth), None, symbol, roles);
    }
    let descriptor = one_artifact_per_language(&world, vec![("javascript", vec![doc])]);
    let (staged, _) = check(
        &world,
        &descriptor,
        DestinationMutations::default(),
        "dispatch shapes",
    );
    let mut dispatches: Vec<_> = staged
        .call_bindings
        .iter()
        .map(|b| {
            assert_eq!(b.resolution, Resolution::Resolved);
            let range = &b.join.anchor.range;
            (range.start, &source[range.start - 2..range.end], b.dispatch)
        })
        .collect();
    dispatches.sort_by_key(|(start, _, _)| *start);
    use trellis::semantic_evidence::Dispatch::{Constructor, Direct, Unknown};
    assert_eq!(
        dispatches
            .into_iter()
            .map(|(_, text, dispatch)| (text, dispatch))
            .collect::<Vec<_>>(),
        [
            ("w W", Constructor),
            ("o.W", Constructor),
            ("; W", Unknown),
            ("; h", Direct),
            ("o.h", Unknown),
        ]
    );
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

    // Java overrides whose target method lives in another document: the global method
    // summary, not the overriding document, supplies the target and its owner predicate.
    let child = "class Child extends Base { void f() {} }";
    let child_doc = |base_f: &str| {
        let doc = DocSpec::new("A.java");
        let doc = occ_at(doc, child, ("Child", 0), None, "pkg Child#", 1);
        let doc = occ_at(doc, child, ("Base", 0), None, "pkg Base#", 8);
        occ_at(doc, child, ("f", 0), None, "pkg Child#f().", 1)
            .symbol("pkg Child#", 7, &["pkg Base#"])
            .symbol("pkg Child#f().", 26, &[base_f])
    };
    let script_base = "class Base { f() {} }";
    let script_doc = occ_at(
        DocSpec::new("B.js"),
        script_base,
        ("Base", 0),
        None,
        "pkg Base#",
        1,
    );
    let script_doc = occ_at(script_doc, script_base, ("f", 0), None, "pkg Base#f().", 1)
        .symbol("pkg Base#", 7, &[])
        .symbol("pkg Base#f().", 26, &[]);
    fixtures.push(Fixture {
        name: "java override owner in another language",
        files: vec![
            ("A.java".into(), child.into()),
            ("B.js".into(), script_base.into()),
        ],
        artifacts: vec![
            ("java", vec![child_doc("pkg Base#f().")]),
            ("javascript", vec![script_doc]),
        ],
    });
    // The foreign method's owner type is named by a different symbol, so its owner predicate
    // fails and no override may be published. A lookup confined to the overriding document
    // would miss the method and publish one.
    let java_base = "class Base { void f() {} }";
    let mismatched = occ_at(
        DocSpec::new("B.java"),
        java_base,
        ("Base", 0),
        None,
        "pkg Other#",
        1,
    );
    let mismatched = occ_at(mismatched, java_base, ("f", 0), None, "pkg Base#f().", 1)
        .symbol("pkg Other#", 7, &[])
        .symbol("pkg Base#f().", 26, &[]);
    fixtures.push(Fixture {
        name: "foreign java method owner predicate fails",
        files: vec![
            ("A.java".into(), child.into()),
            ("B.java".into(), java_base.into()),
        ],
        artifacts: vec![("java", vec![child_doc("pkg Base#f()."), mismatched])],
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
            // Captured diagnostics are document-local Common results; nothing persists them.
            testing::observe_document_common(move |side, _document, facts, _conn| {
                if side == 0 {
                    seen.borrow_mut()
                        .extend(facts.diagnostics.values().flatten().cloned());
                }
            });
            let joined = world.join_with(&descriptor, &BatchLimits::PRODUCTION, mutations);
            testing::clear_document_common_observer();
            let (mut admitted, sealed, _) =
                joined.unwrap_or_else(|error| panic!("{case}: {error:#}"));
            let staged = testing::staged_publication(&mut admitted).unwrap();
            let rejoined = world.memory_rejoin(&descriptor, mutations).unwrap();
            let memory = rejoined.publication;
            assert_same_publication(&staged, &memory, &case);
            // The per-document tallies sum to the whole-corpus oracle's report counts.
            assert_eq!(
                report_tallies(sealed.report()),
                report_tallies(&rejoined.report),
                "{case}: report tallies"
            );
            world
                .full_validation(&descriptor, mutations, &staged)
                .unwrap_or_else(|error| panic!("{case}: full validation: {error:#}"));
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
            let overrides: Vec<_> = staged
                .type_relationships
                .iter()
                .filter(|r| r.kind == trellis::semantic_evidence::RelationshipKind::Overrides)
                .collect();
            if fixture.name == "java override owner in another language" {
                assert_eq!(overrides.len(), 1, "{case}");
                assert!(
                    matches!(&overrides[0].target, trellis::semantic_evidence::Target::Internal { document, .. }
                        if document.path == "B.js" && document.language == "javascript"),
                    "{case}: {overrides:?}"
                );
            }
            if fixture.name == "foreign java method owner predicate fails" {
                assert!(overrides.is_empty(), "{case}: {overrides:?}");
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

/// The unit-expansion budget bounds one document's working set plus the document cache, not
/// the batch: documents that each fit join even when their charges sum past the budget, while
/// one document that does not fit is still denied, typed.
#[test]
fn documents_that_each_fit_join_although_their_sum_exceeds_the_budget() {
    const DOCUMENTS: usize = 24;
    let source = calls_source(48, "\n");
    let path = |n: usize| format!("src/D{n:02}.js");
    let world = world(|root| {
        for n in 0..DOCUMENTS {
            write_files(root, &[(path(n).as_str(), source.as_str())]);
        }
    });
    let docs = |count: usize| {
        (0..count)
            .map(|n| {
                let mut doc = DocSpec::new(&path(n));
                for (line, column) in positions(&source, "f()") {
                    doc = doc.occurrence(
                        &[line, column, column + 1],
                        &[line, column, column + 3],
                        "external pkg f.",
                        0,
                    );
                }
                doc
            })
            .collect::<Vec<_>>()
    };
    let run = |count: usize, limits: &BatchLimits| {
        let descriptor = one_artifact_per_language(&world, vec![("javascript", docs(count))]);
        world
            .join_with(&descriptor, limits, DestinationMutations::default())
            .map(|(mut admitted, _, counters)| {
                (
                    testing::staged_publication(&mut admitted).unwrap(),
                    counters,
                )
            })
    };
    let (_, one) = run(1, &BatchLimits::PRODUCTION).unwrap();
    let peak = one.join_reservation_high_water_bytes;
    assert!(peak > 0);
    let (unbounded, many) = run(DOCUMENTS, &BatchLimits::PRODUCTION).unwrap();
    assert_eq!(unbounded.call_bindings.len(), 48 * DOCUMENTS);
    let mut limits = BatchLimits::PRODUCTION;
    limits.unit_expansion_bytes = 3 * peak;
    limits.document_cache_bytes = peak;
    // Under the production cache bound the cache alone kept more live charge than the bounded
    // budget allows: the bounded run fits only because its cache is held to its own bound.
    assert!(
        many.document_cache_high_water_bytes > limits.unit_expansion_bytes,
        "fixture too small: {} vs {}",
        many.document_cache_high_water_bytes,
        limits.unit_expansion_bytes
    );
    let (bounded, counters) = run(DOCUMENTS, &limits).unwrap();
    assert_same_publication(&bounded, &unbounded, "bounded budget");
    assert!(counters.join_reservation_high_water_bytes <= limits.unit_expansion_bytes);
    assert!(counters.document_cache_high_water_bytes <= limits.document_cache_bytes);
    // One document larger than the whole budget is still refused, typed.
    let mut tight = BatchLimits::PRODUCTION;
    tight.unit_expansion_bytes = peak / 2;
    let error = run(1, &tight)
        .map(|_| ())
        .expect_err("an oversized document is denied");
    assert!(matches!(error.downcast_ref::<ScipCaptureError>(),
        Some(ScipCaptureError::LimitExceeded(name)) if name == "unit expansion"));
}

/// One JavaScript document calling a function it declares `calls` times, once from each of
/// as many small functions, with its batch.
fn local_calls_world(calls: usize) -> (World, (PathBuf, String)) {
    let mut source = "function f(){}\n".to_owned();
    for n in 0..calls {
        source.push_str(&format!("function g{n}(){{ f(); }}\n"));
    }
    let world = world(|root| write_files(root, &[("src/A.js", source.as_str())]));
    let mut doc = DocSpec::new("src/A.js").occurrence(&[0, 9, 10], &[0, 0, 14], "pkg f.", 1);
    for (line, column) in positions(&source, "f();") {
        doc = doc.occurrence(
            &[line, column, column + 1],
            &[line, column, column + 3],
            "pkg f.",
            0,
        );
    }
    let descriptor = one_artifact_per_language(&world, vec![("javascript", vec![doc])]);
    (world, descriptor)
}

/// The join's live reservations track the Rust storage it really holds (owner ruling
/// 2026-10-30, #394): the most this thread held at once during the join never exceeds the
/// charged high water by more than a fixed 256 KiB, which covers the join's own Rust working
/// state outside the budget's owners (SQL text, the row ledger, rusqlite's statement cache);
/// and the charge is at most 1.25x that real peak. The fixture is one large document, so its
/// owners dominate.
#[test]
fn live_reservations_track_the_joins_real_storage() {
    for calls in [500, 4_000] {
        let (world, descriptor) = local_calls_world(calls);
        let mut admitted = world.admit(&descriptor).unwrap();
        let limits = BatchLimits::PRODUCTION;
        let mut budget = LookupBudget::new(&limits);
        let mut counters = BatchCounters::default();
        let destination = world.store.index_baseline().unwrap();
        let (joined, real) = counting::peak(|| {
            join_staged(
                &mut admitted,
                &world.store,
                destination,
                &limits,
                &mut budget,
                &mut counters,
                DestinationMutations::default(),
                &world.cancel,
            )
        });
        joined.unwrap();
        let publication = testing::staged_publication(&mut admitted).unwrap();
        assert_eq!(publication.call_bindings.len(), calls);
        let charged = counters.join_reservation_high_water_bytes;
        assert!(
            real <= charged + 256 * 1024,
            "{calls} calls: real peak {real} above charged {charged}"
        );
        assert!(
            charged * 4 <= real * 5,
            "{calls} calls: charged {charged} vs real peak {real}"
        );
    }
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
        let seen = Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::new()));
        let observed = seen.clone();
        testing::observe_document_common(move |side, document, facts, _conn| {
            if document.path == "src/A.js" {
                let reference = facts
                    .references
                    .iter()
                    .find(|row| row.spelling == "target")
                    .unwrap();
                assert_eq!(reference.resolution, Resolution::Resolved);
                assert!(
                    matches!(&reference.declared_target, Some(trellis::semantic_evidence::Target::Internal { document, .. }) if document.path == "src/B.js")
                );
                observed.lock().unwrap().insert(side);
            }
        });
        let (mut admitted, _, _) = world
            .join_with(
                &descriptor,
                &BatchLimits::PRODUCTION,
                DestinationMutations::default(),
            )
            .unwrap();
        testing::clear_document_common_observer();
        assert_eq!(
            *seen.lock().unwrap(),
            std::collections::BTreeSet::from([0, 1])
        );
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
fn native_join_reservation_rejects_before_document_loading() {
    let world = definitions_world();
    let descriptor = split_definitions(&world, false);
    let mut admitted = world.admit(&descriptor).unwrap();
    let mut limits = BatchLimits::PRODUCTION;
    limits.unit_expansion_bytes = 1;
    let mut budget = LookupBudget::new(&limits);
    let mut counters = BatchCounters::default();
    testing::reset_native_document_loads();
    let error = join_staged(
        &mut admitted,
        &world.store,
        world.store.index_baseline().unwrap(),
        &limits,
        &mut budget,
        &mut counters,
        DestinationMutations::default(),
        &world.cancel,
    )
    .unwrap_err();
    assert!(matches!(error.downcast_ref::<ScipCaptureError>(),
        Some(ScipCaptureError::LimitExceeded(name)) if name == "unit expansion"));
    assert_eq!(testing::native_document_loads(), 0);
    let mut budget = LookupBudget::new(&BatchLimits::PRODUCTION);
    join_staged(
        &mut admitted,
        &world.store,
        world.store.index_baseline().unwrap(),
        &BatchLimits::PRODUCTION,
        &mut budget,
        &mut counters,
        DestinationMutations::default(),
        &world.cancel,
    )
    .unwrap();
    assert!(testing::native_document_loads() > 0);
    assert!(counters.join_reservation_high_water_bytes > 0);
    assert!(
        counters.join_reservation_high_water_bytes <= BatchLimits::PRODUCTION.unit_expansion_bytes
    );
}

#[test]
fn streaming_common_uses_only_final_eligible_presence() {
    for definitions in [false, true] {
        let world = definitions_world();
        let (a, _) = definitions_docs();
        let b = if definitions {
            DocSpec::new("src/B.js").occurrence(&[0, 9, 15], &[], "pkg target.", 1)
        } else {
            DocSpec::new("src/B.js").symbol("pkg target.", 17, &[])
        };
        let descriptor = one_artifact_per_language(&world, vec![("javascript", vec![a, b])]);
        write_files(
            &world.root,
            &[("src/B.js", "function target(){ return 1; }\n")],
        );
        world.republish();
        let seen = Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::new()));
        let observed = seen.clone();
        testing::observe_document_common(move |side, document, facts, conn| {
            if document.path != "src/A.js" {
                return;
            }
            // Presence is read from the admitted claims: all of them for the captured side,
            // those of eligible documents for the destination.
            let presence: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM claims c WHERE symbol='pkg target.' AND (?1=0 OR EXISTS \
                     (SELECT 1 FROM w_docs w WHERE w.source_ord=c.source_ord AND w.eligible=1 AND w.failed=0))",
                    [side],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(presence, if side == 0 { 1 } else { 0 });
            let reference = facts
                .references
                .iter()
                .find(|row| row.spelling == "target")
                .unwrap();
            assert_eq!(
                reference.resolution,
                if side == 0 {
                    Resolution::Unresolved
                } else {
                    Resolution::External
                }
            );
            observed.lock().unwrap().insert(side);
        });
        world
            .join_with(
                &descriptor,
                &BatchLimits::PRODUCTION,
                DestinationMutations::default(),
            )
            .unwrap();
        testing::clear_document_common_observer();
        assert_eq!(
            *seen.lock().unwrap(),
            std::collections::BTreeSet::from([0, 1])
        );
    }
}

#[test]
fn streaming_exact_definition_authority_survives_incompatible_common_kind() {
    let world = world(|root| write_files(root, &[("J.java", "class A {}\n")]));
    let doc = DocSpec::new("J.java")
        .occurrence(&[0, 6, 7], &[], "pkg A#", 1)
        .symbol("pkg A#", 26, &[]);
    let descriptor = one_artifact_per_language(&world, vec![("java", vec![doc])]);
    let seen = Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::new()));
    let observed = seen.clone();
    testing::observe_document_common(move |side, document, facts, conn| {
        if document.path != "J.java" {
            return;
        }
        // A shared pin keeps one side of global rows: side 1 reads side 0's eligible rows.
        let exact: i64 = conn.query_row(
            "SELECT COUNT(*) FROM g_definitions g WHERE side=0 AND symbol='pkg A#' AND common_compatible=0 \
             AND (?1=0 OR EXISTS (SELECT 1 FROM w_docs w WHERE w.source_ord=g.owner_doc AND w.eligible=1 AND w.failed=0))",
            [side], |row| row.get(0),
        ).unwrap();
        assert_eq!(exact, 1);
        let side_one: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM g_definitions WHERE side=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(side_one, 0);
        assert_eq!(facts.declaration_bindings.len(), 1);
        assert!(facts.declaration_bindings[0].syntax_id.is_some());
        assert!(facts.symbols.iter().all(|row| row.declarations.is_empty()));
        observed.lock().unwrap().insert(side);
    });
    world
        .join_with(
            &descriptor,
            &BatchLimits::PRODUCTION,
            DestinationMutations::default(),
        )
        .unwrap();
    testing::clear_document_common_observer();
    assert_eq!(
        *seen.lock().unwrap(),
        std::collections::BTreeSet::from([0, 1])
    );
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
    // A producer artifact error, typed (#394: rust-analyzer gives `crate/` and `main().` to
    // every Cargo target), not an internal one.
    assert_eq!(
        error.downcast_ref::<ScipCaptureError>(),
        Some(&ScipCaptureError::InvalidArtifact(
            "duplicate semantic symbol key".into()
        )),
        "{error:#}"
    );
}

#[test]
fn forged_derived_from_is_rejected_before_publication() {
    let world = definitions_world();
    let (a, b) = definitions_docs();
    let descriptor = one_artifact_per_language(&world, vec![("javascript", vec![a, b])]);
    // A provenance row names its raw claim by ID: a missing claim, a claim of another
    // document and an unknown evidence kind are each rejected by the closure.
    for sql in [
        "UPDATE out_provenance SET claim_id=-1 WHERE ord=(SELECT MIN(ord) FROM out_provenance)",
        "UPDATE out_provenance SET claim_id=(SELECT c.claim_id FROM claims c JOIN sources s ON s.ord=c.source_ord \
         JOIN out_coverage v ON v.ord=out_provenance.doc WHERE s.path<>v.path LIMIT 1) \
         WHERE ord=(SELECT MIN(ord) FROM out_provenance)",
        "UPDATE out_provenance SET kind=7 WHERE ord=(SELECT MIN(ord) FROM out_provenance)",
    ] {
        testing::before_join_closure(move |conn| {
            assert_eq!(conn.execute(sql, []).unwrap(), 1, "{sql}");
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
            "{sql}: {error:#}"
        );
    }
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
fn compact_output_rows_decode_to_the_publication_and_corruption_is_typed() {
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
    let decoded = testing::staged_publication(&mut admitted).unwrap();
    let memory = world.memory_publication(&descriptor, DestinationMutations::default());
    assert_same_publication(&decoded, &memory, "compact rows");
    {
        let conn = admitted.stage_mut().conn().unwrap();
        // IDs are stored as their bytes, a fact body without its provenance ID, and one
        // provenance template per document.
        let (blobs, empty, templates, documents): (i64, i64, i64, i64) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM out_provenance WHERE typeof(id)='blob' AND length(id)=32), \
                 (SELECT COUNT(*) FROM out_references WHERE json_extract(body,'$.provenanceId')=''), \
                 (SELECT COUNT(*) FROM out_coverage WHERE provenance IS NOT NULL), \
                 (SELECT COUNT(DISTINCT doc) FROM out_provenance)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(blobs as usize, decoded.provenance.len());
        assert_eq!(empty as usize, decoded.references.len());
        assert_eq!(templates, documents);
    }
    // Each defect is a typed read failure, never a partial publication.
    for (sql, message) in [
        (
            "UPDATE out_symbols SET doc=doc+100",
            "staged row without coverage",
        ),
        (
            "UPDATE out_references SET doc=-1 WHERE ord=(SELECT MAX(ord) FROM out_references)",
            "staged output order",
        ),
        (
            "UPDATE out_coverage SET provenance=NULL",
            "staged provenance without template",
        ),
        (
            "UPDATE out_provenance SET id=x'00' WHERE ord=0",
            "staged output row",
        ),
        (
            "UPDATE out_provenance SET kind=9 WHERE ord=0",
            "staged output row",
        ),
        (
            "UPDATE out_references SET provenance_id=x'0102' WHERE ord=0",
            "staged output row",
        ),
        (
            "UPDATE out_symbols SET body=json_set(body,'$.provenanceId','x') WHERE ord=0",
            "staged output row",
        ),
        (
            "UPDATE out_coverage SET provenance=json_set(provenance,'$.id','x') WHERE provenance IS NOT NULL",
            "staged output row",
        ),
    ] {
        let (mut admitted, _, _) = world
            .join_with(
                &descriptor,
                &BatchLimits::PRODUCTION,
                DestinationMutations::default(),
            )
            .unwrap();
        let changed = admitted
            .stage_mut()
            .conn()
            .unwrap()
            .execute(sql, [])
            .unwrap();
        assert!(changed > 0, "{sql}");
        let error = testing::staged_publication(&mut admitted).unwrap_err();
        assert!(format!("{error:#}").contains(message), "{sql}: {error:#}");
    }
}

#[test]
fn staged_closure_rejects_cross_row_defects() {
    let world = definitions_world();
    let (a, b) = definitions_docs();
    let descriptor = one_artifact_per_language(&world, vec![("javascript", vec![a, b])]);
    let cases: [(&str, &str, &str); 3] = [
        (
            "INSERT INTO out_declaration_bindings SELECT (SELECT MAX(ord)+1 FROM out_declaration_bindings), doc, provenance_id, body \
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
            "UPDATE out_coverage SET provenance=json_set(provenance,'$.contentHash',?1) WHERE provenance IS NOT NULL",
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
    ] {
        let mut limits = BatchLimits::PRODUCTION;
        edit(&mut limits);
        let error = world
            .join_with(&descriptor, &limits, DestinationMutations::default())
            .map(|_| ())
            .expect_err(name);
        assert_eq!(detail(&error), name, "{error:#}");
    }
    // The compact rows of two small documents fit the pages the stage schema already holds,
    // so the physical output meter is exercised on a batch whose rows grow the stage file.
    let mixed = mixed_language_world();
    let mixed_descriptor = mixed_language_batch(&mixed);
    let limits = BatchLimits {
        output_bytes: 10,
        ..BatchLimits::PRODUCTION
    };
    let error = mixed
        .join_with(&mixed_descriptor, &limits, DestinationMutations::default())
        .map(|_| ())
        .expect_err("output bytes");
    assert_eq!(detail(&error), "output bytes", "{error:#}");
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
fn streaming_join_charges_only_its_retained_owners_and_each_output_row_once() {
    let world = definitions_world();
    let (a, b) = definitions_docs();
    let descriptor = one_artifact_per_language(&world, vec![("javascript", vec![a, b])]);
    for changed in [false, true] {
        if changed {
            write_files(&world.root, &[("src/C.js", "function other(){ }\n")]);
            world.republish();
        }
        let (mut admitted, _, counters) = world
            .join_with(
                &descriptor,
                &BatchLimits::PRODUCTION,
                DestinationMutations::default(),
            )
            .unwrap();
        let ledger = &counters.staged_row_attempts;
        let owners = [
            "seed:retained-native",
            "nat_docs",
            "nat_declarations",
            "g_definitions",
            "g_methods",
            "w_docs",
            "out_coverage",
            "out_provenance",
            "out_symbols",
            "out_declaration_bindings",
            "out_references",
            "out_call_bindings",
            "out_type_relationships",
        ];
        assert!(
            ledger.keys().all(|owner| owners.contains(&owner.as_str())),
            "changed {changed}: {ledger:?}"
        );
        let conn = admitted.stage_mut().conn().unwrap();
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(
            tables.iter().all(|t| !t.starts_with("f_")
                && !t.starts_with("ix_")
                && (!t.starts_with("w_") || t == "w_docs")),
            "no whole-corpus mirror table: {tables:?}"
        );
        // Each final row is one charged insertion; the decision table holds one row per
        // captured document.
        for table in owners
            .iter()
            .filter(|o| o.starts_with("out_") || **o == "w_docs")
        {
            let rows: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(
                ledger.get(*table).copied().unwrap_or(0),
                rows as u64,
                "changed {changed}: {table}"
            );
        }
        let captured_documents: i64 = conn
            .query_row("SELECT COUNT(*) FROM nat_docs WHERE rev=0", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(ledger["w_docs"], captured_documents as u64);
        assert!(ledger["out_references"] > 0 && ledger["out_provenance"] > 0);
        // A shared pin writes no destination-side global rows: its eligible context is the
        // eligible view of the captured rows. A changed destination is discovered again.
        let side_one: i64 = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM g_definitions WHERE side=1) + (SELECT COUNT(*) FROM g_methods WHERE side=1)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(side_one > 0, changed, "changed {changed}");
        // Declaration status covers exact-definition targets only (this fixture has no
        // duplicate syntax IDs), never every native declaration. A shared pin keeps the
        // captured status of its targets. An unshared pin keeps the destination status of
        // every captured or destination target, and captured status only for destination
        // targets that are not captured ones (none here).
        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |row| row.get(0)).unwrap() };
        let captured_status = count("SELECT COUNT(*) FROM nat_declarations WHERE rev=0");
        let destination_status = count("SELECT COUNT(*) FROM nat_declarations WHERE rev=1");
        let captured_targets = count(
            "SELECT COUNT(*) FROM (SELECT DISTINCT owner_doc, syntax_id FROM g_definitions WHERE side=0)",
        );
        let all_targets =
            count("SELECT COUNT(*) FROM (SELECT DISTINCT owner_doc, syntax_id FROM g_definitions)");
        let other_targets = count(
            "SELECT COUNT(*) FROM (SELECT DISTINCT owner_doc, syntax_id FROM g_definitions g WHERE side=1 \
             AND NOT EXISTS (SELECT 1 FROM g_definitions c WHERE c.side=0 AND c.owner_doc=g.owner_doc \
             AND c.syntax_id=g.syntax_id))",
        );
        assert!(captured_targets > 0);
        if changed {
            assert_eq!(
                (captured_status, destination_status),
                (other_targets, all_targets),
                "changed"
            );
            assert_eq!(other_targets, 0);
        } else {
            assert_eq!((captured_status, destination_status), (captured_targets, 0));
        }
        // Native status is rewritten every attempt; only the document inventory is retained.
        let retained: i64 = conn
            .query_row("SELECT COUNT(*) FROM nat_docs", [], |row| row.get(0))
            .unwrap();
        assert!(retained > 0);
        assert_eq!(
            ledger["nat_declarations"],
            (captured_status + destination_status) as u64,
            "changed {changed}"
        );
    }
}

/// An unshared pin keeps destination declaration status only for exact-definition targets.
/// Each case changes the destination's declarations between the pins (a target renamed, moved,
/// removed, its referrer edited, or a competing declaration added) and the staged publication
/// must still equal the independent memory join, with the same report and full validation.
#[test]
fn unshared_target_status_matches_memory_when_destination_declarations_change() {
    type Files = &'static [(&'static str, &'static str)];
    let edits: [(&str, Files, &[&str]); 6] = [
        (
            "target renamed",
            &[("src/B.js", "function other(){ }\n")],
            &[],
        ),
        (
            "target moved",
            &[("src/B.js", "\n\nfunction target(){ }\n")],
            &[],
        ),
        ("target removed", &[], &["src/B.js"]),
        (
            "referrer edited",
            &[("src/A.js", "function run(){ target(); }\n// edit\n")],
            &[],
        ),
        (
            "competing declaration",
            &[("src/C.js", "function target(){ }\n")],
            &[],
        ),
        // Unchanged sources, but the destination measures B's target under another syntax
        // ID: a destination-only target whose captured status the remaining pass writes.
        (
            "target re-measured",
            &[("src/C.js", "function other(){ }\n")],
            &[],
        ),
    ];
    for (case, writes, removals) in edits {
        let world = definitions_world();
        let (a, b) = definitions_docs();
        let descriptor = one_artifact_per_language(&world, vec![("javascript", vec![a, b])]);
        write_files(&world.root, writes);
        for path in removals {
            fs::remove_file(world.root.join(path)).unwrap();
        }
        world.republish();
        let mutations = DestinationMutations {
            renamed_b_target: case == "target re-measured",
            ..Default::default()
        };
        let (mut admitted, sealed, counters) = world
            .join_with(&descriptor, &BatchLimits::PRODUCTION, mutations)
            .unwrap();
        let staged = testing::staged_publication(&mut admitted).unwrap();
        let rejoined = world.memory_rejoin(&descriptor, mutations).unwrap();
        assert_same_publication(&staged, &rejoined.publication, case);
        assert_eq!(
            report_tallies(sealed.report()),
            report_tallies(&rejoined.report),
            "{case}: report tallies"
        );
        let full = world.full_validation(&descriptor, mutations, &staged);
        let reference = world.full_validation(&descriptor, mutations, &rejoined.publication);
        assert_eq!(
            full.is_ok(),
            reference.is_ok(),
            "{case}: {full:?} {reference:?}"
        );
        // The status rows are bounded by the targets: each one names a target key.
        let conn = admitted.stage_mut().conn().unwrap();
        let (status, keyed): (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), COUNT(*) FILTER (WHERE EXISTS (SELECT 1 FROM g_definitions g \
                 JOIN sources s ON s.ord=g.owner_doc JOIN nat_docs d ON d.language=s.language \
                 AND d.path=s.path WHERE d.rev=n.rev AND d.ord=n.doc_ord AND g.syntax_id=n.syntax_id)) \
                 FROM nat_declarations n",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, keyed, "{case}");
        assert_eq!(
            counters.staged_row_attempts["nat_declarations"], status as u64,
            "{case}"
        );
        let destination_only: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM g_definitions g WHERE side=1 AND NOT EXISTS \
                 (SELECT 1 FROM g_definitions c WHERE c.side=0 AND c.owner_doc=g.owner_doc \
                 AND c.syntax_id=g.syntax_id)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(destination_only > 0, case == "target re-measured", "{case}");
    }
}

/// On a shared pin eligibility and settlement run as one walk that assumes every document with
/// claims is eligible. Whichever document breaks the assumption, the result must equal the
/// separate passes', and the discarded rows stay charged.
#[test]
fn fused_settlement_falls_back_to_the_separate_passes_exactly() {
    let world = definitions_world();
    let (a, b) = definitions_docs();
    let descriptor = one_artifact_per_language(&world, vec![("javascript", vec![a, b])]);
    let run = |separate: bool, failed: Option<&str>| {
        testing::separate_settlement(separate);
        testing::fail_eligibility_of(failed);
        let result = world.join_with(
            &descriptor,
            &BatchLimits::PRODUCTION,
            DestinationMutations::default(),
        );
        testing::separate_settlement(false);
        testing::fail_eligibility_of(None);
        let (mut admitted, sealed, counters) = result.unwrap();
        let publication = testing::staged_publication(&mut admitted).unwrap();
        let conn = admitted.stage_mut().conn().unwrap();
        let coverage: i64 = conn
            .query_row("SELECT COUNT(*) FROM out_coverage", [], |row| row.get(0))
            .unwrap();
        (
            publication,
            sealed.report().clone(),
            counters.staged_row_attempts["out_coverage"],
            coverage as u64,
        )
    };
    for failed in [None, Some("src/A.js"), Some("src/B.js")] {
        let case = format!("{failed:?}");
        let (fused, fused_report, fused_attempts, rows) = run(false, failed);
        let (separate, separate_report, separate_attempts, separate_rows) = run(true, failed);
        assert_same_publication(&fused, &separate, &case);
        assert_eq!(fused_report, separate_report, "{case}");
        assert_eq!((rows, separate_rows), (2, 2), "{case}");
        assert_eq!(separate_attempts, 2, "{case}");
        // A.js settles first: when B.js fails, A.js's settled rows were discarded but stay
        // charged. When A.js fails, nothing was settled before the assumption broke.
        let discarded = u64::from(failed == Some("src/B.js"));
        assert_eq!(fused_attempts, 2 + discarded, "{case}");
        if failed.is_none() {
            assert!(fused.references.iter().any(|r| r.spelling == "target"));
        } else {
            assert!(
                fused
                    .coverage
                    .iter()
                    .any(|c| c.state == trellis::semantic_evidence::CoverageState::Failed)
            );
        }
    }
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
    // The per-call callable probe (#481) is a keyed search of the covering owner index, never
    // a scan of a side's definitions.
    let sql = testing::join_target_callable_sql();
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
        plan.iter().any(|step| step.contains(
            "USING COVERING INDEX g_definitions_owner (side=? AND owner_doc=? AND syntax_id=?)"
        )),
        "{sql}: {plan:?}"
    );
}

impl World {
    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open_with_flags(
            &self.index_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap()
    }

    fn count(&self, sql: &str) -> i64 {
        self.db().query_row(sql, [], |row| row.get(0)).unwrap()
    }

    /// Joins `descriptor` against the head and publishes it, returning the join for inspection.
    fn publish(
        &self,
        descriptor: &(PathBuf, String),
    ) -> anyhow::Result<(
        IndexPin,
        AdmittedBatch,
        trellis::semantic_batch::JoinedBatch,
    )> {
        let (mut admitted, joined, _) = self.join_with(
            descriptor,
            &BatchLimits::PRODUCTION,
            DestinationMutations::default(),
        )?;
        let leader = self.store.publication_for_tests()?;
        let pin = self.store.publish_semantic_batch(
            &joined,
            admitted.stage_mut(),
            joined.destination_pin(),
            &leader,
            &self.cancel,
        )?;
        Ok((pin, admitted, joined))
    }
}

/// Canonical, order-independent form of one table of a publication.
fn canonical<T: serde::Serialize>(rows: &[T]) -> Vec<String> {
    let mut rows: Vec<String> = rows
        .iter()
        .map(|row| serde_json::to_string(row).unwrap())
        .collect();
    rows.sort();
    rows
}

fn assert_published_matches_staged(
    published: &trellis::semantic_evidence::SemanticSnapshot,
    staged: &SemanticPublication,
) {
    assert_eq!(canonical(&published.coverage), canonical(&staged.coverage));
    assert_eq!(
        canonical(&published.provenance),
        canonical(&staged.provenance)
    );
    assert_eq!(canonical(&published.symbols), canonical(&staged.symbols));
    assert_eq!(
        canonical(&published.declaration_bindings),
        canonical(&staged.declaration_bindings)
    );
    assert_eq!(
        canonical(&published.references),
        canonical(&staged.references)
    );
    assert_eq!(
        canonical(&published.call_bindings),
        canonical(&staged.call_bindings)
    );
    assert_eq!(
        canonical(&published.type_relationships),
        canonical(&staged.type_relationships)
    );
}

#[test]
fn two_artifacts_for_one_producer_store_artifact_keyed_raw_rows() {
    let world = definitions_world();
    let descriptor = split_definitions(&world, false);
    let parsed = Descriptor::parse(&fs::read(&descriptor.0).unwrap()).unwrap();
    let (pin, mut admitted, joined) = world.publish(&descriptor).unwrap();
    assert_eq!(
        pin.index_revision,
        joined.destination_pin().index_revision + 1
    );
    let db = world.db();
    assert_eq!(world.count("SELECT COUNT(*) FROM semantic_raw_batches"), 1);
    let captures: Vec<(i64, String, i64)> = db
        .prepare("SELECT ordinal,artifact_hash,length(binary_bytes) FROM semantic_raw_captures ORDER BY ordinal")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(captures.len(), 2);
    for (capture, artifact) in captures.iter().zip(&parsed.artifacts) {
        assert_eq!(capture.1, artifact.sha256);
        assert_eq!(capture.2 as u64, artifact.byte_length);
    }
    for ordinal in 0..2 {
        let documents: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM semantic_raw_documents d JOIN semantic_raw_captures c ON c.capture_id=d.capture_id WHERE c.ordinal=?1",
                [ordinal],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(documents, 1, "artifact {ordinal}");
    }
    let proofs = world.count("SELECT COUNT(*) FROM semantic_raw_proofs");
    assert_eq!(proofs, claim_count(&mut admitted));
    assert_eq!(
        world.count(
            "SELECT COUNT(*) FROM semantic_revision_raw_selections s JOIN semantic_raw_batches b ON b.batch_id=s.batch_id"
        ),
        1
    );
    let published = world.store.semantic_at(pin).unwrap();
    let staged = testing::staged_publication(&mut admitted).unwrap();
    assert_published_matches_staged(&published, &staged);
    assert!(
        published
            .provenance
            .iter()
            .filter_map(|p| p.basis.as_ref())
            .map(|basis| basis.artifact_hash.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == 2
    );
    let report = world
        .store
        .semantic_batch_report_at(pin, &parsed_sha(&descriptor))
        .unwrap()
        .unwrap();
    assert_eq!(report["descriptorSha256"], json!(descriptor.1));
}

fn parsed_sha(descriptor: &(PathBuf, String)) -> String {
    descriptor.1.clone()
}

/// `(proof_id, kind, digest, language, path)` of every stored proof, ordinal-free.
fn stored_proofs(world: &World) -> Vec<(String, String, String, String, String)> {
    world
        .db()
        .prepare("SELECT proof_id,record_kind,fact_digest,language,path FROM semantic_raw_proofs ORDER BY proof_id")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn proof_ids_and_fact_digests_ignore_page_split_and_artifact_order() {
    let variant = |world: &World, swap: bool, rows: usize| {
        let (a, b) = definitions_docs();
        let mut fixture = world.fixture().rows_per_page(rows);
        if swap {
            fixture.artifact("javascript", vec![b]);
            fixture.artifact("javascript", vec![a]);
        } else {
            fixture.artifact("javascript", vec![a]);
            fixture.artifact("javascript", vec![b]);
        }
        world.write(&fixture)
    };
    // Within one revision the staged proof and provenance identities are the same however the
    // batch is split.
    let world = definitions_world();
    let identities = |descriptor: &(PathBuf, String)| {
        let (mut admitted, _, _) = world
            .join_with(
                descriptor,
                &BatchLimits::PRODUCTION,
                DestinationMutations::default(),
            )
            .unwrap();
        let claims: Vec<(String, String)> = admitted
            .stage_mut()
            .conn()
            .unwrap()
            .prepare("SELECT proof_id,fact_digest FROM claims ORDER BY proof_id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let publication = testing::staged_publication(&mut admitted).unwrap();
        let mut provenance: Vec<String> = publication
            .provenance
            .iter()
            .map(|p| format!("{}|{:?}", p.id, p.derived_from))
            .collect();
        provenance.sort();
        (claims, provenance)
    };
    let (claims, provenance) = identities(&variant(&world, false, 1));
    assert_eq!(claims.len(), 5);
    for (swap, rows) in [(false, 2), (true, 1), (true, 8)] {
        let (other_claims, other_provenance) = identities(&variant(&world, swap, rows));
        assert_eq!(claims, other_claims, "swap {swap}, rows {rows}");
        assert_eq!(provenance, other_provenance, "swap {swap}, rows {rows}");
    }
    // Published rows carry the same fact digests whatever the split.
    let digests = |swap: bool, rows: usize| {
        let world = definitions_world();
        let descriptor = variant(&world, swap, rows);
        world.publish(&descriptor).unwrap();
        stored_proofs(&world)
            .into_iter()
            .map(|(_, kind, digest, language, path)| (kind, digest, language, path))
            .collect::<Vec<_>>()
    };
    let mut expected = digests(false, 1);
    expected.sort();
    for (swap, rows) in [(false, 2), (true, 1), (true, 8)] {
        let mut other = digests(swap, rows);
        other.sort();
        assert_eq!(expected, other, "swap {swap}, rows {rows}");
    }
}

#[test]
fn identical_envelopes_are_deduplicated_per_authenticated_tuple() {
    let world = definitions_world();
    let (a, b) = definitions_docs();
    // The same occurrence is recorded twice in one artifact.
    let repeated = a.clone().occurrence(&[0, 9, 12], &[], "pkg run.", 1);
    let descriptor = one_artifact_per_language(&world, vec![("javascript", vec![repeated, b])]);
    let (_, mut admitted, _) = world.publish(&descriptor).unwrap();
    assert_eq!(world.count("SELECT COUNT(*) FROM semantic_raw_proofs"), 5);
    assert_eq!(claim_count(&mut admitted), 5);
    assert_eq!(
        world.count("SELECT COUNT(DISTINCT capture_id||fact_digest) FROM semantic_raw_proofs"),
        5
    );
    // The same facts in a second world carry the same digests: the dedup key is the fact, not
    // the artifact position.
    let other = definitions_world();
    let descriptor =
        one_artifact_per_language(&other, vec![("javascript", vec![a, definitions_docs().1])]);
    other.publish(&descriptor).unwrap();
    let digests = |world: &World| -> Vec<String> {
        world
            .db()
            .prepare("SELECT fact_digest FROM semantic_raw_proofs ORDER BY fact_digest")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(digests(&world), digests(&other));
}

#[test]
fn empty_document_languages_bind_through_batch_publication_and_stored_rejoin() {
    let world = world(|root| {
        write_files(
            root,
            &[
                ("src/A.js", DEFINITIONS_A),
                ("src/B.js", DEFINITIONS_B),
                ("src/C.py", "def f():\n    f()\n"),
            ],
        )
    });
    let (a, b) = definitions_docs();
    let python = DocSpec::new("src/C.py")
        .occurrence(&[0, 4, 5], &[], "pkg f().", 1)
        .occurrence(&[1, 4, 5], &[1, 4, 7], "pkg f().", 0);
    let mut fixture = world.fixture();
    let mut originals = Vec::new();
    for (language, docs) in [("javascript", vec![a, b]), ("python", vec![python])] {
        let ordinal = fixture.artifact(language, docs.clone());
        let mut index =
            scip::types::Index::parse_from_bytes(&fixture.scip_bytes(language, &docs)).unwrap();
        for document in &mut index.documents {
            document.language.clear();
        }
        let bytes = index.write_to_bytes().unwrap();
        fixture.override_artifact_raw(ordinal, bytes.clone());
        originals.push(bytes);
    }
    let descriptor = world.write(&fixture);
    let parsed = Descriptor::parse(&fs::read(&descriptor.0).unwrap()).unwrap();
    let (pin, mut admitted, _) = world.publish(&descriptor).unwrap();
    let staged = testing::staged_publication(&mut admitted).unwrap();
    world
        .full_validation(&descriptor, DestinationMutations::default(), &staged)
        .unwrap();
    let published = world.store.semantic_at(pin).unwrap();
    assert_published_matches_staged(&published, &staged);
    let assert_bound = |snapshot: &trellis::semantic_evidence::SemanticSnapshot| {
        use trellis::semantic_evidence::Language;
        assert!(!snapshot.provenance.is_empty());
        for proof in &snapshot.provenance {
            let expected = if proof.document.path.ends_with(".py") {
                ("python", Language::Python)
            } else {
                ("javascript", Language::Javascript)
            };
            assert_eq!(proof.document.language, expected.0);
            assert_eq!(proof.basis.as_ref().unwrap().language, expected.1);
        }
    };
    assert_bound(&published);
    let proofs = stored_proofs(&world);
    assert_eq!(proofs.len(), 7);
    for (_, _, _, language, path) in &proofs {
        assert_eq!(
            language,
            if path.ends_with(".py") {
                "python"
            } else {
                "javascript"
            }
        );
    }
    let db = world.db();
    for (artifact, original) in parsed.artifacts.iter().zip(&originals) {
        let (stored_hash, stored): (String, Vec<u8>) = db
            .query_row(
                "SELECT artifact_hash,binary_bytes FROM semantic_raw_captures WHERE ordinal=?1",
                [artifact.ordinal],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored, *original);
        assert_eq!(stored_hash, artifact.sha256);
        assert_eq!(stored_hash, semantic_batch_support::sha256(original));
        let index = scip::types::Index::parse_from_bytes(&stored).unwrap();
        assert!(
            index
                .documents
                .iter()
                .all(|document| document.language.is_empty())
        );
    }
    drop(db);
    let imported = world.basis_artifacts();
    assert_eq!(imported.len(), 3);
    // Native recovery authenticates the stored originals again, not the admitted stage.
    write_files(&world.root, &[("Unrelated.java", "class Unrelated {}\n")]);
    world.republish_with_rejoin();
    assert_eq!(world.rejoin_failures(), 0);
    assert_eq!(world.basis_artifacts(), imported);
    assert_eq!(stored_proofs(&world), proofs);
    assert_bound(
        &world
            .store
            .semantic_at(world.store.index_baseline().unwrap())
            .unwrap(),
    );
}

// These marker fields are copied from the real scip-typescript 0.4.0 and scip-python
// 0.6.6 outputs diagnosed in #423. Both omit SymbolInformation.kind (numeric 0).
fn real_module_marker_batch(language: &str) {
    let (files, a, b, marker_symbols) = match language {
        "javascript" => (
            vec![
                // The producer fixture's package root names its module markers (#443).
                (
                    "package.json",
                    "{\"name\":\"trellis-producer-fixture-js\",\"version\":\"1.0.0\"}\n",
                ),
                (
                    "src/a.js",
                    "export function greet(name) {\n  return \"hello \" + name;\n}\n",
                ),
                (
                    "src/b.js",
                    "import { greet } from \"./a.js\";\n\nexport function welcome(user) {\n  return greet(user) + \"!\";\n}\n",
                ),
            ],
            DocSpec::new("src/a.js")
                .occurrence(
                    &[0, 0, 0],
                    &[0, 0, 3, 0],
                    "scip-typescript npm trellis-producer-fixture-js 1.0.0 src/`a.js`/",
                    1,
                )
                .symbol(
                    "scip-typescript npm trellis-producer-fixture-js 1.0.0 src/`a.js`/",
                    0,
                    &[],
                )
                .occurrence(
                    &[0, 16, 21],
                    &[],
                    "scip-typescript npm trellis-producer-fixture-js 1.0.0 src/`a.js`/greet().",
                    1,
                )
                .symbol(
                    "scip-typescript npm trellis-producer-fixture-js 1.0.0 src/`a.js`/greet().",
                    17,
                    &[],
                ),
            DocSpec::new("src/b.js")
                .occurrence(
                    &[0, 0, 0],
                    &[0, 0, 5, 0],
                    "scip-typescript npm trellis-producer-fixture-js 1.0.0 src/`b.js`/",
                    1,
                )
                .symbol(
                    "scip-typescript npm trellis-producer-fixture-js 1.0.0 src/`b.js`/",
                    0,
                    &[],
                )
                .occurrence(
                    &[3, 9, 14],
                    &[3, 9, 20],
                    "scip-typescript npm trellis-producer-fixture-js 1.0.0 src/`a.js`/greet().",
                    0,
                ),
            [
                "scip-typescript npm trellis-producer-fixture-js 1.0.0 src/`a.js`/",
                "scip-typescript npm trellis-producer-fixture-js 1.0.0 src/`b.js`/",
            ],
        ),
        "python" => (
            vec![
                (
                    "pkg/a.py",
                    "def greet(name):\n    return \"hello \" + name\n",
                ),
                (
                    "pkg/b.py",
                    "from pkg.a import greet\n\n\ndef welcome(user):\n    return greet(user) + \"!\"\n",
                ),
            ],
            DocSpec::new("pkg/a.py")
                .occurrence(
                    &[0, 0, 0],
                    &[],
                    "scip-python python trellis-workspace 0 `pkg.a`/__init__:",
                    1,
                )
                .symbol(
                    "scip-python python trellis-workspace 0 `pkg.a`/__init__:",
                    0,
                    &[],
                )
                .occurrence(
                    &[0, 4, 9],
                    &[],
                    "scip-python python trellis-workspace 0 `pkg.a`/greet().",
                    1,
                )
                .symbol(
                    "scip-python python trellis-workspace 0 `pkg.a`/greet().",
                    12,
                    &[],
                ),
            DocSpec::new("pkg/b.py")
                .occurrence(
                    &[0, 0, 0],
                    &[],
                    "scip-python python trellis-workspace 0 `pkg.b`/__init__:",
                    1,
                )
                .symbol(
                    "scip-python python trellis-workspace 0 `pkg.b`/__init__:",
                    0,
                    &[],
                )
                .occurrence(
                    &[4, 11, 16],
                    &[4, 11, 22],
                    "scip-python python trellis-workspace 0 `pkg.a`/greet().",
                    8,
                ),
            [
                "scip-python python trellis-workspace 0 `pkg.a`/__init__:",
                "scip-python python trellis-workspace 0 `pkg.b`/__init__:",
            ],
        ),
        _ => unreachable!(),
    };
    let world = world(|root| write_files(root, &files));
    let descriptor = one_artifact_per_language(&world, vec![(language, vec![a, b])]);
    let memory = world.memory_admission(&descriptor);
    let markers: Vec<_> = memory
        .merged
        .claims
        .iter()
        .filter(|claim| {
            claim.envelope.record_kind == "occurrence"
                && marker_symbols.contains(&claim.envelope.raw["symbol"].as_str().unwrap())
        })
        .collect();
    assert_eq!(markers.len(), 2);
    for marker in &markers {
        assert_eq!(
            marker.envelope.raw["range"],
            json!({"encoding":"legacy","values":[0,0,0]})
        );
        assert_eq!(marker.envelope.raw["symbolRoles"], json!(1));
    }
    let (pin, mut admitted, _) = world.publish(&descriptor).unwrap();
    let staged = testing::staged_publication(&mut admitted).unwrap();
    world
        .full_validation(&descriptor, DestinationMutations::default(), &staged)
        .unwrap();
    let published = world.store.semantic_at(pin).unwrap();
    assert_published_matches_staged(&published, &staged);
    for symbol in marker_symbols {
        let marker = published
            .symbols
            .iter()
            .find(|row| row.key.symbol == symbol)
            .unwrap();
        assert!(
            marker.declarations.is_empty(),
            "file marker is not a measured declaration name"
        );
    }
    assert!(
        published
            .references
            .iter()
            .any(|row| row.spelling == "greet")
    );
    assert!(
        published
            .call_bindings
            .iter()
            .any(|row| row.call_id.is_some())
    );
    assert!(
        published
            .declaration_bindings
            .iter()
            .any(|row| row.syntax_id.is_some())
    );
    // No measured row may be fabricated from either zero-width file marker.
    for marker in markers {
        let derived: Vec<_> = published
            .provenance
            .iter()
            .filter(|row| {
                row.derived_from
                    .as_ref()
                    .is_some_and(|from| from.provenance_id == marker.captured_proof_id)
            })
            .map(|row| &row.id)
            .collect();
        assert!(
            published
                .declaration_bindings
                .iter()
                .all(|row| !derived.contains(&&row.provenance_id))
        );
        assert!(
            published
                .references
                .iter()
                .all(|row| !derived.contains(&&row.provenance_id))
        );
        assert!(
            published
                .call_bindings
                .iter()
                .all(|row| !derived.contains(&&row.provenance_id))
        );
    }
    let db = world.db();
    for claim in &memory.merged.claims {
        let (kind, digest, bytes): (String, String, Vec<u8>) = db.query_row(
            "SELECT record_kind,fact_digest,canonical_bytes FROM semantic_raw_proofs WHERE proof_id=?1",
            [&claim.captured_proof_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        assert_eq!(kind, claim.envelope.record_kind);
        assert_eq!(digest, claim.fact_digest);
        assert_eq!(bytes, claim.canonical_bytes);
    }
    let (stored_hash, stored): (String, Vec<u8>) = db
        .query_row(
            "SELECT artifact_hash,binary_bytes FROM semantic_raw_captures",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored, memory.artifacts[0]);
    assert_eq!(
        stored_hash,
        semantic_batch_support::sha256(&memory.artifacts[0])
    );
    drop(db);
    let proofs = stored_proofs(&world);
    drop(admitted);
    write_files(&world.root, &[("Unrelated.java", "class Unrelated {}\n")]);
    world.republish_with_rejoin();
    assert_eq!(world.rejoin_failures(), 0);
    assert_eq!(stored_proofs(&world), proofs);
}

#[test]
fn real_typescript_empty_module_markers_publish_and_rejoin_without_measured_anchors() {
    real_module_marker_batch("javascript");
}

/// Real scip-typescript 0.4.0 bytes for issue #444: its reference occurrences carry no
/// enclosing range, and calls bind at their exact measured callee spans on both join paths.
#[test]
fn real_typescript_calls_bind_identically_in_batch_and_memory_joins() {
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/semantic-import/issue444-javascript-calls");
    let read = |path: &str| fs::read_to_string(fixture_dir.join(path)).unwrap();
    let (lib, main) = (read("src/lib.js"), read("src/main.js"));
    let world = world(|root| {
        write_files(
            root,
            &[
                ("src/lib.js", &lib),
                ("src/main.js", &main),
                // The fixture's package root names its module markers (#443).
                (
                    "package.json",
                    r#"{"name":"issue444-fixture","version":"1.0.0"}"#,
                ),
            ],
        )
    });
    let mut fixture = world.fixture().reference("scip-typescript");
    let ordinal = fixture.artifact(
        "javascript",
        vec![DocSpec::new("src/lib.js"), DocSpec::new("src/main.js")],
    );
    fixture.override_artifact_raw(ordinal, fs::read(fixture_dir.join("index.scip")).unwrap());
    let descriptor = world.write(&fixture);
    let (mut admitted, _, _) = world
        .join_with(
            &descriptor,
            &BatchLimits::PRODUCTION,
            DestinationMutations::default(),
        )
        .unwrap();
    let staged = testing::staged_publication(&mut admitted).unwrap();
    drop(admitted);
    let memory = world.memory_publication(&descriptor, DestinationMutations::default());
    assert_same_publication(&staged, &memory, "real scip-typescript calls");
    // Callee and dispatch (owner ruling 2026-10-30, #470): plain calls of free functions,
    // the imported `helper` included, are direct. scip-typescript's kind 0 (unspecified) defers
    // to the measured native kind (#473), so `new Widget(3)` binds the class constructor and is
    // constructor, and the member call `w.run()` binds the method with dispatch unknown.
    let callees = |bindings: &[trellis::semantic_evidence::CallBinding]| {
        let mut callees: Vec<_> = bindings
            .iter()
            .map(|binding| {
                let range = &binding.join.anchor.range;
                assert!(binding.call_id.is_some());
                (&main[range.start..range.end], binding.dispatch)
            })
            .collect();
        callees.sort_by_key(|(callee, _)| *callee);
        callees
    };
    {
        use trellis::semantic_evidence::Dispatch::{Constructor, Direct, Unknown};
        assert_eq!(
            callees(&staged.call_bindings),
            [
                ("Widget", Constructor),
                ("helper", Direct),
                ("plain", Direct),
                ("plain", Direct),
                ("register", Direct),
                ("run", Unknown),
            ]
        );
    }
    for name in ["helper", "Widget", "run"] {
        let binding = staged
            .call_bindings
            .iter()
            .find(|binding| {
                let range = &binding.join.anchor.range;
                &main[range.start..range.end] == name
            })
            .unwrap();
        assert_eq!(binding.resolution, Resolution::Resolved, "{name}");
    }

    let (pin, mut admitted, _) = world.publish(&descriptor).unwrap();
    let staged = testing::staged_publication(&mut admitted).unwrap();
    drop(admitted);
    let published = world.store.semantic_at(pin).unwrap();
    assert_published_matches_staged(&published, &staged);
    assert_eq!(published.call_bindings.len(), 6);
    // An unrelated edit rejoins every unchanged call binding at the new revision.
    write_files(&world.root, &[("Unrelated.java", "class Unrelated {}\n")]);
    world.republish_with_rejoin();
    assert_eq!(world.rejoin_failures(), 0);
    let head = world.store.index_baseline().unwrap();
    assert_eq!(
        callees(&world.store.semantic_at(head).unwrap().call_bindings),
        callees(&staged.call_bindings)
    );
}

/// #473: the real #444 bytes with an explicit SCIP kind on the constructor and `run` symbols.
/// `Constructor` (9) agrees only with the constructor; `Method` (26) agrees with both native
/// `method` declarations; `Function` (17) conflicts with both. Batch equals memory in each case.
#[test]
fn real_typescript_explicit_symbol_kind_is_checked_identically_in_batch_and_memory() {
    use protobuf::EnumOrUnknown;
    use trellis::semantic_evidence::Dispatch::{Constructor, Direct, Unknown};
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/semantic-import/issue444-javascript-calls");
    let read = |path: &str| fs::read_to_string(fixture_dir.join(path)).unwrap();
    let (lib, main) = (read("src/lib.js"), read("src/main.js"));
    for (kind, widget, run) in [
        (
            9,
            (Resolution::Resolved, Constructor),
            (Resolution::Unresolved, Unknown),
        ),
        (
            26,
            (Resolution::Resolved, Constructor),
            (Resolution::Resolved, Unknown),
        ),
        (
            17,
            (Resolution::Unresolved, Unknown),
            (Resolution::Unresolved, Unknown),
        ),
    ] {
        let mut index = scip::types::Index::parse_from_bytes(
            &fs::read(fixture_dir.join("index.scip")).unwrap(),
        )
        .unwrap();
        let mut replaced = 0;
        for info in index
            .documents
            .iter_mut()
            .flat_map(|d| d.symbols.iter_mut())
        {
            if ["Widget#`<constructor>`().", "Widget#run()."]
                .iter()
                .any(|suffix| info.symbol.ends_with(suffix))
            {
                info.kind = EnumOrUnknown::from_i32(kind);
                replaced += 1;
            }
        }
        assert_eq!(replaced, 2);
        let world = world(|root| {
            write_files(
                root,
                &[
                    ("src/lib.js", &lib),
                    ("src/main.js", &main),
                    (
                        "package.json",
                        r#"{"name":"issue444-fixture","version":"1.0.0"}"#,
                    ),
                ],
            )
        });
        let mut fixture = world.fixture().reference("scip-typescript");
        let ordinal = fixture.artifact(
            "javascript",
            vec![DocSpec::new("src/lib.js"), DocSpec::new("src/main.js")],
        );
        fixture.override_artifact_raw(ordinal, index.write_to_bytes().unwrap());
        let descriptor = world.write(&fixture);
        let (mut admitted, _, _) = world
            .join_with(
                &descriptor,
                &BatchLimits::PRODUCTION,
                DestinationMutations::default(),
            )
            .unwrap();
        let staged = testing::staged_publication(&mut admitted).unwrap();
        drop(admitted);
        let memory = world.memory_publication(&descriptor, DestinationMutations::default());
        assert_same_publication(&staged, &memory, &format!("explicit kind {kind}"));
        let mut callees: Vec<_> = staged
            .call_bindings
            .iter()
            .map(|binding| {
                let range = &binding.join.anchor.range;
                (
                    &main[range.start..range.end],
                    (binding.resolution, binding.dispatch),
                )
            })
            .collect();
        callees.sort_by_key(|(callee, _)| *callee);
        let direct = (Resolution::Resolved, Direct);
        assert_eq!(
            callees,
            [
                ("Widget", widget),
                ("helper", direct),
                ("plain", direct),
                ("plain", direct),
                ("register", direct),
                ("run", run),
            ],
            "explicit kind {kind}"
        );
    }
}

/// Real scip-typescript 0.4.0 bytes for issue #466 (`index` with the fixture's `tsconfig.json`,
/// producer metadata dropped). `new Set(..)` names four merged lib declarations and `Error(..)`
/// two, so each call publishes one ambiguous row per occurrence, all sharing its `call_id`.
/// The call graph, sequence and line navigation read them as one ambiguous call carrying the
/// whole candidate union, without failing or scheduling recovery.
#[test]
fn real_typescript_ambiguous_calls_read_as_one_edge_with_the_union() {
    use trellis::graph_query::{BoundaryReason, GraphQuery, Visit};
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/semantic-import/issue466-ambiguous-calls");
    let main = fs::read_to_string(fixture_dir.join("src/main.js")).unwrap();
    let world = world(|root| {
        write_files(
            root,
            &[
                ("src/main.js", &main),
                (
                    "package.json",
                    r#"{"name":"issue466-fixture","version":"1.0.0"}"#,
                ),
            ],
        )
    });
    let mut fixture = world.fixture().reference("scip-typescript");
    let ordinal = fixture.artifact("javascript", vec![DocSpec::new("src/main.js")]);
    fixture.override_artifact_raw(ordinal, fs::read(fixture_dir.join("index.scip")).unwrap());
    let descriptor = world.write(&fixture);
    let (pin, admitted, _) = world.publish(&descriptor).unwrap();
    drop(admitted);
    let published = world.store.semantic_at(pin).unwrap();
    let rows_of = |callee: &str| -> Vec<_> {
        published
            .call_bindings
            .iter()
            .filter(|b| {
                let range = &b.join.anchor.range;
                &main[range.start..range.end] == callee
            })
            .collect()
    };
    for (callee, count) in [("Set", 4), ("Error", 2)] {
        let rows = rows_of(callee);
        assert_eq!(rows.len(), count, "{callee} rows");
        // Plain-call shape, but ambiguous: never direct (#470).
        assert!(rows.iter().all(|b| b.resolution == Resolution::Ambiguous
            && b.call_id == rows[0].call_id
            && b.candidates.len() == count
            && b.dispatch == trellis::semantic_evidence::Dispatch::Unknown));
    }
    let producer = published.producers[0].id.clone();
    let (_, native, _) = index_workspace_bundle(
        &IndexOptions::new(world.root.clone()),
        world.store.root_id(),
        &world.cancel,
        |_| {},
    )
    .unwrap();
    let caller = native
        .declarations
        .iter()
        .find(|d| d.name.as_deref() == Some("caller"))
        .unwrap()
        .syntax_id
        .clone();

    let answer = world
        .store
        .evidence_response()
        .unwrap()
        .graph_query(
            &GraphQuery {
                root_syntax_id: caller.clone(),
                semantic_producer_id: Some(producer.clone()),
                depth: 2,
                max_nodes: 150,
                max_calls: 500,
                include_external: true,
                include_tests: true,
            },
            None,
            &mut || Ok(()),
        )
        .unwrap();
    let graph = answer.outcome.expect("contract success");
    let ambiguous: Vec<_> = graph
        .edges
        .iter()
        .filter(|e| e.boundary_reason == BoundaryReason::Ambiguous)
        .collect();
    assert_eq!(ambiguous.len(), 2);
    for edge in &ambiguous {
        assert_eq!(edge.visit, Visit::Boundary);
        let binding = edge.binding.as_ref().unwrap();
        let callee = &main[binding.join.anchor.range.start..binding.join.anchor.range.end];
        let rows = rows_of(callee);
        let lowest = rows.iter().map(|b| &b.provenance_id).min().unwrap();
        assert_eq!(&binding.provenance_id, lowest, "{callee}");
        assert_eq!(binding.candidates, rows[0].candidates, "{callee}");
        for row in &rows {
            assert!(
                graph.provenance.iter().any(|p| p.id == row.provenance_id),
                "{callee} proof"
            );
        }
    }

    let view = world
        .store
        .sequence_at(&caller, pin, false)
        .unwrap()
        .unwrap();
    fn ambiguous_steps(steps: &[trellis::behavior::SequenceStep]) -> usize {
        steps
            .iter()
            .map(|step| {
                usize::from(step.call_evidence.as_ref().is_some_and(|e| {
                    e.boundary_reason == BoundaryReason::Ambiguous
                        && e.resolution == Some(Resolution::Ambiguous)
                })) + ambiguous_steps(&step.children)
                    + ambiguous_steps(&step.alternate)
            })
            .sum()
    }
    assert_eq!(ambiguous_steps(&view.steps), 2);
    assert_eq!(view.semantic_warnings, graph.warnings);
    for line in 1..=3 {
        world
            .store
            .navigation_at(
                &serde_json::from_value(json!({"expectedRevision":pin,"path":"src/main.js",
                    "line":line,"semanticProducerId":producer}))
                .unwrap(),
            )
            .unwrap();
    }
    assert!(!world.store.recovery_pending_for_tests());
}

#[test]
fn real_python_empty_module_markers_publish_and_rejoin_without_measured_anchors() {
    real_module_marker_batch("python");
}

#[test]
fn invalid_empty_or_reversed_occurrences_reject_typed_without_partial_publication() {
    let world = world(|root| write_files(root, &[("src/a.js", "function greet() {}\n")]));
    let head = world.store.index_baseline().unwrap();
    for (range, symbol, roles, kind) in [
        (vec![0, 0, 0], "pkg greet().", 1, 17),
        (vec![0, 0, 0], "pkg greet().", 0, 17),
        (
            vec![0, 0, 0],
            "scip-typescript npm trellis-producer-fixture-js 1.0.0 src/`a.js`/",
            0,
            0,
        ),
        (
            vec![0, 1, 0],
            "scip-typescript npm trellis-producer-fixture-js 1.0.0 src/`a.js`/",
            1,
            0,
        ),
    ] {
        let doc = DocSpec::new("src/a.js")
            .occurrence(&[0, 9, 14], &[], "pkg valid().", 1)
            .symbol("pkg valid().", 17, &[])
            .occurrence(&range, &[], symbol, roles)
            .symbol(symbol, kind, &[]);
        let descriptor = one_artifact_per_language(&world, vec![("javascript", vec![doc])]);
        let error = world.publish(&descriptor).map(|_| ()).unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<ScipCaptureError>(),
                Some(ScipCaptureError::InvalidArtifact(_))
            ),
            "{range:?}, {symbol}, roles {roles}: {error:#}"
        );
        assert_nothing_published(&world, head);
        assert!(world.stage_dirs().is_empty());
    }
}

/// Owner ruling 2026-10-29 (#439): `relative_path` on a signature `Document` (as scip-java writes
/// it) is presentation-only through batch admission, publication and stored reauthentication.
#[test]
fn signature_relative_path_is_presentation_only_in_batch_and_stored_rejoin() {
    let publish = |signature_path: Option<&[u8]>| {
        let world = definitions_world();
        let (a, b) = definitions_docs();
        let docs = vec![a, b];
        let mut fixture = world.fixture();
        let ordinal = fixture.artifact("javascript", docs.clone());
        let mut index =
            scip::types::Index::parse_from_bytes(&fixture.scip_bytes("javascript", &docs)).unwrap();
        let mut signature = scip::types::Signature::new();
        signature.text = "function run()".into();
        if let Some(path) = signature_path {
            // The pinned crate's `Signature` has no field 1; carry it as the raw wire field.
            signature
                .special_fields
                .mut_unknown_fields()
                .add_length_delimited(1, path.to_vec());
        }
        index.documents[0].symbols[0].signature_documentation = Some(signature).into();
        fixture.override_artifact_raw(ordinal, index.write_to_bytes().unwrap());
        let descriptor = world.write(&fixture);
        (world, descriptor)
    };
    let digests = |world: &World| {
        stored_proofs(world)
            .into_iter()
            .map(|(_, kind, digest, language, path)| (kind, digest, language, path))
            .collect::<std::collections::BTreeSet<_>>()
    };
    let (without, descriptor) = publish(None);
    without.publish(&descriptor).unwrap();
    let (world, descriptor) = publish(Some(b"src/A.js"));
    world.publish(&descriptor).unwrap();
    let proofs = stored_proofs(&world);
    assert_eq!(proofs.len(), 5);
    assert_eq!(digests(&world), digests(&without));
    // Native recovery authenticates the stored original, field 1 included, again.
    write_files(&world.root, &[("Unrelated.java", "class Unrelated {}\n")]);
    world.republish_with_rejoin();
    assert_eq!(world.rejoin_failures(), 0);
    assert_eq!(stored_proofs(&world), proofs);

    let (malformed, descriptor) = publish(Some(&[0x73, 0xff, 0xfe]));
    let error = expect_err(malformed.admit(&descriptor));
    assert_eq!(
        error.downcast_ref::<ScipCaptureError>(),
        Some(&ScipCaptureError::InvalidArtifact(
            "signature relative path".into()
        ))
    );
}

#[test]
fn nonempty_document_language_mismatch_still_fails_typed_in_batch_admission() {
    let world = definitions_world();
    let (a, b) = definitions_docs();
    let docs = vec![a, b];
    let mut fixture = world.fixture();
    let ordinal = fixture.artifact("javascript", docs.clone());
    let mut index =
        scip::types::Index::parse_from_bytes(&fixture.scip_bytes("javascript", &docs)).unwrap();
    index.documents[0].language = "python".into();
    fixture.override_artifact_raw(ordinal, index.write_to_bytes().unwrap());
    let error = expect_err(world.admit(&world.write(&fixture)));
    assert_eq!(
        error.downcast_ref::<ScipCaptureError>(),
        Some(&ScipCaptureError::InvalidArtifact(
            "document source binding".into()
        ))
    );
    assert!(world.stage_dirs().is_empty());
}

#[test]
fn original_artifact_bytes_are_retained_byte_identical() {
    let world = definitions_world();
    let descriptor = split_definitions(&world, true);
    let parsed = Descriptor::parse(&fs::read(&descriptor.0).unwrap()).unwrap();
    world.publish(&descriptor).unwrap();
    let db = world.db();
    for artifact in &parsed.artifacts {
        let original = fs::read(world.root.join(&artifact.relative_path)).unwrap();
        let stored: Vec<u8> = db
            .query_row(
                "SELECT binary_bytes FROM semantic_raw_captures WHERE ordinal=?1",
                [artifact.ordinal],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, original, "artifact {}", artifact.ordinal);
    }
    let stored_batch: Vec<u8> = db
        .query_row(
            "SELECT descriptor_bytes FROM semantic_raw_batches",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored_batch, fs::read(&descriptor.0).unwrap());
}

#[test]
fn replayed_batch_refills_partially_collected_raw_rows() {
    let world = definitions_world();
    // A reference to no measured syntax is stored as a proof that no provenance row uses.
    let a = definitions_docs()
        .0
        .occurrence(&[0, 0, 1], &[], "pkg unused.", 0);
    let mut fixture = world.fixture();
    fixture.artifact("javascript", vec![a]);
    fixture.exclude("javascript", "src/B.js", "unsupported");
    let descriptor = world.write(&fixture);
    let (first, _, _) = world.publish(&descriptor).unwrap();
    let captures = world.count("SELECT capture_id FROM semantic_raw_captures");
    let before = (
        world.count("SELECT COUNT(*) FROM semantic_raw_proofs"),
        world.count("SELECT COUNT(*) FROM semantic_raw_documents"),
        world.count("SELECT COUNT(*) FROM semantic_raw_exclusions"),
    );
    assert!(before.0 > 0 && before.1 > 0 && before.2 == 1, "{before:?}");
    // Part of the batch was collected while it was unselected.
    world
        .store
        .with_index_sql_for_tests(|db| {
            db.execute("DELETE FROM semantic_raw_documents", [])?;
            db.execute("DELETE FROM semantic_raw_exclusions", [])?;
            let removed = db.execute(
                "DELETE FROM semantic_raw_proofs WHERE rowid IN (SELECT p.rowid FROM semantic_raw_proofs p WHERE NOT EXISTS(SELECT 1 FROM semantic_provenance v WHERE v.capture_id=p.capture_id AND v.derived_from_provenance_id=p.proof_id))",
                [],
            )?;
            assert!(removed > 0, "no unreferenced proof to collect");
            Ok(())
        })
        .unwrap();
    // The request is replayed after publication, against the new head.
    let (second, _, _) = world.publish(&descriptor).unwrap();
    assert_eq!(second.index_revision, first.index_revision + 1);
    let after = (
        world.count("SELECT COUNT(*) FROM semantic_raw_proofs"),
        world.count("SELECT COUNT(*) FROM semantic_raw_documents"),
        world.count("SELECT COUNT(*) FROM semantic_raw_exclusions"),
    );
    assert_eq!(before, after);
    assert_eq!(world.count("SELECT COUNT(*) FROM semantic_raw_batches"), 1);
    assert_eq!(
        world.count("SELECT capture_id FROM semantic_raw_captures"),
        captures
    );
    assert_eq!(world.count("SELECT COUNT(*) FROM semantic_raw_captures"), 1);
}

#[test]
fn artifact_captured_by_another_batch_fails_typed() {
    let world = definitions_world();
    let first = split_definitions(&world, false);
    let (pin, _, _) = world.publish(&first).unwrap();
    // A different descriptor over a revision that shares the first artifact.
    let (a, _) = definitions_docs();
    let mut fixture = world.fixture();
    fixture.artifact("javascript", vec![a]);
    fixture.exclude("javascript", "src/B.js", "notRequested");
    let second = world.write(&fixture);
    let error = world.publish(&second).err().expect("shared artifact");
    assert!(
        format!("{error:#}")
            .starts_with("invalid_request: artifact already captured by another batch"),
        "{error:#}"
    );
    assert_eq!(world.store.index_baseline().unwrap(), pin);
    assert_eq!(world.count("SELECT COUNT(*) FROM semantic_raw_batches"), 1);
    assert_eq!(world.count("SELECT COUNT(*) FROM semantic_raw_captures"), 2);
    assert_eq!(
        world.count("SELECT COUNT(*) FROM semantic_revision_raw_selections WHERE batch_id NOT IN (SELECT batch_id FROM semantic_raw_batches)"),
        0
    );
}

#[test]
fn generation_change_before_publish_is_a_revision_conflict() {
    let world = definitions_world();
    let descriptor = split_definitions(&world, false);
    let (mut admitted, joined, _) = world
        .join_with(
            &descriptor,
            &BatchLimits::PRODUCTION,
            DestinationMutations::default(),
        )
        .unwrap();
    let leader = world.store.publication_for_tests().unwrap();
    // The head moves between the join and the publication.
    write_files(&world.root, &[("src/C.js", "function other(){ }\n")]);
    world.republish();
    let moved = world.store.index_baseline().unwrap();
    assert_ne!(moved, joined.destination_pin());
    let error = world
        .store
        .publish_semantic_batch(
            &joined,
            admitted.stage_mut(),
            joined.destination_pin(),
            &leader,
            &world.cancel,
        )
        .unwrap_err();
    assert!(
        format!("{error:#}").starts_with("revision conflict"),
        "{error:#}"
    );
    // A pin from another generation never matches the join either.
    let foreign = IndexPin {
        index_generation: uuid::Uuid::new_v4(),
        ..joined.destination_pin()
    };
    let error = world
        .store
        .publish_semantic_batch(
            &joined,
            admitted.stage_mut(),
            foreign,
            &leader,
            &world.cancel,
        )
        .unwrap_err();
    assert!(
        format!("{error:#}").starts_with("revision conflict"),
        "{error:#}"
    );
    assert_eq!(world.store.index_baseline().unwrap(), moved);
    assert_eq!(world.count("SELECT COUNT(*) FROM semantic_raw_batches"), 0);
    assert_eq!(
        world.count("SELECT COUNT(*) FROM semantic_revision_raw_selections"),
        0
    );
    // The retry joins again on the same stage, which reopens its connection, and publishes.
    let mut budget = LookupBudget::new(&BatchLimits::PRODUCTION);
    let rejoined = join_staged(
        &mut admitted,
        &world.store,
        moved,
        &BatchLimits::PRODUCTION,
        &mut budget,
        &mut BatchCounters::default(),
        DestinationMutations::default(),
        &world.cancel,
    )
    .unwrap();
    let pin = world
        .store
        .publish_semantic_batch(
            &rejoined,
            admitted.stage_mut(),
            moved,
            &leader,
            &world.cancel,
        )
        .unwrap();
    assert_eq!(pin.index_revision, moved.index_revision + 1);
    assert_eq!(world.count("SELECT COUNT(*) FROM semantic_raw_batches"), 1);
}

#[test]
fn synthetic_producer_row_records_generator_source_identity_kind() {
    let world = definitions_world();
    let (a, b) = definitions_docs();
    let descriptor = one_artifact_per_language(&world, vec![("javascript", vec![a, b])]);
    world.publish(&descriptor).unwrap();
    let kinds: Vec<(String, String)> = world
        .db()
        .prepare("SELECT id,identity_kind FROM semantic_producers")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(kinds.len(), 1);
    assert_eq!(kinds[0].1, "generatorSource");
    let report = world
        .store
        .semantic_batch_report_at(world.store.index_baseline().unwrap(), &descriptor.1)
        .unwrap()
        .unwrap();
    assert_eq!(report["identityKind"], json!("generatorSource"));
}

impl World {
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

    fn head_key(&self) -> String {
        let pin = self.store.index_baseline().unwrap();
        format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision)
    }

    /// `(document path, artifact hash)` of every stored basis at the head.
    fn basis_artifacts(&self) -> Vec<(String, String)> {
        self.db()
            .prepare("SELECT DISTINCT c.path,b.artifact_hash FROM semantic_basis b JOIN semantic_coverage c ON c.tuple_id=b.tuple_id WHERE c.revision_id=?1 ORDER BY c.path")
            .unwrap()
            .query_map([self.head_key()], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn rejoin_failures(&self) -> i64 {
        self.db()
            .query_row(
                "SELECT COUNT(*) FROM semantic_coverage WHERE revision_id=?1 AND diagnostic LIKE 'selected raw capture unavailable%'",
                [self.head_key()],
                |row| row.get(0),
            )
            .unwrap()
    }
}

fn mixed_language_world() -> World {
    world(|root| {
        write_files(
            root,
            &[
                ("J.java", JAVA_TYPES),
                ("src/A.js", DEFINITIONS_A),
                ("src/B.js", DEFINITIONS_B),
            ],
        )
    })
}

fn mixed_language_batch(world: &World) -> (PathBuf, String) {
    let (a, b) = definitions_docs();
    one_artifact_per_language(
        world,
        vec![("java", vec![java_doc()]), ("javascript", vec![a, b])],
    )
}

#[test]
fn native_rejoin_after_batch_import_rejoins_every_artifact() {
    let world = mixed_language_world();
    let descriptor = mixed_language_batch(&world);
    let parsed = Descriptor::parse(&fs::read(&descriptor.0).unwrap()).unwrap();
    world.publish(&descriptor).unwrap();
    let imported = world.basis_artifacts();
    let hash_of = |language: &str| {
        parsed
            .artifacts
            .iter()
            .find(|artifact| artifact.language == language)
            .unwrap()
            .sha256
            .clone()
    };
    assert_eq!(
        imported,
        vec![
            ("J.java".to_owned(), hash_of("java")),
            ("src/A.js".to_owned(), hash_of("javascript")),
            ("src/B.js".to_owned(), hash_of("javascript")),
        ]
    );
    // A native edit rejoins the stored batch over the new head: both originals are
    // authenticated against their own binary.
    write_files(&world.root, &[("src/C.js", "function other(){ }\n")]);
    world.republish_with_rejoin();
    assert_eq!(world.rejoin_failures(), 0);
    assert_eq!(world.basis_artifacts(), imported);
    let rejoined = world
        .store
        .selected_raw_inputs_at(world.store.index_baseline().unwrap())
        .unwrap();
    assert_eq!(rejoined.len(), 1);
    assert_eq!(rejoined[0].captures.len(), 2);
}

#[test]
fn multi_artifact_reference_batch_records_multi_artifact_fallback() {
    let world = definitions_world();
    let (a, b) = definitions_docs();
    let mut fixture = world.fixture().reference("scip-typescript");
    fixture.artifact("javascript", vec![a]);
    fixture.artifact("javascript", vec![b]);
    let descriptor = world.write(&fixture);
    world.publish(&descriptor).unwrap();
    let counts = world._state.path().join("work-counts.json");
    // SAFETY: each test runs in its own process under nextest, and no other thread reads this.
    unsafe { std::env::set_var("TRELLIS_TEST_LOCAL_WORK_COUNTS_FILE", &counts) };
    write_files(&world.root, &[("src/C.js", "function other(){ }\n")]);
    let options = IndexOptions::new(world.root.clone());
    let capture = trellis::capture::Capture::admit(&options, &world.cancel, &|_| {}).unwrap();
    let leader = world.store.publication_for_tests().unwrap();
    world
        .store
        .publish_local_for_tests(&options, &capture, &leader, &world.cancel)
        .unwrap();
    let work: Value = serde_json::from_slice(&fs::read(&counts).unwrap()).unwrap();
    let reasons: Vec<&str> = work["fallbackReasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|reason| reason.as_str().unwrap())
        .collect();
    assert!(reasons.contains(&"multi_artifact_batch"), "{work}");
    assert_eq!(world.rejoin_failures(), 0);
    assert_eq!(world.basis_artifacts().len(), 2);
}

/// Publishes one batch, then a native-only revision that drops its selection, then a second
/// batch. The first batch is selected only by the older pin, which nothing captures. Returns
/// that pin and the head, with the retention clock past the older pin's release horizon.
fn replaced_batch_world() -> (World, IndexPin, IndexPin) {
    let world = definitions_world();
    world.store.set_retention_clock_for_tests(1_000, 0);
    let first = split_definitions(&world, false);
    let (older, _, _) = world.publish(&first).unwrap();
    world.store.set_retention_clock_for_tests(1_001, 1);
    write_files(&world.root, &[("src/C.js", "function other(){ }\n")]);
    world.republish();
    world.store.set_retention_clock_for_tests(1_002, 2);
    let (a, b) = definitions_docs();
    let a = a.occurrence(&[0, 0, 1], &[], "pkg extra.", 0);
    let mut fixture = world.fixture();
    fixture.artifact("javascript", vec![a, b, DocSpec::new("src/C.js")]);
    let second = world.write(&fixture);
    let (head, _, _) = world.publish(&second).unwrap();
    assert_eq!(world.count("SELECT COUNT(*) FROM semantic_raw_batches"), 2);
    assert_eq!(world.count("SELECT COUNT(*) FROM semantic_raw_captures"), 3);
    world.store.set_retention_clock_for_tests(1_902, 902);
    (world, older, head)
}

/// Releases `pin`, whose garbage collection proceeds in bounded steps.
fn release(world: &World, pin: IndexPin, leader: &trellis::store::topology::WriterGuard) {
    for _ in 0..64 {
        match world.store.release_revision(pin, leader) {
            Ok(()) => return,
            Err(error) if format!("{error:#}").starts_with("storage_busy") => {}
            Err(error) => panic!("{error:#}"),
        }
    }
    panic!("release did not finish");
}

#[test]
fn unreferenced_batches_are_collected_with_their_captures() {
    let (world, older, head) = replaced_batch_world();
    let first_batch = world.count("SELECT MIN(batch_id) FROM semantic_raw_batches");
    let leader = world.store.publication_for_tests().unwrap();
    release(&world, older, &leader);
    let of_first = |table: &str| -> i64 {
        world.count(&format!(
            "SELECT COUNT(*) FROM {table} WHERE batch_id={first_batch}"
        ))
    };
    assert_eq!(of_first("semantic_raw_batches"), 0);
    assert_eq!(of_first("semantic_raw_captures"), 0);
    assert_eq!(of_first("semantic_raw_exclusions"), 0);
    assert_eq!(
        world.count("SELECT COUNT(*) FROM semantic_raw_proofs p JOIN semantic_raw_captures c ON c.capture_id=p.capture_id WHERE c.batch_id<>(SELECT MAX(batch_id) FROM semantic_raw_batches)"),
        0
    );
    // The batch the head selects is untouched and still reads.
    assert_eq!(world.count("SELECT COUNT(*) FROM semantic_raw_batches"), 1);
    assert_eq!(world.count("SELECT COUNT(*) FROM semantic_raw_captures"), 1);
    assert!(world.count("SELECT COUNT(*) FROM semantic_raw_proofs") > 0);
    assert!(world.count("SELECT COUNT(*) FROM semantic_raw_documents") > 0);
    assert!(!world.store.semantic_at(head).unwrap().coverage.is_empty());
    assert_eq!(world.store.selected_raw_inputs_at(head).unwrap().len(), 1);
}

#[test]
fn gc_keeps_proofs_referenced_by_retained_provenance() {
    let (world, older, head) = replaced_batch_world();
    let first_batch = world.count("SELECT MIN(batch_id) FROM semantic_raw_batches");
    // A retained provenance row of the head still derives from one proof of the first batch.
    let (capture_id, proof_id): (i64, String) = world
        .db()
        .query_row(
            "SELECT p.capture_id,p.proof_id FROM semantic_raw_proofs p JOIN semantic_raw_captures c ON c.capture_id=p.capture_id WHERE c.batch_id=?1 ORDER BY p.rowid LIMIT 1",
            [first_batch],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    world
        .store
        .with_index_sql_for_tests(|db| {
            let changed = db.execute(
                "UPDATE semantic_provenance SET capture_id=?1,derived_from_provenance_id=?2,derived_from_record_kind=(SELECT record_kind FROM semantic_raw_proofs WHERE capture_id=?1 AND proof_id=?2),derived_from_fact_digest=(SELECT fact_digest FROM semantic_raw_proofs WHERE capture_id=?1 AND proof_id=?2) WHERE rowid=(SELECT v.rowid FROM semantic_provenance v JOIN semantic_coverage c ON c.tuple_id=v.tuple_id WHERE c.revision_id=?3 AND v.capture_id IS NOT NULL LIMIT 1)",
                rusqlite::params![capture_id, proof_id, format!("pin:v1:{}:{}", head.index_generation, head.index_revision)],
            )?;
            assert_eq!(changed, 1);
            Ok(())
        })
        .unwrap();
    let leader = world.store.publication_for_tests().unwrap();
    release(&world, older, &leader);
    let survivors: Vec<(i64, String)> = world
        .db()
        .prepare("SELECT p.capture_id,p.proof_id FROM semantic_raw_proofs p JOIN semantic_raw_captures c ON c.capture_id=p.capture_id WHERE c.batch_id=?1")
        .unwrap()
        .query_map([first_batch], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(survivors, vec![(capture_id, proof_id)]);
    assert_eq!(
        world.count(&format!(
            "SELECT COUNT(*) FROM semantic_raw_captures WHERE batch_id={first_batch}"
        )),
        1
    );
    assert_eq!(
        world.count(&format!(
            "SELECT COUNT(*) FROM semantic_raw_batches WHERE batch_id={first_batch}"
        )),
        1
    );
}

fn assert_nothing_published(world: &World, head: IndexPin) {
    assert_eq!(world.store.index_baseline().unwrap(), head);
    for table in [
        "semantic_raw_batches",
        "semantic_raw_captures",
        "semantic_raw_documents",
        "semantic_raw_proofs",
        "semantic_raw_exclusions",
        "semantic_revision_raw_selections",
        "semantic_coverage",
        "semantic_import_reports",
    ] {
        assert_eq!(
            world.count(&format!("SELECT COUNT(*) FROM {table}")),
            0,
            "{table}"
        );
    }
}

#[test]
fn failed_publication_leaves_no_partial_rows_and_the_batch_publishes_on_retry() {
    use trellis::store::BatchPublishFault;
    let world = definitions_world();
    let descriptor = split_definitions(&world, false);
    let (mut admitted, joined, _) = world
        .join_with(
            &descriptor,
            &BatchLimits::PRODUCTION,
            DestinationMutations::default(),
        )
        .unwrap();
    let head = joined.destination_pin();
    let leader = world.store.publication_for_tests().unwrap();
    let fault = |admitted: &mut AdmittedBatch, fault| {
        world
            .store
            .publish_semantic_batch_with_fault_for_tests(
                &joined,
                admitted.stage_mut(),
                head,
                &leader,
                &world.cancel,
                fault,
            )
            .unwrap_err()
    };
    let error = fault(&mut admitted, BatchPublishFault::SqlFault);
    assert!(format!("{error:#}").contains("NOT NULL"), "{error:#}");
    assert_nothing_published(&world, head);
    let error = fault(&mut admitted, BatchPublishFault::InsertDiskFull);
    assert!(
        format!("{error:#}").starts_with("storage_full: database or disk is full"),
        "{error:#}"
    );
    assert!(error.is::<trellis::store::StorageFull>(), "{error:#}");
    assert_nothing_published(&world, head);
    let error = fault(&mut admitted, BatchPublishFault::PrecommitCancel);
    assert!(format!("{error:#}").contains("cancelled"), "{error:#}");
    world
        .cancel
        .store(false, std::sync::atomic::Ordering::Relaxed);
    assert_nothing_published(&world, head);
    // The stage was detached after each failure, so the same joined batch still publishes.
    let pin = world
        .store
        .publish_semantic_batch(&joined, admitted.stage_mut(), head, &leader, &world.cancel)
        .unwrap();
    assert_eq!(pin.index_revision, head.index_revision + 1);
    assert_eq!(world.count("SELECT COUNT(*) FROM semantic_raw_batches"), 1);
}

#[test]
fn cancelled_publication_changes_nothing() {
    let world = definitions_world();
    let descriptor = split_definitions(&world, false);
    let (mut admitted, joined, _) = world
        .join_with(
            &descriptor,
            &BatchLimits::PRODUCTION,
            DestinationMutations::default(),
        )
        .unwrap();
    let leader = world.store.publication_for_tests().unwrap();
    world
        .cancel
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let error = world
        .store
        .publish_semantic_batch(
            &joined,
            admitted.stage_mut(),
            joined.destination_pin(),
            &leader,
            &world.cancel,
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("cancelled"), "{error:#}");
    assert_nothing_published(&world, joined.destination_pin());
}

/// A provenance row whose raw claim vanished after the seal is a corrupt stage: publication
/// fails typed and rolls back, never publishing the document without that row and its facts.
#[test]
fn provenance_row_without_its_claim_after_the_seal_rolls_back() {
    let world = definitions_world();
    let descriptor = split_definitions(&world, false);
    let (mut admitted, joined, _) = world
        .join_with(
            &descriptor,
            &BatchLimits::PRODUCTION,
            DestinationMutations::default(),
        )
        .unwrap();
    let removed = admitted
        .stage_mut()
        .conn()
        .unwrap()
        .execute(
            "DELETE FROM claims WHERE claim_id=(SELECT claim_id FROM out_provenance ORDER BY ord LIMIT 1)",
            [],
        )
        .unwrap();
    assert_eq!(removed, 1);
    admitted.stage_mut().release_connection();
    let leader = world.store.publication_for_tests().unwrap();
    let head = joined.destination_pin();
    let error = world
        .store
        .publish_semantic_batch(&joined, admitted.stage_mut(), head, &leader, &world.cancel)
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("staged provenance without claim"),
        "{error:#}"
    );
    assert_nothing_published(&world, head);
}

#[test]
fn ambiguous_commit_is_resolved_from_durable_state() {
    use trellis::store::BatchPublishFault;
    let world = definitions_world();
    let descriptor = split_definitions(&world, false);
    let (mut admitted, joined, _) = world
        .join_with(
            &descriptor,
            &BatchLimits::PRODUCTION,
            DestinationMutations::default(),
        )
        .unwrap();
    let head = joined.destination_pin();
    let leader = world.store.publication_for_tests().unwrap();
    let publish = |admitted: &mut AdmittedBatch, fault| {
        world.store.publish_semantic_batch_with_fault_for_tests(
            &joined,
            admitted.stage_mut(),
            head,
            &leader,
            &world.cancel,
            fault,
        )
    };
    // The COMMIT never ran: the head is still the expected pin, which is a retryable conflict.
    let error = publish(&mut admitted, BatchPublishFault::CommitLostBeforeCommit).unwrap_err();
    assert!(
        format!("{error:#}").starts_with("revision conflict"),
        "{error:#}"
    );
    assert_nothing_published(&world, head);
    // A COMMIT that fails from contention and rolls back stays typed `storage_busy`. A revision
    // conflict would re-join the batch and charge the shared lookup budget a second time.
    let error = publish(&mut admitted, BatchPublishFault::CommitBusyBeforeCommit).unwrap_err();
    assert!(
        format!("{error:#}").starts_with("storage_busy"),
        "{error:#}"
    );
    assert!(
        !format!("{error:#}").contains("revision conflict"),
        "{error:#}"
    );
    assert_nothing_published(&world, head);
    // The COMMIT ran but its result was lost: the durable head carries this descriptor.
    let pin = publish(&mut admitted, BatchPublishFault::CommitLostAfterCommit).unwrap();
    assert_eq!(pin.index_revision, head.index_revision + 1);
    assert_eq!(world.store.index_baseline().unwrap(), pin);
    assert_eq!(world.count("SELECT COUNT(*) FROM semantic_raw_batches"), 1);
    // A head that does not select this descriptor is never reported as this publication.
    let error = world
        .store
        .resolve_ambiguous_batch_commit(head, pin, &"0".repeat(64))
        .unwrap_err();
    assert!(
        format!("{error:#}").starts_with("revision conflict"),
        "{error:#}"
    );
    assert_eq!(
        world
            .store
            .resolve_ambiguous_batch_commit(head, pin, &descriptor.1)
            .unwrap(),
        pin
    );
}

#[test]
fn joined_batch_of_an_earlier_attempt_is_refused_after_a_later_join_failed() {
    let world = definitions_world();
    let descriptor = split_definitions(&world, false);
    let (mut admitted, earlier, _) = world
        .join_with(
            &descriptor,
            &BatchLimits::PRODUCTION,
            DestinationMutations::default(),
        )
        .unwrap();
    let head = earlier.destination_pin();
    // A later attempt over the same stage fails part way and leaves partial output rows.
    let mut tight = BatchLimits::PRODUCTION;
    tight.lookup_steps = 1;
    let mut budget = LookupBudget::new(&tight);
    let failed = join_staged(
        &mut admitted,
        &world.store,
        head,
        &tight,
        &mut budget,
        &mut BatchCounters::default(),
        DestinationMutations::default(),
        &world.cancel,
    );
    assert!(failed.is_err());
    let leader = world.store.publication_for_tests().unwrap();
    let error = world
        .store
        .publish_semantic_batch(&earlier, admitted.stage_mut(), head, &leader, &world.cancel)
        .unwrap_err();
    assert!(format!("{error:#}").contains("superseded"), "{error:#}");
    assert_nothing_published(&world, head);
    // A successful join certifies the stage again.
    let mut budget = LookupBudget::new(&BatchLimits::PRODUCTION);
    let rejoined = join_staged(
        &mut admitted,
        &world.store,
        head,
        &BatchLimits::PRODUCTION,
        &mut budget,
        &mut BatchCounters::default(),
        DestinationMutations::default(),
        &world.cancel,
    )
    .unwrap();
    assert_ne!(rejoined.attempt(), earlier.attempt());
    world
        .store
        .publish_semantic_batch(
            &rejoined,
            admitted.stage_mut(),
            head,
            &leader,
            &world.cancel,
        )
        .unwrap();
}

#[test]
fn publication_statements_use_the_stage_indexes() {
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
    admitted.stage_mut().release_connection();
    let db = rusqlite::Connection::open(&world.index_db).unwrap();
    db.execute(
        "ATTACH DATABASE ?1 AS stage",
        [admitted.stage().db_path().to_str().unwrap()],
    )
    .unwrap();
    for sql in trellis::store::batch_publication_statements_for_tests() {
        let mut statement = db.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let params = vec![rusqlite::types::Value::Null; statement.parameter_count()];
        let plan: Vec<String> = statement
            .query_map(rusqlite::params_from_iter(params), |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        // No statement sorts: raw rows follow their staged table, and every output family is
        // read by one cursor in `ord` (rowid) order.
        assert!(
            plan.iter().all(|step| !step.contains("TEMP B-TREE")),
            "{sql}: {plan:?}"
        );
        // Joined stage tables are searched by key, never scanned per driving row. The ordinal
        // pairs are a constant list of at most 64 rows. SQLite plans a child scan for the
        // foreign key onto raw proofs, but runs it only while a violation is outstanding.
        let scans: Vec<&String> = plan
            .iter()
            .filter(|step| {
                step.starts_with("SCAN")
                    && !["SCAN m", "SCAN CONSTANT ROW", "SCAN semantic_provenance"]
                        .contains(&step.as_str())
            })
            .collect();
        assert!(scans.len() <= 1, "{sql}: {plan:?}");
        if sql.contains("out_provenance") {
            assert!(
                plan.iter()
                    .any(|step| step.starts_with("SEARCH c USING INTEGER PRIMARY KEY")),
                "{sql}: {plan:?}"
            );
        }
    }
}

#[test]
fn whole_publication_validator_runs_over_every_original_of_a_batch() {
    let world = mixed_language_world();
    let descriptor = mixed_language_batch(&world);
    world.publish(&descriptor).unwrap();
    let counts = world._state.path().join("work-counts.json");
    // SAFETY: each test runs in its own process under nextest, and no other thread reads this.
    unsafe { std::env::set_var("TRELLIS_TEST_LOCAL_WORK_COUNTS_FILE", &counts) };
    write_files(&world.root, &[("src/C.js", "function other(){ }\n")]);
    world.republish_with_rejoin();
    let work: Value = serde_json::from_slice(&fs::read(&counts).unwrap()).unwrap();
    // The FULL rejoin decoded and validated both originals against their own binaries.
    assert_eq!(work["originalRawDecodes"], json!(2), "{work}");
    assert_eq!(world.rejoin_failures(), 0);
    assert_eq!(world.basis_artifacts().len(), 3);
    // A stored original that no longer matches its hash makes that artifact's documents fail
    // typed instead of validating against another artifact's binary.
    world
        .store
        .with_index_sql_for_tests(|db| {
            let changed = db.execute(
                "UPDATE semantic_raw_captures SET binary_bytes=zeroblob(length(binary_bytes)) WHERE language='java'",
                [],
            )?;
            assert_eq!(changed, 1);
            Ok(())
        })
        .unwrap();
    write_files(&world.root, &[("src/D.js", "function more(){ }\n")]);
    world.republish_with_rejoin();
    assert!(world.rejoin_failures() > 0);
}

/// Two references at one call site that name different symbols.
fn two_references_one_call() -> (World, (PathBuf, String)) {
    let world = world(|root| {
        write_files(
            root,
            &[
                ("src/A.js", DEFINITIONS_A),
                ("src/B.js", "function target(){ }\nfunction other(){ }\n"),
            ],
        )
    });
    let descriptor = one_artifact_per_language(
        &world,
        vec![(
            "javascript",
            vec![
                DocSpec::new("src/A.js")
                    .occurrence(&[0, 9, 12], &[], "pkg run.", 1)
                    .occurrence(&[0, 16, 22], &[0, 16, 24], "pkg target.", 0)
                    .occurrence(&[0, 16, 22], &[0, 16, 24], "pkg other.", 0)
                    .symbol("pkg run.", 17, &[]),
                DocSpec::new("src/B.js")
                    .occurrence(&[0, 9, 15], &[], "pkg target.", 1)
                    .occurrence(&[1, 9, 14], &[], "pkg other.", 1)
                    .symbol("pkg target.", 17, &[])
                    .symbol("pkg other.", 17, &[]),
            ],
        )],
    );
    (world, descriptor)
}

#[test]
fn two_references_resolving_one_native_call_agree_across_paths() {
    let (world, descriptor) = two_references_one_call();
    let mutations = DestinationMutations::default();
    let memory = world.memory_publication(&descriptor, mutations);
    let (mut admitted, _, _) = world
        .join_with(&descriptor, &BatchLimits::PRODUCTION, mutations)
        .unwrap();
    let staged = testing::staged_publication(&mut admitted).unwrap();
    assert_same_publication(&staged, &memory, "two references, one call");
    // Both references bind the one native call, and each binding carries the same ambiguous
    // candidate set, so the join never produces contradictory resolved targets for it.
    for (name, publication) in [("staged", &staged), ("memory", &memory)] {
        let bindings = &publication.call_bindings;
        assert_eq!(bindings.len(), 2, "{name}");
        assert_eq!(bindings[0].call_id, bindings[1].call_id, "{name}");
        for binding in bindings {
            assert_eq!(binding.resolution, Resolution::Ambiguous, "{name}");
            assert_eq!(binding.candidates.len(), 2, "{name}");
            assert_eq!(binding.candidates, bindings[0].candidates, "{name}");
        }
        world
            .full_validation(&descriptor, mutations, publication)
            .unwrap_or_else(|error| panic!("{name}: {error:#}"));
    }
}

#[test]
fn bindings_reaching_one_call_with_conflicting_targets_resolve_to_the_ambiguous_union() {
    let (world, descriptor) = two_references_one_call();
    let mutations = DestinationMutations::default();
    let memory = world.memory_publication(&descriptor, mutations);
    // Make the first binding of the shared call site claim one resolved target while the second
    // keeps the ambiguous pair, as two references with different targets would on one native call.
    let mut edited = false;
    testing::before_row_validation(move |kind, row| {
        if kind == "callResolution" && !edited {
            edited = true;
            let first = row["candidates"][0].clone();
            row["resolution"] = json!("resolved");
            row["declaredTarget"] = first;
            row["candidates"] = json!([]);
            row["staleTarget"] = json!(false);
        }
    });
    let joined = world.join_with(&descriptor, &BatchLimits::PRODUCTION, mutations);
    testing::clear_row_hook();
    let (mut admitted, _, _) = joined.unwrap();
    let staged = testing::staged_publication(&mut admitted).unwrap();
    assert_same_publication(&staged, &memory, "conflicting targets on one call site");
    world
        .full_validation(&descriptor, mutations, &staged)
        .unwrap_or_else(|error| panic!("{error:#}"));
    assert_eq!(staged.call_bindings.len(), 2);
    for binding in &staged.call_bindings {
        assert_eq!(binding.resolution, Resolution::Ambiguous);
        assert!(binding.declared_target.is_none());
        assert_eq!(binding.candidates.len(), 2);
    }
}

type PublicationEdit = fn(&mut SemanticPublication);

#[test]
fn cross_row_defects_are_rejected_by_whole_publication_validation_on_both_paths() {
    let (world, descriptor) = two_references_one_call();
    let mutations = DestinationMutations::default();
    // The join itself guarantees these three families, so no producer input can build them.
    // Each defect is applied to the staged rows through the closure hook and to the in-memory
    // publication, and the whole-publication validator must reject both.
    let cases: [(&str, &str, PublicationEdit, &str); 3] = [
        (
            "contradictory resolved call targets",
            "UPDATE out_call_bindings SET body=json_set(body,'$.resolution','resolved','$.declaredTarget',json(json_extract(body,'$.candidates[0]')),'$.candidates',json('[]'),'$.staleTarget',json('false')) WHERE ord=(SELECT MIN(ord) FROM out_call_bindings)",
            |publication| {
                let binding = &mut publication.call_bindings[0];
                binding.resolution = Resolution::Resolved;
                binding.declared_target = Some(binding.candidates[0].clone());
                binding.candidates.clear();
                binding.stale_target = Some(false);
            },
            "contradictory resolved call targets",
        ),
        (
            "freshness differs from the independent recomputation",
            "UPDATE out_coverage SET provenance=json_set(provenance,'$.freshness','stale') WHERE provenance IS NOT NULL",
            |publication| {
                for proof in &mut publication.provenance {
                    proof.freshness = Freshness::Stale;
                }
            },
            "claimed freshness differs from captured basis",
        ),
        (
            "coverage observed roles differ from the emitted evidence",
            "UPDATE out_coverage SET body=json_set(body,'$.observedRoles',json('[]'))",
            |publication| {
                for row in &mut publication.coverage {
                    row.observed_roles.clear();
                }
            },
            "coverage observed roles differ from admitted evidence",
        ),
    ];
    for (name, sql, edit, message) in cases {
        testing::before_join_closure(move |conn| {
            assert!(conn.execute(sql, []).unwrap() > 0, "{name}: nothing edited");
        });
        match world.join_with(&descriptor, &BatchLimits::PRODUCTION, mutations) {
            // A staged join that rejects the defect itself satisfies the differential.
            Err(_) => {}
            Ok((mut admitted, _, _)) => {
                let staged = testing::staged_publication(&mut admitted).unwrap();
                let error = world
                    .full_validation(&descriptor, mutations, &staged)
                    .expect_err(name);
                assert!(format!("{error:#}").contains(message), "{name}: {error:#}");
            }
        }
        let mut memory = world.memory_publication(&descriptor, mutations);
        edit(&mut memory);
        let error = world
            .full_validation(&descriptor, mutations, &memory)
            .expect_err(name);
        assert!(format!("{error:#}").contains(message), "{name}: {error:#}");
    }
}

#[test]
fn an_invalid_destination_is_rejected_for_both_outputs_by_whole_publication_validation() {
    let world = definitions_world();
    let (a, b) = definitions_docs();
    let descriptor = one_artifact_per_language(&world, vec![("javascript", vec![a, b])]);
    write_files(&world.root, &[("src/C.js", "function other(){ }\n")]);
    world.republish();
    // A destination that declares one target twice is invalid. The memory path's validator
    // rejects the publication built against it, and so must it reject the staged rows.
    let mutations = DestinationMutations {
        duplicate_b_target: true,
        ..Default::default()
    };
    let memory = world.memory_publication(&descriptor, mutations);
    let error = world
        .full_validation(&descriptor, mutations, &memory)
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("duplicate measured declaration ID"),
        "{error:#}"
    );
    if let Ok((mut admitted, _, _)) =
        world.join_with(&descriptor, &BatchLimits::PRODUCTION, mutations)
    {
        let staged = testing::staged_publication(&mut admitted).unwrap();
        let error = world
            .full_validation(&descriptor, mutations, &staged)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("duplicate measured declaration ID"),
            "{error:#}"
        );
    }
}

// Real scip-typescript 0.4.0 output (`index --infer-tsconfig`) for the workspace in
// `tests/fixtures/scip-module-roots/workspace`. Only its Metadata unit was dropped, so the
// document units are the producer's exact bytes; `module_roots_artifact` re-adds metadata for
// the test workspace. The producer names `src/a.js` relative to the project root
// (`. . src/`a.js`/`) and `tools/x/lib/c.js` relative to its nearest package root
// (`@fixture/x 2.0.0 lib/`c.js`/`), per owner ruling 2026-10-29 (#443).
const MODULE_ROOTS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/scip-module-roots"
);
const PROJECT_MARKER: &str = "scip-typescript npm . . src/`a.js`/";
const NESTED_MARKER: &str = "scip-typescript npm @fixture/x 2.0.0 lib/`c.js`/";

fn copy_tree(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir_all(&target).unwrap();
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn module_roots_world(edit: impl FnOnce(&Path)) -> World {
    world(|root| {
        copy_tree(&Path::new(MODULE_ROOTS).join("workspace"), root);
        edit(root);
    })
}

fn module_roots_artifact(world: &World) -> Vec<u8> {
    let mut index = scip::types::Index::new();
    let mut metadata = scip::types::Metadata::new();
    metadata.project_root = format!("file://{}", world.root.display());
    metadata.text_document_encoding = protobuf::EnumOrUnknown::from_i32(1);
    let mut tool = scip::types::ToolInfo::new();
    tool.name = "scip-typescript".into();
    tool.version = "0.4.0".into();
    metadata.tool_info = Some(tool).into();
    index.metadata = Some(metadata).into();
    let mut bytes = index.write_to_bytes().unwrap();
    bytes.extend(fs::read(Path::new(MODULE_ROOTS).join("documents.scip")).unwrap());
    bytes
}

fn module_roots_batch(world: &World, artifact: Vec<u8>) -> (PathBuf, String) {
    let mut fixture = world.fixture().reference("scip-typescript");
    let docs = fixture.paths("javascript");
    assert_eq!(docs.len(), 3);
    let ordinal = fixture.artifact("javascript", docs);
    fixture.override_artifact_raw(ordinal, artifact);
    world.write(&fixture)
}

fn marker_claims(world: &World, descriptor: &(PathBuf, String)) -> Vec<(String, String, u64)> {
    let mut markers: Vec<_> = world
        .memory_admission(descriptor)
        .merged
        .claims
        .iter()
        .filter(|claim| claim.envelope.record_kind == "occurrence")
        .filter(|claim| claim.envelope.raw["range"]["values"] == json!([0, 0, 0]))
        .map(|claim| {
            (
                claim.envelope.relative_path.clone(),
                claim.envelope.raw["symbol"].as_str().unwrap().to_owned(),
                claim.envelope.raw["symbolRoles"].as_u64().unwrap(),
            )
        })
        .collect();
    markers.sort();
    markers
}

#[test]
fn real_typescript_project_and_package_relative_markers_publish_and_rejoin() {
    let world = module_roots_world(|_| {});
    let descriptor = module_roots_batch(&world, module_roots_artifact(&world));
    assert_eq!(
        marker_claims(&world, &descriptor),
        [
            ("src/a.js".into(), PROJECT_MARKER.into(), 1),
            (
                "src/b.js".into(),
                "scip-typescript npm . . src/`b.js`/".into(),
                1
            ),
            ("tools/x/lib/c.js".into(), NESTED_MARKER.into(), 1),
        ]
    );
    // The in-memory oracle and the staged batch join build the same publication.
    let memory = world.memory_publication(&descriptor, DestinationMutations::default());
    let (pin, mut admitted, _) = world.publish(&descriptor).unwrap();
    let staged = testing::staged_publication(&mut admitted).unwrap();
    assert_same_publication(&staged, &memory, "module roots");
    world
        .full_validation(&descriptor, DestinationMutations::default(), &staged)
        .unwrap();
    let published = world.store.semantic_at(pin).unwrap();
    assert_published_matches_staged(&published, &staged);
    for marker in [PROJECT_MARKER, NESTED_MARKER] {
        let symbol = published
            .symbols
            .iter()
            .find(|row| row.key.symbol == marker)
            .unwrap();
        assert!(
            symbol.declarations.is_empty(),
            "{marker} is not a measured name"
        );
    }
    assert!(
        published
            .references
            .iter()
            .any(|row| row.spelling == "greet"),
        "ordinary facts in the same documents still join"
    );
    // Stored-original rejoin re-decodes the artifact against the retained package roots.
    let proofs = stored_proofs(&world);
    drop(admitted);
    write_files(&world.root, &[("Unrelated.java", "class Unrelated {}\n")]);
    world.republish_with_rejoin();
    assert_eq!(world.rejoin_failures(), 0);
    assert_eq!(stored_proofs(&world), proofs);
}

/// The batch and the in-memory oracle both reject `descriptor` with a typed artifact error
/// and publish nothing.
fn assert_module_marker_rejected(world: &World, descriptor: &(PathBuf, String), case: &str) {
    let head = world.store.index_baseline().unwrap();
    let memory = world
        .try_memory_admission(descriptor)
        .map(|_| ())
        .unwrap_err();
    assert!(
        matches!(memory, ScipCaptureError::InvalidArtifact(_)),
        "{case}: memory {memory:?}"
    );
    let error = world.publish(descriptor).map(|_| ()).unwrap_err();
    assert!(
        matches!(
            error.downcast_ref::<ScipCaptureError>(),
            Some(ScipCaptureError::InvalidArtifact(_))
        ),
        "{case}: batch {error:#}"
    );
    assert_nothing_published(world, head);
    assert!(world.stage_dirs().is_empty(), "{case}");
}

#[test]
fn real_typescript_markers_reject_without_their_authenticated_package_root() {
    let nested = "tools/x/package.json";
    for (case, path, manifest) in [
        (
            "other package name",
            nested,
            Some(r#"{"name":"@fixture/y","version":"2.0.0"}"#),
        ),
        (
            "other package version",
            nested,
            Some(r#"{"name":"@fixture/x","version":"2.0.1"}"#),
        ),
        (
            "missing version is HEAD",
            nested,
            Some(r#"{"name":"@fixture/x"}"#),
        ),
        ("no package root", nested, None),
        // With a root package.json the anonymous `src/` markers name the wrong package,
        // although their paths equal the workspace-relative document paths.
        (
            "root package root",
            "package.json",
            Some(r#"{"name":"fixture-root","version":"1.0.0"}"#),
        ),
    ] {
        let world = module_roots_world(|root| match manifest {
            Some(text) => fs::write(root.join(path), text).unwrap(),
            None => fs::remove_file(root.join(path)).unwrap(),
        });
        let descriptor = module_roots_batch(&world, module_roots_artifact(&world));
        assert_module_marker_rejected(&world, &descriptor, case);
    }
}

#[test]
fn real_typescript_markers_reject_wrong_paths_other_documents_and_reversed_ranges() {
    let world = module_roots_world(|_| {});
    let original = module_roots_artifact(&world);
    let edit_marker = |path: &str, edit: &dyn Fn(&mut scip::types::Occurrence)| {
        semantic_batch_support::edit_scip(&original, |index| {
            let document = index
                .documents
                .iter_mut()
                .find(|document| document.relative_path == path)
                .unwrap();
            let marker = document
                .occurrences
                .iter_mut()
                .find(|occurrence| occurrence.range == [0, 0, 0])
                .unwrap();
            edit(marker);
        })
    };
    for (case, path, edit) in [
        (
            "path suffix under the wrong root",
            "src/a.js",
            &(|marker: &mut scip::types::Occurrence| {
                marker.symbol = "scip-typescript npm . . `a.js`/".into()
            }) as &dyn Fn(&mut scip::types::Occurrence),
        ),
        (
            "workspace path under the package root",
            "tools/x/lib/c.js",
            &|marker: &mut scip::types::Occurrence| {
                marker.symbol = "scip-typescript npm @fixture/x 2.0.0 tools/x/lib/`c.js`/".into()
            },
        ),
        (
            "marker naming another document",
            "src/a.js",
            &|marker: &mut scip::types::Occurrence| {
                marker.symbol = "scip-typescript npm . . src/`b.js`/".into()
            },
        ),
        (
            "reversed range",
            "tools/x/lib/c.js",
            &|marker: &mut scip::types::Occurrence| marker.range = vec![0, 1, 0],
        ),
    ] {
        let descriptor = module_roots_batch(&world, edit_marker(path, edit));
        assert_module_marker_rejected(&world, &descriptor, case);
    }
}

/// One MCP session over a world's index (#467), served before daemon startup reconciles it.
struct McpSession {
    client: std::os::unix::net::UnixStream,
    reader: std::io::BufReader<std::os::unix::net::UnixStream>,
    session: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    release: std::sync::mpsc::SyncSender<()>,
    next_id: i64,
    pairs: Vec<Value>,
}

impl McpSession {
    fn start(world: &World, session: u64) -> Self {
        use std::io::Write;
        use trellis::daemon::registry::CheckoutRegistry;
        let identity = WorkspaceIdentity::discover(Some(&world.root), &world.root)
            .unwrap()
            .attach_marker()
            .unwrap();
        let state = world._state.path();
        let topology = TopologyRoots::isolated_for_tests(state.join("cache"), state.join("data"));
        let mut registry = CheckoutRegistry::with_roots(topology);
        registry.attach_launch(session, &identity).unwrap();
        let (entered_tx, entered) = std::sync::mpsc::sync_channel(1);
        let (release, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = std::sync::Mutex::new(release_rx);
        // Hold daemon startup: the session reads the published head as the fixture left it.
        registry.set_next_startup_hook_for_tests(Arc::new(move || {
            entered_tx.send(()).unwrap();
            release_rx
                .lock()
                .unwrap()
                .recv_timeout(trellis::testing::HANG_BACKSTOP)
                .unwrap();
        }));
        let registry = Arc::new(tokio::sync::Mutex::new(registry));
        let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
            .unwrap();
        let workspace = trellis::mcp::OpenedWorkspace::new(identity);
        let handle = tokio::runtime::Handle::current();
        let thread = std::thread::spawn(move || {
            trellis::mcp::run_socket(workspace, server, registry, session, handle)
        });
        let reader = std::io::BufReader::new(client.try_clone().unwrap());
        let mut mcp = Self {
            client,
            reader,
            session: Some(thread),
            release,
            next_id: 1,
            pairs: vec![],
        };
        writeln!(
            mcp.client,
            "{}",
            Self::request(1, "trellis_workspace_describe", json!({"schemaVersion":1}))
        )
        .unwrap();
        entered
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap();
        let reply = mcp.reply();
        assert_eq!(reply["result"]["isError"], false, "{reply}");
        mcp
    }

    fn request(id: i64, name: &str, arguments: Value) -> Value {
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,
            "arguments":arguments,"_meta":{
                "io.modelcontextprotocol/protocolVersion":"2026-07-28",
                "io.modelcontextprotocol/clientCapabilities":{}}}})
    }

    fn reply(&mut self) -> Value {
        use std::io::BufRead;
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    /// The structured envelope of one tool call, kept for schema validation.
    fn call(&mut self, name: &str, arguments: Value) -> Value {
        use std::io::Write;
        self.next_id += 1;
        writeln!(
            self.client,
            "{}",
            Self::request(self.next_id, name, arguments)
        )
        .unwrap();
        let reply = self.reply();
        assert_eq!(reply["id"], self.next_id);
        let envelope = reply["result"]["structuredContent"].clone();
        self.pairs
            .push(json!({"name":name,"structuredContent":envelope}));
        envelope
    }

    /// Validates every envelope against the published catalog schemas, then stops.
    fn finish(mut self) {
        use std::io::Write;
        let script = r#"
const {createRequire} = require('node:module');
const {readFileSync} = require('node:fs');
const requireFromTests = createRequire(process.cwd() + '/tests/mcp/package.json');
const Ajv2020 = requireFromTests('ajv/dist/2020.js').default;
const ajv = new Ajv2020({strict: false, allErrors: true});
const catalog = JSON.parse(readFileSync('tests/fixtures/mcp/catalog.json', 'utf8'));
for (const {name, structuredContent} of JSON.parse(readFileSync(0, 'utf8'))) {
    const validate = ajv.compile(catalog.find(tool => tool.name === name).outputSchema);
    if (!validate(structuredContent)) throw Error(`${name}: ${JSON.stringify(validate.errors)}`);
}
"#;
        let mut child = std::process::Command::new("node")
            .args(["-e", script])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(&self.pairs).unwrap())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "structuredContent schema validation: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = self.release.send(());
        let session = self.session.take().unwrap();
        drop(self.reader);
        drop(self.client);
        session.join().unwrap().unwrap();
    }
}

/// A world over a real scip-typescript fixture's sources, with an empty `.git` whose
/// workspace marker exists before indexing, so an MCP session selects this same index.
fn real_fixture_world(fixture: &str, files: &[&str]) -> (World, Vec<String>) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/semantic-import")
        .join(fixture);
    let sources: Vec<(String, String)> = files
        .iter()
        .map(|path| {
            (
                path.to_string(),
                fs::read_to_string(dir.join(path)).unwrap(),
            )
        })
        .collect();
    // The fixture's package root names its module markers (#443).
    let package = format!(
        r#"{{"name":"{}-fixture","version":"1.0.0"}}"#,
        fixture.split('-').next().unwrap()
    );
    let world = world_with(
        |root| {
            fs::create_dir_all(root.join(".git")).unwrap();
            WorkspaceIdentity::discover(Some(root), root)
                .unwrap()
                .attach_marker()
                .unwrap();
            let mut all: Vec<(&str, &str)> = sources
                .iter()
                .map(|(path, text)| (path.as_str(), text.as_str()))
                .collect();
            all.push(("package.json", &package));
            write_files(root, &all);
        },
        true,
    );
    (world, sources.into_iter().map(|(_, text)| text).collect())
}

/// Publish the fixture's real `index.scip` as a scip-typescript import.
fn publish_real_fixture(world: &World, fixture: &str, files: &[&str]) -> (IndexPin, String) {
    let mut batch = world.fixture().reference("scip-typescript");
    let ordinal = batch.artifact(
        "javascript",
        files.iter().map(|path| DocSpec::new(path)).collect(),
    );
    let index = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/semantic-import")
        .join(fixture)
        .join("index.scip");
    batch.override_artifact_raw(ordinal, fs::read(index).unwrap());
    let descriptor = world.write(&batch);
    let (pin, admitted, _) = world.publish(&descriptor).unwrap();
    drop(admitted);
    let producer = world.store.semantic_at(pin).unwrap().producers[0]
        .id
        .clone();
    (pin, producer)
}

fn declaration_ids(world: &World) -> std::collections::BTreeMap<String, String> {
    let (_, native, _) = index_workspace_bundle(
        &IndexOptions::new(world.root.clone()),
        world.store.root_id(),
        &world.cancel,
        |_| {},
    )
    .unwrap();
    native
        .declarations
        .iter()
        .filter_map(|d| Some((d.name.clone()?, d.syntax_id.clone())))
        .collect()
}

/// The query the CLI daemon builds for `trellis query --seed ... [--semantic-producer-id ...]`.
fn view_query(seed: &str, producer: Option<&str>) -> trellis::model::ViewQuery {
    let mut query = json!({"seed": seed});
    if let Some(producer) = producer {
        query["semanticProducerId"] = json!(producer);
    }
    serde_json::from_value(query).unwrap()
}

/// `[internal, external, ambiguous]` calls among the published bindings, each call once.
fn expected_counts(published: &trellis::semantic_evidence::SemanticSnapshot) -> [usize; 3] {
    use trellis::semantic_evidence::Target;
    let mut calls = std::collections::BTreeMap::new();
    for binding in &published.call_bindings {
        let kind = match (&binding.resolution, &binding.declared_target) {
            (Resolution::Resolved, Some(Target::Internal { .. })) => 0,
            (Resolution::External, _) | (Resolution::Resolved, Some(Target::External { .. })) => 1,
            (Resolution::Ambiguous, _) => 2,
            _ => 3,
        };
        calls.insert(binding.call_id.clone().unwrap(), kind);
    }
    let mut counts = [0; 3];
    for kind in calls.values().filter(|kind| **kind < 3) {
        counts[*kind] += 1;
    }
    counts
}

/// #467: with real scip-typescript 0.4.0 calls published, the CLI query, MCP outgoing and
/// incoming calls, status and the methods API show the sole producer's selected bindings.
/// An explicit producer selects the same one; an unpublished or malformed one is typed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_typescript_bindings_show_in_query_mcp_status_and_methods() {
    use trellis::graph_query::BoundaryReason;
    use trellis::model::SemanticState;
    let files = ["src/lib.js", "src/main.js"];
    let (world, sources) = real_fixture_world("issue444-javascript-calls", &files);
    let main = &sources[1];
    let (pin, producer) = publish_real_fixture(&world, "issue444-javascript-calls", &files);
    let ids = declaration_ids(&world);
    let response = world.store.evidence_response().unwrap();

    // CLI query, under the sole-published policy and with the explicit producer.
    for explicit in [None, Some(producer.as_str())] {
        let view = response
            .query_view(&view_query(&ids["caller"], explicit))
            .unwrap()
            .unwrap();
        assert_eq!(view.revision, pin);
        assert_eq!(
            view.semantic.producer_id.as_deref(),
            Some(producer.as_str())
        );
        assert_eq!(view.semantic.note, None);
        assert_eq!(view.nodes[0].provenance.semantic, SemanticState::Fresh);
        let callee = |call_id: &str| {
            let call = view.calls.iter().find(|c| c.id == call_id).unwrap();
            let range = call.callee_range.as_ref().unwrap();
            main[range.start_byte..range.end_byte].to_owned()
        };
        let mut shown: Vec<_> = view
            .semantic
            .bindings
            .iter()
            .map(|b| {
                (
                    callee(&b.call_id),
                    format!("{:?}", b.binding.resolution),
                    format!("{:?}", b.binding.dispatch),
                    b.boundary_reason,
                    b.target_id.clone(),
                )
            })
            .collect();
        shown.sort_by(|a, b| a.0.cmp(&b.0));
        // #470's owner ruling: a resolved plain call to a free function is `Direct`, so the
        // graph follows it (no boundary), as it does `new Widget(3)`, which binds the class
        // constructor and is `Constructor` (#473: scip-typescript's unspecified kind defers to
        // the native kind). The member call `w.run()` binds the method with `Unknown`
        // dispatch, so it keeps a `dispatch` boundary.
        let resolved = |name: &str, dispatch: &str, boundary, target: &str| {
            (
                name.to_owned(),
                "Resolved".to_owned(),
                dispatch.to_owned(),
                boundary,
                Some(ids[target].clone()),
            )
        };
        let followed =
            |name: &str, target: &str| resolved(name, "Direct", BoundaryReason::None, target);
        assert_eq!(
            shown,
            [
                resolved("Widget", "Constructor", BoundaryReason::None, "constructor"),
                followed("helper", "helper"),
                followed("plain", "plain"),
                followed("plain", "plain"),
                followed("register", "register"),
                resolved("run", "Unknown", BoundaryReason::Dispatch, "run"),
            ]
        );
        for binding in &view.semantic.bindings {
            assert_eq!(
                binding.binding.call_id.as_deref(),
                Some(binding.call_id.as_str())
            );
            assert_eq!(binding.freshness, Freshness::Fresh);
            let call = view.calls.iter().find(|c| c.id == binding.call_id).unwrap();
            assert_eq!(call.provenance.semantic, SemanticState::Fresh);
        }
        // A call without a binding stays syntax only.
        assert!(view.calls.iter().any(|call| {
            call.provenance.semantic == SemanticState::Unavailable
                && !view.semantic.bindings.iter().any(|b| b.call_id == call.id)
        }));
    }
    let unpublished = response
        .query_view(&view_query(&ids["caller"], Some("not-published")))
        .unwrap_err();
    assert!(
        unpublished
            .downcast_ref::<trellis::store::ProducerNotPublished>()
            .is_some(),
        "{unpublished:#}"
    );
    let malformed = view_query(&ids["caller"], Some(&"p".repeat(513)));
    assert!(
        malformed
            .validate()
            .unwrap_err()
            .to_string()
            .starts_with("invalid_argument:")
    );

    // Status and the methods API report the same selected producer.
    let published = world.store.semantic_at(pin).unwrap();
    let [internal, external, ambiguous] = expected_counts(&published);
    assert!(internal >= 3, "{internal}");
    for status in [response.status().unwrap(), world.store.status().unwrap()] {
        assert_eq!(
            status.semantic_producer_id.as_deref(),
            Some(producer.as_str())
        );
        assert_eq!(status.semantic_note, None);
        assert_eq!(status.stats.semantic_state, SemanticState::Fresh);
        assert_eq!(
            [
                status.stats.internal,
                status.stats.external,
                status.stats.ambiguous
            ],
            [internal, external, ambiguous]
        );
        assert_eq!(
            status.stats.unresolved,
            status.stats.calls - internal - external - ambiguous
        );
    }
    let methods = response
        .methods_at("src/main.js", Some(pin))
        .unwrap()
        .unwrap();
    for item in methods["items"].as_array().unwrap() {
        assert_eq!(item["symbol"]["provenance"]["semantic"], "fresh");
    }
    drop(response);

    // MCP outgoing and incoming calls.
    let mut mcp = McpSession::start(&world, 467);
    for explicit in [None, Some(&producer)] {
        let mut arguments = json!({"schemaVersion":1,"symbolId":ids["caller"]});
        if let Some(producer) = explicit {
            arguments["semanticProducerId"] = json!(producer);
        }
        let outgoing = mcp.call("trellis_outgoing_calls", arguments);
        let calls = outgoing["data"]["calls"].as_array().unwrap();
        let helper = calls.iter().find(|c| c["calleeText"] == "helper").unwrap();
        assert_eq!(helper["disposition"], "resolved");
        // #470: a resolved plain call is `Direct` and followed, so it has no boundary.
        assert_eq!(helper["boundaryReason"], "none");
        assert_eq!(helper["binding"]["dispatch"], "direct");
        assert_eq!(helper["targetId"], ids["helper"]);
        assert_eq!(helper["evidenceTier"], "semantic");
        assert_eq!(helper["freshness"], "fresh");
        assert_eq!(helper["binding"]["resolution"], "resolved");
        assert_eq!(
            helper["binding"]["declaredTarget"]["syntaxId"],
            ids["helper"]
        );
        // #528: the basis is emitted once per response and referenced by index.
        let bases = outgoing["data"]["semanticBases"].as_array().unwrap();
        assert_eq!(bases.len(), 1, "{outgoing}");
        assert_eq!(helper["semanticBasisRef"], 0);
        assert_eq!(bases[0]["ref"], 0);
        assert_eq!(bases[0]["producerId"], producer.as_str());
        assert!(helper.get("semanticBasis").is_none());
        for proof in outgoing["data"]["provenance"].as_array().unwrap() {
            assert!(proof.get("basis").is_none(), "{proof}");
            let semantic = proof["producerId"] == producer.as_str();
            assert_eq!(
                proof["basisRef"],
                if semantic { json!(0) } else { Value::Null }
            );
        }
        // Compact keeps the answer and drops the proof detail.
        let mut compact_arguments =
            json!({"schemaVersion":1,"symbolId":ids["caller"],"compact":true});
        if let Some(producer) = explicit {
            compact_arguments["semanticProducerId"] = json!(producer);
        }
        let compact = mcp.call("trellis_outgoing_calls", compact_arguments);
        for field in ["coverage", "provenance", "semanticBases"] {
            assert!(compact["data"].get(field).is_none(), "{compact}");
        }
        let compact_helper = compact["data"]["calls"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["calleeText"] == "helper")
            .unwrap();
        assert_eq!(compact_helper["targetId"], ids["helper"]);
        assert_eq!(compact_helper["evidenceTier"], "semantic");
        assert!(compact_helper.get("binding").is_none());
        assert_eq!(compact["warnings"], outgoing["warnings"]);
        assert_eq!(compact["partial"], outgoing["partial"]);
        // A resolved member call has unknown dispatch, so it keeps a dispatch boundary.
        let run = calls.iter().find(|c| c["calleeText"] == "run").unwrap();
        assert_eq!(run["disposition"], "resolved", "{run}");
        assert_eq!(run["boundaryReason"], "dispatch", "{run}");
        assert_eq!(run["targetId"], ids["run"], "{run}");
        // `new Widget(3)` is a resolved constructor call and is followed (#473).
        let widget = calls.iter().find(|c| c["calleeText"] == "Widget").unwrap();
        assert_eq!(widget["binding"]["dispatch"], "constructor", "{widget}");
        assert_eq!(widget["boundaryReason"], "none", "{widget}");
        assert_eq!(widget["targetId"], ids["constructor"], "{widget}");
        assert_eq!(run["binding"]["dispatch"], "unknown", "{run}");
        let unbound = calls.iter().find(|c| c["calleeText"] == "method").unwrap();
        assert_eq!(unbound["binding"], Value::Null);
        assert_eq!(unbound["boundaryReason"], "missingEvidence");
        assert!(
            outgoing["data"]["provenance"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["id"] == helper["binding"]["provenanceId"])
        );
        assert!(
            outgoing["data"]["coverage"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["producerId"] == producer.as_str())
        );
        assert!(
            !outgoing["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w["code"] == "syntaxOnly"),
            "{outgoing}"
        );
    }
    let incoming = mcp.call(
        "trellis_incoming_calls",
        json!({"schemaVersion":1,"symbolId":ids["helper"]}),
    );
    let callers = incoming["data"]["callers"].as_array().unwrap();
    assert_eq!(callers.len(), 1, "{incoming}");
    assert_eq!(callers[0]["ownerSyntaxId"], ids["caller"]);
    assert_eq!(callers[0]["targetId"], ids["helper"]);
    assert_eq!(callers[0]["disposition"], "resolved");
    assert_eq!(callers[0]["evidenceTier"], "semantic");
    assert_eq!(callers[0]["semanticBasisRef"], 0);
    // #528: describe and scopeCoverage report the imported JavaScript evidence beside the
    // native coverage, which keeps its syntax tier; other languages have no producer.
    let describe = mcp.call("trellis_workspace_describe", json!({"schemaVersion":1}));
    assert_eq!(describe["data"]["toolVersion"], 3);
    for (scope, rows) in [
        ("describe", &describe["data"]["languages"]),
        ("incoming", &incoming["data"]["scopeCoverage"]),
    ] {
        let rows = rows.as_array().unwrap();
        let javascript = &rows[3];
        assert_eq!(javascript["language"], "javascript", "{scope}");
        assert_eq!(javascript["evidenceTier"], "syntax", "{scope}");
        assert_eq!(
            javascript["semantic"]["available"], true,
            "{scope}: {javascript}"
        );
        assert_eq!(javascript["semantic"]["producerId"], producer.as_str());
        assert_eq!(javascript["semantic"]["freshness"], "fresh");
        assert!(
            javascript["semantic"]["coverage"]["selected"]
                .as_u64()
                .unwrap()
                > 0
        );
        let rust = &rows[1];
        assert_eq!(rust["semantic"]["available"], false, "{scope}");
        assert_eq!(rust["semantic"]["producerId"], Value::Null);
        assert_eq!(rust["semantic"]["freshness"], "unavailable");
        assert!(rust["semantic"]["note"].is_string());
    }
    // Find returns native declarations; with fresh JavaScript evidence selected it no
    // longer warns syntaxOnly for them.
    let found = mcp.call(
        "trellis_find_symbols",
        json!({"schemaVersion":1,"query":"helper"}),
    );
    assert!(
        !found["data"]["symbols"].as_array().unwrap().is_empty(),
        "{found}"
    );
    assert!(
        !found["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["code"] == "syntaxOnly"),
        "{found}"
    );
    assert_eq!(found["data"]["symbols"][0]["evidenceTier"], "syntax");
    // An explicit producer this revision does not publish, or a malformed one, is typed.
    for name in ["trellis_outgoing_calls", "trellis_incoming_calls"] {
        let failure = mcp.call(
            name,
            json!({"schemaVersion":1,"symbolId":ids["caller"],"semanticProducerId":"not-published"}),
        );
        assert_eq!(failure["error"]["code"], "not_found", "{failure}");
        let invalid = mcp.call(
            name,
            json!({"schemaVersion":1,"symbolId":ids["caller"],"semanticProducerId":""}),
        );
        assert_eq!(invalid["error"]["code"], "invalid_request", "{invalid}");
    }
    mcp.finish();
    assert!(!world.store.recovery_pending_for_tests());
}

/// #467 with #466's real ambiguous calls: `new Set(..)` and `Error(..)` show as ambiguous
/// bindings carrying their whole candidate union, and status counts each call once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_typescript_ambiguous_bindings_show_in_query_mcp_and_status() {
    use trellis::graph_query::BoundaryReason;
    let files = ["src/main.js"];
    let (world, _) = real_fixture_world("issue466-ambiguous-calls", &files);
    let (pin, producer) = publish_real_fixture(&world, "issue466-ambiguous-calls", &files);
    let ids = declaration_ids(&world);
    let published = world.store.semantic_at(pin).unwrap();
    let union = |call_id: &str| {
        published
            .call_bindings
            .iter()
            .find(|b| b.call_id.as_deref() == Some(call_id))
            .unwrap()
            .candidates
            .clone()
    };
    let response = world.store.evidence_response().unwrap();
    let view = response
        .query_view(&view_query(&ids["caller"], None))
        .unwrap()
        .unwrap();
    assert_eq!(
        view.semantic.producer_id.as_deref(),
        Some(producer.as_str())
    );
    let mut sizes: Vec<_> = view
        .semantic
        .bindings
        .iter()
        .filter(|b| b.boundary_reason == BoundaryReason::Ambiguous)
        .map(|b| {
            assert_eq!(b.binding.resolution, Resolution::Ambiguous);
            assert_eq!(b.target_id, None);
            assert_eq!(b.binding.candidates, union(&b.call_id));
            b.binding.candidates.len()
        })
        .collect();
    sizes.sort();
    assert_eq!(sizes, [2, 4]);
    let status = response.status().unwrap();
    assert_eq!(status.stats.ambiguous, 2);
    assert_eq!(
        [
            status.stats.internal,
            status.stats.external,
            status.stats.ambiguous
        ],
        expected_counts(&published)
    );
    drop(response);

    let mut mcp = McpSession::start(&world, 468);
    let outgoing = mcp.call(
        "trellis_outgoing_calls",
        json!({"schemaVersion":1,"symbolId":ids["caller"]}),
    );
    let ambiguous: Vec<_> = outgoing["data"]["calls"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["disposition"] == "ambiguous")
        .collect();
    assert_eq!(ambiguous.len(), 2, "{outgoing}");
    for call in &ambiguous {
        assert_eq!(call["boundaryReason"], "ambiguous");
        assert_eq!(call["targetId"], Value::Null);
        assert_eq!(
            call["binding"]["candidates"],
            serde_json::to_value(union(call["callId"].as_str().unwrap())).unwrap()
        );
        assert!(outgoing["warnings"].as_array().unwrap().iter().any(|w| {
            w["code"] == "bindingAmbiguous" && w["provenanceId"] == call["binding"]["provenanceId"]
        }));
    }
    assert_eq!(outgoing["partial"], true);
    mcp.finish();
    assert!(!world.store.recovery_pending_for_tests());
}

/// #467: with no published producer, and then with two, every read shows syntax only and
/// says why, instead of choosing or merging producers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn none_or_multiple_producers_show_syntax_with_a_note() {
    use trellis::model::SemanticState;
    let files = ["src/lib.js", "src/main.js"];
    // One world per case: a served session's released startup may reconcile its index.
    let check = |world: &World, note: &str, session: u64, producers: usize| {
        let ids = declaration_ids(world);
        let head = world.store.index_baseline().unwrap();
        assert_eq!(
            world.store.semantic_at(head).unwrap().producers.len(),
            producers
        );
        let response = world.store.evidence_response().unwrap();
        let view = response
            .query_view(&view_query(&ids["caller"], None))
            .unwrap()
            .unwrap();
        assert_eq!(view.semantic.producer_id, None);
        assert_eq!(view.semantic.note.as_deref(), Some(note));
        assert!(view.semantic.bindings.is_empty());
        assert!(
            view.calls
                .iter()
                .all(|c| c.provenance.semantic == SemanticState::Unavailable)
        );
        let status = response.status().unwrap();
        assert_eq!(status.semantic_producer_id, None);
        assert_eq!(status.semantic_note.as_deref(), Some(note));
        assert_eq!(status.stats.semantic_state, SemanticState::Unavailable);
        assert_eq!(status.stats.internal + status.stats.ambiguous, 0);
        drop(response);
        let mut mcp = McpSession::start(world, session);
        for (name, symbol, items) in [
            ("trellis_outgoing_calls", &ids["caller"], "calls"),
            ("trellis_incoming_calls", &ids["helper"], "callers"),
        ] {
            let envelope = mcp.call(name, json!({"schemaVersion":1,"symbolId":symbol}));
            let items = envelope["data"][items].as_array().unwrap();
            assert!(!items.is_empty(), "{envelope}");
            for item in items {
                assert_eq!(item["binding"], Value::Null);
                assert_eq!(item["evidenceTier"], "syntax");
                assert_eq!(item["boundaryReason"], "missingEvidence");
            }
            assert_eq!(
                envelope["warnings"].as_array().unwrap().last().unwrap(),
                &json!({"code":"syntaxOnly","message":note,"provenanceId":null})
            );
        }
        mcp.finish();
    };
    let (none, _) = real_fixture_world("issue444-javascript-calls", &files);
    check(&none, trellis::store::NO_PRODUCER_NOTE, 469, 0);
    let (multiple, _) = real_fixture_world("issue444-javascript-calls", &files);
    publish_real_fixture(&multiple, "issue444-javascript-calls", &files);
    let mut second = multiple.fixture();
    let docs = second.paths("javascript");
    second.artifact("javascript", docs);
    let descriptor = multiple.write(&second);
    drop(multiple.publish(&descriptor).unwrap());
    check(
        &multiple,
        &trellis::store::multiple_producers_note("javascript"),
        470,
        2,
    );
}

// Real rust-analyzer 1.98.1 output (`rust-analyzer scip`, the profile-pinned binary) for the
// crate in `tests/fixtures/scip-rust-analyzer/workspace`. Only its Metadata unit (the absolute
// project root) was dropped; the document units are the producer's exact bytes, and
// `rust_analyzer_artifact` re-adds metadata for the test workspace. rust-analyzer writes
// `position_encoding` (field 6) in every signature `Document` and gives same-document references
// their target definition's range as `enclosing_range` (#394).
const RUST_ANALYZER: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/scip-rust-analyzer"
);

fn rust_analyzer_world() -> World {
    world(|root| copy_tree(&Path::new(RUST_ANALYZER).join("workspace"), root))
}

fn rust_analyzer_artifact(world: &World) -> Vec<u8> {
    let mut index = scip::types::Index::new();
    let mut metadata = scip::types::Metadata::new();
    metadata.project_root = format!("file://{}", world.root.display());
    metadata.text_document_encoding = protobuf::EnumOrUnknown::from_i32(1);
    let mut tool = scip::types::ToolInfo::new();
    tool.name = "rust-analyzer".into();
    tool.version = "1.98.1 (48a229ce 2026-09-01)".into();
    metadata.tool_info = Some(tool).into();
    index.metadata = Some(metadata).into();
    let mut bytes = index.write_to_bytes().unwrap();
    bytes.extend(fs::read(Path::new(RUST_ANALYZER).join("documents.scip")).unwrap());
    bytes
}

fn rust_analyzer_batch(world: &World) -> (PathBuf, String) {
    let mut fixture = world.fixture().reference("rust-analyzer");
    let docs = fixture.paths("rust");
    assert_eq!(docs.len(), 2);
    let ordinal = fixture.artifact("rust", docs);
    fixture.override_artifact_raw(ordinal, rust_analyzer_artifact(world));
    world.write(&fixture)
}

/// The staged publication of `descriptor` under `limits`, checked identical to the in-memory
/// oracle's under the same limits.
fn rust_analyzer_publications(
    world: &World,
    descriptor: &(PathBuf, String),
    limits: &BatchLimits,
    case: &str,
) -> SemanticPublication {
    let (mut admitted, _, _) = world
        .join_with(descriptor, limits, DestinationMutations::default())
        .unwrap();
    let staged = testing::staged_publication(&mut admitted).unwrap();
    drop(admitted);
    let memory = world
        .memory_rejoin_with(descriptor, DestinationMutations::default(), limits)
        .unwrap()
        .publication;
    assert_same_publication(&staged, &memory, case);
    staged
}

/// `(document, callee spelling, resolution, dispatch)` of every call binding, sorted.
fn rust_calls(
    world: &World,
    bindings: &[trellis::semantic_evidence::CallBinding],
) -> Vec<[String; 4]> {
    let mut calls: Vec<_> = bindings
        .iter()
        .map(|binding| {
            assert!(binding.call_id.is_some());
            let anchor = &binding.join.anchor;
            let source = fs::read_to_string(world.root.join(&anchor.document.path)).unwrap();
            [
                anchor.document.path.clone(),
                source[anchor.range.start..anchor.range.end].to_owned(),
                format!("{:?}", binding.resolution),
                format!("{:?}", binding.dispatch),
            ]
        })
        .collect();
    calls.sort();
    calls
}

fn coverage_of<'p>(
    publication: &'p SemanticPublication,
    path: &str,
) -> &'p trellis::semantic_evidence::Coverage {
    publication
        .coverage
        .iter()
        .find(|row| row.document_path == path)
        .unwrap()
}

#[test]
fn real_rust_analyzer_artifact_imports_and_its_enclosing_range_references_bind_calls() {
    let documents = fs::read(Path::new(RUST_ANALYZER).join("documents.scip")).unwrap();
    let signatures: Vec<_> = scip::types::Index::parse_from_bytes(&documents)
        .unwrap()
        .documents
        .iter()
        .flat_map(|document| document.symbols.clone())
        .filter_map(|symbol| symbol.signature_documentation.into_option())
        .collect();
    assert!(!signatures.is_empty());
    assert!(
        signatures.iter().all(|signature| signature
            .special_fields
            .unknown_fields()
            .iter()
            .any(|(number, _)| number == 6)),
        "rust-analyzer writes position_encoding in every signature"
    );
    let world = rust_analyzer_world();
    let descriptor = rust_analyzer_batch(&world);
    let staged =
        rust_analyzer_publications(&world, &descriptor, &BatchLimits::PRODUCTION, "real rust");
    world
        .full_validation(&descriptor, DestinationMutations::default(), &staged)
        .unwrap();
    assert!(
        staged
            .coverage
            .iter()
            .all(|row| row.diagnostic.as_deref() != Some("occurrence_cap_exceeded"))
    );
    // `saturating(value, value)` in util.rs, `Counter::new()` and `counter.add(..)` carry an
    // enclosing range (their target's definition) that does not enclose the reference; they
    // bind at their exact callee spans like the cross-document calls, which carry none.
    // Before #394 none of the three bound a call. The recursive `spin(value)` carries its own
    // definition's range, which encloses the reference but joins no invocation: under owner
    // ruling 4 (2026-10-30, #394) that range is absent and the exact callee span binds it.
    // Dispatch (owner ruling 2026-10-30, #470): plain and path calls of free functions are
    // direct; the method call `counter.add(..)` and unresolved or external calls are unknown.
    let call = |path: &str, callee: &str, resolution: &str, dispatch: &str| {
        [path, callee, resolution, dispatch].map(str::to_owned)
    };
    assert_eq!(
        rust_calls(&world, &staged.call_bindings),
        [
            call("src/lib.rs", "add", "Resolved", "Unknown"),
            call("src/lib.rs", "double", "Resolved", "Direct"),
            // rust-analyzer's associated-function kind does not bind the measured method
            // declaration, so `Counter::new()` binds its call but resolves no target.
            call("src/lib.rs", "new", "Unresolved", "Unknown"),
            call("src/lib.rs", "saturating", "Resolved", "Direct"),
            call("src/util.rs", "saturating", "Resolved", "Direct"),
            call("src/util.rs", "saturating_add", "External", "Unknown"),
            call("src/util.rs", "spin", "Resolved", "Direct"),
        ]
    );
    let (pin, mut admitted, _) = world.publish(&descriptor).unwrap();
    let staged = testing::staged_publication(&mut admitted).unwrap();
    drop(admitted);
    let published = world.store.semantic_at(pin).unwrap();
    assert_published_matches_staged(&published, &staged);
    // An unrelated edit rejoins the stored original, field 6 included, with no failure.
    let proofs = stored_proofs(&world);
    write_files(&world.root, &[("Unrelated.java", "class Unrelated {}\n")]);
    world.republish_with_rejoin();
    assert_eq!(world.rejoin_failures(), 0);
    assert_eq!(stored_proofs(&world), proofs);
    let head = world.store.index_baseline().unwrap();
    // The rejoined calls keep their resolution and dispatch (#470).
    assert_eq!(
        rust_calls(
            &world,
            &world.store.semantic_at(head).unwrap().call_bindings
        ),
        rust_calls(&world, &staged.call_bindings)
    );
}

/// Owner ruling 2026-10-30 (#470) through the #40 call graph and the sequence view, on the real
/// rust-analyzer fixture: `run` -> `util::double` -> `saturating` are direct path and plain
/// calls of free functions, so the graph expands into them under the depth and node bounds,
/// while `counter.add(..)` stays a dispatch boundary and `Counter::new()` an unresolved one.
#[test]
fn real_rust_analyzer_direct_calls_expand_the_call_graph_and_sequence() {
    use trellis::graph_query::{BoundaryReason, FrontierReason, GraphQuery, Visit};
    let world = rust_analyzer_world();
    let descriptor = rust_analyzer_batch(&world);
    let (pin, admitted, _) = world.publish(&descriptor).unwrap();
    drop(admitted);
    let producer = world.store.semantic_at(pin).unwrap().producers[0]
        .id
        .clone();
    let (_, native, _) = index_workspace_bundle(
        &IndexOptions::new(world.root.clone()),
        world.store.root_id(),
        &world.cancel,
        |_| {},
    )
    .unwrap();
    let id_of = |name: &str| {
        let found: Vec<_> = native
            .declarations
            .iter()
            .filter(|d| d.name.as_deref() == Some(name) && d.kind == "function")
            .collect();
        assert_eq!(found.len(), 1, "{name}");
        found[0].syntax_id.clone()
    };
    let (run, double, saturating) = (id_of("run"), id_of("double"), id_of("saturating"));
    let graph = |depth: u64, max_nodes: u64| {
        world
            .store
            .evidence_response()
            .unwrap()
            .graph_query(
                &GraphQuery {
                    root_syntax_id: run.clone(),
                    semantic_producer_id: Some(producer.clone()),
                    depth,
                    max_nodes,
                    max_calls: 500,
                    include_external: true,
                    include_tests: true,
                },
                None,
                &mut || Ok(()),
            )
            .unwrap()
            .outcome
            .expect("contract success")
    };
    let nodes = |result: &trellis::graph_query::GraphResult| {
        result
            .nodes
            .iter()
            .map(|n| (n.declaration.syntax_id.clone(), n.depth))
            .collect::<Vec<_>>()
    };
    let edge = |result: &trellis::graph_query::GraphResult, callee: &str| {
        let found: Vec<_> = result
            .edges
            .iter()
            .filter(|e| e.call.spelling.as_deref() == Some(callee))
            .cloned()
            .collect();
        assert_eq!(found.len(), 1, "{callee}");
        found.into_iter().next().unwrap()
    };

    // Depth 2 follows the direct edge into `double`, then `double`'s into `saturating`.
    let deep = graph(2, 150);
    assert_eq!(
        nodes(&deep),
        [
            (run.clone(), 0),
            (double.clone(), 1),
            (saturating.clone(), 2)
        ]
    );
    for (callee, from, to) in [
        ("double", &run, &double),
        ("saturating", &double, &saturating),
    ] {
        let e = edge(&deep, callee);
        assert_eq!(
            (e.visit, e.boundary_reason),
            (Visit::New, BoundaryReason::None)
        );
        assert_eq!((&e.from, e.to.as_ref()), (from, Some(to)));
        let binding = e.binding.unwrap();
        assert_eq!(
            binding.dispatch,
            trellis::semantic_evidence::Dispatch::Direct
        );
    }
    for (callee, reason) in [
        ("add", BoundaryReason::Dispatch),
        ("new", BoundaryReason::Unresolved),
    ] {
        let e = edge(&deep, callee);
        assert_eq!(
            (e.visit, e.boundary_reason),
            (Visit::Boundary, reason),
            "{callee}"
        );
        assert_eq!(e.to, None);
    }
    // The depth bound stops at `saturating`: its own call is depth frontier, not expanded.
    assert!(
        deep.frontier
            .iter()
            .any(|f| f.reason == FrontierReason::Depth && f.node_id == saturating)
    );
    assert!(deep.edges.iter().all(|e| e.from != saturating));

    // Depth 1 admits `double` and leaves its call on the depth frontier.
    let shallow = graph(1, 150);
    assert_eq!(nodes(&shallow), [(run.clone(), 0), (double.clone(), 1)]);
    assert!(
        shallow
            .frontier
            .iter()
            .any(|f| f.reason == FrontierReason::Depth && f.node_id == double)
    );
    assert!(shallow.edges.iter().all(|e| e.from == run));

    // The node bound holds: with room for two nodes the direct call into `saturating` is a
    // node-limit boundary and the result is partial.
    let bounded = graph(2, 2);
    assert_eq!(nodes(&bounded), [(run.clone(), 0), (double.clone(), 1)]);
    let e = edge(&bounded, "saturating");
    assert_eq!(
        (e.visit, e.boundary_reason, e.to),
        (Visit::Boundary, BoundaryReason::NodeLimit, None)
    );
    assert!(bounded.partial);

    // The sequence view takes the same disposition: the direct call names its target.
    let view = world.store.sequence_at(&run, pin, false).unwrap().unwrap();
    fn steps(all: &[trellis::behavior::SequenceStep]) -> Vec<&trellis::behavior::SequenceStep> {
        all.iter()
            .flat_map(|s| {
                std::iter::once(s)
                    .chain(steps(&s.children))
                    .chain(steps(&s.alternate))
            })
            .collect()
    }
    let find = |view: &trellis::behavior::SequenceView, label: &str| {
        let found: Vec<_> = steps(&view.steps)
            .into_iter()
            .filter(|s| {
                s.call_evidence.is_some()
                    && (s.label == label || s.label.ends_with(&format!("::{label}")))
            })
            .cloned()
            .collect();
        assert_eq!(found.len(), 1, "{label}");
        found.into_iter().next().unwrap()
    };
    let evidence = |label: &str| {
        let step = find(&view, label);
        (
            step.target.clone(),
            step.call_evidence.as_ref().unwrap().boundary_reason,
        )
    };
    assert_eq!(
        evidence("double"),
        (Some(double.clone()), BoundaryReason::None)
    );
    assert_eq!(evidence("add"), (None, BoundaryReason::Dispatch));
    assert_eq!(evidence("new"), (None, BoundaryReason::Unresolved));

    // #522: participants come from the resolved targets, and each direct callee's own
    // body is expanded beneath its call on the callee's lane: `run` -> `double` ->
    // `saturating`, whose `saturating_add` is a standard-library call and so incidental.
    assert_eq!(
        view.semantic.tier,
        trellis::behavior::EvidenceTier::Semantic
    );
    assert_eq!(
        view.semantic.producer_id.as_deref(),
        Some(producer.as_str())
    );
    let lanes: Vec<_> = view.participants.iter().map(|p| p.id.clone()).collect();
    assert_eq!(lanes, [run.clone(), double.clone(), saturating.clone()]);
    let into_double = find(&view, "double");
    assert!(into_double.lane.is_none());
    let into_saturating = find(&view, "saturating");
    assert_eq!(into_saturating.lane.as_deref(), Some(double.as_str()));
    assert_eq!(into_saturating.target.as_deref(), Some(saturating.as_str()));
    assert!(
        into_double
            .children
            .iter()
            .any(|s| s.id == into_saturating.id)
    );
    let std_call = find(&view, "saturating_add");
    assert_eq!(std_call.lane.as_deref(), Some(saturating.as_str()));
    let std_evidence = std_call.call_evidence.as_ref().unwrap();
    assert_eq!(std_evidence.boundary_reason, BoundaryReason::External);
    assert_eq!(
        std_evidence
            .semantic_basis
            .as_ref()
            .map(|b| b.producer_id.as_str()),
        Some("rust-analyzer")
    );
    assert!(std_call.hidden);
    assert_eq!((view.hidden_steps, view.incidental_steps), (1, 1));
    // Show all keeps the same evidence but hides nothing.
    let all = world.store.sequence_at(&run, pin, true).unwrap().unwrap();
    assert!(!find(&all, "saturating_add").hidden);
    assert_eq!((all.hidden_steps, all.incidental_steps), (0, 1));

    // A recursive call names its own lane but is never re-entered.
    let spin = id_of("spin");
    let recursive = world.store.sequence_at(&spin, pin, false).unwrap().unwrap();
    let call = find(&recursive, "spin");
    assert_eq!(call.target.as_deref(), Some(spin.as_str()));
    assert!(call.children.is_empty());
}

/// Owner ruling 2026-10-28 (#394, option c), on real rust-analyzer bytes with the
/// per-document cap shrunk below src/lib.rs's 36 occurrences: the document is admitted partial
/// with the typed reason, keeps exactly its definitions, and both join paths agree.
#[test]
fn real_rust_analyzer_over_cap_document_is_admitted_partial_with_its_reason() {
    let world = rust_analyzer_world();
    let descriptor = rust_analyzer_batch(&world);
    let full =
        rust_analyzer_publications(&world, &descriptor, &BatchLimits::PRODUCTION, "uncapped");
    let limits = BatchLimits {
        document_occurrences: 20,
        ..BatchLimits::PRODUCTION
    };
    let capped = rust_analyzer_publications(&world, &descriptor, &limits, "capped");
    let lib = coverage_of(&capped, "src/lib.rs");
    assert_eq!(
        lib.state,
        trellis::semantic_evidence::CoverageState::Partial
    );
    assert_eq!(lib.diagnostic.as_deref(), Some("occurrence_cap_exceeded"));
    assert!(lib.selected);
    // The observed roles are those the kept definition facts author: never `call`.
    let full_lib = coverage_of(&full, "src/lib.rs");
    assert!(
        lib.observed_roles
            .iter()
            .all(|role| full_lib.observed_roles.contains(role))
    );
    assert!(
        lib.observed_roles
            .contains(&trellis::semantic_evidence::Role::Definition)
    );
    assert!(
        !lib.observed_roles
            .contains(&trellis::semantic_evidence::Role::Call)
    );
    assert!(
        full_lib
            .observed_roles
            .contains(&trellis::semantic_evidence::Role::Call)
    );
    // util.rs (16 occurrences) is untouched.
    assert_eq!(
        coverage_of(&capped, "src/util.rs"),
        coverage_of(&full, "src/util.rs")
    );
    let in_lib = |publication: &SemanticPublication| {
        publication
            .declaration_bindings
            .iter()
            .filter(|binding| binding.join.anchor.document.path == "src/lib.rs")
            .cloned()
            .collect::<Vec<_>>()
    };
    assert!(!in_lib(&capped).is_empty());
    assert_eq!(
        in_lib(&capped),
        in_lib(&full),
        "the kept definitions bind as before"
    );
    assert!(
        capped
            .call_bindings
            .iter()
            .all(|binding| binding.join.anchor.document.path != "src/lib.rs"),
        "dropped references author no call"
    );
    // The admitted claims of the capped document are its definitions and symbols only.
    let admission = world
        .try_memory_admission_with(&descriptor, &limits)
        .unwrap();
    assert_eq!(
        admission.merged.capped_documents,
        std::collections::BTreeSet::from([("rust".to_owned(), "src/lib.rs".to_owned())])
    );
    let lib_claims: Vec<_> = admission
        .merged
        .claims
        .iter()
        .filter(|claim| {
            claim.envelope.relative_path == "src/lib.rs"
                && claim.envelope.record_kind == "occurrence"
        })
        .collect();
    assert_eq!(lib_claims.len(), 11);
    assert!(
        lib_claims
            .iter()
            .all(|claim| claim.envelope.raw["symbolRoles"].as_u64().unwrap() & 1 == 1)
    );
    // Publication keeps the partial row and records the capped document with the raw capture.
    let (mut admitted, joined, _) = world
        .join_with(&descriptor, &limits, DestinationMutations::default())
        .unwrap();
    let leader = world.store.publication_for_tests().unwrap();
    let pin = world
        .store
        .publish_semantic_batch(
            &joined,
            admitted.stage_mut(),
            joined.destination_pin(),
            &leader,
            &world.cancel,
        )
        .unwrap();
    drop(leader);
    let published = world.store.semantic_at(pin).unwrap();
    let stored = published
        .coverage
        .iter()
        .find(|row| row.document_path == "src/lib.rs")
        .unwrap();
    assert_eq!(
        (stored.state, &stored.diagnostic, &stored.observed_roles),
        (lib.state, &lib.diagnostic, &lib.observed_roles)
    );
    assert_eq!(
        world.count(
            "SELECT COUNT(*) FROM semantic_raw_documents WHERE capped=1 AND path='src/lib.rs'"
        ),
        1
    );
    assert_eq!(
        world.count("SELECT COUNT(*) FROM semantic_raw_documents WHERE capped=0"),
        1
    );
}

/// A Rust document with an attribute, a macro invocation hiding a definition and an ordinary
/// call: seven occurrences, four of them definitions.
const GAPS_RS: &str = "#[derive(Debug)]\npub struct Point;\n\nmy_macro! { fn hidden() {} }\n\npub fn run() { helper(); }\npub fn helper() {}\n";

fn gaps_doc() -> DocSpec {
    DocSpec::new("src/lib.rs")
        .occurrence(&[0, 9, 14], &[], "external pkg Debug#", 0)
        .occurrence(&[1, 11, 16], &[], "pkg Point#", 1)
        .occurrence(&[3, 0, 8], &[], "external pkg my_macro.", 0)
        .occurrence(&[3, 15, 21], &[], "pkg hidden.", 1)
        .occurrence(&[5, 7, 10], &[], "pkg run.", 1)
        .occurrence(&[5, 15, 21], &[5, 15, 23], "pkg helper.", 0)
        .occurrence(&[6, 7, 13], &[], "pkg helper.", 1)
        .symbol("pkg run.", 17, &[])
        .symbol("pkg helper.", 17, &[])
}

/// #520: a partial document records every reason, not only the diagnostic's. Under the
/// per-document cap the macro-hidden definition still counts as a native gap, so the row is
/// `unsupported_native_gap` and capped at once. Both join paths, the stored row, the import
/// report and status agree.
#[test]
fn partial_document_records_a_native_gap_and_the_cap_together() {
    use trellis::semantic_evidence::{CoverageReasons, CoverageState};
    let world = world(|root| {
        write_files(
            root,
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"gaps\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
                ),
                ("src/lib.rs", GAPS_RS),
            ],
        )
    });
    let mut fixture = world.fixture();
    fixture.artifact("rust", vec![gaps_doc()]);
    let descriptor = world.write(&fixture);
    let capped_limits = BatchLimits {
        document_occurrences: 5,
        ..BatchLimits::PRODUCTION
    };
    let cases = [
        (
            BatchLimits::PRODUCTION,
            "uncapped",
            CoverageReasons::new([2, 1, 0, 0], false, false, false),
            "partial: 1 · native macro/attribute gaps only",
        ),
        (
            capped_limits,
            "capped",
            CoverageReasons::new([1, 0, 0, 0], true, false, false),
            "partial: 1 · native gaps 1, occurrence cap 1",
        ),
    ];
    for (limits, case, expected, summary) in cases {
        let (mut admitted, joined, _) = world
            .join_with(&descriptor, &limits, DestinationMutations::default())
            .unwrap();
        let staged = testing::staged_publication(&mut admitted).unwrap();
        let rejoined = world
            .memory_rejoin_with(&descriptor, DestinationMutations::default(), &limits)
            .unwrap();
        assert_same_publication(&staged, &rejoined.publication, case);
        let row = coverage_of(&staged, "src/lib.rs");
        assert_eq!(row.state, CoverageState::Partial, "{case}");
        assert_eq!(
            row.diagnostic.as_deref(),
            Some("unsupported_native_gap"),
            "{case}"
        );
        assert_eq!(row.reasons, Some(expected.clone()), "{case}");
        // The report gives the same reasons on both paths.
        let reasons = &joined.report()["languages"][0]["partialReasons"];
        assert_eq!(
            reasons, &rejoined.report["languages"][0]["partialReasons"],
            "{case}"
        );
        assert_eq!(reasons["documents"], 1, "{case}");
        assert_eq!(reasons["capped"], u64::from(expected.capped), "{case}");
        assert_eq!(
            reasons["unsupportedOccurrences"],
            json!({"macroInvocation": expected.macro_invocation,
                   "attributeItem": expected.attribute_item, "macroDefinition": 0, "other": 0}),
            "{case}"
        );
        assert_eq!(
            reasons["listed"],
            json!([{"path": "src/lib.rs", "diagnostic": "unsupported_native_gap",
                    "reasons": serde_json::to_value(&expected).unwrap()}]),
            "{case}"
        );
        assert_eq!(reasons["unlisted"], 0, "{case}");
        // Publication stores the reasons, and status summarizes them for the language.
        let leader = world.store.publication_for_tests().unwrap();
        let pin = world
            .store
            .publish_semantic_batch(
                &joined,
                admitted.stage_mut(),
                joined.destination_pin(),
                &leader,
                &world.cancel,
            )
            .unwrap();
        drop(leader);
        let published = world.store.semantic_at(pin).unwrap();
        assert_eq!(
            published
                .coverage
                .iter()
                .find(|row| row.document_path == "src/lib.rs")
                .unwrap()
                .reasons,
            Some(expected.clone()),
            "{case}"
        );
        let status = world.store.status().unwrap();
        let coverage = language_row(&status, "rust").coverage.clone().unwrap();
        assert_eq!(
            (coverage.complete, coverage.partial, coverage.failed),
            (0, 1, 0),
            "{case}"
        );
        assert_eq!(coverage.summary.as_deref(), Some(summary), "{case}");
        assert_eq!(
            coverage.partial_reasons.capped,
            usize::from(expected.capped),
            "{case}"
        );
        assert_eq!(
            coverage
                .partial_reasons
                .unsupported_occurrences
                .macro_invocation,
            expected.macro_invocation,
            "{case}"
        );
    }
    // A later native revision rejoins the selected batch, under production limits, and records
    // the rejoined tuple's reasons again.
    write_files(&world.root, &[("src/other.rs", "pub fn other() {}\n")]);
    world.republish_with_rejoin();
    let head = world.store.index_baseline().unwrap();
    let carried = world.store.semantic_at(head).unwrap();
    assert_eq!(
        carried
            .coverage
            .iter()
            .find(|row| row.document_path == "src/lib.rs")
            .unwrap()
            .reasons,
        Some(CoverageReasons::new([2, 1, 0, 0], false, false, false))
    );
    let status = world.store.status().unwrap();
    assert_eq!(
        language_row(&status, "rust")
            .coverage
            .as_ref()
            .unwrap()
            .summary
            .as_deref(),
        Some("partial: 1 · native macro/attribute gaps only")
    );
    // MCP describe reads the same summary for the language's selected producer.
    let tiers = world
        .store
        .evidence_response()
        .unwrap()
        .mcp_language_tiers()
        .unwrap();
    let rust = tiers[1].partial_reasons.as_ref().unwrap();
    assert_eq!(
        rust.summary.as_deref(),
        Some("partial: 1 · native macro/attribute gaps only")
    );
    assert_eq!(
        rust.partial_reasons.unsupported_occurrences.attribute_item,
        1
    );
    assert!(tiers[0].partial_reasons.is_none());
}

/// The aggregate candidate cap still fails typed and atomically on both paths (#394, a).
#[test]
fn real_rust_analyzer_aggregate_candidate_cap_fails_typed_and_atomically() {
    let world = rust_analyzer_world();
    let descriptor = rust_analyzer_batch(&world);
    let head = world.store.index_baseline().unwrap();
    // 52 occurrences and 20 symbol informations: 72 candidates.
    let limits = BatchLimits {
        candidates: 71,
        ..BatchLimits::PRODUCTION
    };
    let error = world
        .join_with(&descriptor, &limits, DestinationMutations::default())
        .map(|_| ())
        .unwrap_err();
    assert!(
        matches!(
            error.downcast_ref::<ScipCaptureError>(),
            Some(ScipCaptureError::LimitExceeded(name)) if name == "candidate facts"
        ),
        "{error:#}"
    );
    assert_eq!(
        world
            .try_memory_admission_with(&descriptor, &limits)
            .map(|_| ())
            .unwrap_err(),
        ScipCaptureError::LimitExceeded("candidate facts".into())
    );
    assert_nothing_published(&world, head);
    assert!(world.stage_dirs().is_empty());
    let limits = BatchLimits {
        candidates: 72,
        ..BatchLimits::PRODUCTION
    };
    world
        .join_with(&descriptor, &limits, DestinationMutations::default())
        .unwrap();
}

// Real rust-analyzer 1.98.1 output (`rust-analyzer scip`, the profile-pinned binary) for the
// multi-target crate in `tests/fixtures/scip-rust-analyzer-targets/workspace`, Metadata unit
// dropped as for `RUST_ANALYZER`. rust-analyzer repeats `crate/` in all six targets, `main().`
// in the bin, the build script and the example, and `run().` in the library and the
// `tests/other.rs` crate. `tests/common/mod.rs` belongs to two test crates (#394).
const RUST_ANALYZER_TARGETS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/scip-rust-analyzer-targets"
);
const RA_CRATE: &str = "rust-analyzer cargo fixture 0.1.0 crate/";
const RA_MAIN: &str = "rust-analyzer cargo fixture 0.1.0 main().";
const RA_RUN: &str = "rust-analyzer cargo fixture 0.1.0 run().";

fn rust_targets_world() -> World {
    world(|root| copy_tree(&Path::new(RUST_ANALYZER_TARGETS).join("workspace"), root))
}

fn rust_targets_documents() -> Vec<u8> {
    fs::read(Path::new(RUST_ANALYZER_TARGETS).join("documents.scip")).unwrap()
}

/// A batch of the fixture's document units behind Metadata for the test workspace: the
/// producer's exact bytes, or, with `edit`, the units decoded, edited and re-encoded.
fn rust_targets_batch(
    world: &World,
    edit: Option<&dyn Fn(&mut scip::types::Document)>,
) -> (PathBuf, String) {
    let mut metadata = scip::types::Metadata::new();
    metadata.project_root = format!("file://{}", world.root.display());
    metadata.text_document_encoding = protobuf::EnumOrUnknown::from_i32(1);
    let mut tool = scip::types::ToolInfo::new();
    tool.name = "rust-analyzer".into();
    tool.version = "1.98.1 (48a229ce 2026-09-01)".into();
    metadata.tool_info = Some(tool).into();
    let mut header = scip::types::Index::new();
    header.metadata = Some(metadata).into();
    let mut artifact = header.write_to_bytes().unwrap();
    match edit {
        None => artifact.extend(rust_targets_documents()),
        Some(edit) => {
            let mut index =
                scip::types::Index::parse_from_bytes(&rust_targets_documents()).unwrap();
            index.documents.iter_mut().for_each(edit);
            artifact.extend(index.write_to_bytes().unwrap());
        }
    }
    let mut fixture = world.fixture().reference("rust-analyzer");
    let docs = fixture.paths("rust");
    assert_eq!(docs.len(), 8);
    let ordinal = fixture.artifact("rust", docs);
    fixture.override_artifact_raw(ordinal, artifact);
    world.write(&fixture)
}

/// `(scope, scope document path)` of every symbol with key `symbol`, sorted.
fn symbol_scopes(publication: &SemanticPublication, symbol: &str) -> Vec<(String, Option<String>)> {
    let mut scopes: Vec<_> = publication
        .symbols
        .iter()
        .filter(|row| row.key.symbol == symbol)
        .map(|row| {
            (
                format!("{:?}", row.key.scope),
                row.key.document.as_ref().map(|d| d.path.clone()),
            )
        })
        .collect();
    scopes.sort();
    scopes
}

/// `(resolution, declared internal target document)` of the one use reference spelled
/// `spelling` in `path`.
fn use_resolution(
    publication: &SemanticPublication,
    path: &str,
    spelling: &str,
) -> (Resolution, Option<String>) {
    let found: Vec<_> = publication
        .references
        .iter()
        .filter(|r| {
            r.document.path == path
                && r.spelling == spelling
                && r.site == trellis::semantic_evidence::ReferenceSite::Use
        })
        .collect();
    let [reference] = found[..] else {
        panic!("{path} {spelling}: {found:?}");
    };
    (
        reference.resolution,
        match &reference.declared_target {
            Some(trellis::semantic_evidence::Target::Internal { document, .. }) => {
                Some(document.path.clone())
            }
            _ => None,
        },
    )
}

/// Owner ruling 2026-10-30 (#394): each Cargo target is its own symbol namespace, derived from
/// the captured `Cargo.toml` and the target roots. Real rust-analyzer keys repeated across
/// targets import as distinct target-scoped symbols; a use resolves in its own target first,
/// then in the library target that defines the key; a document in two test crates keeps
/// global keys. The batch join, the in-memory oracle and stored-original rejoin agree.
#[test]
fn real_rust_analyzer_keys_repeated_across_cargo_targets_import_per_target() {
    let world = rust_targets_world();
    let descriptor = rust_targets_batch(&world, None);
    let staged =
        rust_analyzer_publications(&world, &descriptor, &BatchLimits::PRODUCTION, "targets");
    world
        .full_validation(&descriptor, DestinationMutations::default(), &staged)
        .unwrap();
    let target = |root: &str| ("Target".to_owned(), Some(root.to_owned()));
    assert_eq!(
        symbol_scopes(&staged, RA_CRATE),
        [
            target("build.rs"),
            target("examples/demo.rs"),
            target("src/lib.rs"),
            target("src/main.rs"),
            target("tests/it.rs"),
            target("tests/other.rs"),
        ]
    );
    assert_eq!(
        symbol_scopes(&staged, RA_MAIN),
        [
            target("build.rs"),
            target("examples/demo.rs"),
            target("src/main.rs")
        ]
    );
    assert_eq!(
        symbol_scopes(&staged, RA_RUN),
        [target("src/lib.rs"), target("tests/other.rs")]
    );
    // A module file is in the target whose crate root reaches it.
    assert_eq!(
        symbol_scopes(&staged, "rust-analyzer cargo fixture 0.1.0 util/helper()."),
        [target("src/lib.rs")]
    );
    // `tests/common/mod.rs` is reached by two test crates: no target, the global key.
    assert_eq!(
        symbol_scopes(
            &staged,
            "rust-analyzer cargo fixture 0.1.0 common/shared()."
        ),
        [("Global".to_owned(), None)]
    );
    // Own target first: `run()` in tests/other.rs is that crate's; `fixture::run()` from the
    // bin and tests/it.rs, which define none, is the library's.
    let resolved = |path: &str| (Resolution::Resolved, Some(path.to_owned()));
    assert_eq!(
        use_resolution(&staged, "tests/other.rs", "run"),
        resolved("tests/other.rs")
    );
    assert_eq!(
        use_resolution(&staged, "tests/it.rs", "run"),
        resolved("src/lib.rs")
    );
    assert_eq!(
        use_resolution(&staged, "src/main.rs", "run"),
        resolved("src/lib.rs")
    );
    assert_eq!(
        use_resolution(&staged, "examples/demo.rs", "helper"),
        resolved("src/util.rs")
    );
    // A document without a determined target (in two test crates) takes a library's key only
    // when that library is the one target defining it.
    assert_eq!(
        use_resolution(&staged, "tests/common/mod.rs", "helper"),
        resolved("src/util.rs")
    );
    // The target is part of each Rust claim's envelope, so of its digest and proof identity.
    let admission = world.memory_admission(&descriptor);
    let crate_claims: Vec<_> = admission
        .merged
        .claims
        .iter()
        .filter(|c| {
            c.envelope.record_kind == "symbolInformation"
                && c.envelope.raw["symbol"] == json!(RA_CRATE)
        })
        .collect();
    assert_eq!(crate_claims.len(), 6);
    assert!(crate_claims.iter().all(|claim| {
        claim
            .envelope
            .cargo_target
            .as_ref()
            .map(|t| t.root.as_str())
            == Some(claim.envelope.relative_path.as_str())
            && claim
                .canonical_bytes
                .starts_with(b"{\"cargoTarget\":{\"kind\":")
    }));
    let shared = admission
        .merged
        .claims
        .iter()
        .find(|c| c.envelope.relative_path == "tests/common/mod.rs")
        .unwrap();
    assert!(shared.envelope.cargo_target.is_none());
    assert!(shared.canonical_bytes.starts_with(b"{\"formatVersion\":"));
    // Published, then rejoined from the stored original after an unrelated edit.
    let (pin, mut admitted, _) = world.publish(&descriptor).unwrap();
    let staged = testing::staged_publication(&mut admitted).unwrap();
    drop(admitted);
    assert_published_matches_staged(&world.store.semantic_at(pin).unwrap(), &staged);
    let proofs = stored_proofs(&world);
    write_files(&world.root, &[("Unrelated.java", "class Unrelated {}\n")]);
    world.republish_with_rejoin();
    assert_eq!(world.rejoin_failures(), 0);
    assert_eq!(stored_proofs(&world), proofs);
    let head = world
        .store
        .semantic_at(world.store.index_baseline().unwrap())
        .unwrap();
    let rejoined = SemanticPublication {
        symbols: head.symbols,
        references: head.references,
        ..staged.clone()
    };
    assert_eq!(
        symbol_scopes(&rejoined, RA_CRATE),
        symbol_scopes(&staged, RA_CRATE)
    );
    assert_eq!(
        use_resolution(&rejoined, "tests/it.rs", "run"),
        resolved("src/lib.rs")
    );
}

/// Asserts the batch join rejects `descriptor` as a duplicate key, typed, that contract
/// validation rejects the in-memory oracle's rows alike, and that nothing is published.
fn assert_duplicate_key_rejected(world: &World, descriptor: &(PathBuf, String), case: &str) {
    let head = world.store.index_baseline().unwrap();
    let error = world
        .join_with(
            descriptor,
            &BatchLimits::PRODUCTION,
            DestinationMutations::default(),
        )
        .map(|_| ())
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<ScipCaptureError>(),
        Some(&ScipCaptureError::InvalidArtifact(
            "duplicate semantic symbol key".into()
        )),
        "{case}: {error:#}"
    );
    // The in-memory oracle builds the same rows; contract validation rejects them.
    let memory = world.memory_publication(descriptor, DestinationMutations::default());
    let invalid = world
        .full_validation(descriptor, DestinationMutations::default(), &memory)
        .unwrap_err();
    assert!(
        format!("{invalid:#}").contains("duplicate semantic symbol key"),
        "{case}: {invalid:#}"
    );
    assert_nothing_published(world, head);
}

/// A key is unique per Cargo target (#394): a second document of one target giving the same
/// key, or a document without a determined target giving a key some target has, still
/// rejects the artifact typed.
#[test]
fn real_rust_analyzer_duplicate_keys_within_a_target_or_against_a_global_key_reject() {
    let world = rust_targets_world();
    let index = scip::types::Index::parse_from_bytes(&rust_targets_documents()).unwrap();
    let library_run = index
        .documents
        .iter()
        .find(|d| d.relative_path == "src/lib.rs")
        .and_then(|d| d.symbols.iter().find(|s| s.symbol == RA_RUN))
        .cloned()
        .unwrap();
    for (case, path) in [
        // src/util.rs is in the library target, which already has `run()`.
        ("same target", "src/util.rs"),
        // tests/common/mod.rs has no target: its global `run()` meets two targets' keys.
        ("global against target", "tests/common/mod.rs"),
    ] {
        let edit = |document: &mut scip::types::Document| {
            if document.relative_path == path {
                document.symbols.push(library_run.clone());
            }
        };
        let descriptor = rust_targets_batch(&world, Some(&edit));
        assert_duplicate_key_rejected(&world, &descriptor, case);
    }
}

// Real rust-analyzer 1.98.1 output for the two-package workspace in
// `tests/fixtures/scip-rust-analyzer-packages/workspace`, Metadata unit dropped: libraries
// `alpha 0.1.0` and `beta 0.2.0` each define `run()`, and beta's test crate calls both.
const RUST_ANALYZER_PACKAGES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/scip-rust-analyzer-packages"
);
const ALPHA_RUN: &str = "rust-analyzer cargo alpha 0.1.0 run().";

/// A batch of the two-package fixture, each symbol spelling passed through `rename` in the
/// documents `rename` is given.
fn rust_packages_batch(world: &World, rename: &dyn Fn(&str, &str) -> String) -> (PathBuf, String) {
    let mut metadata = scip::types::Metadata::new();
    metadata.project_root = format!("file://{}", world.root.display());
    metadata.text_document_encoding = protobuf::EnumOrUnknown::from_i32(1);
    let mut tool = scip::types::ToolInfo::new();
    tool.name = "rust-analyzer".into();
    tool.version = "1.98.1 (48a229ce 2026-09-01)".into();
    metadata.tool_info = Some(tool).into();
    let mut header = scip::types::Index::new();
    header.metadata = Some(metadata).into();
    let mut artifact = header.write_to_bytes().unwrap();
    let documents = fs::read(Path::new(RUST_ANALYZER_PACKAGES).join("documents.scip")).unwrap();
    let mut index = scip::types::Index::parse_from_bytes(&documents).unwrap();
    for document in &mut index.documents {
        let path = document.relative_path.clone();
        for occurrence in &mut document.occurrences {
            occurrence.symbol = rename(&path, &occurrence.symbol);
        }
        for symbol in &mut document.symbols {
            symbol.symbol = rename(&path, &symbol.symbol);
        }
    }
    artifact.extend(index.write_to_bytes().unwrap());
    let mut fixture = world.fixture().reference("rust-analyzer");
    let docs = fixture.paths("rust");
    assert_eq!(docs.len(), 3);
    let ordinal = fixture.artifact("rust", docs);
    fixture.override_artifact_raw(ordinal, artifact);
    world.write(&fixture)
}

/// Supervisor ruling on #394 ruling 2: the library fallback must also match the package
/// rust-analyzer's key names against the admitted manifest's `[package]` name and version.
/// Two workspace libraries defining the same key resolve by package; a key whose package
/// version is not the manifest's keeps the global key.
#[test]
fn real_rust_analyzer_library_fallback_matches_the_keys_package() {
    let world = world(|root| copy_tree(&Path::new(RUST_ANALYZER_PACKAGES).join("workspace"), root));
    let uses = "crates/beta/tests/uses.rs";
    let resolved = |path: &str| (Resolution::Resolved, Some(path.to_owned()));
    let alpha = "crates/alpha/src/lib.rs";
    // The producer's own keys: each call resolves to its package's library.
    let descriptor = rust_packages_batch(&world, &|_, symbol| symbol.to_owned());
    let staged = rust_analyzer_publications(&world, &descriptor, &BatchLimits::PRODUCTION, "own");
    world
        .full_validation(&descriptor, DestinationMutations::default(), &staged)
        .unwrap();
    assert_eq!(
        symbol_scopes(&staged, ALPHA_RUN),
        [("Target".to_owned(), Some(alpha.to_owned()))]
    );
    let calls = |publication: &SemanticPublication| {
        let mut found: Vec<_> = publication
            .references
            .iter()
            .filter(|r| r.document.path == uses && r.spelling == "run")
            .map(|r| {
                (
                    r.resolution,
                    match &r.declared_target {
                        Some(trellis::semantic_evidence::Target::Internal { document, .. }) => {
                            Some(document.path.clone())
                        }
                        _ => None,
                    },
                )
            })
            .collect();
        found.sort_by_key(|(_, path)| path.clone());
        found
    };
    assert_eq!(
        calls(&staged),
        [resolved(alpha), resolved("crates/beta/src/lib.rs")]
    );
    // Both libraries define alpha's key: only alpha's manifest names its package.
    let same_key = |path: &str, symbol: &str| {
        if path == "crates/beta/src/lib.rs" {
            symbol.replace("cargo beta 0.2.0 run().", "cargo alpha 0.1.0 run().")
        } else {
            symbol.to_owned()
        }
    };
    let descriptor = rust_packages_batch(&world, &same_key);
    let staged =
        rust_analyzer_publications(&world, &descriptor, &BatchLimits::PRODUCTION, "same key");
    assert_eq!(
        symbol_scopes(&staged, ALPHA_RUN),
        [
            ("Target".to_owned(), Some(alpha.to_owned())),
            (
                "Target".to_owned(),
                Some("crates/beta/src/lib.rs".to_owned())
            ),
        ]
    );
    assert!(calls(&staged).contains(&resolved(alpha)));
    // A key naming a version the manifest does not state matches no library: global key,
    // which no document defines, so the use stays unresolved.
    let other_version = |_: &str, symbol: &str| symbol.replace("alpha 0.1.0", "alpha 9.9.9");
    let descriptor = rust_packages_batch(&world, &other_version);
    let staged =
        rust_analyzer_publications(&world, &descriptor, &BatchLimits::PRODUCTION, "version");
    assert!(calls(&staged).contains(&(Resolution::Unresolved, None)));
}

// Owner ruling 2026-10-30 (#483): the default semantic producer is chosen per language.

const JS_FILES: [&str; 2] = ["src/lib.js", "src/main.js"];
const JS_FIXTURE: &str = "issue444-javascript-calls";

/// The real rust-analyzer crate and the real scip-typescript sources in one workspace, with
/// the workspace marker an MCP session selects.
fn mixed_world() -> World {
    let js = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/semantic-import")
        .join(JS_FIXTURE);
    world_with(
        |root| {
            fs::create_dir_all(root.join(".git")).unwrap();
            WorkspaceIdentity::discover(Some(root), root)
                .unwrap()
                .attach_marker()
                .unwrap();
            copy_tree(&Path::new(RUST_ANALYZER).join("workspace"), root);
            for path in JS_FILES {
                fs::write(root.join(path), fs::read(js.join(path)).unwrap()).unwrap();
            }
            // The fixture's package root names its module markers (#443).
            fs::write(
                root.join("package.json"),
                r#"{"name":"issue444-fixture","version":"1.0.0"}"#,
            )
            .unwrap();
        },
        true,
    )
}

/// The syntax ID of the declaration named `name` in `path`.
fn declaration_in(world: &World, name: &str, path: &str) -> String {
    let (_, native, _) = index_workspace_bundle(
        &IndexOptions::new(world.root.clone()),
        world.store.root_id(),
        &world.cancel,
        |_| {},
    )
    .unwrap();
    let found: Vec<_> = native
        .declarations
        .iter()
        .filter(|d| d.name.as_deref() == Some(name) && d.document.path == path)
        .collect();
    assert_eq!(found.len(), 1, "{name} in {path}");
    found[0].syntax_id.clone()
}

/// Every published producer ID at the head.
fn head_producers(world: &World) -> Vec<String> {
    let head = world.store.index_baseline().unwrap();
    world
        .store
        .semantic_at(head)
        .unwrap()
        .producers
        .iter()
        .map(|producer| producer.id.clone())
        .collect()
}

fn language_row<'s>(
    status: &'s trellis::model::IndexStatus,
    language: &str,
) -> &'s trellis::model::LanguageSemantic {
    status
        .semantic_languages
        .iter()
        .find(|row| row.language == language)
        .unwrap_or_else(|| panic!("{language}: {:?}", status.semantic_languages))
}

/// The step evidence of each call step in a sequence, by label.
fn sequence_targets(view: &trellis::behavior::SequenceView) -> Vec<(String, Option<String>)> {
    fn steps(all: &[trellis::behavior::SequenceStep]) -> Vec<&trellis::behavior::SequenceStep> {
        all.iter()
            .flat_map(|s| {
                std::iter::once(s)
                    .chain(steps(&s.children))
                    .chain(steps(&s.alternate))
            })
            .collect()
    }
    steps(&view.steps)
        .into_iter()
        .filter(|step| step.call_evidence.is_some())
        .map(|step| (step.label.clone(), step.target.clone()))
        .collect()
}

fn warning<'v>(envelope: &'v Value, code: &str) -> Option<&'v Value> {
    envelope["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["code"] == code)
}

/// With rust-analyzer and scip-typescript both published, each language selects its own
/// producer: status, the CLI query and symbols, MCP and the web sequence. An explicit
/// producer still overrides and is validated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_language_selects_its_own_published_producer() {
    use trellis::graph_query::BoundaryReason;
    use trellis::model::SemanticState;
    let world = mixed_world();
    let rust = rust_analyzer_batch(&world);
    drop(world.publish(&rust).unwrap());
    let (pin, _) = publish_real_fixture(&world, JS_FIXTURE, &JS_FILES);
    assert_eq!(head_producers(&world), ["rust-analyzer", "scip-typescript"]);
    let run = declaration_in(&world, "run", "src/lib.rs");
    let double = declaration_in(&world, "double", "src/util.rs");
    let saturating = declaration_in(&world, "saturating", "src/util.rs");
    let caller = declaration_in(&world, "caller", "src/main.js");
    let helper = declaration_in(&world, "helper", "src/lib.js");
    let response = world.store.evidence_response().unwrap();

    // Status: per-language state and counts, and their combined total.
    for status in [response.status().unwrap(), world.store.status().unwrap()] {
        assert_eq!(status.semantic_producer_id, None);
        assert_eq!(status.semantic_note, None);
        assert_eq!(status.semantic_languages.len(), 2);
        for (language, producer) in [("javascript", "scip-typescript"), ("rust", "rust-analyzer")] {
            let row = language_row(&status, language);
            assert_eq!(row.semantic_producer_id.as_deref(), Some(producer));
            assert_eq!(row.semantic_note, None);
            assert_eq!(row.semantic_state, SemanticState::Fresh, "{row:?}");
            assert!(row.internal > 0, "{row:?}");
            assert_eq!(
                row.internal + row.external + row.ambiguous + row.unresolved,
                row.calls,
                "{row:?}"
            );
        }
        let rows = &status.semantic_languages;
        assert_eq!(
            status.stats.internal,
            rows.iter().map(|row| row.internal).sum::<usize>()
        );
        assert_eq!(
            status.stats.external,
            rows.iter().map(|row| row.external).sum::<usize>()
        );
        assert_eq!(
            status.stats.ambiguous,
            rows.iter().map(|row| row.ambiguous).sum::<usize>()
        );
        assert_eq!(status.stats.semantic_state, SemanticState::Fresh);
    }

    // CLI query: each seed uses its own language's producer, and the walk follows direct
    // bindings to depth 2 (`run` -> `double` -> `saturating`).
    let mut deep = view_query(&run, None);
    deep.depth = 2;
    let view = response.query_view(&deep).unwrap().unwrap();
    assert_eq!(view.revision, pin);
    assert_eq!(view.semantic.producer_id.as_deref(), Some("rust-analyzer"));
    assert_eq!(view.semantic.note, None);
    let nodes: Vec<_> = view.nodes.iter().map(|n| n.id.clone()).collect();
    assert_eq!(nodes, [run.clone(), double.clone(), saturating.clone()]);
    assert!(
        view.nodes
            .iter()
            .all(|n| n.provenance.semantic == SemanticState::Fresh),
        "{:?}",
        view.nodes
            .iter()
            .map(|n| (&n.name, &n.path, n.provenance.semantic))
            .collect::<Vec<_>>()
    );
    assert!(view.semantic.bindings.iter().any(|b| {
        b.boundary_reason == BoundaryReason::None && b.target_id.as_ref() == Some(&saturating)
    }));
    let view = response
        .query_view(&view_query(&caller, None))
        .unwrap()
        .unwrap();
    assert_eq!(
        view.semantic.producer_id.as_deref(),
        Some("scip-typescript")
    );
    assert_eq!(view.semantic.note, None);
    assert!(
        view.semantic
            .bindings
            .iter()
            .any(|b| b.target_id.as_ref() == Some(&helper))
    );
    // An explicit producer overrides, and one that does not cover the language says so.
    let view = response
        .query_view(&view_query(&caller, Some("rust-analyzer")))
        .unwrap()
        .unwrap();
    assert_eq!(view.semantic.producer_id.as_deref(), Some("rust-analyzer"));
    assert_eq!(
        view.semantic.note.as_deref(),
        Some(trellis::store::LANGUAGE_NOTE)
    );
    assert!(view.semantic.bindings.is_empty());
    let unpublished = response
        .query_view(&view_query(&caller, Some("not-published")))
        .unwrap_err();
    assert!(
        unpublished
            .downcast_ref::<trellis::store::ProducerNotPublished>()
            .is_some(),
        "{unpublished:#}"
    );

    // CLI symbols report each language's selected producer state.
    for name in ["double", "helper"] {
        let (_, symbols) = response.symbols_at(name, 10).unwrap();
        let symbol = symbols.iter().find(|s| s.name == name).unwrap();
        assert_eq!(symbol.provenance.semantic, SemanticState::Fresh, "{name}");
    }
    drop(response);

    // The web sequence: each seed's language uses its own producer.
    let rust_sequence = world.store.sequence_at(&run, pin, false).unwrap().unwrap();
    assert!(
        sequence_targets(&rust_sequence)
            .iter()
            .any(|(label, target)| label.contains("double") && target.as_ref() == Some(&double)),
        "{:?}",
        sequence_targets(&rust_sequence)
    );
    let js_sequence = world
        .store
        .sequence_at(&caller, pin, false)
        .unwrap()
        .unwrap();
    assert!(
        sequence_targets(&js_sequence)
            .iter()
            .any(|(label, target)| label.contains("helper") && target.as_ref() == Some(&helper)),
        "{:?}",
        sequence_targets(&js_sequence)
    );
    for view in [&rust_sequence, &js_sequence] {
        assert!(
            !view.warnings.iter().any(|w| w.contains("showing syntax")),
            "{:?}",
            view.warnings
        );
    }

    // MCP: the call graph, outgoing and incoming calls take each language's producer.
    let mut mcp = McpSession::start(&world, 483);
    for (root, target) in [(&run, &double), (&caller, &helper)] {
        let graph = mcp.call(
            "trellis_call_graph",
            json!({"schemaVersion":1,"symbolId":root,"depth":1}),
        );
        let edges = graph["data"]["result"]["edges"].as_array().unwrap();
        assert!(
            edges.iter().any(|e| e["to"] == json!(target)
                && e["boundaryReason"] == "none"
                && e["binding"]["resolution"] == "resolved"),
            "{graph}"
        );
        assert_eq!(warning(&graph, "syntaxOnly"), None, "{graph}");
        let outgoing = mcp.call(
            "trellis_outgoing_calls",
            json!({"schemaVersion":1,"symbolId":root}),
        );
        assert!(
            outgoing["data"]["calls"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["targetId"] == json!(target) && c["evidenceTier"] == "semantic"),
            "{outgoing}"
        );
        assert_eq!(warning(&outgoing, "syntaxOnly"), None, "{outgoing}");
    }
    let incoming = mcp.call(
        "trellis_incoming_calls",
        json!({"schemaVersion":1,"symbolId":double}),
    );
    assert!(
        incoming["data"]["callers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["targetId"] == json!(double)),
        "{incoming}"
    );
    // An explicit producer outside the root's language leaves its calls syntax only (D8).
    let graph = mcp.call(
        "trellis_call_graph",
        json!({"schemaVersion":1,"symbolId":caller,"depth":1,"semanticProducerId":"rust-analyzer"}),
    );
    assert!(
        graph["data"]["result"]["edges"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["binding"].is_null()),
        "{graph}"
    );
    mcp.finish();
    assert!(!world.store.recovery_pending_for_tests());
}

/// Two producers covering JavaScript leave only JavaScript syntax only, with the note naming
/// it; Rust keeps its sole producer. An explicit producer still selects one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_producers_for_one_language_fall_back_for_that_language_only() {
    use trellis::model::SemanticState;
    let world = mixed_world();
    publish_real_fixture(&world, JS_FIXTURE, &JS_FILES);
    // The synthetic producer covers every language; its artifact covers the Rust documents.
    let mut synthetic = world.fixture();
    let docs = synthetic.paths("rust");
    assert_eq!(docs.len(), 2);
    synthetic.artifact("rust", docs);
    let descriptor = world.write(&synthetic);
    let (pin, admitted, _) = world.publish(&descriptor).unwrap();
    drop(admitted);
    let producers = head_producers(&world);
    assert_eq!(producers.len(), 2);
    let synthetic_id = producers
        .iter()
        .find(|id| *id != "scip-typescript")
        .unwrap()
        .clone();
    let note = trellis::store::multiple_producers_note("javascript");
    let run = declaration_in(&world, "run", "src/lib.rs");
    let caller = declaration_in(&world, "caller", "src/main.js");
    let response = world.store.evidence_response().unwrap();

    let status = response.status().unwrap();
    let js = language_row(&status, "javascript");
    assert_eq!(js.semantic_producer_id, None);
    assert_eq!(js.semantic_note.as_deref(), Some(note.as_str()));
    assert_eq!(js.semantic_state, SemanticState::Unavailable);
    assert_eq!(
        (js.internal, js.external, js.ambiguous, js.unresolved),
        (0, 0, 0, js.calls)
    );
    let rust = language_row(&status, "rust");
    assert_eq!(
        rust.semantic_producer_id.as_deref(),
        Some(synthetic_id.as_str())
    );
    assert_eq!(rust.semantic_note, None);
    assert_ne!(rust.semantic_state, SemanticState::Unavailable);
    assert_eq!(
        status.semantic_producer_id.as_deref(),
        Some(synthetic_id.as_str())
    );
    assert_eq!(status.semantic_note.as_deref(), Some(note.as_str()));

    let view = response
        .query_view(&view_query(&caller, None))
        .unwrap()
        .unwrap();
    assert_eq!(view.semantic.producer_id, None);
    assert_eq!(view.semantic.note.as_deref(), Some(note.as_str()));
    assert!(view.semantic.bindings.is_empty());
    let view = response
        .query_view(&view_query(&run, None))
        .unwrap()
        .unwrap();
    assert_eq!(
        view.semantic.producer_id.as_deref(),
        Some(synthetic_id.as_str())
    );
    assert_eq!(view.semantic.note, None);
    // The explicit producer overrides the fallback.
    let view = response
        .query_view(&view_query(&caller, Some("scip-typescript")))
        .unwrap()
        .unwrap();
    assert_eq!(view.semantic.note, None);
    assert!(!view.semantic.bindings.is_empty());
    drop(response);

    let js_sequence = world
        .store
        .sequence_at(&caller, pin, false)
        .unwrap()
        .unwrap();
    assert!(
        js_sequence.warnings.contains(&note),
        "{:?}",
        js_sequence.warnings
    );
    assert!(
        sequence_targets(&js_sequence)
            .iter()
            .all(|(_, target)| target.is_none())
    );
    let rust_sequence = world.store.sequence_at(&run, pin, false).unwrap().unwrap();
    assert!(!rust_sequence.warnings.contains(&note));

    let mut mcp = McpSession::start(&world, 484);
    let syntax_only = json!({"code":"syntaxOnly","message":note,"provenanceId":null});
    let graph = mcp.call(
        "trellis_call_graph",
        json!({"schemaVersion":1,"symbolId":caller,"depth":1}),
    );
    assert_eq!(warning(&graph, "syntaxOnly"), Some(&syntax_only), "{graph}");
    assert!(
        graph["data"]["result"]["edges"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["binding"].is_null()),
        "{graph}"
    );
    let outgoing = mcp.call(
        "trellis_outgoing_calls",
        json!({"schemaVersion":1,"symbolId":caller}),
    );
    assert_eq!(
        warning(&outgoing, "syntaxOnly"),
        Some(&syntax_only),
        "{outgoing}"
    );
    let graph = mcp.call(
        "trellis_call_graph",
        json!({"schemaVersion":1,"symbolId":run,"depth":1}),
    );
    assert_eq!(warning(&graph, "syntaxOnly"), None, "{graph}");
    let graph = mcp.call(
        "trellis_call_graph",
        json!({"schemaVersion":1,"symbolId":caller,"depth":1,"semanticProducerId":"scip-typescript"}),
    );
    assert_eq!(warning(&graph, "syntaxOnly"), None, "{graph}");
    assert!(
        graph["data"]["result"]["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| !e["binding"].is_null()),
        "{graph}"
    );
    mcp.finish();
    assert!(!world.store.recovery_pending_for_tests());
}
