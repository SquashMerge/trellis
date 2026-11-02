//! The admission driver: descriptor, source pass, coverage pass, artifact copy and streamed
//! decode, then the closure checks. Pages and artifacts arrive through `BatchInputs` and the
//! retained native revision through `NativeSource`, so the file-backed driver and the
//! in-memory test admission run the same passes.
use super::{
    AdmittedBatch, BatchCounters, BatchLimits,
    descriptor::{
        ArtifactEntry, Descriptor, Disposition, GenerationOrigin, ImportOrigin, PageEntry,
        PageKind, RowOrder, SourceManifestHasher, SourceRow, check_page_bytes, parse_page,
    },
    limits::STAGE_CHECK_COPIED_BYTES,
    staging::{Stage, StageBudget, StageClaimSink, artifact_file, lookup_assigned},
};
use crate::{
    index_coordinator::{
        CopiedInput, copy_semantic_input_to_stage, copy_stream_to_stage,
        open_frozen_semantic_input, open_semantic_input,
    },
    model::{CancelFlag, IndexPin},
    native_evidence as native,
    scip_admission::{PinnedProducer, admission, file_uri, order},
    scip_raw::{self, AuthenticatedArtifact, DocumentSource, ScipCaptureError, decode_streaming},
    store::{InventoryStream, NativeReader, Store},
};
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::de::DeserializeOwned;
use std::{
    borrow::Cow,
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};

/// Remember genuine reader errors that the decoder's wire-format API cannot carry. EOF
/// synthesized by `read_exact` remains a malformed/truncated artifact, not an I/O fault.
struct TrackedRead<R> {
    inner: R,
    fault: std::rc::Rc<std::cell::RefCell<Option<std::io::Error>>>,
}

#[cfg(all(test, feature = "test-support"))]
thread_local! {
    static FAIL_DECODE_READ: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static PREFLIGHT_FAULT: std::cell::Cell<Option<(&'static str, &'static str)>> = const { std::cell::Cell::new(None) };
}

#[cfg(all(test, feature = "test-support"))]
pub(crate) fn fail_preflight_at(point: &'static str, kind: &'static str) {
    PREFLIGHT_FAULT.with(|fault| fault.set(Some((point, kind))));
}

fn preflight_fault(point: &str) -> Result<()> {
    #[cfg(all(test, feature = "test-support"))]
    if let Some(kind) = PREFLIGHT_FAULT.with(|fault| {
        let current = fault.get();
        if current.is_some_and(|(selected, _)| selected == point) {
            fault.set(None);
            current.map(|(_, kind)| kind)
        } else {
            None
        }
    }) {
        return Err(match kind {
            "corrupt" => rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
                None,
            )
            .into(),
            "corrupt-wrapped" => anyhow::Error::new(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
                None,
            ))
            .context("producer_failed: native read"),
            "conversion" => {
                rusqlite::Error::InvalidColumnType(0, "test".into(), rusqlite::types::Type::Real)
                    .into()
            }
            "busy" => rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                None,
            )
            .into(),
            "native" => {
                crate::store::SelectedIntegrity("incompatible_index: injected native fault".into())
                    .into()
            }
            _ => unreachable!(),
        });
    }
    let _ = point;
    Ok(())
}

#[cfg(all(test, feature = "test-support"))]
pub(crate) fn fail_next_decode_read() {
    FAIL_DECODE_READ.with(|flag| flag.set(true));
}

impl<R: Read> Read for TrackedRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        #[cfg(all(test, feature = "test-support"))]
        let result = if FAIL_DECODE_READ.with(|flag| flag.replace(false)) {
            Err(std::io::Error::from_raw_os_error(libc::EIO))
        } else {
            self.inner.read(buf)
        };
        #[cfg(not(all(test, feature = "test-support")))]
        let result = self.inner.read(buf);
        match result {
            Err(error) if error.kind() != std::io::ErrorKind::Interrupted => {
                let returned = std::io::Error::new(error.kind(), error.to_string());
                *self.fault.borrow_mut() = Some(error);
                Err(returned)
            }
            other => other,
        }
    }
}

