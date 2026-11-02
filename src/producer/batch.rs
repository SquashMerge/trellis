//! Trellis writes the import manifest itself (AC13): the batch descriptor and its pages are
//! generated from the retained native revision and a producer's artifact, so the existing #61
//! admission path validates and publishes them unchanged.

use super::profiles::{self, GenerationTable, Target};
use super::{PRODUCER_OUTPUT_INVALID, TrustProfile};
use crate::semantic_batch::admit::observed;
pub use crate::semantic_batch::admit::{FrozenEvent, FrozenPhase, FrozenSync};
use crate::{
    native_evidence as native,
    scip_admission::PinnedProducer,
    scip_raw::{self, DECODER_VERSION},
    semantic_batch::{
        BatchLimits, StageInput,
        descriptor::{
            ArtifactEntry, Descriptor, DescriptorComponents, DescriptorMetadata,
            DescriptorProducer, DescriptorSourceSet, FORMAT_VERSION, Generation, GenerationOrigin,
            GenerationTrust, PageEntry, PageKind, SourceManifestHasher, Totals,
        },
    },
    store::{NativeReader, Store},
};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};

fn check_cancel(cancel: Option<&crate::model::CancelFlag>) -> Result<()> {
    if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        return Err(crate::scip_raw::ScipCaptureError::Cancelled.into());
    }
    Ok(())
}

/// The artifact's place under a generation directory.
pub const ARTIFACT_PATH: &str = "out/index.scip";
const BATCH_DIR: &str = "batch";
/// Documents an artifact may name.
const MAX_SCANNED_DOCUMENTS: usize = 100_000;
/// Longest document path the scan accepts.
const MAX_PATH_BYTES: usize = 4096;
/// Bytes a page may use besides its rows.
const PAGE_OVERHEAD_BYTES: u64 = 256;

fn invalid(detail: &str) -> anyhow::Error {
    anyhow::anyhow!("{PRODUCER_OUTPUT_INVALID}: {detail}")
}

/// What the manifest needs from an artifact: its project root and the documents it names.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct ArtifactScan {
    pub project_root: String,
    pub documents: BTreeSet<String>,
}

enum Field<'a> {
    Varint,
    Bytes(&'a [u8]),
    Fixed,
}

