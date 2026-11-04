//! Resumable retained admission. Main-index statements and blobs never survive a unit.
use super::*;
use crate::{model::IndexPin, scip_admission::PinnedProducer, store::CanonicalInventoryCursor};
use std::{
    fs::{File, OpenOptions},
    io::{Seek, SeekFrom},
};

pub(crate) struct RetainedStageContinuation {
    stage: Option<Stage>,
    descriptor: Option<Descriptor>,
    sha: String,
    producer: PinnedProducer,
    captured: IndexPin,
    batch_id: i64,
    limits: BatchLimits,
    native_config: Vec<serde_json::Value>,
    module_roots: scip_raw::ModuleRoots,
    cargo_builder: Option<scip_raw::CargoTargetsBuilder>,
    cargo: scip_raw::CargoTargets,
    cargo_continuation: Option<scip_raw::CargoTargetsContinuation>,
    hasher: Option<SourceManifestHasher>,
    order: RowOrder,
    source_cursor: CanonicalInventoryCursor,
    source_count: i64,
    coverage_cursor: i64,
    covered: u64,
    excluded: u64,
    trusted: bool,
    witnessed: bool,
    phase: Phase,
    artifact: usize,
    capture_id: i64,
    copy_offset: u64,
    copy_hash: Sha256,
    decoder: Option<scip_raw::ScipDecodeContinuation<File>>,
    proof_cursor: i64,
    document_cursor: String,
    proof_count: i64,
    document_count: i64,
    inventory_artifact: usize,
    inventory_key: (String, String),
    inventory_count: u64,
    proof_key: String,
    document_key: (String, String),
    exclusion_key: (String, String),
    exclusion_count: u64,
    retained_count: i64,
    missing_source: Option<(i64, String, String)>,
    missing_artifact: usize,
    missing_proof: String,
    missing_found: Option<u32>,
}

#[derive(Clone, Copy)]
enum Phase {
    Inventory,
    Sources,
    Cargo,
    Coverage,
    Assignment,
    Exclusions,
    Artifact,
    Copy,
    Decode,
    Proofs,
    ProofInventory,
    Documents,
    DocumentInventory,
    Verify,
    Done,
}

