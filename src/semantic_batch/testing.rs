//! Test-support entry points: admission over in-memory bytes, and the whole-snapshot-read seam.
use super::{
    AdmittedBatch, BatchCounters, BatchLimits, JoinedBatch, LookupBudget,
    admit::{BatchInputs, NativeSource, Passes, StageOwner, stage_batch},
    descriptor::{
        ArtifactEntry, Descriptor, PageEntry, SourceManifestHasher, basis_artifact_ordinal,
    },
    staging::Stage,
};
use crate::{
    capture::Capture,
    index_coordinator::CopiedInput,
    model::CancelFlag,
    native_evidence::{self as native, captured_revision_header},
    scip_admission::{admission, file_uri, order, path, pinned_producer_by_id},
    scip_raw::{
        self, CapturedClaim, CapturedScipFactV1, CapturedScipInventory, Exclusion, ScipCaptureError,
    },
    semantic_evidence as semantic,
    semantic_import::{DestinationMutations, join_staged},
    store::Store,
};
use anyhow::{Result, anyhow};
use rusqlite::Connection;
use std::{
    cell::Cell,
    collections::BTreeMap,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

thread_local! {
    static NATIVE_DOCUMENT_LOADS: Cell<u64> = const { Cell::new(0) };
}

pub fn reset_native_document_loads() {
    NATIVE_DOCUMENT_LOADS.with(|loads| loads.set(0));
}

pub fn native_document_loads() -> u64 {
    NATIVE_DOCUMENT_LOADS.with(Cell::get)
}

pub(crate) fn note_native_document_load() {
    NATIVE_DOCUMENT_LOADS.with(|loads| loads.set(loads.get() + 1));
}

type DocumentCommonHook =
    Box<dyn FnMut(u8, &native::DocumentKey, &crate::semantic_import::JoinedFacts, &Connection)>;

thread_local! {
    static DOCUMENT_COMMON: std::cell::RefCell<Option<DocumentCommonHook>> = const { std::cell::RefCell::new(None) };
}

/// Observes document-local Common results before legacy settlement can revise their targets.
pub fn observe_document_common(
    hook: impl FnMut(u8, &native::DocumentKey, &crate::semantic_import::JoinedFacts, &Connection)
    + 'static,
) {
    DOCUMENT_COMMON.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

pub fn clear_document_common_observer() {
    DOCUMENT_COMMON.with(|slot| *slot.borrow_mut() = None);
}

pub(crate) fn run_document_common_observer(
    side: u8,
    document: &native::DocumentKey,
    facts: &crate::semantic_import::JoinedFacts,
    conn: &Connection,
) {
    DOCUMENT_COMMON.with(|slot| {
        if let Some(hook) = slot.borrow_mut().as_mut() {
            hook(side, document, facts, conn);
        }
    });
}

type ClosureHook = Box<dyn FnOnce(&Connection)>;

type RowHook = Box<dyn FnMut(&str, &mut serde_json::Value)>;

thread_local! {
    static BEFORE_ROW: std::cell::RefCell<Option<RowHook>> = const { std::cell::RefCell::new(None) };
    static WHOLE_SNAPSHOT_READS: Cell<u64> = const { Cell::new(0) };
    static BEFORE_CLOSURE: std::cell::RefCell<Option<ClosureHook>> = const { std::cell::RefCell::new(None) };
    static BEFORE_COVERAGE: std::cell::RefCell<Option<ClosureHook>> = const { std::cell::RefCell::new(None) };
}

/// Runs `hook` once on this thread, with the stage connection, after the next in-memory
/// admission has staged its sources and before it stages coverage.
pub fn before_coverage_pass(hook: impl FnOnce(&Connection) + 'static) {
    BEFORE_COVERAGE.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

/// Runs `hook` once on this thread, with the stage connection, after the next staged join has
/// emitted its rows and before its closure checks.
pub fn before_join_closure(hook: impl FnOnce(&Connection) + 'static) {
    BEFORE_CLOSURE.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

/// Runs `hook` on every row of the next staged join just before its row-local validation,
/// with the row kind (`symbol`, `declarationBinding`, `reference`, `callBinding`,
/// `typeRelationship`) and the row as JSON. The hook stays until `clear_row_hook`.
pub fn before_row_validation(hook: impl FnMut(&str, &mut serde_json::Value) + 'static) {
    BEFORE_ROW.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

pub fn clear_row_hook() {
    BEFORE_ROW.with(|slot| *slot.borrow_mut() = None);
}

pub(crate) fn edit_row<T: serde::Serialize + serde::de::DeserializeOwned>(
    kind: &str,
    row: &mut T,
) -> Result<()> {
    BEFORE_ROW.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(hook) = slot.as_mut() else {
            return Ok(());
        };
        let mut value = serde_json::to_value(&*row)?;
        hook(kind, &mut value);
        *row = serde_json::from_value(value)?;
        Ok(())
    })
}

pub(crate) fn run_before_closure_hook(conn: &Connection) {
    if let Some(hook) = BEFORE_CLOSURE.with(|slot| slot.borrow_mut().take()) {
        hook(conn);
    }
}

/// Called by `retained_native_snapshot_from_db`: a whole-revision read happened.
pub fn note_whole_snapshot_read() {
    WHOLE_SNAPSHOT_READS.with(|reads| reads.set(reads.get() + 1));
}

/// Whole-revision snapshot reads on this thread since `stage_batch` last started.
pub fn whole_snapshot_reads() -> u64 {
    WHOLE_SNAPSHOT_READS.with(Cell::get)
}

pub(crate) fn reset_whole_snapshot_reads() {
    WHOLE_SNAPSHOT_READS.with(|reads| reads.set(0));
}

struct MemoryInputs<'a> {
    pages: &'a [&'a [u8]],
    next_page: usize,
    artifacts: &'a [&'a [u8]],
}