fn varint(bytes: &[u8], at: &mut usize) -> Result<u64> {
    let mut value = 0_u64;
    for shift in (0..70).step_by(7) {
        let byte = *bytes.get(*at).ok_or_else(|| invalid("artifact wire"))?;
        *at += 1;
        if shift == 63 && byte > 1 {
            return Err(invalid("artifact wire"));
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(invalid("artifact wire"))
}

fn for_each_field(
    bytes: &[u8],
    cancel: Option<&crate::model::CancelFlag>,
    mut visit: impl FnMut(u64, Field<'_>) -> Result<()>,
) -> Result<()> {
    let mut at = 0;
    while at < bytes.len() {
        check_cancel(cancel)?;
        let tag = varint(bytes, &mut at)?;
        let number = tag >> 3;
        let field = match tag & 7 {
            0 => {
                varint(bytes, &mut at)?;
                Field::Varint
            }
            1 | 5 => {
                let width = if tag & 7 == 1 { 8 } else { 4 };
                ensure!(at + width <= bytes.len(), invalid("artifact wire"));
                at += width;
                Field::Fixed
            }
            2 => {
                let len = usize::try_from(varint(bytes, &mut at)?)
                    .map_err(|_| invalid("artifact wire"))?;
                let end = at
                    .checked_add(len)
                    .filter(|end| *end <= bytes.len())
                    .ok_or_else(|| invalid("artifact wire"))?;
                let slice = &bytes[at..end];
                at = end;
                Field::Bytes(slice)
            }
            _ => return Err(invalid("artifact wire")),
        };
        visit(number, field)?;
    }
    Ok(())
}

fn utf8(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).map_err(|_| invalid("artifact text"))
}

fn scan_root(
    bytes: &[u8],
    cancel: Option<&crate::model::CancelFlag>,
    observer: &dyn Fn(FrozenEvent),
    progress: &std::cell::Cell<u64>,
) -> Result<String> {
    let mut text = String::new();
    let mut at = 0;
    while at < bytes.len() {
        check_cancel(cancel)?;
        let mut end = (at + 64 * 1024).min(bytes.len());
        let part = match std::str::from_utf8(&bytes[at..end]) {
            Ok(part) => part,
            Err(error) if error.error_len().is_none() && end < bytes.len() => {
                end = at + error.valid_up_to();
                utf8(&bytes[at..end])?
            }
            Err(_) => return Err(invalid("artifact text")),
        };
        progress.set(progress.get() + 1);
        observer(FrozenEvent::Progress(
            FrozenPhase::ShallowScan,
            progress.get(),
        ));
        check_cancel(cancel)?;
        text.push_str(part);
        at = end;
    }
    Ok(text)
}

/// Reads `Metadata.project_root` (Index field 1, Metadata field 3) and every
/// `Document.relative_path` (Index field 2, Document field 1). Bounded by the artifact size,
/// 100,000 documents and 4096-byte paths; a malformed artifact is `producer_output_invalid`.
pub fn scan_artifact(bytes: &[u8]) -> Result<ArtifactScan> {
    scan_artifact_with(bytes, None, &|_| {})
}

fn scan_artifact_with(
    bytes: &[u8],
    cancel: Option<&crate::model::CancelFlag>,
    observer: &dyn Fn(FrozenEvent),
) -> Result<ArtifactScan> {
    check_cancel(cancel)?;
    ensure!(
        bytes.len() as u64 <= BatchLimits::PRODUCTION.artifact_bytes,
        "limit_exceeded: producer artifact bytes"
    );
    let mut scan = ArtifactScan::default();
    let progress = std::cell::Cell::new(0);
    for_each_field(bytes, cancel, |number, field| {
        progress.set(progress.get() + 1);
        observer(FrozenEvent::Progress(
            FrozenPhase::ShallowScan,
            progress.get(),
        ));
        check_cancel(cancel)?;
        match (number, field) {
            (1, Field::Bytes(metadata)) => for_each_field(metadata, cancel, |number, field| {
                if let (3, Field::Bytes(root)) = (number, field) {
                    scan.project_root = scan_root(root, cancel, observer, &progress)?;
                }
                Ok(())
            })?,
            (2, Field::Bytes(document)) => {
                let mut path = None;
                for_each_field(document, cancel, |number, field| {
                    if let (1, Field::Bytes(relative)) = (number, field) {
                        ensure!(relative.len() <= MAX_PATH_BYTES, invalid("document path"));
                        path = Some(utf8(relative)?.to_owned());
                    }
                    Ok(())
                })?;
                let path = path.ok_or_else(|| invalid("document without path"))?;
                scan.documents.insert(path);
                ensure!(
                    scan.documents.len() <= MAX_SCANNED_DOCUMENTS,
                    invalid("artifact documents")
                );
            }
            _ => {}
        }
        Ok(())
    })?;
    check_cancel(cancel)?;
    Ok(scan)
}

/// The generation object a Trellis run records for its table row.
pub fn run_generation(
    table: &GenerationTable,
    origin: GenerationOrigin,
    generation_id: &str,
) -> Generation {
    Generation {
        origin,
        generation_id: generation_id.to_owned(),
        trust_profile: table.trust_profile.map(|trust| match trust {
            TrustProfile::Restricted => GenerationTrust::Restricted,
            TrustProfile::Trusted => GenerationTrust::Trusted,
        }),
        runtime: table.runtime.clone(),
        observed: table.observed.clone(),
        asserted: table.asserted.clone(),
    }
}

/// The generation object of an owner-configured pickup: nothing observed.
pub fn pickup_generation(generation_id: &str) -> Generation {
    run_generation(
        &profiles::pickup_generation(),
        GenerationOrigin::OwnerPickup,
        generation_id,
    )
}

/// The pin and artifact language of a producer name, or `None` for a name that is neither
/// pinned nor the registered test stub.
pub fn pickup_target(producer: &str) -> Option<(PinnedProducer, &'static str)> {
    let target = Target::parse(producer, profiles::Stub::from_env().is_some())?;
    Some((target.pin()?, target.language()))
}

/// Everything `write_batch` needs besides the retained revision.
pub struct BatchSpec<'a> {
    pub pin: &'a PinnedProducer,
    /// The language the artifact covers.
    pub language: &'a str,
    pub workspace: &'a Path,
    /// The canonical generation directory; the artifact is at `ARTIFACT_PATH` under it.
    pub dir: &'a Path,
    pub generation: Generation,
}

/// A batch ready to import.
#[derive(Debug, Eq, PartialEq)]
pub struct WrittenBatch {
    pub descriptor: PathBuf,
    pub descriptor_sha256: String,
    pub source_revision: String,
}

struct Pages<'a> {
    kind: PageKind,
    prefix: &'static str,
    rows: Vec<serde_json::Value>,
    row_bytes: u64,
    entry_bytes: u64,
    entries: Vec<PageEntry>,
    limits: BatchLimits,
    cancel: Option<crate::model::CancelFlag>,
    observer: &'a dyn Fn(FrozenEvent),
}

