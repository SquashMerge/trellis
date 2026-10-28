//! The admission driver: descriptor, source pass, coverage pass, artifact copy and streamed
//! decode, then the closure checks. Pages and artifacts arrive through `BatchInputs` and the
//! retained native revision through `NativeSource`, so the file-backed driver and the
//! in-memory test admission run the same passes.
use super::{
    AdmittedBatch, BatchCounters, BatchLimits,
    descriptor::{
        ArtifactEntry, Descriptor, Disposition, PageEntry, PageKind, RowOrder,
        SourceManifestHasher, SourceRow, check_page_bytes, parse_page,
    },
    limits::STAGE_CHECK_COPIED_BYTES,
    staging::{Stage, StageBudget, StageClaimSink, artifact_file, lookup_assigned},
};
use crate::{
    index_coordinator::{CopiedInput, copy_semantic_input_to_stage, open_semantic_input},
    model::{CancelFlag, IndexPin},
    native_evidence as native,
    scip_admission::{PinnedProducer, admission, order},
    scip_raw::{self, AuthenticatedArtifact, DocumentSource, ScipCaptureError, decode_streaming},
    store::{InventoryStream, NativeReader, Store},
};
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::de::DeserializeOwned;
use std::{borrow::Cow, io::Read, path::Path, sync::atomic::Ordering};

/// Who owns the stage: the claimed request and its claim incarnation.
#[derive(Debug, Clone, Copy)]
pub struct StageOwner<'a> {
    pub request_id: &'a str,
    pub incarnation: &'a str,
}

/// The retained captured revision: its inventory in stored order and its source bytes.
pub(crate) trait NativeSource {
    fn next_inventory(&mut self) -> Result<Option<native::Document>>;
    fn source_bytes(&mut self, document: &native::Document) -> Result<Vec<u8>>;
}

/// Page and artifact bytes named by the descriptor.
pub(crate) trait BatchInputs {
    /// One page's bytes, read whole. The length must equal the descriptor entry.
    fn page(&mut self, entry: &PageEntry) -> Result<Vec<u8>>;
    /// Copies one artifact to `dest`, reporting the running byte total.
    fn copy_artifact(
        &mut self,
        entry: &ArtifactEntry,
        dest: &Path,
        limits: &BatchLimits,
        progress: &mut dyn FnMut(u64) -> Result<()>,
    ) -> Result<CopiedInput>;
}

/// Everything a pass needs besides the stage and the sources of bytes.
pub(crate) struct Passes<'a> {
    pub descriptor: &'a Descriptor,
    pub producer: &'a PinnedProducer,
    pub limits: &'a BatchLimits,
    pub cancel: &'a CancelFlag,
    pub budget: StageBudget,
}

fn check_cancel(cancel: &CancelFlag) -> std::result::Result<(), ScipCaptureError> {
    if cancel.load(Ordering::Relaxed) {
        Err(ScipCaptureError::Cancelled)
    } else {
        Ok(())
    }
}

fn read_page<R: DeserializeOwned>(
    inputs: &mut dyn BatchInputs,
    entry: &PageEntry,
    counters: &mut BatchCounters,
) -> Result<super::descriptor::Page<R>> {
    let bytes = inputs.page(entry)?;
    counters.page_buffer_begin(bytes.len() as u64);
    let parsed = (|| {
        check_page_bytes(entry, &bytes)?;
        let page = parse_page::<R>(&bytes)?;
        page.check_entry(entry)?;
        Ok::<_, ScipCaptureError>(page)
    })();
    drop(bytes);
    counters.page_buffer_end();
    Ok(parsed?)
}

fn key(language: &str, path: &str) -> (usize, Vec<u8>) {
    (order(language), path.as_bytes().to_vec())
}

