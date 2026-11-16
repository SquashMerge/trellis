//! The retained raw batch of one pending selection, as typed rows (S14b). Retained admission
//! reads `semantic_raw_*` only through this handle: storage builds it on a snapshot it
//! selected, it borrows that snapshot's connection and never hands it out, and each method is
//! one statement (or one incremental blob read), prepared and cached on that connection.
use super::copy::CopyFilter;
use super::*;
use crate::native_records::DocumentKey;
use std::io::{Read, Seek, SeekFrom};

/// The rows of one selected retained batch in one snapshot of the main index.
pub(crate) struct RetainedBatchRows<'a> {
    db: &'a Connection,
    batch_id: i64,
    captured_pin: IndexPin,
}

/// The batch metadata a parsed descriptor must agree with.
pub(crate) struct BatchMetadata<'r> {
    pub producer_id: &'r str,
    pub producer_version: &'r str,
    pub profile_id: &'r str,
    pub source_set_id: &'r str,
    pub captured_revision: &'r str,
    pub source_manifest_hash: &'r str,
    pub decoder_version: &'r str,
}

/// The capture metadata a descriptor's artifact entry must agree with.
pub(crate) struct CaptureKey<'r> {
    pub ordinal: u32,
    pub languages_json: &'r str,
    pub producer_id: &'r str,
    pub artifact_hash: &'r str,
    pub source_set_id: &'r str,
    pub captured_revision: &'r str,
    pub decoder_version: &'r str,
    pub byte_length: i64,
}

/// One retained document accelerator row.
pub(crate) struct RetainedDocument {
    pub language: String,
    pub path: String,
    pub source_hash: String,
    pub wire_digest: String,
    pub capped: i64,
}

/// One retained proof row.
pub(crate) struct RetainedProof {
    pub rowid: i64,
    pub proof_id: String,
    pub record_kind: String,
    pub fact_digest: String,
    pub language: String,
    pub path: String,
    pub canonical_bytes: Vec<u8>,
}

/// One retained proof's identity, borrowed from its row so the caller can reserve its size
/// before it makes owned strings.
pub(crate) struct WitnessProofRow<'r> {
    pub proof_id: &'r str,
    pub record_kind: &'r str,
    pub fact_digest: &'r str,
    pub language: &'r str,
    pub path: &'r str,
}

impl<'a> RetainedBatchRows<'a> {
    /// The retained batch `batch_id` of a selection whose captured pin is `captured_pin`,
    /// both from a pending selection row storage validated in the snapshot of `db`.
    pub(super) fn new(db: &'a Connection, batch_id: i64, captured_pin: IndexPin) -> Self {
        Self {
            db,
            batch_id,
            captured_pin,
        }
    }

    /// [`Self::new`] over a test's own connection.
    #[cfg(test)]
    pub(crate) fn for_tests(db: &'a Connection, batch_id: i64, captured_pin: IndexPin) -> Self {
        Self::new(db, batch_id, captured_pin)
    }
}