impl<'a> Pages<'a> {
    fn new(kind: PageKind, prefix: &'static str, observer: &'a dyn Fn(FrozenEvent)) -> Self {
        Self {
            kind,
            prefix,
            rows: Vec::new(),
            row_bytes: 0,
            entry_bytes: 0,
            entries: Vec::new(),
            limits: BatchLimits::PRODUCTION,
            cancel: None,
            observer,
        }
    }

    fn push(&mut self, dir: &Path, row: serde_json::Value) -> Result<()> {
        check_cancel(self.cancel.as_ref())?;
        let len = serde_json::to_vec(&row)?.len() as u64 + 1;
        if !self.rows.is_empty()
            && (self.rows.len() as u64 >= self.limits.page_rows
                || self.row_bytes + len + PAGE_OVERHEAD_BYTES > self.limits.page_bytes)
        {
            self.flush(dir)?;
        }
        self.rows.push(row);
        self.row_bytes += len;
        Ok(())
    }

    /// Upper bound on the serialized pages already written or buffered.
    fn estimated_bytes(&self) -> u64 {
        self.entry_bytes
            + if self.rows.is_empty() {
                0
            } else {
                self.row_bytes + PAGE_OVERHEAD_BYTES
            }
    }

    fn estimated_pages(&self) -> u64 {
        self.entries.len() as u64 + u64::from(!self.rows.is_empty())
    }

    fn flush(&mut self, dir: &Path) -> Result<()> {
        check_cancel(self.cancel.as_ref())?;
        if self.rows.is_empty() {
            return Ok(());
        }
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct File<'a> {
            format_version: u8,
            kind: PageKind,
            ordinal: usize,
            rows: &'a [serde_json::Value],
        }
        let ordinal = self.entries.len();
        let bytes = serde_json::to_vec(&File {
            format_version: FORMAT_VERSION,
            kind: self.kind,
            ordinal,
            rows: &self.rows,
        })?;
        let name = format!("{}-{ordinal:04}.json", self.prefix);
        write_new(
            &dir.join(&name),
            &bytes,
            self.cancel.as_ref(),
            None,
            self.observer,
            FrozenSync::PageFile,
        )?;
        self.entry_bytes += bytes.len() as u64;
        self.entries.push(PageEntry {
            kind: self.kind,
            ordinal: ordinal as u32,
            relative_path: format!("{BATCH_DIR}/{name}"),
            byte_length: bytes.len() as u64,
            sha256: scip_raw::sha(&bytes),
            row_count: self.rows.len() as u64,
        });
        self.rows.clear();
        self.row_bytes = 0;
        Ok(())
    }
}

fn push_strict_page(
    pages: &mut Pages<'_>,
    other: &Pages<'_>,
    dir: &Path,
    row: serde_json::Value,
    limits: &BatchLimits,
    artifact_bytes: u64,
) -> Result<()> {
    let len = serde_json::to_vec(&row)?.len() as u64 + 1;
    ensure!(
        limits.page_rows > 0 && len.saturating_add(PAGE_OVERHEAD_BYTES) <= limits.page_bytes,
        "limit_exceeded: page bytes"
    );
    let flush = !pages.rows.is_empty()
        && (pages.rows.len() as u64 >= limits.page_rows
            || pages.row_bytes + len + PAGE_OVERHEAD_BYTES > limits.page_bytes);
    let new_page = pages.rows.is_empty() || flush;
    ensure!(
        pages.estimated_pages() + other.estimated_pages() + u64::from(new_page) <= limits.pages,
        "limit_exceeded: pages"
    );
    let page_bytes = pages
        .estimated_bytes()
        .saturating_add(other.estimated_bytes())
        .saturating_add(len)
        .saturating_add(if new_page { PAGE_OVERHEAD_BYTES } else { 0 });
    ensure!(
        page_bytes <= limits.total_page_bytes,
        "limit_exceeded: total page bytes"
    );
    ensure!(
        page_bytes.saturating_add(artifact_bytes) <= limits.disk_growth_bytes,
        "limit_exceeded: operation disk growth"
    );
    pages.push(dir, row)
}

