//! Resumable retained admission. Main-index statements and blobs never survive a unit.
use super::*;
use crate::{model::IndexPin, scip_admission::PinnedProducer, store::CanonicalInventoryCursor};
use std::fs::{File, OpenOptions};

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
    /// Estimated stage bytes written since the stage directory was last measured.
    unchecked: u64,
    /// Witnessed S: the current artifact's proof progress.
    witness: WitnessCursor,
    /// Witnessed S: the documents it copies, whose proof identities are not staged.
    copy: Option<crate::store::native_first::CopyFilter>,
}

/// Stage bytes charged per row a unit changes, beyond the payload it reports.
const ROW_STAGE_BYTES: u64 = 512;

#[derive(Clone, Copy, PartialEq, Eq)]
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
    pub(crate) fn new(
        rows: &RetainedBatchRows<'_>,
        reader: &NativeReader<'_>,
        stage_parent: &Path,
        owner: StageOwner<'_>,
        limits: &BatchLimits,
        cancel: &CancelFlag,
    ) -> Result<Self> {
        check_cancel(cancel)?;
        let batch_id = rows.batch_id();
        let length = rows.descriptor_length()?;
        ensure!(length >= 0, "retained descriptor length");
        if length as u64 > limits.descriptor_bytes {
            return Err(ScipCaptureError::LimitExceeded("descriptor bytes".into()).into());
        }
        let (bytes, stored_sha) = rows.descriptor()?;
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
        let metadata_ok = rows.metadata_matches(&BatchMetadata {
            producer_id: &producer.name,
            producer_version: &producer.version,
            profile_id: &descriptor.profile_id,
            source_set_id: &descriptor.source_set.id,
            captured_revision: &descriptor.source_revision,
            source_manifest_hash: &descriptor.source_manifest_hash,
            decoder_version: &descriptor.decoder_version,
        })?;
        ensure!(metadata_ok, "retained batch metadata");
        // Existing original-publication or complete captured-witness authority, not an old
        // target binding or freshness grant. Missing document accelerators require FULL.
        let original = rows.has_original_authority(
            &producer.name,
            &descriptor.source_revision,
            &descriptor.source_set.id,
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
            unchecked: 0,
            witness: WitnessCursor::default(),
            copy: None,
        })
    }

    /// FULL catch-up re-authenticates every retained original through the bounded decoder, as
    /// before native-first: a stored binary that no longer matches fails that artifact typed.
    pub(crate) fn authenticated(mut self) -> Self {
        self.trusted = false;
        self
    }

    /// Admits only metadata. The owner stages guarded original association pages before join.
    pub(crate) fn new_witnessed(
        rows: &RetainedBatchRows<'_>,
        reader: &NativeReader<'_>,
        stage_parent: &Path,
        owner: StageOwner<'_>,
        limits: &BatchLimits,
        cancel: &CancelFlag,
    ) -> Result<Self> {
        let mut continuation = Self::new(rows, reader, stage_parent, owner, limits, cancel)?;
        continuation.witnessed = true;
        continuation
            .stage
            .as_mut()
            .expect("new stage")
            .conn()?
            .execute_batch(&format!(
                "INSERT INTO witness_state(singleton) VALUES(1); {WITNESS_PENDING_TABLE}"
            ))?;
        Ok(continuation)
    }

    /// One actual stored row, copy block or original top-level SCIP unit. The caller closes
    /// its coherent captured/raw read transaction after every return, before worker selection,
    /// and releases the private stage connection ([`Self::release_connection`]) before it yields.
    pub(crate) fn advance(
        &mut self,
        rows: &RetainedBatchRows<'_>,
        reader: &NativeReader<'_>,
        counters: &mut BatchCounters,
        cancel: &CancelFlag,
    ) -> Result<Option<AdmittedBatch>> {
        self.advance_unit(rows, reader, counters, cancel)
    }

    /// Witnessed S does not stage the proof identities of the documents it copies (#669 H1).
    pub(crate) fn copying(mut self, filter: crate::store::native_first::CopyFilter) -> Self {
        self.copy = Some(filter);
        self
    }

    /// The private stage while admission owns it.
    pub(crate) fn stage_mut(&mut self) -> Option<&mut Stage> {
        self.stage.as_mut()
    }

    fn advance_unit(
        &mut self,
        rows: &RetainedBatchRows<'_>,
        reader: &NativeReader<'_>,
        counters: &mut BatchCounters,
        cancel: &CancelFlag,
    ) -> Result<Option<AdmittedBatch>> {
        check_cancel(cancel)?;
        ensure!(
            reader.pin() == self.captured,
            "retained captured pin changed"
        );
        ensure!(
            rows.batch_id() == self.batch_id,
            "retained batch selection changed"
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
        let phase = self.phase;
        let mut payload = 0u64;
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
                    let key = rows.capture_document_key_after(
                        entry.ordinal,
                        (&self.inventory_key.0, &self.inventory_key.1),
                    )?;
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
                        return Err(crate::store::staging::WitnessFallback(
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
                        let exclusion = rows.exclusion(&language, &path)?;
                        let assignments = rows.assignments(&language, &path)?;
                        let artifact = match assignments.as_slice() {
                            [(ordinal, source_hash)] => {
                                ensure!(
                                    exclusion.is_none()
                                        && *source_hash == hash
                                        && descriptor
                                            .artifacts
                                            .get(*ordinal as usize)
                                            .is_some_and(|a| a.languages.contains(&language)),
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
                                    return Err(crate::store::staging::WitnessFallback(
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
                    if !entry.languages.contains(language) {
                        self.missing_artifact += 1;
                        self.missing_proof.clear();
                    } else {
                        let proof =
                            rows.capture_proof_document_after(entry.ordinal, &self.missing_proof)?;
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
                            .filter(|entry| entry.languages.contains(language));
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
                let next = rows.exclusion_after((&self.exclusion_key.0, &self.exclusion_key.1))?;
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
                    self.capture_id = rows
                        .capture_id(&CaptureKey {
                            ordinal: entry.ordinal,
                            languages_json: &entry.languages_json(),
                            producer_id: &self.producer.name,
                            artifact_hash: &entry.sha256,
                            source_set_id: &descriptor.source_set.id,
                            captured_revision: &descriptor.source_revision,
                            decoder_version: &descriptor.decoder_version,
                            byte_length: entry.byte_length as i64,
                        })?
                        .ok_or_else(|| admission("retained artifact metadata"))?;
                    self.copy_offset = 0;
                    self.copy_hash = Sha256::new();
                    self.proof_cursor = 0;
                    self.document_cursor.clear();
                    self.proof_key.clear();
                    self.document_key = (String::new(), String::new());
                    self.proof_count = 0;
                    self.document_count = 0;
                    self.witness = WitnessCursor::default();
                    if self.trusted {
                        self.phase = Phase::Documents;
                    } else {
                        File::create_new(artifact_file(&dir, entry.ordinal))?;
                        self.phase = Phase::Copy;
                    }
                } else {
                    let captures = rows.capture_count(descriptor.artifacts.len() as i64 + 1)?;
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
                let mut buffer = vec![0; IO_BUFFER_BYTES as usize];
                let count = rows.read_capture(self.capture_id, self.copy_offset, &mut buffer)?;
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
            Phase::Proofs if self.witnessed => {
                let copy = self.copy.as_ref();
                let source_set_id = &descriptor.source_set.id;
                let mut copies = |language: &str, path: &str| match copy {
                    Some(filter) => rows.copies(
                        filter,
                        &crate::native_evidence::DocumentKey {
                            source_set_id: source_set_id.clone(),
                            language: language.to_owned(),
                            path: path.to_owned(),
                        },
                    ),
                    None => Ok(false),
                };
                match advance_witness(
                    rows,
                    self.capture_id,
                    &descriptor.artifacts[self.artifact],
                    conn,
                    &mut self.witness,
                    &self.limits,
                    &mut copies,
                )? {
                    WitnessUnit::Page { high_water, bytes } => {
                        payload = bytes.max(high_water);
                        counters.retained_admission_bytes_high_water =
                            counters.retained_admission_bytes_high_water.max(high_water);
                    }
                    WitnessUnit::Counted(proofs) => self.proof_count += proofs,
                    WitnessUnit::Done => {
                        // Witnessed admission is trusted (an untrusted one fell back above).
                        conn.execute(
                            "UPDATE artifacts SET claims=?2 WHERE ordinal=?1",
                            params![self.artifact as i64, self.proof_count],
                        )?;
                        self.artifact += 1;
                        self.phase = Phase::Artifact;
                    }
                }
            }
            Phase::Proofs => {
                // Trusted admission is witnessed (above): retained envelopes are not stored, so
                // only FULL, which staged its claims from the binary, compares proofs here.
                ensure!(!self.trusted, "retained trusted admission is witnessed");
                let bytes = advance_proof(
                    rows,
                    self.capture_id,
                    &descriptor.artifacts[self.artifact],
                    conn,
                    &mut self.proof_cursor,
                )?;
                if let Some(bytes) = bytes {
                    payload = payload.max(bytes);
                    counters.retained_admission_bytes_high_water =
                        counters.retained_admission_bytes_high_water.max(bytes);
                    self.proof_count += 1;
                } else {
                    self.retained_count = 0;
                    self.proof_key.clear();
                    self.phase = Phase::ProofInventory;
                }
            }
            Phase::Documents => {
                if let Some(retained) = advance_document(
                    rows,
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
                let next = rows.proof_id_after(self.capture_id, &self.proof_key)?;
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
                let next = rows.document_key_after(
                    self.capture_id,
                    (&self.document_key.0, &self.document_key.1),
                )?;
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
                    crate::store::staging::check_proof_identity_closure(conn)?;
                }
                return Ok(Some(AdmittedBatch::new(
                    AdmissionProof::admitted(),
                    self.stage.take().expect("stage"),
                    self.descriptor.take().expect("descriptor"),
                    self.sha.clone(),
                    self.producer.clone(),
                    self.captured,
                )));
            }
        }
        let changed = conn.total_changes() - before;
        // Row units measure the stage directory per MiB staged and at every phase change,
        // as the witness writer does; copy and decode units write in bulk and measure each.
        if phase != self.phase || matches!(phase, Phase::Copy | Phase::Decode) {
            self.unchecked = 0;
            budget.check()?;
        } else {
            budget.charge(
                &mut self.unchecked,
                payload.saturating_add(changed.saturating_mul(ROW_STAGE_BYTES)),
            )?;
        }
        counters.retained_admission_units += 1;
        counters.retained_admission_rows_high_water =
            counters.retained_admission_rows_high_water.max(changed);
        Ok(None)
    }
}

/// Witness proof identities read or staged per admission unit, and staged documents listed.
const WITNESS_PROOF_PAGE: i64 = 256;
/// Retained proof IDs a witnessed unit counts, from the capture's `(capture_id, proof_id)`
/// index alone.
const WITNESS_COUNT_PAGE: i64 = 16_384;

/// Witnessed S's private table of the proof identities it read, before it stages them in
/// proof-ID order.
const WITNESS_PENDING_TABLE: &str = "CREATE TABLE retained_witness_pending(artifact INTEGER NOT NULL, proof_id TEXT NOT NULL, record_kind TEXT NOT NULL, fact_digest TEXT NOT NULL, source_ord INTEGER NOT NULL, PRIMARY KEY(artifact,proof_id)) WITHOUT ROWID;";

/// Witnessed S's progress through one artifact's retained proofs (#1245).
#[derive(Default)]
struct WitnessCursor {
    step: WitnessStep,
    /// Read: the last staged document listed, by path.
    document: String,
    /// Read: the listed document S does not copy whose proofs are being read, as
    /// `(source ordinal, language, path)`, and its last `(record kind, fact digest)`.
    open: Option<(i64, String, String)>,
    key: (String, String),
    /// Stage and count: the last proof ID.
    proof: String,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum WitnessStep {
    /// Reads the proof identities of the documents S does not copy into the pending table.
    #[default]
    Read,
    /// Stages the pending identities in proof-ID order.
    Stage,
    /// Counts the capture's proofs.
    Count,
}

/// What one witnessed proof unit did.
enum WitnessUnit {
    /// Rows read or staged: the largest row reservation and the bytes reserved in all.
    Page { high_water: u64, bytes: u64 },
    /// This many more of the capture's proofs counted.
    Counted(i64),
    /// The artifact's proofs are staged and counted.
    Done,
}

/// The bytes one retained proof identity reserves before its fields are copied out.
fn witness_reservation(row: &WitnessProofRow<'_>, limits: &BatchLimits) -> Result<u64> {
    let mut bytes = 256_u64;
    for value in [
        row.proof_id,
        row.record_kind,
        row.fact_digest,
        row.language,
        row.path,
    ] {
        bytes = bytes.saturating_add((value.len() as u64).saturating_mul(2));
    }
    if bytes > limits.unit_expansion_bytes {
        return Err(ScipCaptureError::LimitExceeded("unit expansion bytes".into()).into());
    }
    Ok(bytes)
}

/// One bounded unit of witnessed S's proof identities for one artifact (#669 H1, #1245).
///
/// S stages the proof identities of only the documents it does not copy (`copies`, by
/// language and path). It lists the artifact's staged documents and reads each such
/// document's proofs through the capture's `(language, path, record kind, fact digest)` index,
/// never another document's. It then stages them in proof-ID order, each with the next lower
/// identity, as a walk of every retained proof in proof-ID order would. The documents S copies
/// keep the rows the import validated and published (#261); their proofs are not read. The
/// artifact's claim total counts every retained proof of the capture, copied or not, on the
/// capture's proof-ID index.
fn advance_witness(
    rows: &RetainedBatchRows<'_>,
    capture: i64,
    entry: &super::super::descriptor::ArtifactEntry,
    conn: &Connection,
    cursor: &mut WitnessCursor,
    limits: &BatchLimits,
    copies: &mut dyn FnMut(&str, &str) -> Result<bool>,
) -> Result<WitnessUnit> {
    let artifact = i64::from(entry.ordinal);
    let (mut high_water, mut bytes) = (0u64, 0u64);
    match cursor.step {
        WitnessStep::Read => {
            let mut insert =
                conn.prepare_cached("INSERT INTO retained_witness_pending VALUES(?1,?2,?3,?4,?5)")?;
            let (mut read, mut listed) = (0i64, 0i64);
            while read < WITNESS_PROOF_PAGE && listed < WITNESS_PROOF_PAGE {
                let Some((source, language, path)) = cursor.open.clone() else {
                    let next: Option<(i64, String, String)> = conn
                        .prepare_cached("SELECT d.source_ord,s.language,d.path FROM documents d JOIN sources s ON s.ord=d.source_ord WHERE d.artifact=?1 AND d.path>?2 ORDER BY d.path LIMIT 1")?
                        .query_row(params![artifact, cursor.document], |row| {
                            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                        })
                        .optional()?;
                    let Some((source, language, path)) = next else {
                        cursor.step = WitnessStep::Stage;
                        break;
                    };
                    listed += 1;
                    cursor.document.clone_from(&path);
                    if !copies(&language, &path)? {
                        cursor.open = Some((source, language, path));
                        cursor.key = (String::new(), String::new());
                    }
                    continue;
                };
                let limit = WITNESS_PROOF_PAGE - read;
                let mut found = 0i64;
                let after = cursor.key.clone();
                rows.document_proofs(
                    capture,
                    (&language, &path),
                    (&after.0, &after.1),
                    limit,
                    &mut |row| {
                        let reserved = witness_reservation(&row, limits)?;
                        #[cfg(feature = "test-support")]
                        crate::semantic_import::note_witness_proof_read();
                        insert.execute(params![
                            artifact,
                            row.proof_id,
                            row.record_kind,
                            row.fact_digest,
                            source
                        ])?;
                        cursor.key = (row.record_kind.to_owned(), row.fact_digest.to_owned());
                        found += 1;
                        high_water = high_water.max(reserved);
                        bytes = bytes.saturating_add(reserved);
                        Ok(())
                    },
                )?;
                read += found;
                if found < limit {
                    cursor.open = None;
                }
            }
        }
        WitnessStep::Stage => {
            let page: Vec<(String, String, String, i64)> = conn
                .prepare_cached("SELECT proof_id,record_kind,fact_digest,source_ord FROM retained_witness_pending WHERE artifact=?1 AND proof_id>?2 ORDER BY proof_id LIMIT ?3")?
                .query_map(params![artifact, cursor.proof, WITNESS_PROOF_PAGE], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<rusqlite::Result<_>>()?;
            let Some((last, ..)) = page.last() else {
                cursor.step = WitnessStep::Count;
                cursor.proof.clear();
                return Ok(WitnessUnit::Page { high_water, bytes });
            };
            cursor.proof.clone_from(last);
            let mut identity: i64 = conn
                .prepare_cached(
                    "SELECT identity_id FROM witness_proofs ORDER BY identity_id LIMIT 1",
                )?
                .query_row([], |row| row.get(0))
                .optional()?
                .unwrap_or(0);
            let mut insert =
                conn.prepare_cached("INSERT INTO witness_proofs VALUES(?1,?2,?3,?4,?5,?6,?7)")?;
            for (proof, kind, digest, source) in &page {
                identity = identity
                    .checked_sub(1)
                    .ok_or_else(|| admission("witness proof identity exhausted"))?;
                crate::store::staging::charge_witness_row(conn, limits)?;
                insert.execute(params![
                    identity, artifact, source, kind, digest, proof, capture
                ])?;
                let reserved =
                    256_u64.saturating_add(2 * (proof.len() + kind.len() + digest.len()) as u64);
                high_water = high_water.max(reserved);
                bytes = bytes.saturating_add(reserved);
            }
        }
        WitnessStep::Count => {
            let (counted, last) =
                rows.proof_ids_after(capture, &cursor.proof, WITNESS_COUNT_PAGE)?;
            let Some(last) = last else {
                return Ok(WitnessUnit::Done);
            };
            cursor.proof = last;
            return Ok(WitnessUnit::Counted(counted));
        }
    }
    Ok(WitnessUnit::Page { high_water, bytes })
}

type StagedDocumentRow = (i64, String, String, String, String, i64);
type StagedProofRow = (i64, String, String, String, String, String, i64);

fn advance_document(
    rows: &RetainedBatchRows<'_>,
    capture: i64,
    entry: &super::super::descriptor::ArtifactEntry,
    conn: &Connection,
    trusted: bool,
    cursor: &mut String,
    key: &mut (String, String),
) -> Result<Option<bool>> {
    if trusted {
        let Some(RetainedDocument {
            language,
            path,
            source_hash: hash,
            wire_digest: wire,
            capped,
        }) = rows.document_after(capture, (&key.0, &key.1))?
        else {
            return Ok(None);
        };
        ensure!(
            entry.languages.contains(&language),
            "retained document language"
        );
        let source = crate::store::staging::lookup_assigned(conn, entry.ordinal, &path)?
            .ok_or_else(|| admission("retained unlisted document"))?;
        ensure!(source.language == language, "retained document language");
        ensure!(source.content_hash == hash, "retained document source");
        conn.execute(
            "INSERT INTO documents VALUES(?1,?2,?3,?4,?5)",
            params![source.ord, entry.ordinal, path, wire, capped],
        )?;
        *key = (language, path);
    } else {
        let row: Option<StagedDocumentRow> = conn.prepare_cached("SELECT d.source_ord,s.language,s.path,s.content_hash,d.wire_digest,d.capped FROM documents d JOIN sources s ON s.ord=d.source_ord WHERE d.artifact=?1 AND d.path>?2 ORDER BY d.path LIMIT 1")?.query_row(
            params![entry.ordinal, cursor.as_str()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        ).optional()?;
        let Some((_id, language, path, source_hash, wire_digest, capped)) = row else {
            return Ok(None);
        };
        let staged = RetainedDocument {
            language,
            path,
            source_hash,
            wire_digest,
            capped,
        };
        let same = rows.document_matches(capture, &staged)?;
        ensure!(
            same.unwrap_or(true),
            "retained document differs from authenticated binary"
        );
        *cursor = staged.path;
        // Missing accelerator rows remain recoverable; only retained rows count.
        return Ok(Some(same.is_some()));
    }
    Ok(Some(true))
}

/// FULL catch-up: the next proof staged from the authenticated binary must be the retained
/// proof of the same identity. Trusted admission is witnessed and walks retained proof
/// identities instead ([`advance_witness`]); retained envelopes are not stored (#1248 PR 3).
fn advance_proof(
    rows: &RetainedBatchRows<'_>,
    capture: i64,
    entry: &super::super::descriptor::ArtifactEntry,
    conn: &Connection,
    cursor: &mut i64,
) -> Result<Option<u64>> {
    let row: Option<StagedProofRow> = conn.prepare_cached("SELECT c.claim_id,c.proof_id,c.record_kind,c.fact_digest,s.language,s.path,length(c.canonical_bytes) FROM claims c JOIN sources s ON s.ord=c.source_ord WHERE c.artifact=?1 AND c.claim_id>?2 ORDER BY c.claim_id LIMIT 1")?.query_row(
        params![entry.ordinal, *cursor], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?)),
    ).optional()?;
    let Some((id, proof_id, record_kind, fact_digest, language, path, byte_length)) = row else {
        return Ok(None);
    };
    let staged = RetainedProof {
        proof_id,
        record_kind,
        fact_digest,
        language,
        path,
    };
    let same = rows.proof_matches(capture, &staged)?.unwrap_or(false);
    ensure!(same, "retained proof differs from authenticated binary");
    *cursor = id;
    Ok(Some(u64::try_from(byte_length)?))
}
#[cfg(test)]
mod tests {
    use super::*;

    fn test_pin() -> IndexPin {
        IndexPin {
            index_generation: uuid::Uuid::nil(),
            index_revision: 1,
        }
    }

    /// The walk witnessed S did before #1245, as it was: every retained proof of the capture in
    /// proof-ID order, reserving and checking each, staging the proofs of the documents it does
    /// not copy with the next lower identity. Returns the proofs it walked.
    fn full_walk(
        rows: &RetainedBatchRows<'_>,
        capture: i64,
        entry: &super::super::super::descriptor::ArtifactEntry,
        conn: &Connection,
        limits: &BatchLimits,
        copied: &[&str],
    ) -> Result<i64> {
        let (mut key, mut walked) = (String::new(), 0i64);
        loop {
            let mut page = Vec::new();
            rows.witness_proofs(capture, &key, WITNESS_PROOF_PAGE, &mut |row| {
                witness_reservation(&row, limits)?;
                ensure!(
                    entry.languages.iter().any(|l| l == row.language),
                    "retained proof language"
                );
                page.push((
                    row.proof_id.to_owned(),
                    row.record_kind.to_owned(),
                    row.fact_digest.to_owned(),
                    row.path.to_owned(),
                ));
                Ok(())
            })?;
            let Some((last, ..)) = page.last() else {
                return Ok(walked);
            };
            key = last.clone();
            for (proof, kind, digest, path) in page {
                walked += 1;
                if copied.contains(&path.as_str()) {
                    continue;
                }
                let source = crate::store::staging::lookup_assigned(conn, entry.ordinal, &path)?
                    .ok_or_else(|| admission("retained unlisted proof"))?
                    .ord;
                let identity: i64 = conn
                    .query_row(
                        "SELECT identity_id FROM witness_proofs ORDER BY identity_id LIMIT 1",
                        [],
                        |row| row.get(0),
                    )
                    .optional()?
                    .unwrap_or(0);
                conn.execute(
                    "INSERT INTO witness_proofs VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![
                        identity - 1,
                        entry.ordinal,
                        source,
                        kind,
                        digest,
                        proof,
                        capture
                    ],
                )?;
            }
        }
    }

    /// Drives [`advance_witness`] over each `(artifact, capture)` to its end. Returns each
    /// artifact's counted proofs and the most rows one unit changed.
    fn witness_all(
        rows: &RetainedBatchRows<'_>,
        conn: &Connection,
        artifacts: &[(super::super::super::descriptor::ArtifactEntry, i64)],
        limits: &BatchLimits,
        copied: &[&str],
    ) -> Result<(Vec<i64>, u64)> {
        let (mut claims, mut most) = (Vec::new(), 0u64);
        for (entry, capture) in artifacts {
            let (mut cursor, mut counted) = (WitnessCursor::default(), 0i64);
            loop {
                let before = conn.total_changes();
                let unit = advance_witness(
                    rows,
                    *capture,
                    entry,
                    conn,
                    &mut cursor,
                    limits,
                    &mut |_, path| Ok(copied.contains(&path)),
                )?;
                most = most.max(conn.total_changes() - before);
                match unit {
                    WitnessUnit::Page { .. } => {}
                    WitnessUnit::Counted(proofs) => counted += proofs,
                    WitnessUnit::Done => break,
                }
            }
            claims.push(counted);
        }
        Ok((claims, most))
    }

    /// Two artifacts, two documents each. `a.java` and `d.java` hold more proofs than one
    /// unit reads, and every capture's proof IDs interleave its documents, so proof-ID order
    /// differs from document order. `b.java` and `c.java` are the ones S copies.
    fn witness_fixture() -> (
        tempfile::TempDir,
        Stage,
        Connection,
        Vec<(super::super::super::descriptor::ArtifactEntry, i64)>,
    ) {
        let parent = tempfile::tempdir().unwrap();
        let mut stage = Stage::create(parent.path(), "test", "witness").unwrap();
        stage.conn().unwrap().execute_batch(&format!("INSERT INTO witness_state(singleton) VALUES(1); {WITNESS_PENDING_TABLE} INSERT INTO sources VALUES(0,'java','a.java','h',1),(1,'java','b.java','h',1),(2,'java','c.java','h',1),(3,'java','d.java','h',1); INSERT INTO coverage VALUES(0,0,NULL),(1,0,NULL),(2,1,NULL),(3,1,NULL); INSERT INTO documents VALUES(0,0,'a.java','w',0),(1,0,'b.java','w',0),(2,1,'c.java','w',0),(3,1,'d.java','w',0)")).unwrap();
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE semantic_raw_proof_documents(document_id INTEGER PRIMARY KEY,capture_id INTEGER NOT NULL,language TEXT NOT NULL,path TEXT NOT NULL,UNIQUE(capture_id,language,path)); INSERT INTO semantic_raw_proof_documents(capture_id,language,path) VALUES(10,'java','a.java'),(10,'java','b.java'),(20,'java','c.java'),(20,'java','d.java'); CREATE TABLE semantic_raw_proofs(capture_id INTEGER NOT NULL,proof_id BLOB NOT NULL,record_kind TEXT NOT NULL,fact_digest BLOB NOT NULL,document_id INTEGER NOT NULL,UNIQUE(capture_id,proof_id),UNIQUE(document_id,record_kind,fact_digest))").unwrap();
        let kinds = ["occurrence", "symbolInformation", "relationship"];
        // Proof IDs are unique across captures, as their artifact-bound digests are.
        let mut k = 0u64;
        for (capture, documents) in [
            (10, [("a.java", 300), ("b.java", 40)]),
            (20, [("c.java", 5), ("d.java", 270)]),
        ] {
            for (path, proofs) in documents {
                for _ in 0..proofs {
                    k += 1;
                    // A permutation of 1..1009: proof IDs interleave the documents.
                    db.execute(
                        "INSERT INTO semantic_raw_proofs VALUES(?1,unhex(?2),?3,unhex(?4),(SELECT document_id FROM semantic_raw_proof_documents WHERE capture_id=?1 AND language='java' AND path=?5))",
                        params![
                            capture,
                            format!("{:064}", k * 37 % 1009),
                            kinds[(k % 3) as usize],
                            format!("{k:064x}"),
                            path
                        ],
                    )
                    .unwrap();
                }
            }
        }
        let entry = |ordinal: u32| super::super::super::descriptor::ArtifactEntry {
            ordinal,
            relative_path: format!("raw{ordinal}"),
            languages: vec!["java".into()],
            sha256: format!("hash{ordinal}"),
            byte_length: 0,
        };
        (parent, stage, db, vec![(entry(0), 10), (entry(1), 20)])
    }

    type StagedWitnessRow = (i64, i64, i64, String, String, String, i64);

    fn staged_witness_proofs(conn: &Connection) -> Vec<StagedWitnessRow> {
        conn.prepare("SELECT identity_id,artifact,source_ord,record_kind,fact_digest,proof_id,capture_id FROM witness_proofs ORDER BY identity_id DESC")
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// #1245: reading only the documents S does not copy stages exactly what the full
    /// proof-ordered walk staged, with the same identities in the same order, and counts the
    /// same claims, for one artifact and for several. No unit changes more than one page of
    /// rows (a staged row also charges `witness_state`).
    #[test]
    fn witnessed_proofs_match_the_full_walk_with_its_identities() {
        let limits = BatchLimits::PRODUCTION;
        let copied = ["b.java", "c.java"];
        for artifacts in [1, 2] {
            let (_reference_dir, mut reference, db, entries) = witness_fixture();
            let entries = &entries[..artifacts];
            let rows = RetainedBatchRows::for_tests(&db, 1, test_pin());
            let reference = reference.conn().unwrap();
            let walked: Vec<i64> = entries
                .iter()
                .map(|(entry, capture)| {
                    full_walk(&rows, *capture, entry, reference, &limits, &copied).unwrap()
                })
                .collect();
            let expected = staged_witness_proofs(reference);
            let (_dir, mut stage, ..) = witness_fixture();
            let conn = stage.conn().unwrap();
            let (claims, most) = witness_all(&rows, conn, entries, &limits, &copied).unwrap();
            assert_eq!(claims, walked);
            assert_eq!(staged_witness_proofs(conn), expected);
            // The copied documents stage nothing; every proof of the others is staged.
            let (staged, total) = if artifacts == 1 {
                (300, 340)
            } else {
                (570, 615)
            };
            assert_eq!(expected.len(), staged);
            assert_eq!(claims.iter().sum::<i64>(), total);
            assert!(most <= 2 * WITNESS_PROOF_PAGE as u64, "{most}");
        }
    }

    /// #1245, #261: S no longer reads a copied document's retained proofs, so a row there that
    /// the full walk refused does not stop it; it is still counted. The same row in a document
    /// S does not copy still fails typed.
    #[test]
    fn witnessed_proofs_read_only_documents_s_does_not_copy() {
        let limits = BatchLimits {
            unit_expansion_bytes: 2_000,
            ..BatchLimits::PRODUCTION
        };
        let copied = ["b.java", "c.java"];
        for (path, in_copied) in [("b.java", true), ("a.java", false)] {
            let (_dir, mut stage, db, entries) = witness_fixture();
            // An oversized proof ID (1,000 hex digits) that sorts before every other one, so
            // the count's page cursor is never this row's.
            db.execute(
                "INSERT INTO semantic_raw_proofs VALUES(10,zeroblob(500),'occurrence',unhex(?1),(SELECT document_id FROM semantic_raw_proof_documents WHERE capture_id=10 AND path=?2))",
                params!["f".repeat(64), path],
            )
            .unwrap();
            let rows = RetainedBatchRows::for_tests(&db, 1, test_pin());
            let conn = stage.conn().unwrap();
            let entries = &entries[..1];
            let walk = full_walk(&rows, 10, &entries[0].0, conn, &limits, &copied);
            assert!(walk.unwrap_err().is::<ScipCaptureError>());
            conn.execute("DELETE FROM witness_proofs", []).unwrap();
            let admitted = witness_all(&rows, conn, entries, &limits, &copied);
            if in_copied {
                assert_eq!(admitted.unwrap().0, [341]);
                assert_eq!(staged_witness_proofs(conn).len(), 300);
            } else {
                let error = admitted.unwrap_err();
                assert!(
                    matches!(
                        error.downcast_ref::<ScipCaptureError>(),
                        Some(ScipCaptureError::LimitExceeded(what)) if what == "unit expansion bytes"
                    ),
                    "{error:#}"
                );
            }
        }
    }

    #[test]
    fn full_document_comparison_yields_even_when_accelerator_is_absent() {
        let parent = tempfile::tempdir().unwrap();
        let mut stage = Stage::create(parent.path(), "test", "compare").unwrap();
        let conn = stage.conn().unwrap();
        conn.execute_batch("INSERT INTO sources VALUES(0,'java','a.java','h',1),(1,'java','b.java','h',1); INSERT INTO artifacts(ordinal,path,languages,sha256,byte_length) VALUES(0,'raw','[\"java\"]','hash',0); INSERT INTO documents VALUES(0,0,'a.java','wa',0),(1,0,'b.java','wb',0)").unwrap();
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE semantic_raw_documents(capture_id INTEGER,language TEXT,path TEXT,source_hash TEXT,wire_digest TEXT,capped INTEGER); INSERT INTO semantic_raw_documents VALUES(1,'java','b.java','h','wb',0)").unwrap();
        let rows = RetainedBatchRows::for_tests(&db, 1, test_pin());
        let entry = super::super::super::descriptor::ArtifactEntry {
            ordinal: 0,
            relative_path: "raw".into(),
            languages: vec!["java".into()],
            sha256: "hash".into(),
            byte_length: 0,
        };
        let mut cursor = String::new();
        assert_eq!(
            advance_document(
                &rows,
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
                &rows,
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
                &rows,
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
    fn trusted_documents_preserve_wire_identity_one_row_per_call() {
        let parent = tempfile::tempdir().unwrap();
        let mut stage = Stage::create(parent.path(), "test", "trusted").unwrap();
        let conn = stage.conn().unwrap();
        conn.execute_batch("INSERT INTO sources VALUES(0,'java','a.java','ha',1),(1,'java','b.java','hb',1); INSERT INTO coverage VALUES(0,0,NULL),(1,0,NULL); INSERT INTO artifacts(ordinal,path,languages,sha256,byte_length) VALUES(0,'raw','[\"java\"]','hash',0)").unwrap();
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE semantic_raw_documents(capture_id INTEGER,language TEXT,path TEXT,source_hash TEXT,wire_digest TEXT,capped INTEGER,PRIMARY KEY(capture_id,language,path)); INSERT INTO semantic_raw_documents VALUES(1,'java','a.java','ha','wire-a',0),(1,'java','b.java','hb','wire-b',1)").unwrap();
        let rows = RetainedBatchRows::for_tests(&db, 1, test_pin());
        let entry = super::super::super::descriptor::ArtifactEntry {
            ordinal: 0,
            relative_path: "raw".into(),
            languages: vec!["java".into()],
            sha256: "hash".into(),
            byte_length: 0,
        };
        let mut key = (String::new(), String::new());
        let mut cursor = String::new();
        for expected in [1, 2] {
            assert_eq!(
                advance_document(&rows, 1, &entry, conn, true, &mut cursor, &mut key).unwrap(),
                Some(true)
            );
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, expected);
        }
        assert_eq!(
            advance_document(&rows, 1, &entry, conn, true, &mut cursor, &mut key).unwrap(),
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
    }

    /// FULL catch-up compares each claim it staged from the authenticated binary with the
    /// retained proof of its ID by identity (kind, fact digest, language, path), one row per
    /// call; the retained envelope bytes are not stored (#1248 PR 3). A changed proof, one
    /// whose fact digest or path differs under the same ID, or one the capture does not
    /// retain, fails typed.
    #[test]
    fn full_catch_up_compares_staged_proofs_by_identity_one_row_per_call() {
        let (p1, p2, d1, d2) = (
            "1".repeat(64),
            "2".repeat(64),
            "a".repeat(64),
            "b".repeat(64),
        );
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE semantic_raw_proof_documents(document_id INTEGER PRIMARY KEY,capture_id INTEGER,language TEXT,path TEXT); INSERT INTO semantic_raw_proof_documents VALUES(7,1,'java','a.java'); CREATE TABLE semantic_raw_proofs(capture_id INTEGER,proof_id BLOB,record_kind TEXT,fact_digest BLOB,document_id INTEGER,PRIMARY KEY(capture_id,proof_id))").unwrap();
        for (proof, digest) in [(&p1, &d1), (&p2, &d2)] {
            db.execute("INSERT INTO semantic_raw_proofs VALUES(1,unhex(?1),'symbolInformation',unhex(?2),7)", params![proof, digest]).unwrap();
        }
        let rows = RetainedBatchRows::for_tests(&db, 1, test_pin());
        let entry = super::super::super::descriptor::ArtifactEntry {
            ordinal: 0,
            relative_path: "raw".into(),
            languages: vec!["java".into()],
            sha256: "hash".into(),
            byte_length: 0,
        };
        let bytes = br#"{"formatVersion":1,"recordKind":"symbolInformation","relativePath":"a.java","positionEncoding":1,"raw":{"symbol":"A#"}}"#;
        let walk = |claims: &[(&str, &str, &str)]| -> (Vec<u64>, Option<String>) {
            let parent = tempfile::tempdir().unwrap();
            let mut stage = Stage::create(parent.path(), "test", "full").unwrap();
            let conn = stage.conn().unwrap();
            conn.execute_batch(
                "INSERT INTO sources VALUES(0,'java','a.java','ha',1),(1,'java','b.java','hb',1)",
            )
            .unwrap();
            for (proof, digest, path) in claims {
                conn.execute("INSERT INTO claims(artifact,source_ord,record_kind,fact_digest,proof_id,canonical_bytes,presence) VALUES(0,(SELECT ord FROM sources WHERE path=?3),'symbolInformation',?2,?1,?4,0)", params![proof, digest, path, bytes.as_slice()]).unwrap();
            }
            let (mut lengths, mut cursor) = (Vec::new(), 0);
            loop {
                match advance_proof(&rows, 1, &entry, conn, &mut cursor) {
                    Ok(Some(length)) => lengths.push(length),
                    Ok(None) => return (lengths, None),
                    Err(error) => return (lengths, Some(error.to_string())),
                }
            }
        };
        let length = bytes.len() as u64;
        assert_eq!(
            walk(&[(&p1, &d1, "a.java"), (&p2, &d2, "a.java")]),
            (vec![length, length], None)
        );
        let differs = Some("retained proof differs from authenticated binary".to_owned());
        // A changed proof: the same ID with another fact digest.
        let changed = "c".repeat(64);
        assert_eq!(
            walk(&[(&p1, &d1, "a.java"), (&p2, &changed, "a.java")]),
            (vec![length], differs.clone())
        );
        assert_eq!(walk(&[(&p1, &d1, "b.java")]), (vec![], differs.clone()));
        let unretained = "3".repeat(64);
        assert_eq!(walk(&[(&unretained, &d1, "a.java")]), (vec![], differs));
    }
}
