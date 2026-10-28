//! Test-support entry points: admission over in-memory bytes, and the whole-snapshot-read seam.
use super::{
    BatchCounters, BatchLimits,
    admit::{BatchInputs, NativeSource, Passes},
    descriptor::{ArtifactEntry, Descriptor, PageEntry},
    staging::Stage,
};
use crate::{
    capture::Capture,
    index_coordinator::CopiedInput,
    model::CancelFlag,
    native_evidence::{self as native, captured_revision_header},
    scip_admission::{admission, path},
    scip_raw::{
        self, CapturedClaim, CapturedScipFactV1, CapturedScipInventory, Exclusion, ScipCaptureError,
    },
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
    static WHOLE_SNAPSHOT_READS: Cell<u64> = const { Cell::new(0) };
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
    descriptor_bytes: &[u8],
    pages: &[&[u8]],
    artifacts: &[&[u8]],
    cancel: &CancelFlag,
) -> std::result::Result<Vec<CapturedScipInventory>, ScipCaptureError> {
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
        let mut statement = conn.prepare(
            "SELECT s.language, s.path, c.exclusion FROM coverage c JOIN sources s ON s.ord=c.source_ord \
             WHERE c.exclusion IS NOT NULL AND s.language=?1 ORDER BY s.ord",
        )?;
        let exclusions = statement
            .query_map([&artifact.language], |row| {
                Ok(Exclusion {
                    language: row.get(0)?,
                    relative_path: row.get(1)?,
                    reason: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
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
/// artifact paths are checked syntactically because no file exists to open.
pub fn admit_in_memory(
    root: &Path,
    native: &native::Artifact,
    sources: &BTreeMap<native::DocumentKey, Vec<u8>>,
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
        descriptor,
        pages,
        artifacts,
        cancel,
    )
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
    admit(
        root,
        &source_set,
        &revision,
        &sources,
        descriptor,
        pages,
        artifacts,
        cancel,
    )
}