impl BatchInputs for MemoryInputs<'_> {
    fn page(&mut self, entry: &PageEntry) -> Result<Vec<u8>> {
        let bytes = self
            .pages
            .get(self.next_page)
            .ok_or_else(|| anyhow!("missing in-memory page"))?;
        self.next_page += 1;
        if bytes.len() as u64 != entry.byte_length {
            return Err(admission("page length").into());
        }
        Ok(bytes.to_vec())
    }

    fn copy_artifact(
        &mut self,
        entry: &ArtifactEntry,
        dest: &Path,
        limits: &BatchLimits,
        progress: &mut dyn FnMut(u64) -> Result<()>,
    ) -> Result<CopiedInput> {
        let bytes = self
            .artifacts
            .get(entry.ordinal as usize)
            .ok_or_else(|| anyhow!("missing in-memory artifact"))?;
        crate::index_coordinator::copy_stream_to_stage(
            *bytes,
            dest,
            limits.artifact_bytes,
            1 << 20,
            progress,
        )
    }
}

struct MemoryNative<'a> {
    documents: std::vec::IntoIter<native::Document>,
    sources: &'a BTreeMap<native::DocumentKey, Vec<u8>>,
}

impl NativeSource for MemoryNative<'_> {
    fn next_inventory(&mut self) -> Result<Option<native::Document>> {
        Ok(self.documents.next())
    }

    fn source_bytes(&mut self, document: &native::Document) -> Result<Vec<u8>> {
        self.sources
            .get(&document.key)
            .cloned()
            .ok_or_else(|| anyhow!("missing captured source"))
    }
}

static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);

#[allow(clippy::too_many_arguments)]
fn admit(
    root: &Path,
    source_set: &native::SourceSet,
    revision: &native::Revision,
    sources: &BTreeMap<native::DocumentKey, Vec<u8>>,
    native_config: &[serde_json::Value],
    descriptor_bytes: &[u8],
    pages: &[&[u8]],
    artifacts: &[&[u8]],
    cancel: &CancelFlag,
) -> std::result::Result<Vec<CapturedScipInventory>, ScipCaptureError> {
    let module_roots = scip_raw::ModuleRoots::from_native_config(native_config);
    let outcome = (|| -> Result<Vec<CapturedScipInventory>> {
        let limits = BatchLimits::PRODUCTION;
        let sha = scip_raw::sha(descriptor_bytes);
        let descriptor = Descriptor::parse_admitted(descriptor_bytes, &sha, &limits)?;
        let producer = descriptor.check_producer()?;
        descriptor.bind_header(root, source_set, revision)?;
        descriptor.check_pages(&limits)?;
        descriptor.check_artifacts(&limits)?;
        for relative in descriptor
            .pages
            .iter()
            .map(|page| &page.relative_path)
            .chain(descriptor.artifacts.iter().map(|a| &a.relative_path))
        {
            if !path(relative) {
                return Err(admission("invalid role path").into());
            }
        }
        let id = NEXT_STAGE.fetch_add(1, Ordering::Relaxed);
        let mut stage = Stage::create(
            &std::env::temp_dir(),
            &format!("mem{}", std::process::id()),
            &id.to_string(),
        )?;
        let passes = Passes {
            descriptor: &descriptor,
            producer: &producer,
            limits: &limits,
            cancel,
            budget: stage.budget(&limits),
            module_roots: &module_roots,
        };
        let mut inputs = MemoryInputs {
            pages,
            next_page: 0,
            artifacts,
        };
        let mut native = MemoryNative {
            documents: revision.documents.clone().into_iter(),
            sources,
        };
        let mut counters = BatchCounters::default();
        let dir = stage.dir().to_owned();
        let conn = stage.conn()?;
        passes.begin(conn, descriptor_bytes, &sha)?;
        passes.source_pass(conn, &mut inputs, &mut native, &mut counters)?;
        if let Some(hook) = BEFORE_COVERAGE.with(|slot| slot.borrow_mut().take()) {
            hook(conn);
        }
        passes.coverage_pass(conn, &mut inputs, &mut counters)?;
        passes.artifact_pass(&dir, conn, &mut inputs, &mut native, &mut counters)?;
        read_inventories(conn, &descriptor)
    })();
    outcome.map_err(|error| match error.downcast::<ScipCaptureError>() {
        Ok(typed) => typed,
        Err(other) => ScipCaptureError::InvalidArtifact(format!("{other:#}")),
    })
}