impl RetainedBatchRows<'_> {
    pub(crate) fn batch_id(&self) -> i64 {
        self.batch_id
    }

    /// The stored descriptor's length, read before its bytes are.
    pub(crate) fn descriptor_length(&self) -> Result<i64> {
        Ok(self
            .db
            .prepare_cached(
                "SELECT length(descriptor_bytes) FROM semantic_raw_batches WHERE batch_id=?1",
            )?
            .query_row([self.batch_id], |row| row.get(0))?)
    }

    /// The stored descriptor bytes and their recorded digest.
    pub(crate) fn descriptor(&self) -> Result<(Vec<u8>, String)> {
        Ok(self.db.prepare_cached("SELECT descriptor_bytes,descriptor_sha256 FROM semantic_raw_batches WHERE batch_id=?1")?.query_row(
            [self.batch_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    }

    /// Whether the batch row records exactly `expected`.
    pub(crate) fn metadata_matches(&self, expected: &BatchMetadata<'_>) -> Result<bool> {
        Ok(self.db.prepare_cached("SELECT producer_id=?2 AND producer_version=?3 AND profile_id=?4 AND source_set_id=?5 AND captured_revision=?6 AND source_manifest_hash=?7 AND decoder_version=?8 FROM semantic_raw_batches WHERE batch_id=?1")?.query_row(
            params![self.batch_id, expected.producer_id, expected.producer_version, expected.profile_id,
                expected.source_set_id, expected.captured_revision,
                expected.source_manifest_hash, expected.decoder_version], |row| row.get(0),
        )?)
    }

    /// Whether an original publication of this batch at the captured pin, or a complete
    /// captured witness, exists: existing authority, not an old target binding or freshness
    /// grant.
    pub(crate) fn has_original_authority(
        &self,
        producer_id: &str,
        native_revision_id: &str,
        source_set_id: &str,
    ) -> Result<bool> {
        Ok(self.db.prepare_cached("SELECT EXISTS(SELECT 1 FROM semantic_revision_raw_selections s JOIN native_revisions r ON r.id=s.revision_id WHERE s.batch_id=?1 AND s.producer_id=?2 AND s.captured_pin=?3 AND r.native_revision_id=?4 AND r.source_set_id=?5) OR EXISTS(SELECT 1 FROM semantic_batch_witnesses WHERE batch_id=?1)")?.query_row(
            params![self.batch_id, producer_id,
                format!("pin:v1:{}:{}", self.captured_pin.index_generation, self.captured_pin.index_revision),
                native_revision_id, source_set_id], |row| row.get(0),
        )?)
    }

    /// The next document accelerator key of the capture at `ordinal`, after `after`.
    pub(crate) fn capture_document_key_after(
        &self,
        ordinal: u32,
        after: (&str, &str),
    ) -> Result<Option<(String, String)>> {
        Ok(self.db.prepare_cached("SELECT d.language,d.path FROM semantic_raw_captures c JOIN semantic_raw_documents d ON d.capture_id=c.capture_id WHERE c.batch_id=?1 AND c.ordinal=?2 AND (d.language,d.path)>(?3,?4) ORDER BY d.language,d.path LIMIT 1")?.query_row(
            params![self.batch_id, ordinal, after.0, after.1], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?)
    }

    /// The recorded exclusion reason of one source document.
    pub(crate) fn exclusion(&self, language: &str, path: &str) -> Result<Option<String>> {
        Ok(self.db.prepare_cached("SELECT reason FROM semantic_raw_exclusions WHERE batch_id=?1 AND language=?2 AND path=?3")?.query_row(
            params![self.batch_id, language, path], |row| row.get(0),
        ).optional()?)
    }

    /// Up to two `(capture ordinal, source hash)` assignments of one source document.
    pub(crate) fn assignments(&self, language: &str, path: &str) -> Result<Vec<(u32, String)>> {
        Ok(self.db.prepare_cached(
            "SELECT c.ordinal,d.source_hash FROM semantic_raw_documents d JOIN semantic_raw_captures c ON c.capture_id=d.capture_id WHERE c.batch_id=?1 AND d.language=?2 AND d.path=?3 LIMIT 2",
        )?.query_map(params![self.batch_id, language, path], |row| Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The next `(proof id, language, path)` of the capture at `ordinal`, after `after`.
    pub(crate) fn capture_proof_document_after(
        &self,
        ordinal: u32,
        after: &str,
    ) -> Result<Option<(String, String, String)>> {
        Ok(self.db.prepare_cached("SELECT p.proof_id,p.language,p.path FROM semantic_raw_captures c JOIN semantic_raw_proofs p ON p.capture_id=c.capture_id WHERE c.batch_id=?1 AND c.ordinal=?2 AND p.proof_id>?3 ORDER BY p.proof_id LIMIT 1")?.query_row(
            params![self.batch_id, ordinal, after],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?)
    }

    /// The next excluded `(language, path)` after `after`.
    pub(crate) fn exclusion_after(&self, after: (&str, &str)) -> Result<Option<(String, String)>> {
        Ok(self.db.prepare_cached("SELECT language,path FROM semantic_raw_exclusions WHERE batch_id=?1 AND (language,path)>(?2,?3) ORDER BY language,path LIMIT 1")?.query_row(
            params![self.batch_id, after.0, after.1],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?)
    }

    /// The capture that records exactly `key`.
    pub(crate) fn capture_id(&self, key: &CaptureKey<'_>) -> Result<Option<i64>> {
        Ok(self.db.prepare_cached("SELECT capture_id FROM semantic_raw_captures WHERE batch_id=?1 AND ordinal=?2 AND languages=?3 AND producer_id=?4 AND artifact_hash=?5 AND source_set_id=?6 AND captured_revision=?7 AND decoder_version=?8 AND length(binary_bytes)=?9")?.query_row(
            params![self.batch_id, key.ordinal, key.languages_json, key.producer_id, key.artifact_hash,
                key.source_set_id, key.captured_revision, key.decoder_version, key.byte_length], |row| row.get(0),
        ).optional()?)
    }

    /// How many captures the batch has, counted up to `limit`.
    pub(crate) fn capture_count(&self, limit: i64) -> Result<i64> {
        Ok(self.db.prepare_cached("SELECT COUNT(*) FROM (SELECT ordinal FROM semantic_raw_captures WHERE batch_id=?1 ORDER BY ordinal LIMIT ?2)")?.query_row( params![self.batch_id, limit], |row| row.get(0))?)
    }

    /// Reads the capture's stored binary from `offset` into `buffer`; `0` at its end.
    pub(crate) fn read_capture(
        &self,
        capture_id: i64,
        offset: u64,
        buffer: &mut [u8],
    ) -> Result<usize> {
        let mut blob = self.db.blob_open(
            rusqlite::MAIN_DB,
            "semantic_raw_captures",
            "binary_bytes",
            capture_id,
            true,
        )?;
        blob.seek(SeekFrom::Start(offset))?;
        Ok(blob.read(buffer)?)
    }

    /// Up to `limit` proof identities of the capture after `after`, in proof order. `each`
    /// sees a row's borrowed fields before anything is copied out of it.
    pub(crate) fn witness_proofs(
        &self,
        capture_id: i64,
        after: &str,
        limit: i64,
        each: &mut dyn FnMut(WitnessProofRow<'_>) -> Result<()>,
    ) -> Result<()> {
        let mut statement = self.db.prepare_cached(
            "SELECT proof_id,record_kind,fact_digest,language,path FROM semantic_raw_proofs          WHERE capture_id=?1 AND proof_id>?2 ORDER BY proof_id LIMIT ?3",
        )?;
        let mut rows = statement.query(params![capture_id, after, limit])?;
        while let Some(row) = rows.next()? {
            each(WitnessProofRow {
                proof_id: row.get_ref(0)?.as_str()?,
                record_kind: row.get_ref(1)?.as_str()?,
                fact_digest: row.get_ref(2)?.as_str()?,
                language: row.get_ref(3)?.as_str()?,
                path: row.get_ref(4)?.as_str()?,
            })?;
        }
        Ok(())
    }

    /// The capture's next proof ID after `after`.
    pub(crate) fn proof_id_after(&self, capture_id: i64, after: &str) -> Result<Option<String>> {
        Ok(self.db.prepare_cached("SELECT proof_id FROM semantic_raw_proofs WHERE capture_id=?1 AND proof_id>?2 ORDER BY proof_id LIMIT 1")?.query_row(
            params![capture_id, after], |row| row.get(0),
        ).optional()?)
    }

    /// The capture's next document accelerator key after `after`.
    pub(crate) fn document_key_after(
        &self,
        capture_id: i64,
        after: (&str, &str),
    ) -> Result<Option<(String, String)>> {
        Ok(self.db.prepare_cached("SELECT language,path FROM semantic_raw_documents WHERE capture_id=?1 AND (language,path)>(?2,?3) ORDER BY language,path LIMIT 1")?.query_row(
            params![capture_id, after.0, after.1], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?)
    }

    /// The capture's next document accelerator row after `after`.
    pub(crate) fn document_after(
        &self,
        capture_id: i64,
        after: (&str, &str),
    ) -> Result<Option<RetainedDocument>> {
        Ok(self.db.prepare_cached("SELECT rowid,language,path,source_hash,wire_digest,capped FROM semantic_raw_documents WHERE capture_id=?1 AND (language,path)>(?2,?3) ORDER BY language,path LIMIT 1")?.query_row(
            params![capture_id, after.0, after.1], |row| Ok(RetainedDocument {
                language: row.get(1)?,
                path: row.get(2)?,
                source_hash: row.get(3)?,
                wire_digest: row.get(4)?,
                capped: row.get(5)?,
            }),
        ).optional()?)
    }

    /// Whether the capture's accelerator row of `document` records exactly it; `None` when
    /// the capture has no such row.
    pub(crate) fn document_matches(
        &self,
        capture_id: i64,
        document: &RetainedDocument,
    ) -> Result<Option<bool>> {
        Ok(self.db.prepare_cached("SELECT source_hash=?4 AND wire_digest=?5 AND capped=?6 FROM semantic_raw_documents WHERE capture_id=?1 AND language=?2 AND path=?3")?.query_row(
            params![capture_id, document.language, document.path, document.source_hash, document.wire_digest, document.capped], |row| row.get(0)).optional()?)
    }

    /// The capture's next proof row after `after`.
    pub(crate) fn proof_after(
        &self,
        capture_id: i64,
        after: &str,
    ) -> Result<Option<RetainedProof>> {
        Ok(self.db.prepare_cached("SELECT rowid,proof_id,record_kind,fact_digest,language,path,canonical_bytes FROM semantic_raw_proofs WHERE capture_id=?1 AND proof_id>?2 ORDER BY proof_id LIMIT 1")?.query_row(
            params![capture_id, after], |row| Ok(RetainedProof {
                rowid: row.get(0)?,
                proof_id: row.get(1)?,
                record_kind: row.get(2)?,
                fact_digest: row.get(3)?,
                language: row.get(4)?,
                path: row.get(5)?,
                canonical_bytes: row.get(6)?,
            }),
        ).optional()?)
    }

    /// Whether the capture's row of `proof` records exactly it; `None` when the capture has
    /// no such row. `proof.rowid` is not compared.
    pub(crate) fn proof_matches(
        &self,
        capture_id: i64,
        proof: &RetainedProof,
    ) -> Result<Option<bool>> {
        Ok(self.db.prepare_cached("SELECT record_kind=?3 AND fact_digest=?4 AND language=?5 AND path=?6 AND canonical_bytes=?7 FROM semantic_raw_proofs WHERE capture_id=?1 AND proof_id=?2")?.query_row(
            params![capture_id, proof.proof_id, proof.record_kind, proof.fact_digest, proof.language, proof.path, proof.canonical_bytes], |row| row.get(0)).optional()?)
    }

    /// Whether S copies `document` under `filter`, read in this snapshot.
    pub(crate) fn copies(&self, filter: &CopyFilter, document: &DocumentKey) -> Result<bool> {
        filter.copies(self.db, document)
    }
}