impl Passes<'_> {
    /// Stores the descriptor and its artifact entries before any page is read.
    pub fn begin(&self, conn: &Connection, descriptor_bytes: &[u8], sha256: &str) -> Result<()> {
        conn.execute(
            "INSERT INTO header(singleton, descriptor_sha256, descriptor_bytes) VALUES(1,?1,?2)",
            params![sha256, descriptor_bytes],
        )?;
        for artifact in &self.descriptor.artifacts {
            conn.execute(
                "INSERT INTO artifacts(ordinal, path, language, sha256, byte_length) VALUES(?1,?2,?3,?4,?5)",
                params![
                    artifact.ordinal,
                    artifact.relative_path,
                    artifact.language,
                    artifact.sha256,
                    artifact.byte_length as i64
                ],
            )?;
        }
        Ok(())
    }

    /// Source pages: strictly ordered rows, merged one-to-one against the streamed retained
    /// inventory, hashed against the descriptor and staged.
    pub fn source_pass(
        &self,
        conn: &Connection,
        inputs: &mut dyn BatchInputs,
        native: &mut dyn NativeSource,
        counters: &mut BatchCounters,
    ) -> Result<()> {
        let descriptor = self.descriptor;
        let mut rows_order = RowOrder::source();
        let mut hasher = SourceManifestHasher::new(&descriptor.source_set.id);
        let mut ord = 0i64;
        for entry in descriptor
            .pages
            .iter()
            .filter(|entry| entry.kind == PageKind::Source)
        {
            check_cancel(self.cancel)?;
            let page = read_page::<SourceRow>(inputs, entry, counters)?;
            conn.execute_batch("BEGIN")?;
            for row in &page.rows {
                row.check(&descriptor.source_set.languages)?;
                rows_order.push(&row.language, &row.relative_path)?;
            }
            for row in &page.rows {
                if ord as u64 >= self.limits.source_documents {
                    return Err(ScipCaptureError::LimitExceeded("source documents".into()).into());
                }
                let Some(retained) = native.next_inventory()? else {
                    return Err(admission("extra captured source").into());
                };
                let mismatch = match key(&row.language, &row.relative_path)
                    .cmp(&key(&retained.key.language, &retained.key.path))
                {
                    std::cmp::Ordering::Less => Some("extra captured source"),
                    std::cmp::Ordering::Greater => Some("missing captured source"),
                    std::cmp::Ordering::Equal => (retained.content_hash != row.content_hash
                        || retained.byte_length as u64 != row.byte_length
                        || retained.key.source_set_id != descriptor.source_set.id
                        || retained.revision_id != descriptor.source_revision)
                        .then_some("captured source mismatch"),
                };
                if let Some(detail) = mismatch {
                    return Err(admission(detail).into());
                }
                let bytes = native.source_bytes(&retained)?;
                if bytes.len() as u64 != row.byte_length
                    || scip_raw::sha(&bytes) != row.content_hash
                {
                    return Err(admission("captured source hash").into());
                }
                hasher.push(&row.language, &row.relative_path, &row.content_hash);
                conn.prepare_cached(
                    "INSERT INTO sources(ord, language, path, content_hash, byte_length) VALUES(?1,?2,?3,?4,?5)",
                )?
                .execute(params![
                    ord,
                    row.language,
                    row.relative_path,
                    row.content_hash,
                    row.byte_length as i64
                ])?;
                ord += 1;
            }
            conn.execute_batch("COMMIT")?;
            self.budget.check()?;
        }
        if native.next_inventory()?.is_some() {
            return Err(admission("missing captured source").into());
        }
        if hasher.finish() != descriptor.source_manifest_hash {
            return Err(admission("source manifest hash").into());
        }
        Ok(())
    }

    /// Coverage pages: strictly ordered rows, each bound to a staged source and either an
    /// artifact of its language or a closed exclusion reason.
    pub fn coverage_pass(
        &self,
        conn: &Connection,
        inputs: &mut dyn BatchInputs,
        counters: &mut BatchCounters,
    ) -> Result<()> {
        let descriptor = self.descriptor;
        let mut rows_order = RowOrder::coverage();
        let (mut covered, mut excluded) = (0u64, 0u64);
        for entry in descriptor
            .pages
            .iter()
            .filter(|entry| entry.kind == PageKind::Coverage)
        {
            check_cancel(self.cancel)?;
            let page = read_page::<super::descriptor::CoverageRow>(inputs, entry, counters)?;
            conn.execute_batch("BEGIN")?;
            for row in &page.rows {
                rows_order.push(&row.language, &row.relative_path)?;
                let disposition = row.disposition()?;
                let source: Option<i64> = {
                    let mut statement = conn
                        .prepare_cached("SELECT ord FROM sources WHERE language=?1 AND path=?2")?;
                    let mut rows = statement.query(params![row.language, row.relative_path])?;
                    rows.next()?.map(|r| r.get(0)).transpose()?
                };
                let Some(source) = source else {
                    return Err(admission("unbound coverage path").into());
                };
                if !self.producer.languages.contains(&row.language) {
                    return Err(admission("unbound coverage path").into());
                }
                let (artifact, exclusion) = match disposition {
                    Disposition::Covered { artifact } => {
                        let bound = descriptor
                            .artifacts
                            .get(artifact as usize)
                            .is_some_and(|entry| entry.language == row.language);
                        if !bound {
                            return Err(admission("coverage artifact").into());
                        }
                        covered += 1;
                        (Some(artifact), None)
                    }
                    Disposition::Excluded(reason) => {
                        excluded += 1;
                        (None, Some(reason.as_str()))
                    }
                };
                conn.prepare_cached(
                    "INSERT INTO coverage(source_ord, artifact, exclusion) VALUES(?1,?2,?3)",
                )?
                .execute(params![source, artifact, exclusion])?;
            }
            conn.execute_batch("COMMIT")?;
            self.budget.check()?;
        }
        let mut requested = 0u64;
        for language in &self.producer.languages {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sources WHERE language=?1",
                [language],
                |r| r.get(0),
            )?;
            requested += count as u64;
        }
        if covered + excluded != requested {
            return Err(admission("coverage completion").into());
        }
        if covered != descriptor.totals.covered_documents
            || excluded != descriptor.totals.excluded_documents
        {
            return Err(admission("page totals").into());
        }
        Ok(())
    }

    /// Copies each artifact into the stage, authenticates the copy against the descriptor,
    /// then decodes the staged copy into `claims`.
    pub fn artifact_pass(
        &self,
        stage_dir: &Path,
        conn: &Connection,
        inputs: &mut dyn BatchInputs,
        native: &mut dyn NativeSource,
        counters: &mut BatchCounters,
    ) -> Result<()> {
        let descriptor = self.descriptor;
        for entry in &descriptor.artifacts {
            check_cancel(self.cancel)?;
            let dest = artifact_file(stage_dir, entry.ordinal);
            let mut last_check = 0u64;
            let copied = inputs.copy_artifact(entry, &dest, self.limits, &mut |total| {
                check_cancel(self.cancel)?;
                if total - last_check >= STAGE_CHECK_COPIED_BYTES {
                    last_check = total;
                    self.budget.check()?;
                }
                Ok(())
            })?;
            self.budget.check()?;
            if copied.len != entry.byte_length {
                return Err(admission("artifact length").into());
            }
            if copied.sha256 != entry.sha256 {
                return Err(admission("artifact digest").into());
            }
            self.decode(entry, &dest, conn, native, counters)?;
            #[cfg(all(feature = "test-support", unix))]
            if entry.ordinal == 0 {
                pause_after_first_artifact()?;
            }
        }
        let unseen: i64 = conn.query_row(
            "SELECT COUNT(*) FROM coverage c WHERE c.artifact IS NOT NULL \
             AND NOT EXISTS (SELECT 1 FROM documents d WHERE d.source_ord=c.source_ord)",
            [],
            |r| r.get(0),
        )?;
        if unseen != 0 {
            return Err(
                ScipCaptureError::InvalidArtifact("missing artifact document".into()).into(),
            );
        }
        Ok(())
    }

    fn decode(
        &self,
        entry: &ArtifactEntry,
        staged: &Path,
        conn: &Connection,
        native: &mut dyn NativeSource,
        counters: &mut BatchCounters,
    ) -> Result<()> {
        let descriptor = self.descriptor;
        let assigned: i64 = conn.query_row(
            "SELECT COUNT(*) FROM coverage WHERE artifact=?1",
            [entry.ordinal],
            |r| r.get(0),
        )?;
        let context = AuthenticatedArtifact {
            artifact_hash: &entry.sha256,
            producer_id: &self.producer.name,
            producer_version: &self.producer.version,
            source_set_id: &descriptor.source_set.id,
            source_revision: &descriptor.source_revision,
            position_encoding: self.producer.position_encoding,
            protocol_version: descriptor.metadata.protocol_version,
            text_document_encoding: descriptor.metadata.text_document_encoding,
            project_root_uri: &descriptor.metadata.project_root,
            assigned_documents: assigned as u64,
        };
        let mut sink = StageClaimSink::new(conn, &self.budget, entry.ordinal, &entry.language);
        let mut fault: Option<anyhow::Error> = None;
        let mut resolve = |path: &str, language: &str| {
            let found = lookup_assigned(conn, entry.ordinal, &entry.language, path);
            let assigned = match found {
                Ok(Some(assigned)) => assigned,
                Ok(None) => {
                    return Err(ScipCaptureError::InvalidArtifact(
                        "unlisted document".into(),
                    ));
                }
                Err(error) => {
                    fault = Some(anyhow::Error::new(error).context("stage read"));
                    return Err(ScipCaptureError::InvalidArtifact("stage storage".into()));
                }
            };
            if language != entry.language {
                return Err(ScipCaptureError::InvalidArtifact(
                    "document source binding".into(),
                ));
            }
            let document = native::Document {
                key: native::DocumentKey {
                    source_set_id: descriptor.source_set.id.clone(),
                    language: entry.language.clone(),
                    path: path.to_owned(),
                },
                revision_id: descriptor.source_revision.clone(),
                content_hash: assigned.content_hash.clone(),
                byte_length: assigned.byte_length,
            };
            match native.source_bytes(&document) {
                Ok(bytes) => Ok(DocumentSource {
                    bytes: Cow::Owned(bytes),
                    content_hash: assigned.content_hash,
                }),
                Err(error) => {
                    fault = Some(error);
                    Err(ScipCaptureError::InvalidArtifact("source read".into()))
                }
            }
        };
        let file = std::fs::File::open(staged).context("staged artifact")?;
        let decoded = decode_streaming(
            file,
            entry.byte_length,
            &context,
            &mut resolve,
            counters,
            self.limits,
            &mut sink,
            self.cancel,
        );
        if let Err(error) = decoded {
            return Err(sink.take_fault().or(fault).unwrap_or_else(|| error.into()));
        }
        sink.finish()?;
        Ok(())
    }
}