fn read_inventories(
    conn: &Connection,
    descriptor: &Descriptor,
) -> Result<Vec<CapturedScipInventory>> {
    let mut inventories = vec![];
    let mut statement = conn.prepare(
        "SELECT s.language, s.path, c.exclusion FROM coverage c JOIN sources s ON s.ord=c.source_ord \
         WHERE c.exclusion IS NOT NULL ORDER BY s.ord",
    )?;
    let exclusions = statement
        .query_map([], |row| {
            let exclusion = Exclusion {
                language: row.get(0)?,
                relative_path: row.get(1)?,
                reason: row.get(2)?,
            };
            Ok((
                basis_artifact_ordinal(&descriptor.artifacts, &exclusion.language, None),
                exclusion,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    for artifact in &descriptor.artifacts {
        let mut statement = conn.prepare(
            "SELECT canonical_bytes, fact_digest, proof_id FROM claims WHERE artifact=?1 \
             ORDER BY source_ord, canonical_bytes",
        )?;
        let claims = statement
            .query_map([artifact.ordinal], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut captured = vec![];
        for (canonical_bytes, fact_digest, proof) in claims {
            let value: serde_json::Value = serde_json::from_slice(&canonical_bytes)?;
            let text = |name: &str| value[name].as_str().map(str::to_owned);
            captured.push(CapturedClaim {
                envelope: CapturedScipFactV1 {
                    format_version: 1,
                    record_kind: text("recordKind").ok_or_else(|| anyhow!("staged claim"))?,
                    relative_path: text("relativePath").ok_or_else(|| anyhow!("staged claim"))?,
                    position_encoding: u8::try_from(
                        value["positionEncoding"].as_u64().unwrap_or(0),
                    )?,
                    raw: value["raw"].clone(),
                },
                canonical_bytes,
                fact_digest,
                captured_proof_id: proof,
                language: artifact.language.clone(),
                artifact_hash: artifact.sha256.clone(),
            });
        }
        let exclusions = exclusions
            .iter()
            .filter(|(assigned, _)| *assigned == artifact.ordinal)
            .map(|(_, exclusion)| exclusion.clone())
            .collect();
        inventories.push(CapturedScipInventory {
            artifact_hashes: vec![artifact.sha256.clone()],
            decoder_version: descriptor.decoder_version.clone(),
            source_manifest_hash: descriptor.source_manifest_hash.clone(),
            completion: descriptor.completion,
            exclusions,
            claims: captured,
        });
    }
    Ok(inventories)
}

/// Runs the full admission over in-memory bytes: one inventory per artifact, in ordinal order.
/// `pages` follow the descriptor's page order; `artifacts` follow artifact ordinals. Page and
/// artifact paths are checked syntactically because no file exists to open. `native_config`
/// is the captured revision's retained native config, the source of package roots.
#[allow(clippy::too_many_arguments)]
pub fn admit_in_memory(
    root: &Path,
    native: &native::Artifact,
    sources: &BTreeMap<native::DocumentKey, Vec<u8>>,
    native_config: &[serde_json::Value],
    descriptor: &[u8],
    pages: &[&[u8]],
    artifacts: &[&[u8]],
    cancel: &CancelFlag,
) -> std::result::Result<Vec<CapturedScipInventory>, ScipCaptureError> {
    admit(
        root,
        &native.source_set,
        &native.revision,
        sources,
        native_config,
        descriptor,
        pages,
        artifacts,
        cancel,
    )
}

/// The retained native config a revision captured from `capture` would hold.
pub fn captured_native_config(capture: &Capture) -> Result<Vec<serde_json::Value>> {
    Ok(crate::native_evidence::captured_components(capture, "")?.1)
}

/// `admit_in_memory` over a fresh `Capture` of the workspace.
pub fn admit_in_memory_from_capture(
    capture: &Capture,
    descriptor: &[u8],
    pages: &[&[u8]],
    artifacts: &[&[u8]],
    cancel: &CancelFlag,
) -> std::result::Result<Vec<CapturedScipInventory>, ScipCaptureError> {
    let root = capture.physical_root();
    let identity = crate::store::topology::WorkspaceIdentity::discover(Some(root), root)
        .map_err(|_| admission("workspace identity"))?;
    let (source_set, revision) = captured_revision_header(capture, root, &identity.record_id)
        .map_err(|_| admission("native captured header"))?;
    let sources = capture
        .files
        .iter()
        .map(|file| {
            (
                native::DocumentKey {
                    source_set_id: revision.source_set_id.clone(),
                    language: file.language.clone(),
                    path: file.path.clone(),
                },
                file.text.as_bytes().to_vec(),
            )
        })
        .collect();
    let native_config =
        captured_native_config(capture).map_err(|_| admission("native captured config"))?;
    admit(
        root,
        &source_set,
        &revision,
        &sources,
        &native_config,
        descriptor,
        pages,
        artifacts,
        cancel,
    )
}

/// Builds the descriptor and page bytes of a batch with one artifact that covers every source
/// of `language` in `capture`. Sources of the producer's other languages are excluded as
/// `notRequested`. The artifact is named `artifact.scip` and the pages `source-00000.json` and
/// `coverage-00000.json`; pass the returned pages and the artifact to `admit_in_memory_from_capture`.
pub fn single_artifact_batch(
    capture: &Capture,
    producer: &str,
    language: &str,
    artifact: &[u8],
) -> Result<(Vec<u8>, Vec<Vec<u8>>)> {
    let root = capture.physical_root();
    let identity = crate::store::topology::WorkspaceIdentity::discover(Some(root), root)?;
    let (source_set, revision) = captured_revision_header(capture, root, &identity.record_id)?;
    let pinned = pinned_producer_by_id(producer).ok_or_else(|| anyhow!("unpinned producer"))?;
    let mut files: Vec<_> = capture.files.iter().collect();
    files.sort_by(|a, b| {
        (order(&a.language), a.path.as_bytes()).cmp(&(order(&b.language), b.path.as_bytes()))
    });
    let mut hasher = SourceManifestHasher::new(&source_set.id);
    let mut source_rows = vec![];
    let mut coverage_rows = vec![];
    let mut excluded = 0u64;
    for file in &files {
        hasher.push(&file.language, &file.path, &file.hash);
        source_rows.push(serde_json::json!({
            "byteLength": file.text.len(), "contentHash": file.hash,
            "language": file.language, "relativePath": file.path,
        }));
        if !pinned.languages.contains(&file.language) {
            continue;
        }
        coverage_rows.push(if file.language == language {
            serde_json::json!({"artifact": 0, "exclusion": null,
                "language": file.language, "relativePath": file.path})
        } else {
            excluded += 1;
            serde_json::json!({"artifact": null, "exclusion": "notRequested",
                "language": file.language, "relativePath": file.path})
        });
    }
    let mut pages = vec![];
    let mut entries = vec![];
    for (kind, rows) in [("source", &source_rows), ("coverage", &coverage_rows)] {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "formatVersion": 1, "kind": kind, "ordinal": 0, "rows": rows,
        }))?;
        entries.push(serde_json::json!({
            "kind": kind, "ordinal": 0, "relativePath": format!("{kind}-00000.json"),
            "byteLength": bytes.len(), "sha256": scip_raw::sha(&bytes), "rowCount": rows.len(),
        }));
        pages.push(bytes);
    }
    let page_bytes: usize = pages.iter().map(Vec::len).sum();
    let descriptor = serde_json::json!({
        "formatVersion": 1,
        "trustOrigin": pinned.trust_origin,
        "profileId": pinned.profile_id,
        "decoderVersion": scip_raw::DECODER_VERSION,
        "producer": {
            "name": pinned.name, "version": pinned.version,
            "identityKind": pinned.identity_kind.as_str(),
            "executableHash": pinned.executable_hash,
            "positionEncoding": pinned.position_encoding, "languages": pinned.languages,
        },
        "sourceSet": {
            "id": source_set.id, "rootId": source_set.root_id,
            "languages": source_set.languages, "dependencies": source_set.dependencies,
        },
        "sourceRevision": revision.id,
        "sourceManifestHash": hasher.finish(),
        "components": {
            "toolchainHash": revision.toolchain_hash, "configHash": revision.config_hash,
            "dependencyHash": revision.dependency_hash,
        },
        "metadata": {"protocolVersion": 0, "projectRoot": file_uri(root)?, "textDocumentEncoding": 1},
        "completion": if excluded == 0 { "complete" } else { "partial" },
        "totals": {
            "sourceDocuments": source_rows.len(),
            "coveredDocuments": coverage_rows.len() as u64 - excluded,
            "excludedDocuments": excluded,
            "pageBytes": page_bytes, "artifactBytes": artifact.len(),
        },
        "pages": entries,
        "artifacts": [{
            "ordinal": 0, "relativePath": "artifact.scip", "byteLength": artifact.len(),
            "sha256": scip_raw::sha(artifact), "language": language,
        }],
    });
    Ok((serde_json::to_vec(&descriptor)?, pages))
}

static NEXT_JOIN: AtomicU64 = AtomicU64::new(0);

/// Stages the batch named by `descriptor_path` and joins it against the store's current head,
/// in the calling thread. The stage is removed when the result is dropped.
pub fn stage_and_join(
    store: &Store,
    root: &Path,
    descriptor_path: &Path,
    descriptor_sha256: &str,
    limits: &BatchLimits,
    cancel: &CancelFlag,
) -> Result<(JoinedBatch, BatchCounters)> {
    let (_, joined, counters) = stage_and_join_admitted(
        store,
        root,
        descriptor_path,
        descriptor_sha256,
        limits,
        DestinationMutations::default(),
        cancel,
    )?;
    Ok((joined, counters))
}

/// `stage_and_join` that keeps the stage alive and applies destination edits.
pub fn stage_and_join_admitted(
    store: &Store,
    root: &Path,
    descriptor_path: &Path,
    descriptor_sha256: &str,
    limits: &BatchLimits,
    mutations: DestinationMutations,
    cancel: &CancelFlag,
) -> Result<(AdmittedBatch, JoinedBatch, BatchCounters)> {
    let mut counters = BatchCounters::default();
    let request = format!("join-{}", NEXT_JOIN.fetch_add(1, Ordering::Relaxed));
    let mut admitted = stage_batch(
        store,
        root,
        StageOwner {
            request_id: &request,
            incarnation: "claim-1",
        },
        descriptor_path,
        descriptor_sha256,
        limits,
        &mut counters,
        cancel,
    )?;
    let mut budget = LookupBudget::new(limits);
    let joined = join_staged(
        &mut admitted,
        store,
        store.index_baseline()?,
        limits,
        &mut budget,
        &mut counters,
        mutations,
        cancel,
    )?;
    Ok((admitted, joined, counters))
}

/// The staged `out_*` rows, read back as the publication the in-memory join would build.
pub fn staged_publication(batch: &mut AdmittedBatch) -> Result<semantic::SemanticPublication> {
    let producer = batch.producer().producer();
    let conn = batch.stage_mut().conn()?;
    fn rows<T: serde::de::DeserializeOwned>(conn: &Connection, table: &str) -> Result<Vec<T>> {
        let mut statement = conn.prepare(&format!("SELECT body FROM {table} ORDER BY ord"))?;
        let bodies = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        bodies
            .iter()
            .map(|body| Ok(serde_json::from_str(body)?))
            .collect()
    }
    Ok(semantic::SemanticPublication {
        producers: vec![producer],
        coverage: rows(conn, "out_coverage")?,
        provenance: rows(conn, "out_provenance")?,
        symbols: rows(conn, "out_symbols")?,
        declaration_bindings: rows(conn, "out_declaration_bindings")?,
        references: rows(conn, "out_references")?,
        call_bindings: rows(conn, "out_call_bindings")?,
        type_relationships: rows(conn, "out_type_relationships")?,
    })
}

/// The ordered cursor queries of the staged join, for query-plan tests.
pub fn join_cursor_sql() -> Vec<String> {
    crate::semantic_import::staged_cursor_sql()
}