impl RetainedStageContinuation {
    /// Creates only bounded descriptor/header state. No source or artifact is decoded here.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        db: &Connection,
        reader: &NativeReader<'_>,
        batch_id: i64,
        stage_parent: &Path,
        owner: StageOwner<'_>,
        limits: &BatchLimits,
        cancel: &CancelFlag,
    ) -> Result<Self> {
        check_cancel(cancel)?;
        let length: i64 = db
            .prepare_cached(
                "SELECT length(descriptor_bytes) FROM semantic_raw_batches WHERE batch_id=?1",
            )?
            .query_row([batch_id], |row| row.get(0))?;
        ensure!(length >= 0, "retained descriptor length");
        if length as u64 > limits.descriptor_bytes {
            return Err(ScipCaptureError::LimitExceeded("descriptor bytes".into()).into());
        }
        let (bytes, stored_sha): (Vec<u8>, String) = db.prepare_cached("SELECT descriptor_bytes,descriptor_sha256 FROM semantic_raw_batches WHERE batch_id=?1")?.query_row(
            [batch_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
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
        let metadata_ok: bool = db.prepare_cached("SELECT producer_id=?2 AND producer_version=?3 AND profile_id=?4 AND source_set_id=?5 AND captured_revision=?6 AND source_manifest_hash=?7 AND decoder_version=?8 FROM semantic_raw_batches WHERE batch_id=?1")?.query_row(
            params![batch_id, producer.name, producer.version, descriptor.profile_id,
                descriptor.source_set.id, descriptor.source_revision,
                descriptor.source_manifest_hash, descriptor.decoder_version], |row| row.get(0),
        )?;
        ensure!(metadata_ok, "retained batch metadata");
        // Existing original-publication or complete captured-witness authority, not an old
        // target binding or freshness grant. Missing document accelerators require FULL.
        let original: bool = db.prepare_cached("SELECT EXISTS(SELECT 1 FROM semantic_revision_raw_selections s JOIN native_revisions r ON r.id=s.revision_id WHERE s.batch_id=?1 AND s.producer_id=?2 AND s.captured_pin=?3 AND r.native_revision_id=?4 AND r.source_set_id=?5) OR EXISTS(SELECT 1 FROM semantic_batch_witnesses WHERE batch_id=?1)")?.query_row(
            params![batch_id, producer.name,
                format!("pin:v1:{}:{}", reader.pin().index_generation, reader.pin().index_revision),
                descriptor.source_revision, descriptor.source_set.id], |row| row.get(0),
        )?;
        let trusted = original;
        let native_config = vec![];
        let module_roots = scip_raw::ModuleRoots::default();
        let cargo_builder = None;
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
        stage.conn()?.execute_batch("CREATE TABLE retained_assignments(artifact INTEGER PRIMARY KEY, documents INTEGER NOT NULL); CREATE INDEX retained_claims_artifact ON claims(artifact,claim_id)")?;
        let hasher = Some(SourceManifestHasher::new(&descriptor.source_set.id));
        Ok(Self {
            stage: Some(stage),
            descriptor: Some(descriptor),
            sha,
            producer,
            captured: reader.pin(),
            batch_id,
            limits: limits.clone(),
            native_config,
            module_roots,
            cargo_builder,
            cargo: scip_raw::CargoTargets::default(),
            cargo_continuation: None,
            hasher,
            order: RowOrder::source(),
            source_cursor: CanonicalInventoryCursor::default(),
            source_count: 0,
            coverage_cursor: -1,
            covered: 0,
            excluded: 0,
            trusted,
            witnessed: false,
            phase: Phase::Inventory,
            artifact: 0,
            capture_id: 0,
            copy_offset: 0,
            copy_hash: Sha256::new(),
            decoder: None,
            proof_cursor: 0,
            document_cursor: String::new(),
            proof_count: 0,
            document_count: 0,
            inventory_artifact: 0,
            inventory_key: (String::new(), String::new()),
            inventory_count: 0,
            proof_key: String::new(),
            document_key: (String::new(), String::new()),
            exclusion_key: (String::new(), String::new()),
            exclusion_count: 0,
            retained_count: 0,
            missing_source: None,
            missing_artifact: 0,
            missing_proof: String::new(),
            missing_found: None,
        })
    }

    /// FULL catch-up re-authenticates every retained original through the bounded decoder, as
    /// before native-first: a stored binary that no longer matches fails that artifact typed.
    pub(crate) fn authenticated(mut self) -> Self {
        self.trusted = false;
        self
    }

    /// Admits only metadata. The owner stages guarded original association pages before join.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_witnessed(
        db: &Connection,
        reader: &NativeReader<'_>,
        batch_id: i64,
        stage_parent: &Path,
        owner: StageOwner<'_>,
        limits: &BatchLimits,
        cancel: &CancelFlag,
    ) -> Result<Self> {
        let mut continuation =
            Self::new(db, reader, batch_id, stage_parent, owner, limits, cancel)?;
        continuation.witnessed = true;
        continuation
            .stage
            .as_mut()
            .expect("new stage")
            .conn()?
            .execute("INSERT INTO witness_state(singleton) VALUES(1)", [])?;
        Ok(continuation)
    }

    /// One actual stored row, copy block or original top-level SCIP unit. The caller closes
    /// its coherent captured/raw read transaction after every return, before worker selection,
    /// and releases the private stage connection ([`Self::release_connection`]) before it yields.
    pub(crate) fn advance(
        &mut self,
        db: &Connection,
        reader: &NativeReader<'_>,
        counters: &mut BatchCounters,
        cancel: &CancelFlag,
    ) -> Result<Option<AdmittedBatch>> {
        self.advance_unit(db, reader, counters, cancel)
    }

    pub(crate) fn release_connection(&mut self) {
        if let Some(stage) = self.stage.as_mut() {
            stage.release_connection();
        }
    }

    fn advance_unit(
        &mut self,
        db: &Connection,
        reader: &NativeReader<'_>,
        counters: &mut BatchCounters,
        cancel: &CancelFlag,
    ) -> Result<Option<AdmittedBatch>> {
        check_cancel(cancel)?;
        ensure!(
            reader.pin() == self.captured,
            "retained captured pin changed"
        );
        let descriptor = self
            .descriptor
            .as_ref()
            .ok_or_else(|| admission("retained stage complete"))?;
        let stage = self
            .stage
            .as_mut()
            .ok_or_else(|| admission("retained stage complete"))?;
        let budget = stage.budget(&self.limits);
        let dir = stage.dir().to_owned();
        let conn = stage.conn()?;
        let before = conn.total_changes();
        let passes = Passes {
            descriptor,
            producer: &self.producer,
            limits: &self.limits,
            cancel,
            budget: budget.clone(),
            module_roots: &self.module_roots,
            native_config: &self.native_config,
        };
        match self.phase {
            Phase::Inventory => {
                // Count document accelerators by their indexed capture/language/path key.
                // A missing accelerator selects the original bounded decoder, not a repair
                // of an already validated retained row.
                if self.trusted && self.inventory_artifact < descriptor.artifacts.len() {
                    let entry = &descriptor.artifacts[self.inventory_artifact];
                    let key: Option<(String, String)> = db.prepare_cached("SELECT d.language,d.path FROM semantic_raw_captures c JOIN semantic_raw_documents d ON d.capture_id=c.capture_id WHERE c.batch_id=?1 AND c.ordinal=?2 AND (d.language,d.path)>(?3,?4) ORDER BY d.language,d.path LIMIT 1")?.query_row(
                        params![self.batch_id, entry.ordinal, self.inventory_key.0, self.inventory_key.1], |row| Ok((row.get(0)?, row.get(1)?)),
                    ).optional()?;
                    if let Some(key) = key {
                        self.inventory_key = key;
                        self.inventory_count += 1;
                    } else {
                        self.inventory_artifact += 1;
                        self.inventory_key = (String::new(), String::new());
                    }
                } else {
                    self.trusted =
                        self.trusted && self.inventory_count == descriptor.totals.covered_documents;
                    if self.witnessed && !self.trusted {
                        return Err(super::super::staging::WitnessFallback(
                            "original admission or document inventory incomplete",
                        )
                        .into());
                    }
                    if !self.trusted {
                        self.native_config = reader.retained_components()?.1;
                        self.module_roots =
                            scip_raw::ModuleRoots::from_native_config(&self.native_config);
                        self.cargo_builder = Some(
                            scip_raw::CargoTargetsBuilder::from_native_config(&self.native_config),
                        );
                    }
                    self.phase = Phase::Sources;
                }
            }
            Phase::Sources => {
                if let Some(document) = reader
                    .canonical_inventory_page(&mut self.source_cursor, 1)?
                    .pop()
                {
                    if self.source_count as u64 >= self.limits.source_documents {
                        return Err(
                            ScipCaptureError::LimitExceeded("source documents".into()).into()
                        );
                    }
                    SourceRow {
                        language: document.key.language.clone(),
                        relative_path: document.key.path.clone(),
                        content_hash: document.content_hash.clone(),
                        byte_length: document.byte_length as u64,
                    }
                    .check(&descriptor.source_set.languages)?;
                    self.order
                        .push(&document.key.language, &document.key.path)?;
                    ensure!(
                        document.key.source_set_id == descriptor.source_set.id
                            && document.revision_id == descriptor.source_revision,
                        "captured source mismatch"
                    );
                    let hasher = self.hasher.as_mut().expect("source phase hasher");
                    if self.trusted {
                        hasher.push(
                            &document.key.language,
                            &document.key.path,
                            &document.content_hash,
                        );
                        conn.execute(
                            "INSERT INTO sources VALUES(?1,?2,?3,?4,?5)",
                            params![
                                self.source_count,
                                document.key.language,
                                document.key.path,
                                document.content_hash,
                                document.byte_length as i64
                            ],
                        )?;
                    } else {
                        let mut native = ReaderSource {
                            reader,
                            stream: None,
                        };
                        passes.stage_source(
                            conn,
                            &mut native,
                            &document,
                            self.source_count,
                            self.cargo_builder.as_mut().expect("full cargo builder"),
                            hasher,
                        )?;
                    }
                    self.source_count += 1;
                } else {
                    ensure!(
                        self.source_count as u64 == descriptor.totals.source_documents,
                        "retained source total"
                    );
                    ensure!(
                        self.hasher.take().expect("source phase hasher").finish()
                            == descriptor.source_manifest_hash,
                        "source manifest hash"
                    );
                    if let Some(builder) = self.cargo_builder.take() {
                        self.cargo_continuation = Some(builder.into_continuation());
                        self.phase = Phase::Cargo;
                    } else {
                        self.phase = Phase::Coverage;
                    }
                }
            }
            Phase::Cargo => {
                if let Some(cargo) = self
                    .cargo_continuation
                    .as_mut()
                    .expect("cargo continuation")
                    .advance()
                {
                    self.cargo = cargo;
                    self.cargo_continuation = None;
                    self.phase = Phase::Coverage;
                }
            }
            Phase::Coverage => {
                let source: Option<(i64, String, String, String)> = conn.prepare_cached("SELECT ord,language,path,content_hash FROM sources WHERE ord>?1 ORDER BY ord LIMIT 1")?.query_row(
                    [self.coverage_cursor], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                ).optional()?;
                if let Some((ord, language, path, hash)) = source {
                    self.coverage_cursor = ord;
                    if self.producer.languages.contains(&language) {
                        let exclusion: Option<String> = db.prepare_cached("SELECT reason FROM semantic_raw_exclusions WHERE batch_id=?1 AND language=?2 AND path=?3")?.query_row(
                            params![self.batch_id, language, path], |row| row.get(0),
                        ).optional()?;
                        let assignments = db.prepare_cached(
                            "SELECT c.ordinal,d.source_hash FROM semantic_raw_documents d JOIN semantic_raw_captures c ON c.capture_id=d.capture_id WHERE c.batch_id=?1 AND d.language=?2 AND d.path=?3 LIMIT 2",
                        )?.query_map(params![self.batch_id, language, path], |row| Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
                        let artifact = match assignments.as_slice() {
                            [(ordinal, source_hash)] => {
                                ensure!(
                                    exclusion.is_none()
                                        && *source_hash == hash
                                        && descriptor
                                            .artifacts
                                            .get(*ordinal as usize)
                                            .is_some_and(|a| a.language == language),
                                    "retained document assignment"
                                );
                                self.covered += 1;
                                Some(*ordinal)
                            }
                            [] if exclusion.is_some() => {
                                ensure!(
                                    matches!(
                                        exclusion.as_deref(),
                                        Some("notRequested" | "unsupported" | "failed")
                                    ),
                                    "retained coverage completion"
                                );
                                self.excluded += 1;
                                None
                            }
                            [] => {
                                if self.witnessed {
                                    return Err(super::super::staging::WitnessFallback(
                                        "required raw document missing",
                                    )
                                    .into());
                                }
                                ensure!(!self.trusted, "retained missing validated document");
                                self.missing_source = Some((ord, language, path));
                                self.missing_artifact = 0;
                                self.missing_proof.clear();
                                self.missing_found = None;
                                self.phase = Phase::Assignment;
                                return Ok(None);
                            }
                            _ => {
                                return Err(
                                    admission("retained duplicate document assignment").into()
                                );
                            }
                        };
                        conn.execute(
                            "INSERT INTO coverage VALUES(?1,?2,?3)",
                            params![ord, artifact, exclusion],
                        )?;
                        if let Some(artifact) = artifact {
                            conn.execute("INSERT INTO retained_assignments VALUES(?1,1) ON CONFLICT(artifact) DO UPDATE SET documents=documents+1", [artifact])?;
                        }
                    }
                } else {
                    ensure!(
                        self.covered == descriptor.totals.covered_documents
                            && self.excluded == descriptor.totals.excluded_documents,
                        "retained coverage totals"
                    );
                    self.phase = Phase::Exclusions;
                }
            }
            Phase::Assignment => {
                let (ord, language, path) =
                    self.missing_source.as_ref().expect("assignment source");
                if let Some(entry) = descriptor.artifacts.get(self.missing_artifact) {
                    if entry.language != *language {
                        self.missing_artifact += 1;
                        self.missing_proof.clear();
                    } else {
                        let proof: Option<(String, String, String)> = db.prepare_cached("SELECT p.proof_id,p.language,p.path FROM semantic_raw_captures c JOIN semantic_raw_proofs p ON p.capture_id=c.capture_id WHERE c.batch_id=?1 AND c.ordinal=?2 AND p.proof_id>?3 ORDER BY p.proof_id LIMIT 1")?.query_row(
                            params![self.batch_id, entry.ordinal, self.missing_proof],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                        ).optional()?;
                        if let Some((proof, proof_language, proof_path)) = proof {
                            self.missing_proof = proof;
                            if proof_language == *language && proof_path == *path {
                                ensure!(
                                    self.missing_found
                                        .is_none_or(|found| found == entry.ordinal),
                                    "retained ambiguous document assignment"
                                );
                                self.missing_found = Some(entry.ordinal);
                                self.missing_artifact += 1;
                                self.missing_proof.clear();
                            }
                        } else {
                            self.missing_artifact += 1;
                            self.missing_proof.clear();
                        }
                    }
                } else {
                    let artifact = if let Some(artifact) = self.missing_found {
                        artifact
                    } else {
                        let mut entries = descriptor
                            .artifacts
                            .iter()
                            .filter(|entry| entry.language == *language);
                        let artifact = entries
                            .next()
                            .ok_or_else(|| admission("retained document assignment"))?;
                        ensure!(
                            entries.next().is_none(),
                            "retained ambiguous document assignment"
                        );
                        artifact.ordinal
                    };
                    conn.execute(
                        "INSERT INTO coverage VALUES(?1,?2,NULL)",
                        params![ord, artifact],
                    )?;
                    conn.execute("INSERT INTO retained_assignments VALUES(?1,1) ON CONFLICT(artifact) DO UPDATE SET documents=documents+1", [artifact])?;
                    self.covered += 1;
                    self.missing_source = None;
                    self.phase = Phase::Coverage;
                }
            }
            Phase::Exclusions => {
                let next: Option<(String, String)> = db.prepare_cached("SELECT language,path FROM semantic_raw_exclusions WHERE batch_id=?1 AND (language,path)>(?2,?3) ORDER BY language,path LIMIT 1")?.query_row(
                    params![self.batch_id, self.exclusion_key.0, self.exclusion_key.1],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                ).optional()?;
                if let Some(key) = next {
                    self.exclusion_key = key;
                    self.exclusion_count += 1;
                } else {
                    ensure!(
                        self.exclusion_count == self.excluded,
                        "retained exclusion inventory"
                    );
                    self.phase = Phase::Artifact;
                }
            }
            Phase::Artifact => {
                if let Some(entry) = descriptor.artifacts.get(self.artifact) {
                    self.capture_id = db.prepare_cached("SELECT capture_id FROM semantic_raw_captures WHERE batch_id=?1 AND ordinal=?2 AND language=?3 AND producer_id=?4 AND artifact_hash=?5 AND source_set_id=?6 AND captured_revision=?7 AND decoder_version=?8 AND length(binary_bytes)=?9")?.query_row(
                        params![self.batch_id, entry.ordinal, entry.language, self.producer.name, entry.sha256,
                            descriptor.source_set.id, descriptor.source_revision, descriptor.decoder_version, entry.byte_length as i64], |row| row.get(0),
                    ).optional()?.ok_or_else(|| admission("retained artifact metadata"))?;
                    self.copy_offset = 0;
                    self.copy_hash = Sha256::new();
                    self.proof_cursor = 0;
                    self.document_cursor.clear();
                    self.proof_key.clear();
                    self.document_key = (String::new(), String::new());
                    self.proof_count = 0;
                    self.document_count = 0;
                    if self.trusted {
                        self.phase = Phase::Documents;
                    } else {
                        File::create_new(artifact_file(&dir, entry.ordinal))?;
                        self.phase = Phase::Copy;
                    }
                } else {
                    let captures: i64 = db.prepare_cached("SELECT COUNT(*) FROM (SELECT ordinal FROM semantic_raw_captures WHERE batch_id=?1 ORDER BY ordinal LIMIT ?2)")?.query_row( params![self.batch_id, descriptor.artifacts.len() as i64 + 1], |row| row.get(0))?;
                    ensure!(
                        captures as usize == descriptor.artifacts.len(),
                        "retained artifact inventory"
                    );
                    self.coverage_cursor = -1;
                    self.phase = Phase::Verify;
                }
            }
            Phase::Copy => {
                let entry = &descriptor.artifacts[self.artifact];
                let mut blob = db.blob_open(
                    rusqlite::MAIN_DB,
                    "semantic_raw_captures",
                    "binary_bytes",
                    self.capture_id,
                    true,
                )?;
                blob.seek(SeekFrom::Start(self.copy_offset))?;
                let mut buffer = vec![0; IO_BUFFER_BYTES as usize];
                let count = blob.read(&mut buffer)?;
                if count > 0 {
                    let mut file = OpenOptions::new()
                        .append(true)
                        .open(artifact_file(&dir, entry.ordinal))?;
                    file.write_all(&buffer[..count])?;
                    self.copy_hash.update(&buffer[..count]);
                    self.copy_offset += count as u64;
                } else {
                    ensure!(
                        self.copy_offset == entry.byte_length
                            && hex::encode(self.copy_hash.clone().finalize()) == entry.sha256,
                        "retained artifact digest"
                    );
                    #[cfg(feature = "test-support")]
                    crate::semantic_import::note_original_raw_decode();
                    self.decoder = Some(scip_raw::ScipDecodeContinuation::new(
                        File::open(artifact_file(&dir, entry.ordinal))?,
                        entry.byte_length,
                        &self.limits,
                    )?);
                    self.phase = Phase::Decode;
                }
            }
            Phase::Decode => {
                let entry = &descriptor.artifacts[self.artifact];
                let mut native = ReaderSource {
                    reader,
                    stream: None,
                };
                let assigned: i64 = conn
                    .prepare_cached("SELECT documents FROM retained_assignments WHERE artifact=?1")?
                    .query_row([entry.ordinal], |row| row.get(0))
                    .optional()?
                    .unwrap_or(0);
                let assigned = u64::try_from(assigned)
                    .map_err(|_| admission("invalid retained assigned document count"))?;
                if passes.decode_advance(
                    entry,
                    self.decoder.as_mut().expect("decode cursor"),
                    conn,
                    &mut native,
                    &self.cargo,
                    counters,
                    assigned,
                )? {
                    self.decoder = None;
                    self.phase = Phase::Proofs;
                }
            }
            Phase::Proofs => {
                let bytes = if self.witnessed {
                    advance_witness_proof(
                        db,
                        self.capture_id,
                        &descriptor.artifacts[self.artifact],
                        conn,
                        &mut self.proof_key,
                        &self.limits,
                    )?
                } else {
                    advance_proof(
                        db,
                        self.capture_id,
                        &descriptor.artifacts[self.artifact],
                        conn,
                        self.trusted,
                        &mut self.proof_cursor,
                        &mut self.proof_key,
                    )?
                };
                if let Some(bytes) = bytes {
                    counters.retained_admission_bytes_high_water =
                        counters.retained_admission_bytes_high_water.max(bytes);
                    self.proof_count += 1;
                } else {
                    if self.trusted {
                        conn.execute(
                            "UPDATE artifacts SET claims=?2 WHERE ordinal=?1",
                            params![self.artifact as i64, self.proof_count],
                        )?;
                        self.artifact += 1;
                        self.phase = Phase::Artifact;
                    } else {
                        self.retained_count = 0;
                        self.proof_key.clear();
                        self.phase = Phase::ProofInventory;
                    }
                }
            }
            Phase::Documents => {
                if let Some(retained) = advance_document(
                    db,
                    self.capture_id,
                    &descriptor.artifacts[self.artifact],
                    conn,
                    self.trusted,
                    &mut self.document_cursor,
                    &mut self.document_key,
                )? {
                    self.document_count += i64::from(retained);
                } else {
                    if self.trusted {
                        conn.execute(
                            "UPDATE artifacts SET documents=?2 WHERE ordinal=?1",
                            params![self.artifact as i64, self.document_count],
                        )?;
                        self.phase = Phase::Proofs;
                    } else {
                        self.retained_count = 0;
                        self.document_key = (String::new(), String::new());
                        self.phase = Phase::DocumentInventory;
                    }
                }
            }
            Phase::ProofInventory => {
                let next: Option<String> = db.prepare_cached("SELECT proof_id FROM semantic_raw_proofs WHERE capture_id=?1 AND proof_id>?2 ORDER BY proof_id LIMIT 1")?.query_row(
                    params![self.capture_id, self.proof_key], |row| row.get(0),
                ).optional()?;
                if let Some(key) = next {
                    self.proof_key = key;
                    self.retained_count += 1;
                } else {
                    ensure!(
                        self.retained_count == self.proof_count,
                        "retained proof inventory"
                    );
                    self.phase = Phase::Documents;
                }
            }
            Phase::DocumentInventory => {
                let next: Option<(String, String)> = db.prepare_cached("SELECT language,path FROM semantic_raw_documents WHERE capture_id=?1 AND (language,path)>(?2,?3) ORDER BY language,path LIMIT 1")?.query_row(
                    params![self.capture_id, self.document_key.0, self.document_key.1], |row| Ok((row.get(0)?, row.get(1)?)),
                ).optional()?;
                if let Some(key) = next {
                    self.document_key = key;
                    self.retained_count += 1;
                } else {
                    ensure!(
                        self.retained_count == self.document_count,
                        "retained document inventory"
                    );
                    self.artifact += 1;
                    self.phase = Phase::Artifact;
                }
            }
            Phase::Verify => {
                let row: Option<(i64, bool)> = conn.prepare_cached("SELECT c.source_ord,EXISTS(SELECT 1 FROM documents d WHERE d.source_ord=c.source_ord) FROM coverage c WHERE c.source_ord>?1 AND c.artifact IS NOT NULL ORDER BY c.source_ord LIMIT 1")?.query_row(
                    [self.coverage_cursor], |row| Ok((row.get(0)?, row.get(1)?)),
                ).optional()?;
                if let Some((ord, present)) = row {
                    ensure!(present, "retained missing artifact document");
                    self.coverage_cursor = ord;
                } else {
                    self.phase = Phase::Done;
                }
            }
            Phase::Done => {
                if self.witnessed {
                    super::super::staging::check_proof_identity_closure(conn)?;
                }
                return Ok(Some(AdmittedBatch::new(
                    self.stage.take().expect("stage"),
                    self.descriptor.take().expect("descriptor"),
                    self.sha.clone(),
                    self.producer.clone(),
                    self.captured,
                )));
            }
        }
        budget.check()?;
        counters.retained_admission_units += 1;
        counters.retained_admission_rows_high_water = counters
            .retained_admission_rows_high_water
            .max(conn.total_changes() - before);
        Ok(None)
    }
}

/// Identity-only retained read. Reserve borrowed scalar fields before making owned strings;
/// neither canonical proof bytes nor the SCIP binary participates in witness admission.
fn advance_witness_proof(
    db: &Connection,
    capture: i64,
    entry: &super::super::descriptor::ArtifactEntry,
    conn: &Connection,
    key: &mut String,
    limits: &BatchLimits,
) -> Result<Option<u64>> {
    let mut statement = db.prepare_cached(
        "SELECT proof_id,record_kind,fact_digest,language,path FROM semantic_raw_proofs          WHERE capture_id=?1 AND proof_id>?2 ORDER BY proof_id LIMIT 1",
    )?;
    let mut rows = statement.query(params![capture, key.as_str()])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let mut bytes = 256_u64;
    for index in 0..5 {
        let value = row.get_ref(index)?.as_str()?;
        bytes = bytes.saturating_add((value.len() as u64).saturating_mul(2));
    }
    if bytes > limits.unit_expansion_bytes {
        return Err(ScipCaptureError::LimitExceeded("unit expansion bytes".into()).into());
    }
    let proof: String = row.get(0)?;
    let kind: String = row.get(1)?;
    let digest: String = row.get(2)?;
    let language: String = row.get(3)?;
    let path: String = row.get(4)?;
    ensure!(language == entry.language, "retained proof language");
    let source = super::super::staging::lookup_assigned(conn, entry.ordinal, &language, &path)?
        .ok_or_else(|| admission("retained unlisted proof"))?;
    let present: bool = conn
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM documents WHERE source_ord=?1 AND artifact=?2)",
        )?
        .query_row(params![source.ord, entry.ordinal], |row| row.get(0))?;
    ensure!(present, "retained proof document");
    let identity: Option<i64> = conn
        .prepare_cached("SELECT identity_id FROM witness_proofs ORDER BY identity_id LIMIT 1")?
        .query_row([], |row| row.get(0))
        .optional()?;
    let identity = identity
        .unwrap_or(0)
        .checked_sub(1)
        .ok_or_else(|| admission("witness proof identity exhausted"))?;
    super::super::staging::charge_witness_row(conn, limits)?;
    conn.prepare_cached("INSERT INTO witness_proofs VALUES(?1,?2,?3,?4,?5,?6,?7)")?
        .execute(params![
            identity,
            entry.ordinal,
            source.ord,
            kind,
            digest,
            proof,
            capture
        ])?;
    *key = proof;
    Ok(Some(bytes))
}