/// Test hook: once artifact 0 is staged, binds `TRELLIS_TEST_SEMANTIC_STAGING_SOCKET`, signals
/// `.ready` and blocks until the test connects. A `.done` marker skips the pause, so a
/// recovering run proceeds.
#[cfg(all(feature = "test-support", unix))]
fn pause_after_first_artifact() -> Result<()> {
    use std::os::unix::net::UnixListener;
    let Some(path) = std::env::var_os("TRELLIS_TEST_SEMANTIC_STAGING_SOCKET") else {
        return Ok(());
    };
    let path = std::path::PathBuf::from(path);
    if !path.with_extension("done").exists() {
        let listener = UnixListener::bind(&path)?;
        std::fs::write(path.with_extension("ready"), [])?;
        let (_stream, _) = listener.accept()?;
    }
    Ok(())
}

/// File-backed pages and artifacts, opened with the no-follow rules under the workspace root.
pub(crate) struct FileInputs<'a> {
    pub root: &'a Path,
}

impl BatchInputs for FileInputs<'_> {
    fn page(&mut self, entry: &PageEntry) -> Result<Vec<u8>> {
        let (file, len) = open_semantic_input(self.root, &self.root.join(&entry.relative_path))?;
        if len != entry.byte_length {
            return Err(admission("page length").into());
        }
        let mut bytes = Vec::with_capacity(len as usize);
        file.take(len + 1).read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn copy_artifact(
        &mut self,
        entry: &ArtifactEntry,
        dest: &Path,
        limits: &BatchLimits,
        progress: &mut dyn FnMut(u64) -> Result<()>,
    ) -> Result<CopiedInput> {
        copy_semantic_input_to_stage(
            self.root,
            &self.root.join(&entry.relative_path),
            dest,
            limits.artifact_bytes,
            progress,
        )
    }
}