fn write_new(
    path: &Path,
    bytes: &[u8],
    cancel: Option<&crate::model::CancelFlag>,
    progress: Option<&dyn Fn(u64)>,
    observer: &dyn Fn(FrozenEvent),
    sync: FrozenSync,
) -> Result<()> {
    check_cancel(cancel)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    let mut total = 0;
    for chunk in bytes.chunks(1024 * 1024) {
        check_cancel(cancel)?;
        file.write_all(chunk)?;
        total += chunk.len() as u64;
        if let Some(progress) = progress {
            progress(total);
        }
    }
    observer(FrozenEvent::BeforeSync(sync));
    check_cancel(cancel)?;
    match sync {
        FrozenSync::SnapshotFile => snapshot_sync_fault("file")?,
        FrozenSync::PageFile => snapshot_sync_fault("page file")?,
        FrozenSync::DescriptorFile => snapshot_sync_fault("descriptor file")?,
        _ => {}
    }
    file.sync_all()?;
    Ok(())
}

#[cfg(all(test, feature = "test-support"))]
type BeforeFrozenValidate = Box<dyn FnOnce(&mut WrittenBatch)>;

#[cfg(all(test, feature = "test-support"))]
thread_local! {
    static SNAPSHOT_SYNC_FAULT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
    static SNAPSHOT_ANCESTOR_FAULT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    pub(super) static BEFORE_FROZEN_VALIDATE: std::cell::RefCell<Option<BeforeFrozenValidate>> = Default::default();
}

#[cfg(all(test, feature = "test-support"))]
pub(super) fn fail_snapshot_sync(point: Option<&'static str>) {
    SNAPSHOT_SYNC_FAULT.with(|fault| fault.set(point));
}

#[cfg(all(test, feature = "test-support"))]
pub(super) fn fail_snapshot_ancestor(depth: Option<usize>) {
    SNAPSHOT_ANCESTOR_FAULT.with(|fault| fault.set(depth));
}

fn snapshot_sync_ancestor_fault(depth: usize) -> std::io::Result<()> {
    snapshot_sync_fault("ancestor directory")?;
    #[cfg(all(test, feature = "test-support"))]
    if SNAPSHOT_ANCESTOR_FAULT.with(|fault| fault.get() == Some(depth)) {
        return Err(std::io::Error::other("injected ancestor sync failure"));
    }
    let _ = depth;
    Ok(())
}

fn snapshot_sync_fault(point: &str) -> std::io::Result<()> {
    #[cfg(all(test, feature = "test-support"))]
    if SNAPSHOT_SYNC_FAULT.with(|fault| fault.get() == Some(point)) {
        return Err(std::io::Error::other("injected snapshot sync failure"));
    }
    let _ = point;
    Ok(())
}

/// Reads the artifact under the generation directory, hashing it as it goes.
fn read_artifact(
    dir: &Path,
    limit: u64,
    cancel: Option<&crate::model::CancelFlag>,
    observer: &dyn Fn(FrozenEvent),
) -> Result<(Vec<u8>, String)> {
    check_cancel(cancel)?;
    let path = dir.join(ARTIFACT_PATH);
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!("producer_output_missing: no artifact")
            } else {
                anyhow::Error::new(error).context("producer_failed: artifact open")
            }
        })?;
    ensure!(
        file.metadata()?.is_file(),
        invalid("artifact is not a file")
    );
    let mut bytes = Vec::new();
    let mut reader = file.take(limit.saturating_add(1));
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut hasher = Sha256::new();
    loop {
        check_cancel(cancel)?;
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        ensure!(
            (bytes.len() as u64).saturating_add(count as u64) <= limit,
            "limit_exceeded: producer artifact bytes"
        );
        hasher.update(&buffer[..count]);
        bytes.extend_from_slice(&buffer[..count]);
        observer(FrozenEvent::Progress(
            FrozenPhase::SnapshotRead,
            bytes.len() as u64,
        ));
    }
    check_cancel(cancel)?;
    let sha256 = hex::encode(hasher.finalize());
    Ok((bytes, sha256))
}

/// Writes `batch/descriptor.json` and its pages under `spec.dir` from the retained revision
/// `reader` reads and the artifact at `ARTIFACT_PATH`.
///
/// Every inventory document gets a source row. Documents of the pin's languages get a
/// coverage row: covered by the artifact when it names the document, `failed` when it is the
/// artifact's language and the artifact omits it, and `notRequested` for any other language.
pub fn write_batch(reader: &NativeReader<'_>, spec: &BatchSpec<'_>) -> Result<WrittenBatch> {
    write_batch_inner(
        reader,
        spec,
        false,
        &BatchLimits::PRODUCTION,
        None,
        &|_| {},
        None,
    )
}

