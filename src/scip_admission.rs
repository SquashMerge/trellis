//! A trusted selection binds original captured bytes to native source identity and a checked-in producer.
pub use crate::scip_raw::{
    CapturedClaim, CapturedScipFactV1, CapturedScipInventory, Completion, Exclusion,
    ScipCaptureError,
};
use crate::{
    capture::Capture,
    model::CancelFlag,
    native_evidence::captured_revision_header,
    native_ids::canonical,
    scip_raw::{self, AuthenticatedScipDocument, AuthenticatedScipInput},
    semantic_batch::descriptor::{DescriptorProducer, IdentityKind},
    semantic_evidence::{self, Language, PositionEncoding, SemanticKind},
};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{OnceLock, atomic::Ordering},
};

#[derive(Debug, Clone)]
pub struct TrustedSelection {
    pub expected_manifest_hash: String,
    pub profile_id: String,
    pub producer_name: String,
}
pub struct ReferenceProfileAdmission<'a> {
    input: AuthenticatedScipInput<'a>,
    pinned: PinnedProducer,
}
impl ReferenceProfileAdmission<'_> {
    pub fn admitted_encoding(&self) -> u8 {
        self.input.position_encoding
    }
    pub fn producer(&self) -> semantic_evidence::Producer {
        self.pinned.producer()
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Manifest {
    format_version: u8,
    trust_origin: String,
    completion: Completion,
    decoder_version: String,
    profile_id: String,
    producer: Producer,
    source_set: SourceSet,
    source_revision: String,
    source_manifest_hash: String,
    components: Components,
    metadata: Metadata,
    source_documents: Vec<Doc>,
    documents: Vec<Doc>,
    exclusions: Vec<Exclusion>,
    artifacts: Vec<Artifact>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Producer {
    name: String,
    version: String,
    executable_hash: String,
    position_encoding: u8,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceSet {
    id: String,
    root_id: String,
    languages: Vec<String>,
    dependencies: Vec<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Components {
    toolchain_hash: String,
    config_hash: String,
    dependency_hash: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Metadata {
    protocol_version: i32,
    project_root: String,
    text_document_encoding: i32,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Doc {
    language: String,
    relative_path: String,
    content_hash: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Artifact {
    kind: String,
    path: String,
    hash: String,
    language: String,
    documents: Vec<String>,
}
#[derive(Deserialize)]
struct Hardware {
    #[serde(rename = "profileId")]
    profile_id: String,
    #[serde(rename = "semanticProducers")]
    semantic_producers: Vec<HardwareProducer>,
}
#[derive(Deserialize)]
struct HardwareProducer {
    name: String,
    version: String,
    #[serde(rename = "executableHash")]
    executable_hash: String,
    #[serde(rename = "positionEncoding")]
    position_encoding: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyntheticProfile {
    profile_id: String,
    identity_kind: String,
    generator: SyntheticGenerator,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyntheticGenerator {
    name: String,
    version: String,
    source_sha256: String,
    position_encoding: String,
    languages: Vec<String>,
}

pub const REFERENCE_TRUST_ORIGIN: &str = "referenceProfile";
pub const SYNTHETIC_TRUST_ORIGIN: &str = "syntheticProfile";

/// A producer identity fixed by a checked-in profile, never by the descriptor that names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedProducer {
    pub name: String,
    pub version: String,
    pub identity_kind: IdentityKind,
    /// For `generatorSource` identities this is the SHA-256 of the generator source.
    pub executable_hash: String,
    pub position_encoding: u8,
    pub languages: Vec<String>,
    pub trust_origin: &'static str,
    pub profile_id: String,
}

impl PinnedProducer {
    pub fn producer(&self) -> semantic_evidence::Producer {
        semantic_evidence::Producer {
            id: self.name.clone(),
            version: self.version.clone(),
            executable_hash: self.executable_hash.clone(),
            kind: SemanticKind::Semantic,
            languages: self
                .languages
                .iter()
                .filter_map(|language| semantic_language(language))
                .collect(),
            position_encoding: match self.position_encoding {
                1 => PositionEncoding::Utf8,
                2 => PositionEncoding::Utf16,
                _ => PositionEncoding::UnicodeScalar,
            },
        }
    }
}

struct Profiles {
    reference: Vec<PinnedProducer>,
    synthetic: PinnedProducer,
}

fn semantic_language(name: &str) -> Option<Language> {
    match name {
        "java" => Some(Language::Java),
        "rust" => Some(Language::Rust),
        "python" => Some(Language::Python),
        "javascript" => Some(Language::Javascript),
        _ => None,
    }
}

fn encoding_code(name: &str) -> Option<u8> {
    match name {
        "utf8" => Some(1),
        "utf16" => Some(2),
        "unicodeScalar" => Some(3),
        _ => None,
    }
}

/// The language of each real reference producer.
pub fn producer_language(producer_id: &str) -> Option<&'static str> {
    match producer_id {
        "scip-java" => Some("java"),
        "scip-typescript" => Some("javascript"),
        "scip-python" => Some("python"),
        "rust-analyzer" => Some("rust"),
        _ => None,
    }
}

/// The language of each producer the witness fast path accepts. `scip-java` is omitted on
/// purpose: its rejoin has always taken the FULL path, and that behavior is preserved.
pub fn witness_language(producer_id: &str) -> Option<&'static str> {
    match producer_id {
        "scip-java" => None,
        other => producer_language(other),
    }
}

fn profiles() -> Result<&'static Profiles, ScipCaptureError> {
    static PROFILES: OnceLock<Option<Profiles>> = OnceLock::new();
    PROFILES
        .get_or_init(load_profiles)
        .as_ref()
        .ok_or_else(|| admission("checked-in producer profile"))
}

fn load_profiles() -> Option<Profiles> {
    let hardware: Hardware = serde_json::from_str(include_str!(
        "../docs/semantic-evidence/hardware/mac-mini-m5-pro-v1.json"
    ))
    .ok()?;
    let synthetic: SyntheticProfile = serde_json::from_str(include_str!(
        "../docs/semantic-evidence/synthetic-profiles/trellis-synthetic-scip-v1.json"
    ))
    .ok()?;
    let reference = hardware
        .semantic_producers
        .iter()
        .filter_map(|producer| {
            let language = producer_language(&producer.name)?;
            Some(PinnedProducer {
                name: producer.name.clone(),
                version: producer.version.clone(),
                identity_kind: IdentityKind::Executable,
                executable_hash: producer.executable_hash.clone(),
                position_encoding: encoding_code(producer.position_encoding.as_deref()?)?,
                languages: vec![language.into()],
                trust_origin: REFERENCE_TRUST_ORIGIN,
                profile_id: hardware.profile_id.clone(),
            })
            .filter(|pinned| hexhash(&pinned.executable_hash))
        })
        .collect();
    let generator = synthetic.generator;
    if synthetic.identity_kind != "generatorSource"
        || !hexhash(&generator.source_sha256)
        || generator
            .languages
            .iter()
            .any(|language| semantic_language(language).is_none())
    {
        return None;
    }
    Some(Profiles {
        reference,
        synthetic: PinnedProducer {
            name: generator.name,
            version: generator.version,
            identity_kind: IdentityKind::GeneratorSource,
            executable_hash: generator.source_sha256,
            position_encoding: encoding_code(&generator.position_encoding)?,
            languages: generator.languages,
            trust_origin: SYNTHETIC_TRUST_ORIGIN,
            profile_id: synthetic.profile_id,
        },
    })
}

/// The pin for a producer id, as the store re-derives it from stored rows.
pub fn pinned_producer_by_id(producer_id: &str) -> Option<PinnedProducer> {
    let profiles = profiles().ok()?;
    if profiles.synthetic.name == producer_id {
        return Some(profiles.synthetic.clone());
    }
    profiles
        .reference
        .iter()
        .find(|pinned| pinned.name == producer_id)
        .cloned()
}

/// The checked-in pin that a declared producer must equal in every field.
pub fn pinned_producer(
    trust_origin: &str,
    profile_id: &str,
    declared: &DescriptorProducer,
) -> Result<PinnedProducer, ScipCaptureError> {
    let profiles = profiles()?;
    let pinned = match trust_origin {
        REFERENCE_TRUST_ORIGIN => profiles
            .reference
            .iter()
            .find(|pinned| pinned.name == declared.name)
            .ok_or_else(|| admission("producer not pinned"))?,
        SYNTHETIC_TRUST_ORIGIN => &profiles.synthetic,
        _ => return Err(admission("producer profile pin")),
    };
    if pinned.profile_id != profile_id
        || pinned.name != declared.name
        || pinned.version != declared.version
        || pinned.identity_kind != declared.identity_kind
        || pinned.executable_hash != declared.executable_hash
        || pinned.position_encoding != declared.position_encoding
        || pinned.languages != declared.languages
    {
        return Err(admission("producer profile pin"));
    }
    Ok(pinned.clone())
}

fn parse_manifest(bytes: &[u8]) -> Result<Manifest, ScipCaptureError> {
    serde_json::from_slice(bytes).map_err(|_| admission("closed manifest"))
}

/// Apply the exact reference-profile manifest grammar to retained bytes before
/// Store interprets their authenticated fields.
pub(crate) fn validate_closed_manifest(bytes: &[u8]) -> Result<(), ScipCaptureError> {
    if bytes.len() > 1024 * 1024 {
        return Err(ScipCaptureError::LimitExceeded("admitted bytes".into()));
    }
    parse_manifest(bytes).map(|_| ())
}

pub(crate) fn admission(s: &str) -> ScipCaptureError {
    ScipCaptureError::InvalidAdmission(s.into())
}
pub(crate) fn hexhash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(crate) fn path(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('/')
        && !s.contains(['\\', '\0'])
        && s.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
}
pub(crate) fn validate_selected_role(root: &Path, p: &Path) -> Result<String, ScipCaptureError> {
    if !p.is_absolute() {
        return Err(admission("selected role is not absolute"));
    }
    relative(root, p)
}
fn relative(root: &Path, p: &Path) -> Result<String, ScipCaptureError> {
    let r = p
        .strip_prefix(root)
        .map_err(|_| admission("role outside root"))?
        .to_str()
        .ok_or_else(|| admission("non-UTF8 role"))?;
    if !path(r) {
        return Err(admission("invalid role path"));
    }
    let mut current = root.to_owned();
    for component in r.split('/') {
        current.push(component);
        let metadata =
            std::fs::symlink_metadata(&current).map_err(|_| admission("selected role absent"))?;
        if metadata.file_type().is_symlink() {
            return Err(admission("symlinked selected role"));
        }
    }
    Ok(r.into())
}
pub(crate) fn file_uri(root: &Path) -> Result<String, ScipCaptureError> {
    let s = root
        .to_str()
        .filter(|s| s.starts_with('/'))
        .ok_or_else(|| admission("non-UTF8 physical root"))?;
    let mut out = String::from("file://");
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    Ok(out)
}
pub(crate) fn order(language: &str) -> usize {
    match language {
        "java" => 0,
        "rust" => 1,
        "python" => 2,
        "javascript" => 3,
        _ => usize::MAX,
    }
}
fn sorted(rows: &[Doc]) -> bool {
    rows.windows(2).all(|w| {
        (order(&w[0].language), w[0].relative_path.as_bytes())
            < (order(&w[1].language), w[1].relative_path.as_bytes())
    })
}
fn checked_docs(rows: &[Doc]) -> Result<(), ScipCaptureError> {
    if !sorted(rows)
        || rows.iter().any(|d| {
            order(&d.language) == usize::MAX || !path(&d.relative_path) || !hexhash(&d.content_hash)
        })
    {
        return Err(admission("invalid document list"));
    }
    Ok(())
}
impl<'a> ReferenceProfileAdmission<'a> {
    pub fn from_trusted_selection(
        capture: &'a Capture,
        artifact_path: &Path,
        artifact_bytes: &'a [u8],
        manifest_path: &Path,
        manifest_bytes: &'a [u8],
        selection: &TrustedSelection,
        cancel: &CancelFlag,
    ) -> Result<Self, ScipCaptureError> {
        if cancel.load(Ordering::Relaxed) {
            return Err(ScipCaptureError::Cancelled);
        }
        let root = capture.physical_root();
        let identity = crate::store::topology::WorkspaceIdentity::discover(Some(root), root)
            .map_err(|_| admission("workspace identity"))?;
        identity
            .verify()
            .map_err(|_| admission("workspace identity changed"))?;
        let (source_set, revision) = captured_revision_header(capture, root, &identity.record_id)
            .map_err(|_| admission("native captured header"))?;
        let sources: BTreeMap<_, _> = capture
            .files
            .iter()
            .map(|f| ((f.language.as_str(), f.path.as_str()), f.text.as_bytes()))
            .collect();
        Self::from_native_sources(
            root,
            &source_set,
            &revision,
            &sources,
            artifact_path,
            artifact_bytes,
            manifest_path,
            manifest_bytes,
            selection,
            cancel,
        )
    }

    /// Admit original producer bytes against a retained native revision and its exact source bytes.
    #[allow(clippy::too_many_arguments)]
    pub fn from_retained_native(
        root: &Path,
        native: &crate::native_evidence::Artifact,
        sources: &'a BTreeMap<crate::native_evidence::DocumentKey, Vec<u8>>,
        artifact_path: &Path,
        artifact_bytes: &'a [u8],
        manifest_path: &Path,
        manifest_bytes: &'a [u8],
        selection: &TrustedSelection,
        cancel: &CancelFlag,
    ) -> Result<Self, ScipCaptureError> {
        let files: BTreeMap<_, _> = sources
            .iter()
            .map(|(key, bytes)| ((key.language.as_str(), key.path.as_str()), bytes.as_slice()))
            .collect();
        Self::from_native_sources(
            root,
            &native.source_set,
            &native.revision,
            &files,
            artifact_path,
            artifact_bytes,
            manifest_path,
            manifest_bytes,
            selection,
            cancel,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_native_sources(
        root: &Path,
        source_set: &crate::native_evidence::SourceSet,
        revision: &crate::native_evidence::Revision,
        files: &BTreeMap<(&str, &str), &'a [u8]>,
        artifact_path: &Path,
        artifact_bytes: &'a [u8],
        manifest_path: &Path,
        manifest_bytes: &'a [u8],
        selection: &TrustedSelection,
        cancel: &CancelFlag,
    ) -> Result<Self, ScipCaptureError> {
        if cancel.load(Ordering::Relaxed) {
            return Err(ScipCaptureError::Cancelled);
        }
        if manifest_bytes.len() > 1024 * 1024 || artifact_bytes.len() > 64 * 1024 * 1024 {
            return Err(ScipCaptureError::LimitExceeded("admitted bytes".into()));
        }
        if !hexhash(&selection.expected_manifest_hash)
            || scip_raw::sha(manifest_bytes) != selection.expected_manifest_hash
        {
            return Err(admission("expected manifest digest"));
        }
        let manifest = parse_manifest(manifest_bytes)?;
        let artifact_relative = relative(root, artifact_path)?;
        let _ = relative(root, manifest_path)?;
        if manifest.format_version != 1
            || manifest.trust_origin != "referenceProfile"
            || manifest.decoder_version != scip_raw::DECODER_VERSION
            || manifest.profile_id != selection.profile_id
            || manifest.producer.name != selection.producer_name
        {
            return Err(admission("manifest identity"));
        }
        let language = producer_language(&selection.producer_name)
            .ok_or_else(|| admission("unsupported producer"))?;
        let pinned = pinned_producer(
            REFERENCE_TRUST_ORIGIN,
            &selection.profile_id,
            &DescriptorProducer {
                name: manifest.producer.name.clone(),
                version: manifest.producer.version.clone(),
                identity_kind: IdentityKind::Executable,
                executable_hash: manifest.producer.executable_hash.clone(),
                position_encoding: manifest.producer.position_encoding,
                languages: vec![language.into()],
            },
        )?;
        if source_set.id != revision.source_set_id
            || manifest.source_set.id != source_set.id
            || manifest.source_set.root_id != source_set.root_id
            || manifest.source_set.languages != source_set.languages
            || manifest.source_set.dependencies != source_set.dependencies
            || manifest.source_revision != revision.id
            || manifest.components.toolchain_hash != revision.toolchain_hash
            || manifest.components.config_hash != revision.config_hash
            || manifest.components.dependency_hash != revision.dependency_hash
            || ![
                &manifest.components.toolchain_hash,
                &manifest.components.config_hash,
                &manifest.components.dependency_hash,
            ]
            .iter()
            .all(|h| hexhash(h))
        {
            return Err(admission("native revision components"));
        }
        let uri = file_uri(root)?;
        if manifest.metadata.project_root != uri
            || manifest.metadata.protocol_version != 0
            || manifest.metadata.text_document_encoding != 1
        {
            return Err(admission("metadata binding"));
        }
        checked_docs(&manifest.source_documents)?;
        checked_docs(&manifest.documents)?;
        if manifest.source_documents.len() != files.len()
            || manifest.source_documents.len() != revision.documents.len()
            || manifest.documents.is_empty()
            || manifest.documents.len() > 1000
        {
            return Err(admission("document count"));
        }
        if files.len() > 100000 {
            return Err(ScipCaptureError::LimitExceeded("source files".into()));
        }
        let rev: BTreeMap<_, _> = revision
            .documents
            .iter()
            .map(|d| ((d.key.language.as_str(), d.key.path.as_str()), d))
            .collect();
        if rev.len() != revision.documents.len() {
            return Err(admission("duplicate captured source"));
        }
        let mut source_keys = BTreeSet::new();
        let mut source_paths = BTreeSet::new();
        let mut source_hashes = Vec::new();
        for d in &manifest.source_documents {
            if cancel.load(Ordering::Relaxed) {
                return Err(ScipCaptureError::Cancelled);
            }
            if !source_paths.insert(d.relative_path.as_str()) {
                return Err(admission("duplicate source path"));
            }
            let key = (d.language.as_str(), d.relative_path.as_str());
            let f = files
                .get(&key)
                .ok_or_else(|| admission("missing captured source"))?;
            let r = rev
                .get(&key)
                .ok_or_else(|| admission("missing revision source"))?;
            if r.content_hash != d.content_hash
                || r.key.source_set_id != source_set.id
                || r.revision_id != revision.id
                || r.byte_length != f.len()
                || scip_raw::sha(f) != d.content_hash
                || !source_keys.insert(key)
            {
                return Err(admission("captured source hash"));
            }
            source_hashes.push(json!({"document":{"sourceSetId":source_set.id,"language":d.language,"path":d.relative_path},"contentHash":d.content_hash}));
        }
        if scip_raw::sha(&canonical(&json!(source_hashes))) != manifest.source_manifest_hash
            || !hexhash(&manifest.source_manifest_hash)
        {
            return Err(admission("source manifest hash"));
        }
        let artifact = manifest
            .artifacts
            .first()
            .filter(|_| manifest.artifacts.len() == 1)
            .ok_or_else(|| admission("artifact count"))?;
        if artifact.kind != "scipBinary"
            || artifact.path != artifact_relative
            || artifact.hash != scip_raw::sha(artifact_bytes)
            || artifact.language != language
        {
            return Err(admission("artifact identity"));
        }
        let mut covered = BTreeSet::new();
        let mut doc_paths = BTreeSet::new();
        let mut docs = Vec::new();
        for d in &manifest.documents {
            let key = (d.language.as_str(), d.relative_path.as_str());
            let f = files
                .get(&key)
                .ok_or_else(|| admission("unlisted covered source"))?;
            if d.language != language
                || d.content_hash != scip_raw::sha(f)
                || !doc_paths.insert(d.relative_path.as_str())
                || !source_keys.contains(&key)
            {
                return Err(admission("covered document"));
            }
            covered.insert(key);
            docs.push(AuthenticatedScipDocument {
                language: d.language.clone(),
                relative_path: d.relative_path.clone(),
                source_bytes: f,
                content_hash: d.content_hash.clone(),
            });
        }
        if artifact
            .documents
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            != manifest
                .documents
                .iter()
                .map(|d| d.relative_path.as_str())
                .collect::<Vec<_>>()
        {
            return Err(admission("artifact document list"));
        }
        let mut excluded = BTreeSet::new();
        for e in &manifest.exclusions {
            let key = (e.language.as_str(), e.relative_path.as_str());
            if e.language != language
                || !path(&e.relative_path)
                || !matches!(e.reason.as_str(), "notRequested" | "unsupported" | "failed")
                || !source_keys.contains(&key)
                || covered.contains(&key)
                || !excluded.insert(key)
            {
                return Err(admission("exclusion"));
            }
        }
        if manifest
            .exclusions
            .windows(2)
            .any(|w| w[0].relative_path.as_bytes() >= w[1].relative_path.as_bytes())
            || source_keys
                .iter()
                .filter(|(lang, _)| *lang == language)
                .any(|k| !covered.contains(k) && !excluded.contains(k))
            || (manifest.completion == Completion::Complete) != manifest.exclusions.is_empty()
        {
            return Err(admission("coverage completion"));
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(ScipCaptureError::Cancelled);
        }
        Ok(Self {
            input: AuthenticatedScipInput {
                artifact_bytes,
                artifact_hash: artifact.hash.clone(),
                producer_id: pinned.name.clone(),
                profile_id: pinned.profile_id.clone(),
                source_set_id: source_set.id.clone(),
                source_revision: revision.id.clone(),
                source_manifest_hash: manifest.source_manifest_hash,
                decoder_version: scip_raw::DECODER_VERSION,
                completion: manifest.completion,
                exclusions: manifest.exclusions,
                project_root_uri: uri,
                producer_version: pinned.version.clone(),
                position_encoding: pinned.position_encoding,
                protocol_version: 0,
                text_document_encoding: 1,
                documents: docs,
            },
            pinned,
        })
    }
}
pub fn decode_reference_profile(
    admission: &ReferenceProfileAdmission<'_>,
    cancel: &CancelFlag,
) -> Result<CapturedScipInventory, ScipCaptureError> {
    scip_raw::decode_authenticated(&admission.input, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::IndexOptions;
    use serde_json::Value;
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    struct RawInputs {
        artifact_path: std::path::PathBuf,
        manifest_path: std::path::PathBuf,
        artifact: Vec<u8>,
        manifest: Vec<u8>,
    }
    impl RawInputs {
        fn load(root: &Path, artifact: &Path) -> Self {
            let manifest_path = root.join("manifest.json");
            Self {
                artifact_path: artifact.to_owned(),
                manifest_path: manifest_path.clone(),
                artifact: fs::read(artifact).unwrap(),
                manifest: fs::read(manifest_path).unwrap(),
            }
        }
        fn admit<'a>(
            &'a self,
            capture: &'a Capture,
            selection: &TrustedSelection,
            cancel: &CancelFlag,
        ) -> Result<ReferenceProfileAdmission<'a>, ScipCaptureError> {
            ReferenceProfileAdmission::from_trusted_selection(
                capture,
                &self.artifact_path,
                &self.artifact,
                &self.manifest_path,
                &self.manifest,
                selection,
                cancel,
            )
        }
    }
    const WIRE: &str = "122e0a087372632f412e6a73220a6a617661736372697074120b0a0300090a1202412318011a090a0241232811320166";
    fn fixture() -> (
        tempfile::TempDir,
        Capture,
        TrustedSelection,
        Value,
        CancelFlag,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let canonical_root = fs::canonicalize(dir.path()).unwrap();
        let root = canonical_root.as_path();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/A.js"), "function f(){g();}\n").unwrap();
        fs::write(root.join("artifact.scip"), hex::decode(WIRE).unwrap()).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let options = IndexOptions::new(root.to_owned());
        let initial = Capture::admit(&options, &cancel, &|_| {}).unwrap();
        let identity = crate::store::topology::WorkspaceIdentity::discover(
            Some(initial.physical_root()),
            initial.physical_root(),
        )
        .unwrap();
        let (set, rev) =
            captured_revision_header(&initial, initial.physical_root(), &identity.record_id)
                .unwrap();
        let f = &initial.files[0];
        let profile: Hardware = serde_json::from_str(include_str!(
            "../docs/semantic-evidence/hardware/mac-mini-m5-pro-v1.json"
        ))
        .unwrap();
        let producer = profile
            .semantic_producers
            .iter()
            .find(|p| p.name == "scip-typescript")
            .unwrap();
        let doc = json!({"language":"javascript","relativePath":"src/A.js","contentHash":f.hash});
        let manifest_hash = scip_raw::sha(&canonical(
            &json!([{"document":{"sourceSetId":set.id,"language":"javascript","path":"src/A.js"},"contentHash":f.hash}]),
        ));
        let manifest = json!({"formatVersion":1,"trustOrigin":"referenceProfile","completion":"complete","decoderVersion":scip_raw::DECODER_VERSION,"profileId":profile.profile_id,
            "producer":{"name":producer.name,"version":producer.version,"executableHash":producer.executable_hash,"positionEncoding":2},
            "sourceSet":{"id":set.id,"rootId":set.root_id,"languages":set.languages,"dependencies":set.dependencies},
            "sourceRevision":rev.id,"sourceManifestHash":manifest_hash,
            "components":{"toolchainHash":rev.toolchain_hash,"configHash":rev.config_hash,"dependencyHash":rev.dependency_hash},
            "metadata":{"protocolVersion":0,"projectRoot":file_uri(initial.physical_root()).unwrap(),"textDocumentEncoding":1},
            "sourceDocuments":[doc],"documents":[doc],"exclusions":[],
            "artifacts":[{"kind":"scipBinary","path":"artifact.scip","hash":scip_raw::sha(&hex::decode(WIRE).unwrap()),"language":"javascript","documents":["src/A.js"]}]});
        let bytes = serde_json::to_vec(&manifest).unwrap();
        fs::write(root.join("manifest.json"), &bytes).unwrap();
        let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
        let selection = TrustedSelection {
            expected_manifest_hash: scip_raw::sha(&bytes),
            profile_id: "mac-mini-m5-pro-v1".into(),
            producer_name: "scip-typescript".into(),
        };
        (dir, capture, selection, manifest, cancel)
    }
    #[test]
    fn scip_trusted_capture_and_unattested_changes() {
        let (root, capture, selection, manifest, cancel) = fixture();
        let raw = RawInputs::load(
            capture.physical_root(),
            &capture.physical_root().join("artifact.scip"),
        );
        let admitted = raw.admit(&capture, &selection, &cancel).unwrap();
        let inventory = decode_reference_profile(&admitted, &cancel).unwrap();
        assert_eq!(inventory.claims.len(), 2);
        assert_eq!(
            inventory.artifact_hashes,
            [scip_raw::sha(&hex::decode(WIRE).unwrap())]
        );
        let mut wrong = selection.clone();
        wrong.expected_manifest_hash = "0".repeat(64);
        assert!(matches!(
            raw.admit(&capture, &wrong, &cancel),
            Err(ScipCaptureError::InvalidAdmission(_))
        ));
        wrong = selection.clone();
        wrong.producer_name = "rust-analyzer".into();
        assert!(raw.admit(&capture, &wrong, &cancel).is_err());
        for (pointer, value) in [
            ("/producer/executableHash", json!("0".repeat(64))),
            ("/sourceDocuments/0/contentHash", json!("1".repeat(64))),
            ("/components/configHash", json!("2".repeat(64))),
            ("/components/toolchainHash", json!("2".repeat(64))),
            ("/components/dependencyHash", json!("2".repeat(64))),
            ("/sourceRevision", json!("revision:v2:wrong")),
            ("/sourceManifestHash", json!("4".repeat(64))),
            ("/sourceSet/rootId", json!("other")),
            ("/artifacts/0/hash", json!("3".repeat(64))),
            ("/artifacts/0/path", json!("../escape")),
            ("/metadata/projectRoot", json!("file:///wrong")),
            ("/producer/positionEncoding", json!(3)),
            ("/producer/version", json!("untrusted-version")),
            ("/profileId", json!("other-profile")),
            ("/completion", json!("partial")),
            (
                "/exclusions",
                json!([{"language":"javascript","relativePath":"src/A.js","reason":"failed"}]),
            ),
        ] {
            let mut changed = manifest.clone();
            *changed.pointer_mut(pointer).unwrap() = value;
            let bytes = serde_json::to_vec(&changed).unwrap();
            fs::write(root.path().join("manifest.json"), &bytes).unwrap();
            let expected = TrustedSelection {
                expected_manifest_hash: scip_raw::sha(&bytes),
                ..selection.clone()
            };
            let mutated = RawInputs::load(root.path(), &root.path().join("artifact.scip"));
            let recaptured = Capture::admit(
                &IndexOptions {
                    workspace_root: fs::canonicalize(root.path()).unwrap(),
                    max_file_bytes: 2 * 1024 * 1024,
                },
                &cancel,
                &|_| {},
            )
            .unwrap();
            assert!(
                mutated.admit(&recaptured, &expected, &cancel).is_err(),
                "unexpected admitted mutated {pointer}"
            );
            assert!(
                mutated.admit(&recaptured, &selection, &cancel).is_err(),
                "unattested mutated {pointer}"
            );
        }
        cancel.store(true, Ordering::Relaxed);
        assert!(matches!(
            raw.admit(&capture, &selection, &cancel),
            Err(ScipCaptureError::Cancelled)
        ));
        assert!(matches!(
            decode_reference_profile(&admitted, &cancel),
            Err(ScipCaptureError::Cancelled)
        ));
    }
    #[test]
    fn manifest_only_encoding_change_rejected_with_unchanged_producer_pin() {
        let (root, _capture, selection, mut manifest, cancel) = fixture();
        let profile: Hardware = serde_json::from_str(include_str!(
            "../docs/semantic-evidence/hardware/mac-mini-m5-pro-v1.json"
        ))
        .unwrap();
        let pinned = profile
            .semantic_producers
            .iter()
            .find(|p| p.name == selection.producer_name)
            .unwrap();
        assert_eq!(pinned.position_encoding.as_deref(), Some("utf16"));
        assert_eq!(
            manifest.pointer("/producer/version").unwrap(),
            &json!(pinned.version)
        );
        assert_eq!(
            manifest.pointer("/producer/executableHash").unwrap(),
            &json!(pinned.executable_hash)
        );
        *manifest.pointer_mut("/producer/positionEncoding").unwrap() = json!(1);
        let bytes = serde_json::to_vec(&manifest).unwrap();
        fs::write(root.path().join("manifest.json"), &bytes).unwrap();
        let physical = fs::canonicalize(root.path()).unwrap();
        let capture = Capture::admit(
            &IndexOptions {
                workspace_root: physical.clone(),
                max_file_bytes: 2 * 1024 * 1024,
            },
            &cancel,
            &|_| {},
        )
        .unwrap();
        let raw = RawInputs::load(&physical, &physical.join("artifact.scip"));
        let selected = TrustedSelection {
            expected_manifest_hash: scip_raw::sha(&bytes),
            ..selection
        };
        assert!(matches!(raw.admit(&capture, &selected, &cancel),
            Err(ScipCaptureError::InvalidAdmission(reason)) if reason == "producer profile pin"));
    }
    #[test]
    fn scip_path_and_uri_reject_escapes() {
        assert!(relative(Path::new("/safe"), Path::new("/outside/file")).is_err());
        assert!(relative(Path::new("/safe"), Path::new("/safe/../other")).is_err());
        assert!(!path("/absolute"));
        assert!(!path("../escape"));
        assert!(!path("link/../escape"));
        assert_eq!(
            file_uri(Path::new("/tmp/a b")).unwrap(),
            "file:///tmp/a%20b"
        );
    }
    #[test]
    fn scip_manifest_original_bytes_limit_precedes_parse() {
        let (root, _, selection, _, cancel) = fixture();
        fs::write(
            root.path().join("manifest.json"),
            vec![b' '; 1024 * 1024 + 1],
        )
        .unwrap();
        let options = IndexOptions::new(root.path().to_owned());
        let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
        let raw = RawInputs::load(root.path(), &root.path().join("artifact.scip"));
        assert!(matches!(
            raw.admit(&capture, &selection, &cancel),
            Err(ScipCaptureError::LimitExceeded(_))
        ));
    }
    #[test]
    fn scip_admission_rejects_captured_symlinked_ancestor_role() {
        let (root, _, mut selection, mut manifest, cancel) = fixture();
        let physical = fs::canonicalize(root.path()).unwrap();
        let linked = physical.join("linked");
        fs::create_dir(&linked).unwrap();
        fs::write(linked.join("artifact.scip"), hex::decode(WIRE).unwrap()).unwrap();
        manifest["artifacts"][0]["path"] = json!("linked/artifact.scip");
        let bytes = serde_json::to_vec(&manifest).unwrap();
        fs::write(physical.join("manifest.json"), &bytes).unwrap();
        selection.expected_manifest_hash = scip_raw::sha(&bytes);
        let options = IndexOptions {
            workspace_root: physical.clone(),
            max_file_bytes: 2 * 1024 * 1024,
        };
        let baseline = Capture::admit(&options, &cancel, &|_| {}).unwrap();
        let identity =
            crate::store::topology::WorkspaceIdentity::discover(Some(&physical), &physical)
                .unwrap();
        let (_, revision) =
            captured_revision_header(&baseline, &physical, &identity.record_id).unwrap();
        manifest["sourceRevision"] = json!(revision.id);
        manifest["components"] = json!({
            "toolchainHash": revision.toolchain_hash,
            "configHash": revision.config_hash,
            "dependencyHash": revision.dependency_hash,
        });
        let bytes = serde_json::to_vec(&manifest).unwrap();
        fs::write(physical.join("manifest.json"), &bytes).unwrap();
        selection.expected_manifest_hash = scip_raw::sha(&bytes);
        let baseline = Capture::admit(&options, &cancel, &|_| {}).unwrap();
        let raw = RawInputs::load(&physical, &linked.join("artifact.scip"));
        let admitted = raw.admit(&baseline, &selection, &cancel).unwrap();
        assert_eq!(
            decode_reference_profile(&admitted, &cancel)
                .unwrap()
                .claims
                .len(),
            2
        );

        let outside = tempfile::tempdir().unwrap();
        fs::write(
            outside.path().join("artifact.scip"),
            hex::decode(WIRE).unwrap(),
        )
        .unwrap();
        fs::remove_dir_all(&linked).unwrap();
        std::os::unix::fs::symlink(outside.path(), &linked).unwrap();
        let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
        let linked_raw = RawInputs::load(&physical, &linked.join("artifact.scip"));
        assert!(matches!(
            linked_raw.admit(&capture, &selection, &cancel),
            Err(ScipCaptureError::InvalidAdmission(message)) if message == "symlinked selected role"
        ));
    }
    #[test]
    fn explicit_inputs_reject_symlinked_role() {
        let (root, capture, selection, _, cancel) = fixture();
        let link = root.path().join("symlink.scip");
        std::os::unix::fs::symlink(root.path().join("artifact.scip"), &link).unwrap();
        let raw = RawInputs::load(
            capture.physical_root(),
            &capture.physical_root().join("symlink.scip"),
        );
        assert!(
            matches!(raw.admit(&capture, &selection, &cancel), Err(ScipCaptureError::InvalidAdmission(message)) if message == "symlinked selected role")
        );
    }
    fn declared(pinned: &PinnedProducer) -> DescriptorProducer {
        DescriptorProducer {
            name: pinned.name.clone(),
            version: pinned.version.clone(),
            identity_kind: pinned.identity_kind,
            executable_hash: pinned.executable_hash.clone(),
            position_encoding: pinned.position_encoding,
            languages: pinned.languages.clone(),
        }
    }
    #[test]
    fn synthetic_profile_pins_generator_identity() {
        let synthetic = pinned_producer_by_id("trellis-synthetic-scip").unwrap();
        let profile: Value = serde_json::from_str(include_str!(
            "../docs/semantic-evidence/synthetic-profiles/trellis-synthetic-scip-v1.json"
        ))
        .unwrap();
        assert_eq!(synthetic.identity_kind, IdentityKind::GeneratorSource);
        assert_eq!(synthetic.trust_origin, "syntheticProfile");
        assert_eq!(synthetic.profile_id, "trellis-synthetic-scip-v1");
        assert_eq!(synthetic.version, "1");
        assert_eq!(synthetic.position_encoding, 1);
        assert_eq!(
            synthetic.languages,
            ["java", "rust", "python", "javascript"]
        );
        assert_eq!(
            synthetic.executable_hash,
            profile["generator"]["sourceSha256"].as_str().unwrap()
        );
        assert_eq!(
            synthetic.executable_hash,
            scip_raw::sha(include_bytes!(
                "../tools/semantic-contract/generate-performance-scip.py"
            ))
        );
        let producer = synthetic.producer();
        assert_eq!(producer.id, "trellis-synthetic-scip");
        assert_eq!(producer.executable_hash, synthetic.executable_hash);
        assert_eq!(producer.languages.len(), 4);
        assert_eq!(producer.position_encoding, PositionEncoding::Utf8);
        assert_eq!(
            pinned_producer(
                "syntheticProfile",
                &synthetic.profile_id,
                &declared(&synthetic)
            )
            .unwrap(),
            synthetic
        );
    }
    #[test]
    fn reference_profile_producers_are_unchanged() {
        let profile: Hardware = serde_json::from_str(include_str!(
            "../docs/semantic-evidence/hardware/mac-mini-m5-pro-v1.json"
        ))
        .unwrap();
        for (id, language, semantic_language) in [
            ("scip-java", "java", Language::Java),
            ("scip-typescript", "javascript", Language::Javascript),
            ("scip-python", "python", Language::Python),
            ("rust-analyzer", "rust", Language::Rust),
        ] {
            let row = profile
                .semantic_producers
                .iter()
                .find(|p| p.name == id)
                .unwrap();
            let pinned = pinned_producer_by_id(id).unwrap();
            let encoding = match row.position_encoding.as_deref().unwrap() {
                "utf8" => (1, PositionEncoding::Utf8),
                "utf16" => (2, PositionEncoding::Utf16),
                _ => (3, PositionEncoding::UnicodeScalar),
            };
            assert_eq!(producer_language(id), Some(language));
            assert_eq!(pinned.identity_kind, IdentityKind::Executable);
            assert_eq!(pinned.trust_origin, "referenceProfile");
            assert_eq!(pinned.profile_id, profile.profile_id);
            assert_eq!(pinned.position_encoding, encoding.0);
            assert_eq!(pinned.languages, [language]);
            assert_eq!(
                pinned.producer(),
                semantic_evidence::Producer {
                    id: id.into(),
                    version: row.version.clone(),
                    executable_hash: row.executable_hash.clone(),
                    kind: SemanticKind::Semantic,
                    languages: vec![semantic_language],
                    position_encoding: encoding.1,
                }
            );
            assert_eq!(
                pinned_producer("referenceProfile", &profile.profile_id, &declared(&pinned))
                    .unwrap(),
                pinned
            );
        }
        assert_eq!(pinned_producer_by_id("scip-unknown"), None);
        assert_eq!(producer_language("scip-unknown"), None);
    }
    #[test]
    fn synthetic_and_reference_identities_cannot_be_crossed() {
        let synthetic = pinned_producer_by_id("trellis-synthetic-scip").unwrap();
        let reference = pinned_producer_by_id("rust-analyzer").unwrap();
        let pin = |origin: &str, profile: &str, producer: &DescriptorProducer| match pinned_producer(
            origin, profile, producer,
        ) {
            Err(ScipCaptureError::InvalidAdmission(detail)) => detail,
            other => panic!("expected an admission error, got {other:?}"),
        };
        assert_eq!(
            pin(
                "referenceProfile",
                &synthetic.profile_id,
                &declared(&synthetic)
            ),
            "producer not pinned"
        );
        assert_eq!(
            pin(
                "syntheticProfile",
                &reference.profile_id,
                &declared(&synthetic)
            ),
            "producer profile pin"
        );
        assert_eq!(
            pin(
                "syntheticProfile",
                &synthetic.profile_id,
                &declared(&reference)
            ),
            "producer profile pin"
        );
        let mut kind = declared(&synthetic);
        kind.identity_kind = IdentityKind::Executable;
        assert_eq!(
            pin("syntheticProfile", &synthetic.profile_id, &kind),
            "producer profile pin"
        );
        let mut kind = declared(&reference);
        kind.identity_kind = IdentityKind::GeneratorSource;
        assert_eq!(
            pin("referenceProfile", &reference.profile_id, &kind),
            "producer profile pin"
        );
        let mut languages = declared(&synthetic);
        languages.languages.reverse();
        assert_eq!(
            pin("syntheticProfile", &synthetic.profile_id, &languages),
            "producer profile pin"
        );
        assert_eq!(
            pin("localProfile", &reference.profile_id, &declared(&reference)),
            "producer profile pin"
        );
    }
    #[test]
    fn witness_language_keeps_the_fast_path_closure() {
        assert_eq!(witness_language("scip-typescript"), Some("javascript"));
        assert_eq!(witness_language("scip-python"), Some("python"));
        assert_eq!(witness_language("rust-analyzer"), Some("rust"));
        assert_eq!(witness_language("scip-java"), None);
        assert_eq!(witness_language("trellis-synthetic-scip"), None);
    }
}