type DocumentRow = (i64, String, String, String, String, i64);
type ProofRow = (i64, String, String, String, String, String, Vec<u8>);

fn advance_document(
    db: &Connection,
    capture: i64,
    entry: &super::super::descriptor::ArtifactEntry,
    conn: &Connection,
    trusted: bool,
    cursor: &mut String,
    key: &mut (String, String),
) -> Result<Option<bool>> {
    if trusted {
        let row: Option<DocumentRow> = db.prepare_cached("SELECT rowid,language,path,source_hash,wire_digest,capped FROM semantic_raw_documents WHERE capture_id=?1 AND (language,path)>(?2,?3) ORDER BY language,path LIMIT 1")?.query_row(
            params![capture, key.0, key.1], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        ).optional()?;
        let Some((_id, language, path, hash, wire, capped)) = row else {
            return Ok(None);
        };
        ensure!(language == entry.language, "retained document language");
        let source = super::super::staging::lookup_assigned(conn, entry.ordinal, &language, &path)?
            .ok_or_else(|| admission("retained unlisted document"))?;
        ensure!(source.content_hash == hash, "retained document source");
        conn.execute(
            "INSERT INTO documents VALUES(?1,?2,?3,?4,?5)",
            params![source.ord, entry.ordinal, path, wire, capped],
        )?;
        *key = (language, path);
    } else {
        let row: Option<DocumentRow> = conn.prepare_cached("SELECT d.source_ord,s.language,s.path,s.content_hash,d.wire_digest,d.capped FROM documents d JOIN sources s ON s.ord=d.source_ord WHERE d.artifact=?1 AND d.path>?2 ORDER BY d.path LIMIT 1")?.query_row(
            params![entry.ordinal, cursor.as_str()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        ).optional()?;
        let Some((_id, language, path, hash, wire, capped)) = row else {
            return Ok(None);
        };
        let same: Option<bool> = db.prepare_cached("SELECT source_hash=?4 AND wire_digest=?5 AND capped=?6 FROM semantic_raw_documents WHERE capture_id=?1 AND language=?2 AND path=?3")?.query_row(
            params![capture, language, path, hash, wire, capped], |row| row.get(0)).optional()?;
        ensure!(
            same.unwrap_or(true),
            "retained document differs from authenticated binary"
        );
        *cursor = path;
        // Missing accelerator rows remain recoverable; only retained rows count.
        return Ok(Some(same.is_some()));
    }
    Ok(Some(true))
}

fn advance_proof(
    db: &Connection,
    capture: i64,
    entry: &super::super::descriptor::ArtifactEntry,
    conn: &Connection,
    trusted: bool,
    cursor: &mut i64,
    key: &mut String,
) -> Result<Option<u64>> {
    let byte_length;
    if trusted {
        let row: Option<ProofRow> = db.prepare_cached("SELECT rowid,proof_id,record_kind,fact_digest,language,path,canonical_bytes FROM semantic_raw_proofs WHERE capture_id=?1 AND proof_id>?2 ORDER BY proof_id LIMIT 1")?.query_row(
            params![capture, key.as_str()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?)),
        ).optional()?;
        let Some((id, proof, kind, digest, language, path, bytes)) = row else {
            return Ok(None);
        };
        byte_length = bytes.len() as u64;
        ensure!(language == entry.language, "retained proof language");
        let source = super::super::staging::lookup_assigned(conn, entry.ordinal, &language, &path)?
            .ok_or_else(|| admission("retained unlisted proof"))?;
        let present: bool = conn
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM documents WHERE source_ord=?1 AND artifact=?2)",
            )?
            .query_row(params![source.ord, entry.ordinal], |row| row.get(0))?;
        ensure!(present, "retained proof document");
        let claim = super::super::staging::claim_from_stage(
            &bytes,
            digest.clone(),
            proof.clone(),
            language.clone(),
            entry.sha256.clone(),
        )?;
        let (symbol, presence) = super::super::staging::claim_presence(&claim);
        let target = claim.envelope.cargo_target.as_ref();
        conn.execute("INSERT INTO claims(artifact,source_ord,record_kind,fact_digest,proof_id,canonical_bytes,symbol,presence,target_root,target_lib,target_package,target_version) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,CASE WHEN ?9 IS NULL THEN NULL ELSE (SELECT ord FROM sources WHERE language=?10 AND path=?9) END,?11,?12,?13)",
            params![entry.ordinal, source.ord, kind, digest, proof, bytes, symbol, presence,
                target.map(|t| t.root.as_str()), language, target.is_some_and(|t| t.is_lib()),
                target.map(|t| t.package.as_str()), target.and_then(|t| t.version.as_deref())])?;
        *cursor = id;
        *key = proof;
    } else {
        let row: Option<ProofRow> = conn.prepare_cached("SELECT c.claim_id,c.proof_id,c.record_kind,c.fact_digest,s.language,s.path,c.canonical_bytes FROM claims c JOIN sources s ON s.ord=c.source_ord WHERE c.artifact=?1 AND c.claim_id>?2 ORDER BY c.claim_id LIMIT 1")?.query_row(
            params![entry.ordinal, *cursor], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?)),
        ).optional()?;
        let Some((id, proof, kind, digest, language, path, bytes)) = row else {
            return Ok(None);
        };
        byte_length = bytes.len() as u64;
        let same: bool = db.prepare_cached("SELECT record_kind=?3 AND fact_digest=?4 AND language=?5 AND path=?6 AND canonical_bytes=?7 FROM semantic_raw_proofs WHERE capture_id=?1 AND proof_id=?2")?.query_row(
            params![capture, proof, kind, digest, language, path, bytes], |row| row.get(0)).optional()?.unwrap_or(false);
        ensure!(same, "retained proof differs from authenticated binary");
        *cursor = id;
    }
    Ok(Some(byte_length))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_document_comparison_yields_even_when_accelerator_is_absent() {
        let parent = tempfile::tempdir().unwrap();
        let mut stage = Stage::create(parent.path(), "test", "compare").unwrap();
        let conn = stage.conn().unwrap();
        conn.execute_batch("INSERT INTO sources VALUES(0,'java','a.java','h',1),(1,'java','b.java','h',1); INSERT INTO artifacts(ordinal,path,language,sha256,byte_length) VALUES(0,'raw','java','hash',0); INSERT INTO documents VALUES(0,0,'a.java','wa',0),(1,0,'b.java','wb',0)").unwrap();
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE semantic_raw_documents(capture_id INTEGER,language TEXT,path TEXT,source_hash TEXT,wire_digest TEXT,capped INTEGER); INSERT INTO semantic_raw_documents VALUES(1,'java','b.java','h','wb',0)").unwrap();
        let entry = super::super::super::descriptor::ArtifactEntry {
            ordinal: 0,
            relative_path: "raw".into(),
            language: "java".into(),
            sha256: "hash".into(),
            byte_length: 0,
        };
        let mut cursor = String::new();
        assert_eq!(
            advance_document(
                &db,
                1,
                &entry,
                conn,
                false,
                &mut cursor,
                &mut (String::new(), String::new())
            )
            .unwrap(),
            Some(false)
        );
        assert_eq!(cursor, "a.java");
        assert_eq!(
            advance_document(
                &db,
                1,
                &entry,
                conn,
                false,
                &mut cursor,
                &mut (String::new(), String::new())
            )
            .unwrap(),
            Some(true)
        );
        assert_eq!(cursor, "b.java");
        assert_eq!(
            advance_document(
                &db,
                1,
                &entry,
                conn,
                false,
                &mut cursor,
                &mut (String::new(), String::new())
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn trusted_rows_preserve_wire_and_canonical_identity_one_row_per_call() {
        let parent = tempfile::tempdir().unwrap();
        let mut stage = Stage::create(parent.path(), "test", "trusted").unwrap();
        let conn = stage.conn().unwrap();
        conn.execute_batch("INSERT INTO sources VALUES(0,'java','a.java','ha',1),(1,'java','b.java','hb',1); INSERT INTO coverage VALUES(0,0,NULL),(1,0,NULL); INSERT INTO artifacts(ordinal,path,language,sha256,byte_length) VALUES(0,'raw','java','hash',0)").unwrap();
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE semantic_raw_documents(capture_id INTEGER,language TEXT,path TEXT,source_hash TEXT,wire_digest TEXT,capped INTEGER,PRIMARY KEY(capture_id,language,path)); CREATE TABLE semantic_raw_proofs(capture_id INTEGER,proof_id TEXT,record_kind TEXT,fact_digest TEXT,language TEXT,path TEXT,canonical_bytes BLOB,PRIMARY KEY(capture_id,proof_id)); INSERT INTO semantic_raw_documents VALUES(1,'java','a.java','ha','wire-a',0),(1,'java','b.java','hb','wire-b',1)").unwrap();
        let bytes = br#"{"formatVersion":1,"recordKind":"symbolInformation","relativePath":"a.java","positionEncoding":1,"raw":{"symbol":"A#"}}"#;
        for proof in ["p1", "p2"] {
            db.execute("INSERT INTO semantic_raw_proofs VALUES(1,?1,'symbolInformation',?1,'java','a.java',?2)", params![proof, bytes.as_slice()]).unwrap();
        }
        let entry = super::super::super::descriptor::ArtifactEntry {
            ordinal: 0,
            relative_path: "raw".into(),
            language: "java".into(),
            sha256: "hash".into(),
            byte_length: 0,
        };
        let mut key = (String::new(), String::new());
        let mut cursor = String::new();
        for expected in [1, 2] {
            assert_eq!(
                advance_document(&db, 1, &entry, conn, true, &mut cursor, &mut key).unwrap(),
                Some(true)
            );
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, expected);
        }
        assert_eq!(
            advance_document(&db, 1, &entry, conn, true, &mut cursor, &mut key).unwrap(),
            None
        );
        let wire: String = conn
            .query_row(
                "SELECT wire_digest FROM documents WHERE source_ord=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(wire, "wire-b");
        let mut proof = String::new();
        let mut proof_cursor = 0;
        for expected in [1, 2] {
            assert_eq!(
                advance_proof(&db, 1, &entry, conn, true, &mut proof_cursor, &mut proof).unwrap(),
                Some(bytes.len() as u64)
            );
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM claims", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, expected);
        }
        assert_eq!(
            advance_proof(&db, 1, &entry, conn, true, &mut proof_cursor, &mut proof).unwrap(),
            None
        );
        let stored: Vec<u8> = conn
            .query_row(
                "SELECT canonical_bytes FROM claims WHERE proof_id='p1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, bytes);
    }
}