fn write_batch_inner(
    reader: &NativeReader<'_>,
    spec: &BatchSpec<'_>,
    frozen: bool,
    limits: &BatchLimits,
    cancel: Option<&crate::model::CancelFlag>,
    observer: &dyn Fn(FrozenEvent),
    durable_base: Option<&Path>,
) -> Result<WrittenBatch> {
    let (artifact, artifact_sha256) = observed(observer, FrozenPhase::SnapshotRead, || {
        read_artifact(
            spec.dir,
            limits
                .artifact_bytes
                .min(BatchLimits::PRODUCTION.artifact_bytes),
            cancel,
            observer,
        )
    })?;
    let scan = observed(observer, FrozenPhase::ShallowScan, || {
        scan_artifact_with(&artifact, cancel, observer)
    })?;
    let generation = &spec.generation;
    let admitted = StageInput::generated(
        spec.workspace,
        generation.origin,
        &generation.generation_id,
        spec.dir,
    )?;
    let project_root = if scan.project_root.is_empty() {
        admitted.project_roots[0].clone()
    } else if admitted.project_roots.contains(&scan.project_root) {
        scan.project_root.clone()
    } else {
        return Err(invalid("project root"));
    };
    ensure!(
        spec.pin.languages.iter().any(|l| l == spec.language),
        invalid("artifact language")
    );

    let header = reader.header()?;
    let batch_dir = spec.dir.join(BATCH_DIR);
    check_cancel(cancel)?;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&batch_dir)
        .with_context(|| format!("create {}", batch_dir.display()))?;
    let artifact_path = if frozen {
        ensure!(
            artifact.len() as u64 <= limits.total_artifact_bytes,
            "limit_exceeded: total artifact bytes"
        );
        ensure!(
            artifact.len() as u64 <= limits.disk_growth_bytes,
            "limit_exceeded: operation disk growth"
        );
        observed(observer, FrozenPhase::SnapshotWrite, || {
            write_new(
                &batch_dir.join("artifact.scip"),
                &artifact,
                cancel,
                Some(&|total| observer(FrozenEvent::Progress(FrozenPhase::SnapshotWrite, total))),
                observer,
                FrozenSync::SnapshotFile,
            )
        })?;
        "batch/artifact.scip"
    } else {
        ARTIFACT_PATH
    };

    let mut hasher = SourceManifestHasher::new(&header.source_set.id);
    let mut sources = Pages::new(PageKind::Source, "source", observer);
    let mut coverage = Pages::new(PageKind::Coverage, "coverage", observer);
    if frozen {
        sources.limits = limits.clone();
        coverage.limits = limits.clone();
        sources.cancel = cancel.cloned();
        coverage.cancel = cancel.cloned();
    }
    let (mut source_documents, mut covered, mut excluded) = (0_u64, 0_u64, 0_u64);
    let mut inventory = reader.inventory()?;
    observed(observer, FrozenPhase::BatchPages, || -> Result<()> {
        loop {
            check_cancel(cancel)?;
            let Some(document) = inventory.next_document()? else {
                break;
            };
            let native::DocumentKey { language, path, .. } = &document.key;
            hasher.push(language, path, &document.content_hash);
            source_documents += 1;
            observer(FrozenEvent::Progress(
                FrozenPhase::BatchPages,
                source_documents,
            ));
            check_cancel(cancel)?;
            if frozen {
                ensure!(
                    source_documents <= limits.source_documents,
                    "limit_exceeded: source documents"
                );
            }
            let row = serde_json::json!({
                "byteLength": document.byte_length,
                "contentHash": document.content_hash,
                "language": language,
                "relativePath": path,
            });
            if frozen {
                push_strict_page(
                    &mut sources,
                    &coverage,
                    &batch_dir,
                    row,
                    limits,
                    artifact.len() as u64,
                )?;
            } else {
                sources.push(&batch_dir, row)?;
            }
            if !spec.pin.languages.contains(language) {
                continue;
            }
            let (artifact_ordinal, exclusion) = if language != spec.language {
                (None, Some("notRequested"))
            } else if scan.documents.contains(path) {
                (Some(0), None)
            } else {
                (None, Some("failed"))
            };
            if artifact_ordinal.is_some() {
                covered += 1;
            } else {
                excluded += 1;
            }
            let row = serde_json::json!({
                "artifact": artifact_ordinal,
                "exclusion": exclusion,
                "language": language,
                "relativePath": path,
            });
            if frozen {
                push_strict_page(
                    &mut coverage,
                    &sources,
                    &batch_dir,
                    row,
                    limits,
                    artifact.len() as u64,
                )?;
            } else {
                coverage.push(&batch_dir, row)?;
            }
        }
        if source_documents == 0 || covered + excluded == 0 {
            bail!("{PRODUCER_OUTPUT_INVALID}: no documents for the producer language");
        }
        sources.flush(&batch_dir)?;
        coverage.flush(&batch_dir)?;
        Ok(())
    })?;

    let mut pages = sources.entries;
    pages.extend(coverage.entries);
    let page_bytes = pages.iter().map(|page| page.byte_length).sum();
    let descriptor = Descriptor {
        format_version: FORMAT_VERSION,
        trust_origin: spec.pin.trust_origin.to_owned(),
        profile_id: spec.pin.profile_id.clone(),
        decoder_version: DECODER_VERSION.to_owned(),
        producer: DescriptorProducer {
            name: spec.pin.name.clone(),
            version: spec.pin.version.clone(),
            identity_kind: spec.pin.identity_kind,
            executable_hash: spec.pin.executable_hash.clone(),
            position_encoding: spec.pin.position_encoding,
            languages: spec.pin.languages.clone(),
        },
        source_set: DescriptorSourceSet {
            id: header.source_set.id.clone(),
            root_id: header.source_set.root_id.clone(),
            languages: header.source_set.languages.clone(),
            dependencies: header.source_set.dependencies.clone(),
        },
        source_revision: header.revision.id.clone(),
        source_manifest_hash: hasher.finish(),
        components: DescriptorComponents {
            toolchain_hash: header.revision.toolchain_hash.clone(),
            config_hash: header.revision.config_hash.clone(),
            dependency_hash: header.revision.dependency_hash.clone(),
        },
        metadata: DescriptorMetadata {
            protocol_version: 0,
            project_root,
            text_document_encoding: 1,
        },
        completion: if excluded == 0 {
            scip_raw::Completion::Complete
        } else {
            scip_raw::Completion::Partial
        },
        totals: Totals {
            source_documents,
            covered_documents: covered,
            excluded_documents: excluded,
            page_bytes,
            artifact_bytes: artifact.len() as u64,
        },
        pages,
        artifacts: vec![ArtifactEntry {
            ordinal: 0,
            relative_path: artifact_path.to_owned(),
            byte_length: artifact.len() as u64,
            sha256: artifact_sha256,
            language: spec.language.to_owned(),
        }],
        generation: Some(generation.clone()),
    };
    let bytes = serde_json::to_vec(&descriptor)?;
    if frozen {
        ensure!(
            bytes.len() as u64 <= limits.descriptor_bytes,
            "limit_exceeded: descriptor bytes"
        );
        ensure!(
            descriptor
                .totals
                .artifact_bytes
                .saturating_add(descriptor.totals.page_bytes)
                .saturating_add(bytes.len() as u64)
                <= limits.disk_growth_bytes,
            "limit_exceeded: operation disk growth"
        );
    }
    let path = batch_dir.join("descriptor.json");
    write_new(
        &path,
        &bytes,
        cancel,
        None,
        observer,
        FrozenSync::DescriptorFile,
    )?;
    if frozen {
        observed(observer, FrozenPhase::Durability, || -> Result<()> {
            observer(FrozenEvent::BeforeSync(FrozenSync::BatchDirectory));
            check_cancel(cancel)?;
            snapshot_sync_fault("batch directory")?;
            fs::File::open(&batch_dir)?.sync_all()?;
            observer(FrozenEvent::BeforeSync(FrozenSync::GenerationDirectory));
            check_cancel(cancel)?;
            snapshot_sync_fault("generation directory")?;
            fs::File::open(spec.dir)?.sync_all()?;
            let base = durable_base
                .ok_or_else(|| anyhow::anyhow!("invalid_request: missing durable base"))?;
            for (depth, parent) in spec
                .dir
                .ancestors()
                .skip(1)
                .take_while(|path| path.starts_with(base))
                .enumerate()
            {
                observer(FrozenEvent::BeforeSync(FrozenSync::AncestorDirectory(
                    depth,
                )));
                check_cancel(cancel)?;
                snapshot_sync_ancestor_fault(depth)?;
                fs::File::open(parent)?.sync_all()?;
            }
            check_cancel(cancel)?;
            Ok(())
        })?;
    }
    Ok(WrittenBatch {
        descriptor: path,
        descriptor_sha256: scip_raw::sha(&bytes),
        source_revision: descriptor.source_revision,
    })
}