/// Observable boundaries of a frozen producer batch. Decode events surround the production
/// streaming decoder itself, not the shallow producer scan or the preceding stage copy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrozenPhase {
    FrozenBatch,
    SnapshotRead,
    SnapshotWrite,
    ShallowScan,
    BatchPages,
    Durability,
    /// The pre-decode source-disposition pass.
    CoveragePass,
    ArtifactPass,
    FullDecode,
    /// The post-decode check that every covered source was actually emitted.
    CoverageClosure,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrozenEvent {
    Begin(FrozenPhase),
    /// Snapshot read/write counts bytes, batch pages counts inventory documents, and the
    /// shallow scan counts checked steps. Other phases do not currently emit progress.
    Progress(FrozenPhase, u64),
    BeforeSync(FrozenSync),
    End(FrozenPhase, Result<(), String>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrozenSync {
    SnapshotFile,
    PageFile,
    DescriptorFile,
    BatchDirectory,
    GenerationDirectory,
    AncestorDirectory(usize),
}

pub(crate) fn observed<T>(
    observer: &dyn Fn(FrozenEvent),
    phase: FrozenPhase,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    observer(FrozenEvent::Begin(phase));
    let result = operation();
    let status = result.as_ref().map(|_| ()).map_err(|error| {
        if let Some(typed) = preserved_error(error) {
            typed
        } else if let Some(error) = error.downcast_ref::<ScipCaptureError>() {
            match error {
                ScipCaptureError::Cancelled => "cancelled".to_owned(),
                ScipCaptureError::LimitExceeded(detail) => format!("limit_exceeded: {detail}"),
                _ => format!("producer_output_invalid: {}", error.detail()),
            }
        } else {
            let message = error.to_string();
            if crate::store::transient_storage_contention(error) {
                format!("storage_busy: {message}")
            } else {
                format!("producer_failed: {message}")
            }
        }
    });
    observer(FrozenEvent::End(phase, status));
    result
}

pub(crate) fn preserved_error(error: &anyhow::Error) -> Option<String> {
    let preferred = error.chain().find_map(|cause| {
        let message = cause.to_string();
        let class = message.split(':').next().unwrap_or("");
        [
            "root_changed",
            "storage_busy",
            "cancelled",
            "limit_exceeded",
        ]
        .contains(&class)
        .then_some(message)
    });
    if preferred.is_some() {
        return preferred;
    }
    if let Some(error) = error.downcast_ref::<ScipCaptureError>() {
        match error {
            ScipCaptureError::Cancelled => return Some("cancelled".to_owned()),
            ScipCaptureError::LimitExceeded(detail) => {
                return Some(format!("limit_exceeded: {detail}"));
            }
            _ => {}
        }
    }
    error.chain().find_map(|cause| {
        let message = cause.to_string();
        let class = message.split(':').next().unwrap_or("");
        [
            "producer_output_invalid",
            "producer_output_missing",
            "producer_failed",
            "invalid_request",
            "stale_basis",
            "capture_unavailable",
            "index_not_ready",
            "store_unavailable",
            "producer_state_corrupt",
            "incompatible_index",
            "recovery_required",
        ]
        .contains(&class)
        .then_some(message)
    })
}

/// Used only after a live native-reader boundary has classified the error. A stage failure
/// must never be passed here: it may contain the same SQLite codes as live-index corruption.
pub(crate) fn native_read_error(error: anyhow::Error) -> anyhow::Error {
    if preferred_native_error(&error).is_some()
        || crate::store::transient_storage_contention(&error)
    {
        return error;
    }
    if Store::is_index_recovery_failure(&error) {
        return error.context("recovery_required: retained native index corruption");
    }
    error
}

fn preferred_native_error(error: &anyhow::Error) -> Option<String> {
    preserved_error(error).filter(|message| {
        [
            "root_changed",
            "storage_busy",
            "cancelled",
            "limit_exceeded",
            "incompatible_index",
            "recovery_required",
        ]
        .contains(&message.split(':').next().unwrap_or(""))
    })
}

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
    /// Package roots from the captured revision's retained native config (#443).
    pub module_roots: &'a scip_raw::ModuleRoots,
    /// The captured revision's retained native config, the source of Cargo manifests (#394).
    pub native_config: &'a [serde_json::Value],
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
    /// inventory, hashed against the descriptor and staged. Returns the Cargo target of each
    /// Rust document, read from the retained manifests and every authenticated Rust source
    /// (owner ruling 2026-10-30, #394).
    pub fn source_pass(
        &self,
        conn: &Connection,
        inputs: &mut dyn BatchInputs,
        native: &mut dyn NativeSource,
        counters: &mut BatchCounters,
    ) -> Result<scip_raw::CargoTargets> {
        let descriptor = self.descriptor;
        let mut cargo = scip_raw::CargoTargetsBuilder::from_native_config(self.native_config);
        let rust = self.producer.languages.iter().any(|l| l == "rust") && !cargo.is_empty();
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
                if rust && row.language == "rust" {
                    cargo.add_document(&row.relative_path, &bytes);
                }
                hasher.push(&row.language, &row.relative_path, &row.content_hash);
                preflight_fault("stage-source")?;
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
        Ok(cargo.finish())
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
    #[cfg(feature = "test-support")]
    pub fn artifact_pass(
        &self,
        stage_dir: &Path,
        conn: &Connection,
        inputs: &mut dyn BatchInputs,
        native: &mut dyn NativeSource,
        cargo_targets: &scip_raw::CargoTargets,
        counters: &mut BatchCounters,
    ) -> Result<()> {
        self.artifact_pass_observed(
            stage_dir,
            conn,
            inputs,
            native,
            cargo_targets,
            counters,
            &|_| {},
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn artifact_pass_observed(
        &self,
        stage_dir: &Path,
        conn: &Connection,
        inputs: &mut dyn BatchInputs,
        native: &mut dyn NativeSource,
        cargo_targets: &scip_raw::CargoTargets,
        counters: &mut BatchCounters,
        observer: &dyn Fn(FrozenEvent),
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
            observed(observer, FrozenPhase::FullDecode, || {
                self.decode(entry, &dest, conn, native, cargo_targets, counters)
            })?;
            #[cfg(all(feature = "test-support", unix))]
            if entry.ordinal == 0 {
                pause_after_first_artifact()?;
            }
        }
        // Exercise the actual closure query with a missing staged document, using the stage's
        // own connection rather than opening a second handle on its live SQLite file.
        #[cfg(all(test, feature = "test-support"))]
        if PREFLIGHT_FAULT.with(|fault| {
            if fault.get() == Some(("stage-closure", "remove-document")) {
                fault.set(None);
                true
            } else {
                false
            }
        }) {
            let removed = conn.execute(
                "DELETE FROM documents WHERE source_ord=(SELECT MIN(source_ord) FROM documents)",
                [],
            )?;
            anyhow::ensure!(removed == 1, "test fixture missing decoded document");
        }
        observed(observer, FrozenPhase::CoverageClosure, || {
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
        })
    }

    fn decode(
        &self,
        entry: &ArtifactEntry,
        staged: &Path,
        conn: &Connection,
        native: &mut dyn NativeSource,
        cargo_targets: &scip_raw::CargoTargets,
        counters: &mut BatchCounters,
    ) -> Result<()> {
        let descriptor = self.descriptor;
        preflight_fault("stage-artifact")?;
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
            module_roots: self.module_roots,
            cargo_targets,
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
            let language = crate::scip_raw::bind_document_language(
                language,
                &entry.language,
                &self.producer.languages,
            )?;
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
                    language: language.to_owned(),
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
        let read_fault = std::rc::Rc::new(std::cell::RefCell::new(None));
        let reader = TrackedRead {
            inner: file,
            fault: read_fault.clone(),
        };
        let decoded = decode_streaming(
            reader,
            entry.byte_length,
            &context,
            &mut resolve,
            counters,
            self.limits,
            &mut sink,
            self.cancel,
        );
        if let Err(error) = decoded {
            return Err(sink
                .take_fault()
                .or(fault)
                .or_else(|| read_fault.borrow_mut().take().map(anyhow::Error::new))
                .unwrap_or_else(|| error.into()));
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
    pub frozen_store: Option<&'a Store>,
}

fn frozen_input_error(store: &Store, error: anyhow::Error) -> anyhow::Error {
    if let Err(root) = store.verify_root()
        && (root.to_string() == "root_changed" || root.to_string().starts_with("root_changed:"))
    {
        let message = root.to_string();
        return error.context(if message == "root_changed" {
            "root_changed: captured root lost".to_owned()
        } else {
            message
        });
    }
    error
}

impl BatchInputs for FileInputs<'_> {
    fn page(&mut self, entry: &PageEntry) -> Result<Vec<u8>> {
        let path = self.root.join(&entry.relative_path);
        let (file, len) = if let Some(store) = self.frozen_store {
            open_frozen_semantic_input(self.root, &path)
                .map_err(|error| frozen_input_error(store, error))?
        } else {
            open_semantic_input(self.root, &path)?
        };
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
        if let Some(store) = self.frozen_store {
            let (file, len) =
                open_frozen_semantic_input(self.root, &self.root.join(&entry.relative_path))
                    .map_err(|error| frozen_input_error(store, error))?;
            if len > limits.artifact_bytes {
                return Err(ScipCaptureError::LimitExceeded("artifact bytes".into()).into());
            }
            return copy_stream_to_stage(
                file,
                dest,
                limits.artifact_bytes,
                super::limits::IO_BUFFER_BYTES as usize,
                progress,
            );
        }
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
    /// Only failures from the retained index cross `with_native_reader`'s recovery boundary.
    native_fault: Option<anyhow::Error>,
}

impl ReaderSource<'_, '_> {
    fn capture<T>(&mut self, result: Result<T>) -> Result<T> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                let message = preferred_native_error(&error).unwrap_or_else(|| {
                    if crate::store::transient_storage_contention(&error) {
                        "storage_busy: native source read".to_owned()
                    } else if Store::is_index_recovery_failure(&error) {
                        "recovery_required: retained native index corruption".to_owned()
                    } else {
                        "native reader failed".to_owned()
                    }
                });
                self.native_fault = Some(error);
                Err(anyhow::anyhow!(message))
            }
        }
    }
}

impl NativeSource for ReaderSource<'_, '_> {
    fn next_inventory(&mut self) -> Result<Option<native::Document>> {
        let result = preflight_fault("native-source").and_then(|()| match self.stream.as_mut() {
            Some(stream) => stream.next_document(),
            None => Ok(None),
        });
        self.capture(result)
    }

    fn source_bytes(&mut self, document: &native::Document) -> Result<Vec<u8>> {
        let point = if self.stream.is_some() {
            "native-source-bytes"
        } else {
            "native-artifact"
        };
        let result = preflight_fault(point).and_then(|()| self.reader.source_bytes(document));
        self.capture(result)
    }
}

/// Where a batch is read from and which project-root spellings its header may bind to.
#[derive(Debug, Clone)]
pub struct StageInput {
    /// The directory the descriptor, pages and artifacts are read under.
    pub input_root: PathBuf,
    pub project_roots: Vec<String>,
    pub origin: ImportOrigin,
    pub generation_id: Option<String>,
}

impl StageInput {
    /// An external import: the workspace is the input root and `file://<workspace>` the only
    /// project root.
    pub fn external(workspace: &Path) -> std::result::Result<Self, ScipCaptureError> {
        Ok(Self {
            input_root: workspace.to_owned(),
            project_roots: vec![file_uri(workspace)?],
            origin: ImportOrigin::External,
            generation_id: None,
        })
    }

    /// A Trellis-written batch read from its private generation directory. A pickup binds to
    /// the workspace; a run binds to its controlled worktree under the run directory.
    pub fn generated(
        workspace: &Path,
        origin: GenerationOrigin,
        generation_id: &str,
        generation_dir: &Path,
    ) -> std::result::Result<Self, ScipCaptureError> {
        let bare = match origin {
            GenerationOrigin::OwnerPickup => file_uri(workspace)?,
            GenerationOrigin::TrellisRun => format!("{}/worktree", file_uri(generation_dir)?),
        };
        Ok(Self {
            input_root: generation_dir.to_owned(),
            project_roots: vec![bare.clone(), format!("{bare}/")],
            origin: origin.into(),
            generation_id: Some(generation_id.to_owned()),
        })
    }
}

/// Admits the batch named by `descriptor_path` into a new stage. The descriptor and every
/// page and artifact are read under the input root; the captured revision is read from the
/// store.
#[allow(clippy::too_many_arguments)]
pub fn stage_batch(
    store: &Store,
    input: &StageInput,
    owner: StageOwner<'_>,
    descriptor_path: &Path,
    descriptor_sha256: &str,
    limits: &BatchLimits,
    counters: &mut BatchCounters,
    cancel: &CancelFlag,
) -> Result<AdmittedBatch> {
    stage_batch_in(
        store,
        input,
        owner,
        descriptor_path,
        descriptor_sha256,
        limits,
        counters,
        cancel,
        None,
        &|_| {},
    )
}

/// Preflight uses a private parent owned by the producer operation, outside the index startup
/// sweep. All admission passes and resource accounting are otherwise the same as the worker's.
#[allow(clippy::too_many_arguments)]
pub(crate) fn stage_batch_in(
    store: &Store,
    input: &StageInput,
    owner: StageOwner<'_>,
    descriptor_path: &Path,
    descriptor_sha256: &str,
    limits: &BatchLimits,
    counters: &mut BatchCounters,
    cancel: &CancelFlag,
    stage_parent: Option<&Path>,
    observer: &dyn Fn(FrozenEvent),
) -> Result<AdmittedBatch> {
    #[cfg(feature = "test-support")]
    super::testing::reset_whole_snapshot_reads();
    check_cancel(cancel)?;
    let root = input.input_root.as_path();
    let frozen = stage_parent.is_some();
    let (file, len) = if frozen {
        open_frozen_semantic_input(root, descriptor_path)
            .map_err(|error| frozen_input_error(store, error))?
    } else {
        open_semantic_input(root, descriptor_path)?
    };
    if len > limits.descriptor_bytes {
        return Err(ScipCaptureError::LimitExceeded("descriptor bytes".into()).into());
    }
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(len + 1).read_to_end(&mut bytes)?;
    let descriptor = Descriptor::parse_admitted(&bytes, descriptor_sha256, limits)?;
    descriptor.check_origin(input.origin, input.generation_id.as_deref())?;
    let producer = descriptor.check_producer()?;
    let captured: IndexPin = store
        .retained_native_pin_for(&descriptor.source_revision, &descriptor.source_set.id)
        .map_err(|error| {
            if error.to_string().contains("capture_unavailable")
                && matches!(
                    error.downcast_ref::<rusqlite::Error>(),
                    Some(rusqlite::Error::QueryReturnedNoRows)
                )
            {
                anyhow::anyhow!("capture_unavailable: captured revision not retained")
            } else {
                native_read_error(error)
            }
        })?;
    store
        .with_native_reader(captured, |reader| {
            let header = reader.header()?;
            Ok(descriptor.bind_header(
                &input.project_roots,
                &header.source_set,
                &header.revision,
            )?)
        })
        .map_err(native_read_error)?;
    descriptor.check_pages(limits)?;
    descriptor.check_artifacts(limits)?;
    if frozen {
        descriptor
            .check_frozen_paths(root)
            .map_err(|error| frozen_input_error(store, error))?;
    } else {
        descriptor.check_paths(root)?;
    }
    let native_config = store
        .retained_native_components_at(captured)
        .map_err(native_read_error)?
        .1;
    let module_roots = scip_raw::ModuleRoots::from_native_config(&native_config);
    let index_dir = store.semantic_stage_parent()?;
    let mut stage = Stage::create_in(
        stage_parent.unwrap_or(&index_dir),
        &index_dir,
        owner.request_id,
        owner.incarnation,
        if stage_parent.is_some() {
            descriptor
                .totals
                .artifact_bytes
                .saturating_add(descriptor.totals.page_bytes)
                .saturating_add(bytes.len() as u64)
        } else {
            0
        },
    )?;
    let passes = Passes {
        descriptor: &descriptor,
        producer: &producer,
        limits,
        cancel,
        budget: stage.budget(limits),
        module_roots: &module_roots,
        native_config: &native_config,
    };
    let mut inputs = FileInputs {
        root,
        frozen_store: frozen.then_some(store),
    };
    passes.begin(stage.conn()?, &bytes, descriptor_sha256)?;
    let conn = stage.conn()?;
    let cargo_targets = store
        .with_native_reader(captured, |reader| {
            let mut native = ReaderSource {
                reader,
                stream: Some(reader.inventory()?),
                native_fault: None,
            };
            let result = passes.source_pass(conn, &mut inputs, &mut native, counters);
            match native.native_fault.take() {
                Some(error) => Err(error),
                None => Ok(result),
            }
        })
        .map_err(native_read_error)??;
    observed(observer, FrozenPhase::CoveragePass, || {
        passes.coverage_pass(stage.conn()?, &mut inputs, counters)
    })?;
    let dir = stage.dir().to_owned();
    let conn = stage.conn()?;
    store
        .with_native_reader(captured, |reader| {
            let mut native = ReaderSource {
                reader,
                stream: None,
                native_fault: None,
            };
            let result = observed(observer, FrozenPhase::ArtifactPass, || {
                passes.artifact_pass_observed(
                    &dir,
                    conn,
                    &mut inputs,
                    &mut native,
                    &cargo_targets,
                    counters,
                    observer,
                )
            });
            match native.native_fault.take() {
                Some(error) => Err(error),
                None => Ok(result),
            }
        })
        .map_err(native_read_error)??;
    Ok(AdmittedBatch::new(
        stage,
        descriptor,
        descriptor_sha256.to_owned(),
        producer,
        captured,
    ))
}
