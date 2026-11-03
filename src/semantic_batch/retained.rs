//! Re-admits retained raw rows through the bounded stage and the original SCIP decoder.
use super::{
    AdmittedBatch, BatchCounters, BatchLimits,
    admit::{NativeSource, Passes, ReaderSource, StageOwner, check_cancel},
    descriptor::{Descriptor, RowOrder, SourceManifestHasher, SourceRow},
    limits::{IO_BUFFER_BYTES, STAGE_CHECK_COPIED_BYTES, STAGE_CHECK_ROWS},
    staging::{Stage, artifact_file},
};
use crate::{
    model::CancelFlag,
    scip_admission::admission,
    scip_raw::{self, ScipCaptureError},
    store::NativeReader,
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::Path,
};

/// The caller supplies the captured reader and raw rows from one coherent transaction.
#[allow(clippy::too_many_arguments)]
pub(crate) fn stage_retained_batch(
    db: &Connection,
    reader: &NativeReader<'_>,
    batch_id: i64,
    stage_parent: &Path,
    owner: StageOwner<'_>,
    limits: &BatchLimits,
    counters: &mut BatchCounters,
    cancel: &CancelFlag,
) -> Result<AdmittedBatch> {
    check_cancel(cancel)?;
    let descriptor_length: i64 = db.query_row(
        "SELECT length(descriptor_bytes) FROM semantic_raw_batches WHERE batch_id=?1",
        [batch_id],
        |r| r.get(0),
    )?;
    if descriptor_length as u64 > limits.descriptor_bytes {
        return Err(ScipCaptureError::LimitExceeded("descriptor bytes".into()).into());
    }
    let (bytes, stored_sha): (Vec<u8>, String) = db.query_row(
        "SELECT descriptor_bytes,descriptor_sha256 FROM semantic_raw_batches WHERE batch_id=?1",
        [batch_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    // The descriptor bytes are the original admitted serialization, not a reserialization.
    let sha = scip_raw::sha(&bytes);
    ensure!(sha == stored_sha, "retained descriptor digest");
    let descriptor = Descriptor::parse_admitted(&bytes, &sha, limits)?;
    let producer = descriptor.check_producer()?;
    let header = reader.header()?;
    descriptor.bind_header(
        &reader.reuse_project_roots(&descriptor)?,
        &header.source_set,
        &header.revision,
    )?;
    descriptor.check_pages(limits)?;
    descriptor.check_artifacts(limits)?;
    let metadata_ok: bool = db.query_row(
        "SELECT producer_id=?2 AND producer_version=?3 AND profile_id=?4 AND source_set_id=?5 AND captured_revision=?6 AND source_manifest_hash=?7 AND decoder_version=?8 FROM semantic_raw_batches WHERE batch_id=?1",
        params![batch_id, producer.name, producer.version, descriptor.profile_id,
            descriptor.source_set.id, descriptor.source_revision,
            descriptor.source_manifest_hash, descriptor.decoder_version],
        |r| r.get(0),
    )?;
    ensure!(metadata_ok, "retained batch metadata");
    let native_config = reader.retained_components()?.1;
    let module_roots = scip_raw::ModuleRoots::from_native_config(&native_config);
    let mut stage = Stage::create(stage_parent, owner.request_id, owner.incarnation)?;
    let passes = Passes {
        descriptor: &descriptor,
        producer: &producer,
        limits,
        cancel,
        budget: stage.budget(limits),
        module_roots: &module_roots,
        native_config: &native_config,
    };
    passes.begin(stage.conn()?, &bytes, &sha)?;
    let mut native = ReaderSource {
        reader,
        stream: Some(reader.inventory()?),
    };
    let cargo = sources(stage.conn()?, &passes, &mut native)?;
    coverage(db, batch_id, stage.conn()?, &passes)?;
    native.stream = None;
    for entry in &descriptor.artifacts {
        check_cancel(cancel)?;
        let capture_id: Option<i64> = db.query_row(
            "SELECT capture_id FROM semantic_raw_captures WHERE batch_id=?1 AND ordinal=?2 AND language=?3 AND producer_id=?4 AND artifact_hash=?5 AND source_set_id=?6 AND captured_revision=?7 AND decoder_version=?8 AND length(binary_bytes)=?9",
            params![batch_id, entry.ordinal, entry.language, producer.name, entry.sha256,
                descriptor.source_set.id, descriptor.source_revision, descriptor.decoder_version,
                entry.byte_length as i64],
            |r| r.get(0),
        ).optional()?;
        let capture_id = capture_id.ok_or_else(|| admission("retained artifact metadata"))?;
        let dest = artifact_file(stage.dir(), entry.ordinal);
        copy_artifact(
            db,
            capture_id,
            &dest,
            entry.byte_length,
            &entry.sha256,
            &passes,
        )?;
        #[cfg(feature = "test-support")]
        crate::semantic_import::note_original_raw_decode();
        passes.decode(entry, &dest, stage.conn()?, &mut native, &cargo, counters)?;
        compare_capture(db, capture_id, entry.ordinal, stage.conn()?, &passes)?;
    }
    let captures: i64 = db.query_row(
        "SELECT COUNT(*) FROM semantic_raw_captures WHERE batch_id=?1",
        [batch_id],
        |r| r.get(0),
    )?;
    ensure!(
        captures as usize == descriptor.artifacts.len(),
        "retained artifact inventory"
    );
    let unseen: i64 = stage.conn()?.query_row(
        "SELECT COUNT(*) FROM coverage c WHERE c.artifact IS NOT NULL AND NOT EXISTS (SELECT 1 FROM documents d WHERE d.source_ord=c.source_ord)",
        [], |r| r.get(0),
    )?;
    ensure!(unseen == 0, "retained missing artifact document");
    Ok(AdmittedBatch::new(
        stage,
        descriptor,
        sha,
        producer,
        reader.pin(),
    ))
}

fn sources(
    conn: &Connection,
    passes: &Passes<'_>,
    native: &mut dyn NativeSource,
) -> Result<scip_raw::CargoTargets> {
    let descriptor = passes.descriptor;
    let mut cargo = scip_raw::CargoTargetsBuilder::from_native_config(passes.native_config);
    let mut hasher = SourceManifestHasher::new(&descriptor.source_set.id);
    let mut order = RowOrder::source();
    let mut ord = 0i64;
    conn.execute_batch("BEGIN")?;
    while let Some(document) = native.next_inventory()? {
        check_cancel(passes.cancel)?;
        if ord as u64 >= passes.limits.source_documents {
            return Err(ScipCaptureError::LimitExceeded("source documents".into()).into());
        }
        SourceRow {
            language: document.key.language.clone(),
            relative_path: document.key.path.clone(),
            content_hash: document.content_hash.clone(),
            byte_length: document.byte_length as u64,
        }
        .check(&descriptor.source_set.languages)?;
        order.push(&document.key.language, &document.key.path)?;
        if document.key.source_set_id != descriptor.source_set.id
            || document.revision_id != descriptor.source_revision
        {
            return Err(admission("captured source mismatch").into());
        }
        passes.stage_source(conn, native, &document, ord, &mut cargo, &mut hasher)?;
        ord += 1;
        if (ord as u64).is_multiple_of(STAGE_CHECK_ROWS) {
            conn.execute_batch("COMMIT; BEGIN")?;
            passes.budget.check()?;
        }
    }
    conn.execute_batch("COMMIT")?;
    passes.budget.check()?;
    ensure!(
        ord as u64 == descriptor.totals.source_documents,
        "retained source total"
    );
    if hasher.finish() != descriptor.source_manifest_hash {
        return Err(admission("source manifest hash").into());
    }
    Ok(cargo.finish())
}

fn coverage(db: &Connection, batch_id: i64, conn: &Connection, passes: &Passes<'_>) -> Result<()> {
    let mut statement =
        conn.prepare("SELECT ord,language,path,content_hash FROM sources ORDER BY ord")?;
    let mut rows = statement.query([])?;
    let (mut covered, mut excluded) = (0u64, 0u64);
    conn.execute_batch("BEGIN")?;
    while let Some(row) = rows.next()? {
        check_cancel(passes.cancel)?;
        let ord: i64 = row.get(0)?;
        let language: String = row.get(1)?;
        if !passes.producer.languages.contains(&language) {
            continue;
        }
        let path: String = row.get(2)?;
        let hash: String = row.get(3)?;
        let exclusion: Option<String> = db.query_row(
            "SELECT reason FROM semantic_raw_exclusions WHERE batch_id=?1 AND language=?2 AND path=?3",
            params![batch_id, language, path], |r| r.get(0),
        ).optional()?;
        let mut assignments = db.prepare_cached(
            "SELECT c.ordinal,d.source_hash FROM semantic_raw_documents d JOIN semantic_raw_captures c ON c.capture_id=d.capture_id WHERE c.batch_id=?1 AND d.language=?2 AND d.path=?3",
        )?;
        let mut assigned = assignments.query(params![batch_id, language, path])?;
        let artifact = if let Some(assignment) = assigned.next()? {
            let ordinal: u32 = assignment.get(0)?;
            let source_hash: String = assignment.get(1)?;
            ensure!(
                exclusion.is_none()
                    && source_hash == hash
                    && passes
                        .descriptor
                        .artifacts
                        .get(ordinal as usize)
                        .is_some_and(|a| a.language == language),
                "retained document assignment"
            );
            ensure!(
                assigned.next()?.is_none(),
                "retained duplicate document assignment"
            );
            covered += 1;
            Some(ordinal)
        } else if let Some(reason) = exclusion.as_deref() {
            ensure!(
                matches!(reason, "notRequested" | "unsupported" | "failed"),
                "retained coverage completion"
            );
            excluded += 1;
            None
        } else {
            // Document rows accelerate reuse; the authenticated binary and its proofs
            // can recover a missing row without inventing an artifact assignment.
            let ordinal = missing_document_artifact(db, batch_id, &language, &path, passes)?;
            covered += 1;
            Some(ordinal)
        };
        conn.prepare_cached(
            "INSERT INTO coverage(source_ord,artifact,exclusion) VALUES(?1,?2,?3)",
        )?
        .execute(params![ord, artifact, exclusion])?;
        if (covered + excluded) % STAGE_CHECK_ROWS == 0 {
            passes.budget.check()?;
        }
    }
    conn.execute_batch("COMMIT")?;
    passes.budget.check()?;
    ensure!(
        covered == passes.descriptor.totals.covered_documents
            && excluded == passes.descriptor.totals.excluded_documents,
        "retained coverage totals"
    );
    let retained_exclusions: i64 = db.query_row(
        "SELECT COUNT(*) FROM semantic_raw_exclusions WHERE batch_id=?1",
        [batch_id],
        |r| r.get(0),
    )?;
    ensure!(
        retained_exclusions as u64 == excluded,
        "retained exclusion inventory"
    );
    Ok(())
}

fn missing_document_artifact(
    db: &Connection,
    batch_id: i64,
    language: &str,
    path: &str,
    passes: &Passes<'_>,
) -> Result<u32> {
    let mut statement = db.prepare_cached(
        "SELECT DISTINCT c.ordinal FROM semantic_raw_proofs p JOIN semantic_raw_captures c ON c.capture_id=p.capture_id WHERE c.batch_id=?1 AND p.language=?2 AND p.path=?3 LIMIT 2",
    )?;
    let assignments = statement
        .query_map(params![batch_id, language, path], |row| {
            row.get::<_, u32>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let ordinal = match assignments.as_slice() {
        [ordinal] => *ordinal,
        [] => {
            let mut artifacts = passes
                .descriptor
                .artifacts
                .iter()
                .filter(|artifact| artifact.language == language);
            let artifact = artifacts
                .next()
                .ok_or_else(|| admission("retained document assignment"))?;
            ensure!(
                artifacts.next().is_none(),
                "retained ambiguous document assignment"
            );
            artifact.ordinal
        }
        _ => return Err(admission("retained ambiguous document assignment").into()),
    };
    ensure!(
        passes
            .descriptor
            .artifacts
            .get(ordinal as usize)
            .is_some_and(|artifact| artifact.language == language),
        "retained document assignment"
    );
    Ok(ordinal)
}

fn copy_artifact(
    db: &Connection,
    capture_id: i64,
    dest: &Path,
    length: u64,
    expected: &str,
    passes: &Passes<'_>,
) -> Result<()> {
    let mut blob = db.blob_open(
        rusqlite::MAIN_DB,
        "semantic_raw_captures",
        "binary_bytes",
        capture_id,
        true,
    )?;
    let mut output = std::fs::File::create_new(dest)?;
    let mut buffer = vec![0u8; IO_BUFFER_BYTES as usize];
    let mut hasher = Sha256::new();
    let (mut copied, mut checked) = (0u64, 0u64);
    loop {
        check_cancel(passes.cancel)?;
        let read = blob.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
        copied += read as u64;
        if copied - checked >= STAGE_CHECK_COPIED_BYTES {
            passes.budget.check()?;
            checked = copied;
        }
    }
    passes.budget.check()?;
    ensure!(
        copied == length && hex::encode(hasher.finalize()) == expected,
        "retained artifact digest"
    );
    Ok(())
}

/// Keyed proof probes hold one canonical envelope at a time; totals detect extra retained rows.
fn compare_capture(
    db: &Connection,
    capture_id: i64,
    artifact: u32,
    conn: &Connection,
    passes: &Passes<'_>,
) -> Result<()> {
    let mut claims = conn.prepare(
        "SELECT c.proof_id,c.record_kind,c.fact_digest,s.language,s.path,c.canonical_bytes FROM claims c JOIN sources s ON s.ord=c.source_ord WHERE c.artifact=?1",
    )?;
    let mut rows = claims.query([artifact])?;
    let mut proofs = db.prepare_cached(
        "SELECT record_kind=?3 AND fact_digest=?4 AND language=?5 AND path=?6 AND canonical_bytes=?7 FROM semantic_raw_proofs WHERE capture_id=?1 AND proof_id=?2",
    )?;
    let mut count = 0i64;
    while let Some(row) = rows.next()? {
        check_cancel(passes.cancel)?;
        let proof: String = row.get(0)?;
        let kind: String = row.get(1)?;
        let digest: String = row.get(2)?;
        let language: String = row.get(3)?;
        let path: String = row.get(4)?;
        let bytes: Vec<u8> = row.get(5)?;
        let same: bool = proofs
            .query_row(
                params![capture_id, proof, kind, digest, language, path, bytes],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);
        ensure!(same, "retained proof differs from authenticated binary");
        count += 1;
    }
    let retained: i64 = db.query_row(
        "SELECT COUNT(*) FROM semantic_raw_proofs WHERE capture_id=?1",
        [capture_id],
        |r| r.get(0),
    )?;
    ensure!(count == retained, "retained proof inventory");
    let mut documents = conn.prepare(
        "SELECT s.language,s.path,s.content_hash,d.wire_digest,d.capped FROM documents d JOIN sources s ON s.ord=d.source_ord WHERE d.artifact=?1",
    )?;
    let mut rows = documents.query([artifact])?;
    let mut retained_documents = db.prepare_cached(
        "SELECT source_hash=?4 AND wire_digest=?5 AND capped=?6 FROM semantic_raw_documents WHERE capture_id=?1 AND language=?2 AND path=?3",
    )?;
    let mut count = 0i64;
    while let Some(row) = rows.next()? {
        check_cancel(passes.cancel)?;
        let language: String = row.get(0)?;
        let path: String = row.get(1)?;
        let hash: String = row.get(2)?;
        let wire: String = row.get(3)?;
        let capped: i64 = row.get(4)?;
        let same: Option<bool> = retained_documents
            .query_row(
                params![capture_id, language, path, hash, wire, capped],
                |r| r.get(0),
            )
            .optional()?;
        ensure!(
            same.unwrap_or(true),
            "retained document differs from authenticated binary"
        );
        count += i64::from(same.is_some());
    }
    let retained: i64 = db.query_row(
        "SELECT COUNT(*) FROM semantic_raw_documents WHERE capture_id=?1",
        [capture_id],
        |r| r.get(0),
    )?;
    ensure!(count == retained, "retained document inventory");
    Ok(())
}