/// Freeze the bytes read from producer output and fully admit that exact snapshot. The caller
/// owns the canonical generation directory and must prevent its cleanup for this entire call;
/// in a daemon run, hold the run's admission lease and coordinate shutdown with the run owner.
/// This does not enqueue anything: retain the generation until any accepted request settles,
/// including when enqueue returns an ambiguous error. Compare `source_revision` with the planned
/// revision before enqueue, and use the same limits and cancellation policy as the worker.
/// The generation directory must be canonical and under this store's cache; Store activation
/// creates that cache through the durable topology path, so it is the sync traversal's base.
pub fn write_frozen_batch(
    store: &Store,
    spec: &BatchSpec<'_>,
    limits: &BatchLimits,
    counters: &mut crate::semantic_batch::BatchCounters,
    cancel: &crate::model::CancelFlag,
) -> Result<WrittenBatch> {
    write_frozen_batch_observed(store, spec, limits, counters, cancel, &|_| {})
}

/// Like `write_frozen_batch`, with synchronous phase boundaries and progress events. A caller
/// can timestamp events and sample its process RSS; an `End` carries the actual typed failure.
pub fn write_frozen_batch_observed(
    store: &Store,
    spec: &BatchSpec<'_>,
    limits: &BatchLimits,
    counters: &mut crate::semantic_batch::BatchCounters,
    cancel: &crate::model::CancelFlag,
    observer: &dyn Fn(FrozenEvent),
) -> Result<WrittenBatch> {
    write_frozen_batch_inner(store, spec, limits, counters, cancel, observer, None)
}

