//! Trellis writes the import manifest itself (AC13): the batch descriptor and its pages are
//! generated from the retained native revision and a producer's artifact, so the existing #61
//! admission path validates and publishes them unchanged.

use super::profiles::{self, GenerationTable, Target};
use super::{PRODUCER_OUTPUT_INVALID, TrustProfile};
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
    store::NativeReader,
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
};

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

fn for_each_field(bytes: &[u8], mut visit: impl FnMut(u64, Field<'_>) -> Result<()>) -> Result<()> {
    let mut at = 0;
    while at < bytes.len() {
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

/// Reads `Metadata.project_root` (Index field 1, Metadata field 3) and every
/// `Document.relative_path` (Index field 2, Document field 1). Bounded by the artifact size,
/// 100,000 documents and 4096-byte paths; a malformed artifact is `producer_output_invalid`.
pub fn scan_artifact(bytes: &[u8]) -> Result<ArtifactScan> {
    ensure!(
        bytes.len() as u64 <= BatchLimits::PRODUCTION.artifact_bytes,
        "limit_exceeded: producer artifact bytes"
    );
    let mut scan = ArtifactScan::default();
    for_each_field(bytes, |number, field| {
        match (number, field) {
            (1, Field::Bytes(metadata)) => for_each_field(metadata, |number, field| {
                if let (3, Field::Bytes(root)) = (number, field) {
                    scan.project_root = utf8(root)?.to_owned();
                }
                Ok(())
            })?,
            (2, Field::Bytes(document)) => {
                let mut path = None;
                for_each_field(document, |number, field| {
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

struct Pages {
    kind: PageKind,
    prefix: &'static str,
    rows: Vec<serde_json::Value>,
    row_bytes: u64,
    entries: Vec<PageEntry>,
    limits: BatchLimits,
}

impl Pages {
    fn new(kind: PageKind, prefix: &'static str) -> Self {
        Self {
            kind,
            prefix,
            rows: Vec::new(),
            row_bytes: 0,
            entries: Vec::new(),
            limits: BatchLimits::PRODUCTION,
        }
    }

    fn push(&mut self, dir: &Path, row: serde_json::Value) -> Result<()> {
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

    fn flush(&mut self, dir: &Path) -> Result<()> {
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
        write_new(&dir.join(&name), &bytes)?;
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

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Reads the artifact under the generation directory, hashing it as it goes.
fn read_artifact(dir: &Path) -> Result<(Vec<u8>, String)> {
    let path = dir.join(ARTIFACT_PATH);
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|_| anyhow::anyhow!("producer_output_missing: no artifact"))?;
    let limit = BatchLimits::PRODUCTION.artifact_bytes;
    ensure!(
        file.metadata()?.is_file(),
        invalid("artifact is not a file")
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "limit_exceeded: producer artifact bytes"
    );
    let sha256 = hex::encode(Sha256::digest(&bytes));
    Ok((bytes, sha256))
}

/// Writes `batch/descriptor.json` and its pages under `spec.dir` from the retained revision
/// `reader` reads and the artifact at `ARTIFACT_PATH`.
///
/// Every inventory document gets a source row. Documents of the pin's languages get a
/// coverage row: covered by the artifact when it names the document, `failed` when it is the
/// artifact's language and the artifact omits it, and `notRequested` for any other language.
pub fn write_batch(reader: &NativeReader<'_>, spec: &BatchSpec<'_>) -> Result<WrittenBatch> {
    let (artifact, artifact_sha256) = read_artifact(spec.dir)?;
    let scan = scan_artifact(&artifact)?;
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
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&batch_dir)
        .with_context(|| format!("create {}", batch_dir.display()))?;

    let mut hasher = SourceManifestHasher::new(&header.source_set.id);
    let mut sources = Pages::new(PageKind::Source, "source");
    let mut coverage = Pages::new(PageKind::Coverage, "coverage");
    let (mut source_documents, mut covered, mut excluded) = (0_u64, 0_u64, 0_u64);
    let mut inventory = reader.inventory()?;
    while let Some(document) = inventory.next_document()? {
        let native::DocumentKey { language, path, .. } = &document.key;
        hasher.push(language, path, &document.content_hash);
        source_documents += 1;
        sources.push(
            &batch_dir,
            serde_json::json!({
                "byteLength": document.byte_length,
                "contentHash": document.content_hash,
                "language": language,
                "relativePath": path,
            }),
        )?;
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
        coverage.push(
            &batch_dir,
            serde_json::json!({
                "artifact": artifact_ordinal,
                "exclusion": exclusion,
                "language": language,
                "relativePath": path,
            }),
        )?;
    }
    if source_documents == 0 || covered + excluded == 0 {
        bail!("{PRODUCER_OUTPUT_INVALID}: no documents for the producer language");
    }
    sources.flush(&batch_dir)?;
    coverage.flush(&batch_dir)?;

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
            relative_path: ARTIFACT_PATH.to_owned(),
            byte_length: artifact.len() as u64,
            sha256: artifact_sha256,
            language: spec.language.to_owned(),
        }],
        generation: Some(generation.clone()),
    };
    let bytes = serde_json::to_vec(&descriptor)?;
    let path = batch_dir.join("descriptor.json");
    write_new(&path, &bytes)?;
    Ok(WrittenBatch {
        descriptor: path,
        descriptor_sha256: scip_raw::sha(&bytes),
        source_revision: descriptor.source_revision,
    })
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
