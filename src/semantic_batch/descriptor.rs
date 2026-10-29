//! The closed batch descriptor and page grammar, the header-binding predicate and the
//! incremental source-manifest hash.
//!
//! Parsing is serde only and never admits: native rejoin re-parses stored `descriptor_bytes`
//! through the same type. Admission is the separate `check_*` family, run in wire-format order.
use super::BatchLimits;
use crate::{
    native_evidence as native,
    native_ids::canonical_into,
    scip_admission::{
        PinnedProducer, admission, file_uri, hexhash, order, path, pinned_producer,
        validate_selected_role,
    },
    scip_raw::{self, Completion, ScipCaptureError},
};
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, path::Path};

type Result<T> = std::result::Result<T, ScipCaptureError>;

pub const FORMAT_VERSION: u8 = 1;

/// A key that must be present even when its value is `null`.
fn nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error> {
    Option::deserialize(deserializer)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IdentityKind {
    #[serde(rename = "executable")]
    Executable,
    #[serde(rename = "generatorSource")]
    GeneratorSource,
}

impl IdentityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Executable => "executable",
            Self::GeneratorSource => "generatorSource",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DescriptorProducer {
    pub name: String,
    pub version: String,
    pub identity_kind: IdentityKind,
    pub executable_hash: String,
    pub position_encoding: u8,
    pub languages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DescriptorSourceSet {
    pub id: String,
    pub root_id: String,
    pub languages: Vec<String>,
    pub dependencies: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DescriptorComponents {
    pub toolchain_hash: String,
    pub config_hash: String,
    pub dependency_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DescriptorMetadata {
    pub protocol_version: i32,
    pub project_root: String,
    pub text_document_encoding: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Totals {
    pub source_documents: u64,
    pub covered_documents: u64,
    pub excluded_documents: u64,
    pub page_bytes: u64,
    pub artifact_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PageKind {
    Source,
    Coverage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PageEntry {
    pub kind: PageKind,
    pub ordinal: u32,
    pub relative_path: String,
    pub byte_length: u64,
    pub sha256: String,
    pub row_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ArtifactEntry {
    pub ordinal: u32,
    pub relative_path: String,
    pub byte_length: u64,
    pub sha256: String,
    pub language: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Descriptor {
    pub format_version: u8,
    pub trust_origin: String,
    pub profile_id: String,
    pub decoder_version: String,
    pub producer: DescriptorProducer,
    pub source_set: DescriptorSourceSet,
    pub source_revision: String,
    pub source_manifest_hash: String,
    pub components: DescriptorComponents,
    pub metadata: DescriptorMetadata,
    pub completion: Completion,
    pub totals: Totals,
    pub pages: Vec<PageEntry>,
    pub artifacts: Vec<ArtifactEntry>,
}

impl Descriptor {
    /// Serde-only parse of the closed grammar. No admission rule is applied.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).map_err(|_| admission("closed descriptor"))
    }

    /// Check 1: size, digest, then the closed grammar, format version and decoder version.
    pub fn parse_admitted(
        bytes: &[u8],
        expected_sha256: &str,
        limits: &BatchLimits,
    ) -> Result<Self> {
        if bytes.len() as u64 > limits.descriptor_bytes {
            return Err(ScipCaptureError::LimitExceeded("descriptor bytes".into()));
        }
        if !hexhash(expected_sha256) || scip_raw::sha(bytes) != expected_sha256 {
            return Err(admission("descriptor digest"));
        }
        let descriptor = Self::parse(bytes)?;
        if descriptor.format_version != FORMAT_VERSION {
            return Err(admission("descriptor format version"));
        }
        if descriptor.decoder_version != scip_raw::DECODER_VERSION {
            return Err(admission("descriptor decoder version"));
        }
        Ok(descriptor)
    }

    /// Check 2: the declared producer must equal the checked-in pin exactly.
    pub fn check_producer(&self) -> Result<PinnedProducer> {
        pinned_producer(&self.trust_origin, &self.profile_id, &self.producer)
    }

    /// Check 3: the header must name the retained captured revision.
    pub fn bind_header(
        &self,
        root: &Path,
        source_set: &native::SourceSet,
        revision: &native::Revision,
    ) -> Result<()> {
        let hashes = [
            &self.components.toolchain_hash,
            &self.components.config_hash,
            &self.components.dependency_hash,
        ];
        if source_set.id != revision.source_set_id
            || self.source_set.id != source_set.id
            || self.source_set.root_id != source_set.root_id
            || self.source_set.languages != source_set.languages
            || self.source_set.dependencies != source_set.dependencies
            || self.source_revision != revision.id
            || self.components.toolchain_hash != revision.toolchain_hash
            || self.components.config_hash != revision.config_hash
            || self.components.dependency_hash != revision.dependency_hash
            || !hashes.iter().all(|hash| hexhash(hash))
        {
            return Err(admission("native revision components"));
        }
        if self.metadata.project_root != file_uri(root)?
            || self.metadata.protocol_version != 0
            || self.metadata.text_document_encoding != 1
        {
            return Err(admission("metadata binding"));
        }
        if !hexhash(&self.source_manifest_hash) {
            return Err(admission("source manifest hash"));
        }
        Ok(())
    }

    /// Check 4: page entries and totals.
    pub fn check_pages(&self, limits: &BatchLimits) -> Result<()> {
        if self.pages.len() as u64 > limits.pages {
            return Err(ScipCaptureError::LimitExceeded("pages".into()));
        }
        let first_coverage = self
            .pages
            .iter()
            .position(|page| page.kind == PageKind::Coverage)
            .unwrap_or(self.pages.len());
        if self.pages[first_coverage..]
            .iter()
            .any(|page| page.kind == PageKind::Source)
        {
            return Err(admission("page order"));
        }
        if first_coverage == 0 || first_coverage == self.pages.len() {
            return Err(admission("page ordinals"));
        }
        let (mut page_bytes, mut source_rows, mut coverage_rows) = (0u64, 0u64, 0u64);
        for (index, page) in self.pages.iter().enumerate() {
            let expected = if page.kind == PageKind::Source {
                index
            } else {
                index - first_coverage
            };
            if usize::try_from(page.ordinal) != Ok(expected) {
                return Err(admission("page ordinals"));
            }
            if page.byte_length > limits.page_bytes {
                return Err(ScipCaptureError::LimitExceeded("page bytes".into()));
            }
            if page.row_count > limits.page_rows {
                return Err(ScipCaptureError::LimitExceeded("page rows".into()));
            }
            if page.row_count == 0 {
                return Err(admission("page rows"));
            }
            if !hexhash(&page.sha256) {
                return Err(admission("page digest"));
            }
            page_bytes = page_bytes
                .checked_add(page.byte_length)
                .ok_or_else(|| admission("page totals"))?;
            let rows = if page.kind == PageKind::Source {
                &mut source_rows
            } else {
                &mut coverage_rows
            };
            *rows += page.row_count;
        }
        if page_bytes > limits.total_page_bytes {
            return Err(ScipCaptureError::LimitExceeded("total page bytes".into()));
        }
        if self.totals.source_documents > limits.source_documents {
            return Err(ScipCaptureError::LimitExceeded("source documents".into()));
        }
        if page_bytes != self.totals.page_bytes
            || source_rows != self.totals.source_documents
            || self
                .totals
                .covered_documents
                .checked_add(self.totals.excluded_documents)
                != Some(coverage_rows)
        {
            return Err(admission("page totals"));
        }
        if (self.completion == Completion::Complete) != (self.totals.excluded_documents == 0) {
            return Err(admission("coverage completion"));
        }
        Ok(())
    }

    /// Check 5: artifact entries and totals.
    pub fn check_artifacts(&self, limits: &BatchLimits) -> Result<()> {
        if self.artifacts.len() as u64 > limits.artifacts {
            return Err(ScipCaptureError::LimitExceeded("artifacts".into()));
        }
        if self.artifacts.is_empty() {
            return Err(admission("artifact count"));
        }
        let (mut total, mut digests, mut paths) = (0u64, BTreeSet::new(), BTreeSet::new());
        for (index, artifact) in self.artifacts.iter().enumerate() {
            if usize::try_from(artifact.ordinal) != Ok(index) {
                return Err(admission("artifact ordinals"));
            }
            if !self.producer.languages.contains(&artifact.language) {
                return Err(admission("artifact language"));
            }
            if artifact.byte_length > limits.artifact_bytes {
                return Err(ScipCaptureError::LimitExceeded("artifact bytes".into()));
            }
            if !hexhash(&artifact.sha256) {
                return Err(admission("artifact digest"));
            }
            if !digests.insert(artifact.sha256.as_str())
                || !paths.insert(artifact.relative_path.as_str())
            {
                return Err(admission("duplicate artifact"));
            }
            total = total
                .checked_add(artifact.byte_length)
                .ok_or_else(|| admission("artifact totals"))?;
        }
        if total > limits.total_artifact_bytes {
            return Err(ScipCaptureError::LimitExceeded(
                "total artifact bytes".into(),
            ));
        }
        if total != self.totals.artifact_bytes {
            return Err(admission("artifact totals"));
        }
        Ok(())
    }

    /// Check 6: every named file is under the root with no symlink component.
    pub fn check_paths(&self, root: &Path) -> Result<()> {
        let paths = self
            .pages
            .iter()
            .map(|page| page.relative_path.as_str())
            .chain(
                self.artifacts
                    .iter()
                    .map(|artifact| artifact.relative_path.as_str()),
            );
        for relative in paths {
            validate_selected_role(root, &root.join(relative))?;
        }
        Ok(())
    }
}

/// One page file: the header is checked against its descriptor entry, then the rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Page<R> {
    pub format_version: u8,
    pub kind: PageKind,
    pub ordinal: u32,
    pub rows: Vec<R>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SourceRow {
    pub byte_length: u64,
    pub content_hash: String,
    pub language: String,
    pub relative_path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExclusionReason {
    NotRequested,
    Unsupported,
    Failed,
}

impl ExclusionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotRequested => "notRequested",
            Self::Unsupported => "unsupported",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CoverageRow {
    #[serde(deserialize_with = "nullable")]
    pub artifact: Option<u32>,
    #[serde(deserialize_with = "nullable")]
    pub exclusion: Option<ExclusionReason>,
    pub language: String,
    pub relative_path: String,
}

/// What a coverage row says about its document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Covered { artifact: u32 },
    Excluded(ExclusionReason),
}

impl CoverageRow {
    /// Exactly one of `artifact` and `exclusion` is non-null.
    pub fn disposition(&self) -> Result<Disposition> {
        match (self.artifact, self.exclusion) {
            (Some(artifact), None) => Ok(Disposition::Covered { artifact }),
            (None, Some(reason)) => Ok(Disposition::Excluded(reason)),
            _ => Err(admission("coverage disposition")),
        }
    }
}

impl SourceRow {
    /// Shape of one source row against the source set it must belong to.
    pub fn check(&self, languages: &[String]) -> Result<()> {
        if !languages.contains(&self.language)
            || !hexhash(&self.content_hash)
            || !path(&self.relative_path)
        {
            return Err(admission("source row"));
        }
        Ok(())
    }
}

/// Verifies a page file read whole against its descriptor entry.
pub fn check_page_bytes(entry: &PageEntry, bytes: &[u8]) -> Result<()> {
    if bytes.len() as u64 != entry.byte_length {
        return Err(admission("page length"));
    }
    if scip_raw::sha(bytes) != entry.sha256 {
        return Err(admission("page digest"));
    }
    Ok(())
}

/// Serde-only parse of a page whose bytes already passed `check_page_bytes`.
pub fn parse_page<R: DeserializeOwned>(bytes: &[u8]) -> Result<Page<R>> {
    serde_json::from_slice(bytes).map_err(|_| admission("closed page"))
}

impl<R> Page<R> {
    /// The internal header must equal the descriptor entry, and the row count must match.
    pub fn check_entry(&self, entry: &PageEntry) -> Result<()> {
        if self.format_version != FORMAT_VERSION
            || self.kind != entry.kind
            || self.ordinal != entry.ordinal
        {
            return Err(admission("reordered page"));
        }
        if self.rows.len() as u64 != entry.row_count {
            return Err(admission("page rows"));
        }
        Ok(())
    }
}

/// Strictly increasing `(order(language), path bytes)` across every page of one kind.
pub struct RowOrder {
    previous: Option<(usize, Vec<u8>)>,
    duplicate: &'static str,
    unsorted: &'static str,
}

impl RowOrder {
    pub fn source() -> Self {
        Self::new("duplicate source path", "unsorted source rows")
    }

    pub fn coverage() -> Self {
        Self::new("duplicate coverage row", "unsorted coverage rows")
    }

    fn new(duplicate: &'static str, unsorted: &'static str) -> Self {
        Self {
            previous: None,
            duplicate,
            unsorted,
        }
    }

    pub fn push(&mut self, language: &str, relative_path: &str) -> Result<()> {
        let key = (order(language), relative_path.as_bytes().to_vec());
        if key.0 == usize::MAX {
            return Err(admission("invalid document list"));
        }
        match self.previous.as_ref().map(|previous| previous.cmp(&key)) {
            Some(std::cmp::Ordering::Equal) => return Err(admission(self.duplicate)),
            Some(std::cmp::Ordering::Greater) => return Err(admission(self.unsorted)),
            _ => {}
        }
        self.previous = Some(key);
        Ok(())
    }
}

/// The ordinal of the artifact that carries a document's basis (§4.4 e). A covered document
/// uses its assigned artifact. An excluded one uses the lowest-ordinal artifact of its own
/// language, or artifact 0 when none matches. Staged joins and native rejoin both call this.
pub fn basis_artifact_ordinal(
    artifacts: &[ArtifactEntry],
    language: &str,
    assigned: Option<u32>,
) -> u32 {
    assigned
        .or_else(|| {
            artifacts
                .iter()
                .filter(|artifact| artifact.language == language)
                .map(|artifact| artifact.ordinal)
                .min()
        })
        .unwrap_or(0)
}

/// Hashes the canonical array of `{document, contentHash}` rows without holding the array.
pub struct SourceManifestHasher {
    source_set_id: String,
    hasher: Sha256,
    first: bool,
}

impl SourceManifestHasher {
    pub fn new(source_set_id: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"[");
        Self {
            source_set_id: source_set_id.to_owned(),
            hasher,
            first: true,
        }
    }

    pub fn push(&mut self, language: &str, relative_path: &str, content_hash: &str) {
        if !std::mem::take(&mut self.first) {
            self.hasher.update(b",");
        }
        let row = json!({
            "document": {"sourceSetId": self.source_set_id, "language": language, "path": relative_path},
            "contentHash": content_hash,
        });
        canonical_into(&row, &mut |bytes| self.hasher.update(bytes));
    }

    pub fn finish(mut self) -> String {
        self.hasher.update(b"]");
        hex::encode(self.hasher.finalize())
    }
}