/// Admit a run snapshot only if the captured head is still its planned revision.
pub fn write_frozen_batch_at_revision(
    store: &Store,
    spec: &BatchSpec<'_>,
    limits: &BatchLimits,
    counters: &mut crate::semantic_batch::BatchCounters,
    cancel: &crate::model::CancelFlag,
    revision: &str,
) -> Result<WrittenBatch> {
    write_frozen_batch_inner(
        store,
        spec,
        limits,
        counters,
        cancel,
        &|_| {},
        Some(revision),
    )
}

fn write_frozen_batch_inner(
    store: &Store,
    spec: &BatchSpec<'_>,
    limits: &BatchLimits,
    counters: &mut crate::semantic_batch::BatchCounters,
    cancel: &crate::model::CancelFlag,
    observer: &dyn Fn(FrozenEvent),
    revision: Option<&str>,
) -> Result<WrittenBatch> {
    observed(observer, FrozenPhase::FrozenBatch, || {
        (|| {
            check_cancel(Some(cancel))?;
            let base = fs::canonicalize(&store.state_topology().0.cache)
                .context("producer_failed: durable cache base")?;
            let generation_dir =
                fs::canonicalize(spec.dir).context("producer_failed: generation directory")?;
            ensure!(
                spec.dir.is_absolute()
                    && spec.dir == generation_dir
                    && spec.dir.starts_with(&base)
                    && spec.dir != base,
                "invalid_request: generation outside durable cache"
            );
            let baseline = store
                .index_baseline()
                .map_err(crate::semantic_batch::admit::native_read_error)?;
            #[allow(unused_mut)]
            let mut written = store
                .with_native_reader(baseline, |reader| {
                    if let Some(revision) = revision {
                        ensure!(
                            reader.header()?.revision.id == revision,
                            "stale_basis: head revision changed"
                        );
                    }
                    write_batch_inner(
                        reader,
                        spec,
                        true,
                        limits,
                        Some(cancel),
                        observer,
                        Some(&base),
                    )
                })
                .map_err(crate::semantic_batch::admit::native_read_error)?;
            #[cfg(all(test, feature = "test-support"))]
            BEFORE_FROZEN_VALIDATE.with(|hook| {
                if let Some(hook) = hook.borrow_mut().take() {
                    hook(&mut written);
                }
            });
            validate_frozen_batch_observed(
                store,
                spec.workspace,
                spec.dir,
                &written,
                spec.generation.origin,
                &spec.generation.generation_id,
                limits,
                counters,
                cancel,
                observer,
            )?;
            check_cancel(Some(cancel))?;
            Ok(written)
        })()
        .map_err(strict_error)
    })
}