/// A retained revision read through the store's native reader.
pub(crate) struct ReaderSource<'r, 'a> {
    pub reader: &'r NativeReader<'a>,
    pub stream: Option<InventoryStream<'r>>,
}

impl NativeSource for ReaderSource<'_, '_> {
    fn next_inventory(&mut self) -> Result<Option<native::Document>> {
        match self.stream.as_mut() {
            Some(stream) => stream.next_document(),
            None => Ok(None),
        }
    }

    fn source_bytes(&mut self, document: &native::Document) -> Result<Vec<u8>> {
        self.reader.source_bytes(document)
    }
}

/// Admits the batch named by `descriptor_path` into a new stage. The descriptor and every
/// page and artifact are read under `root`; the captured revision is read from the store.
#[allow(clippy::too_many_arguments)]
pub fn stage_batch(
    store: &Store,
    root: &Path,
    owner: StageOwner<'_>,
    descriptor_path: &Path,
    descriptor_sha256: &str,
    limits: &BatchLimits,
    counters: &mut BatchCounters,
    cancel: &CancelFlag,
) -> Result<AdmittedBatch> {
    #[cfg(feature = "test-support")]
    super::testing::reset_whole_snapshot_reads();
    check_cancel(cancel)?;
    let (file, len) = open_semantic_input(root, descriptor_path)?;
    if len > limits.descriptor_bytes {
        return Err(ScipCaptureError::LimitExceeded("descriptor bytes".into()).into());
    }
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(len + 1).read_to_end(&mut bytes)?;
    let descriptor = Descriptor::parse_admitted(&bytes, descriptor_sha256, limits)?;
    let producer = descriptor.check_producer()?;
    let captured: IndexPin = store
        .retained_native_pin_for(&descriptor.source_revision, &descriptor.source_set.id)
        .map_err(|error| {
            if error.to_string().contains("capture_unavailable") {
                anyhow::anyhow!("capture_unavailable: captured revision not retained")
            } else {
                error
            }
        })?;
    store.with_native_reader(captured, |reader| {
        let header = reader.header()?;
        Ok(descriptor.bind_header(root, &header.source_set, &header.revision)?)
    })?;
    descriptor.check_pages(limits)?;
    descriptor.check_artifacts(limits)?;
    descriptor.check_paths(root)?;
    let mut stage = Stage::create(
        &store.semantic_stage_parent()?,
        owner.request_id,
        owner.incarnation,
    )?;
    let passes = Passes {
        descriptor: &descriptor,
        producer: &producer,
        limits,
        cancel,
        budget: stage.budget(limits),
    };
    let mut inputs = FileInputs { root };
    passes.begin(stage.conn()?, &bytes, descriptor_sha256)?;
    store.with_native_reader(captured, |reader| {
        let mut native = ReaderSource {
            reader,
            stream: Some(reader.inventory()?),
        };
        passes.source_pass(stage.conn()?, &mut inputs, &mut native, counters)
    })?;
    passes.coverage_pass(stage.conn()?, &mut inputs, counters)?;
    let dir = stage.dir().to_owned();
    store.with_native_reader(captured, |reader| {
        let mut native = ReaderSource {
            reader,
            stream: None,
        };
        passes.artifact_pass(&dir, stage.conn()?, &mut inputs, &mut native, counters)
    })?;
    Ok(AdmittedBatch::new(
        stage,
        descriptor,
        descriptor_sha256.to_owned(),
        producer,
        captured,
    ))
}