fn strict_error(error: anyhow::Error) -> anyhow::Error {
    use crate::scip_raw::ScipCaptureError;
    if let Some(typed) = crate::semantic_batch::admit::preserved_error(&error) {
        return if error.to_string() == typed {
            error
        } else {
            error.context(typed)
        };
    }
    if let Some(error) = error.downcast_ref::<ScipCaptureError>() {
        return match error {
            ScipCaptureError::LimitExceeded(detail) => anyhow::anyhow!("limit_exceeded: {detail}"),
            ScipCaptureError::Cancelled => anyhow::anyhow!("cancelled"),
            _ => anyhow::anyhow!("producer_output_invalid: {}", error.detail()),
        };
    }
    if crate::store::transient_storage_contention(&error) {
        return error.context("storage_busy: frozen batch preflight");
    }
    if error.chain().any(|cause| cause.is::<std::io::Error>()) {
        return error.context("producer_failed: frozen batch I/O");
    }
    error.context("producer_failed: frozen batch preflight")
}

/// Re-admit a frozen batch using the production passes. The isolated temporary stage cannot be
/// removed by the index startup sweep; dropping admission removes it without publishing.
#[allow(clippy::too_many_arguments)]
#[cfg(all(test, feature = "test-support"))]
pub(super) fn validate_frozen_batch(
    store: &Store,
    workspace: &Path,
    dir: &Path,
    written: &WrittenBatch,
    origin: GenerationOrigin,
    generation_id: &str,
    limits: &BatchLimits,
    counters: &mut crate::semantic_batch::BatchCounters,
    cancel: &crate::model::CancelFlag,
) -> Result<()> {
    validate_frozen_batch_observed(
        store,
        workspace,
        dir,
        written,
        origin,
        generation_id,
        limits,
        counters,
        cancel,
        &|_| {},
    )
}

#[allow(clippy::too_many_arguments)]
fn validate_frozen_batch_observed(
    store: &Store,
    workspace: &Path,
    dir: &Path,
    written: &WrittenBatch,
    origin: GenerationOrigin,
    generation_id: &str,
    limits: &BatchLimits,
    counters: &mut crate::semantic_batch::BatchCounters,
    cancel: &crate::model::CancelFlag,
    observer: &dyn Fn(FrozenEvent),
) -> Result<()> {
    let input = StageInput::generated(workspace, origin, generation_id, dir)?;
    let incarnation = uuid::Uuid::new_v4().simple().to_string();
    let admitted = crate::semantic_batch::admit::stage_batch_in(
        store,
        &input,
        crate::semantic_batch::StageOwner {
            request_id: generation_id,
            incarnation: &incarnation,
        },
        &written.descriptor,
        &written.descriptor_sha256,
        limits,
        counters,
        cancel,
        Some(&dir.join(BATCH_DIR)),
        observer,
    )
    .map_err(strict_error)?;
    drop(admitted);
    Ok(())
}

/// Copies the owner's artifact into `<dir>/out/index.scip`, hashing while copying. The source
/// is opened without following a symlink and must be a regular file within the artifact limit.
pub fn copy_artifact(source: &Path, dir: &Path) -> Result<(String, u64)> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(source)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!("producer_output_missing: artifact not found")
            } else {
                anyhow::anyhow!("producer_output_invalid: artifact open")
            }
        })?;
    ensure!(
        file.metadata()?.is_file(),
        invalid("artifact is not a regular file")
    );
    let out_dir = dir.join("out");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&out_dir)
        .with_context(|| format!("create {}", out_dir.display()))?;
    let limit = BatchLimits::PRODUCTION.artifact_bytes;
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join(ARTIFACT_PATH))?;
    let mut reader = file.take(limit + 1);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 20];
    let mut total = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total += read as u64;
        ensure!(total <= limit, "limit_exceeded: artifact bytes");
        hasher.update(&buffer[..read]);
        out.write_all(&buffer[..read])?;
    }
    out.sync_all()?;
    Ok((hex::encode(hasher.finalize()), total))
}

#[cfg(test)]
mod error_tests {
    #[test]
    fn typed_context_is_preserved_over_underlying_io() {
        for class in [
            "root_changed",
            "storage_busy",
            "cancelled",
            "limit_exceeded",
        ] {
            let error = anyhow::Error::new(std::io::Error::other("disk"))
                .context(format!("{class}: context"))
                .context("producer_failed: outer wrapper");
            assert_eq!(
                super::strict_error(error).to_string(),
                format!("{class}: context")
            );
        }
    }
}
