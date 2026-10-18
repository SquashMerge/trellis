//! Closed native-only in-memory evidence, derived from the admitted immutable capture.
use crate::{
    capture::Capture,
    model::{CancelFlag, SourceFile},
    native_ids::{IdentityRegistry, canonical, digest, extraction_context},
};
use anyhow::{Context, Result, ensure};
use icu_normalizer::ComposingNormalizerBorrowed;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use tree_sitter::Node;

fn required_nullable<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

const LANGUAGES: [&str; 4] = ["java", "rust", "python", "javascript"];
pub(crate) const PRODUCER: &str = "trellis.native.syntax";
/// Native producer descriptor version. Any change that can alter a native measured field or
/// projection, or that starts reading another input, must change it (Decision 0003).
pub(crate) const NATIVE_VERSION: &str = "native-v5";
/// The native producer's declared extraction-input inventory (Decision 0003), keyed by its
/// descriptor `(id, version)` and language. `extract` measures one document from only its own
/// bytes, language and path; the tree-sitter grammars and Unicode normalization tables it uses
/// are compiled into the executable and fixed by `NATIVE_VERSION`. It reads no configuration,
/// toolchain or dependency capture, and no other source document, so every language declares
/// the explicit empty inventory. An unlisted descriptor or language has no declaration.
type LanguageInventory = (&'static str, &'static [&'static str]);
const EXTRACTION_INPUTS: &[(&str, &str, &[LanguageInventory])] = &[(
    PRODUCER,
    NATIVE_VERSION,
    &[
        ("java", &[]),
        ("rust", &[]),
        ("python", &[]),
        ("javascript", &[]),
    ],
)];

// Private unit-test fault injection only. Integration tests compile the ordinary
// library, and release builds have no override, branch, or alternate inventory.
#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum ExtractionAuthFault {
    Undeclared,
    Mismatched,
}
#[cfg(test)]
thread_local! {
    static EXTRACTION_AUTH_FAULT: std::cell::RefCell<Option<(String, String, ExtractionAuthFault)>> =
        const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
pub(crate) struct ExtractionAuthFaultGuard;
#[cfg(test)]
impl Drop for ExtractionAuthFaultGuard {
    fn drop(&mut self) {
        EXTRACTION_AUTH_FAULT.with(|fault| *fault.borrow_mut() = None);
    }
}
#[cfg(test)]
pub(crate) fn inject_extraction_auth_fault(
    revision: &str,
    language: &str,
    fault: ExtractionAuthFault,
) -> ExtractionAuthFaultGuard {
    EXTRACTION_AUTH_FAULT.with(|slot| {
        assert!(
            slot.borrow().is_none(),
            "nested extraction authentication injection"
        );
        *slot.borrow_mut() = Some((revision.to_owned(), language.to_owned(), fault));
    });
    ExtractionAuthFaultGuard
}
/// Authenticate the declared inventory of `producer` for `language` against the captured
/// revision and derive the document's extraction context. This is the fail-closed
/// precondition for minting any `occ:v2` ID: an undeclared descriptor/language or a declared
/// component without a valid captured revision digest refuses, and nothing is minted.
pub(crate) fn native_extraction_context(
    producer: &Producer,
    language: &str,
    revision: &Revision,
) -> Result<String> {
    #[cfg(test)]
    if let Some(fault) = EXTRACTION_AUTH_FAULT.with(|slot| {
        slot.borrow().as_ref().and_then(|(id, lang, fault)| {
            (id == &revision.id && lang == language).then_some(*fault)
        })
    }) {
        return match fault {
            ExtractionAuthFault::Undeclared => {
                anyhow::bail!("undeclared native extraction-input inventory")
            }
            ExtractionAuthFault::Mismatched => authenticated_extraction_context(
                language,
                &["config"],
                &[("config", Some(b"test-only mismatched captured config"))],
                revision,
            ),
        };
    }
    let inventory = declared_extraction_inputs(&producer.id, &producer.version, language)?;
    // The v4 parser reads no external component: all grammars and normalization tables are
    // fixed in this executable. Other captured inputs affect revision admission, not syntax.
    authenticated_extraction_context(language, inventory, &[], revision)
}
fn declared_extraction_inputs(
    producer_id: &str,
    producer_version: &str,
    language: &str,
) -> Result<&'static [&'static str]> {
    EXTRACTION_INPUTS
        .iter()
        .find(|(id, version, _)| *id == producer_id && *version == producer_version)
        .and_then(|(_, _, languages)| languages.iter().find(|(l, _)| *l == language))
        .map(|(_, inventory)| *inventory)
        .context("undeclared native extraction-input inventory")
}

/// Verify the descriptor's declared inventory before matching a stored witness.
/// This v4 producer declares no external per-document inputs. Should a future
/// producer declare any, selected reuse MUST supply captured bytes rather than
/// silently comparing against the old empty extraction context.
pub(crate) fn declared_selected_extraction_context(
    producer_id: &str,
    producer_version: &str,
    language: &str,
) -> Result<String> {
    let inventory = declared_extraction_inputs(producer_id, producer_version, language)?;
    selected_extraction_context_for_inventory(language, inventory)
}

fn selected_extraction_context_for_inventory(language: &str, inventory: &[&str]) -> Result<String> {
    ensure!(
        inventory.is_empty(),
        "external native extraction inputs require captured selected-reuse proof"
    );
    extraction_context(language, &[])
}

fn authenticated_extraction_context(
    language: &str,
    inventory: &[&str],
    observed: &[(&str, Option<&[u8]>)],
    revision: &Revision,
) -> Result<String> {
    let mut declared = BTreeSet::new();
    for name in inventory {
        ensure!(
            matches!(*name, "toolchain" | "config" | "dependency") && declared.insert(*name),
            "unsupported or duplicate native extraction component: {name}"
        );
    }
    let mut captured = BTreeMap::new();
    for (name, bytes) in observed {
        ensure!(
            declared.contains(name) && captured.insert(*name, *bytes).is_none(),
            "undeclared or duplicate captured native extraction component: {name}"
        );
    }
    let mut components = Vec::with_capacity(inventory.len());
    for name in inventory {
        let bytes = captured
            .get(name)
            .context("absent native extraction component capture")?
            .context("absent native extraction component bytes")?;
        let expected = match *name {
            "toolchain" => &revision.toolchain_hash,
            "config" => &revision.config_hash,
            "dependency" => &revision.dependency_hash,
            _ => unreachable!(),
        };
        ensure!(
            valid_hash(expected) && hash(bytes) == *expected,
            "mismatched native extraction component: {name}"
        );
        components.push(((*name).to_owned(), expected.clone()));
    }
    components.sort();
    extraction_context(language, &components)
}
fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn text(s: &str) -> Result<()> {
    ensure!(!s.is_empty(), "empty text");
    Ok(())
}
fn safe_path(s: &str) -> Result<()> {
    text(s)?;
    ensure!(
        !s.starts_with('/')
            && !s.contains(['\\', '\0'])
            && s.split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "invalid native path: {s}"
    );
    Ok(())
}
fn valid_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn language_order(s: &str) -> Result<usize> {
    LANGUAGES
        .iter()
        .position(|l| *l == s)
        .context("unsupported native language")
}
fn span(text: &str, r: &Range, empty: bool) -> Result<()> {
    ensure!(
        r.start <= r.end
            && r.end <= text.len()
            && (empty || r.start < r.end)
            && text.is_char_boundary(r.start)
            && text.is_char_boundary(r.end),
        "invalid native byte range"
    );
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Producer {
    pub id: String,
    pub version: String,
    pub executable_hash: String,
    pub kind: String,
    pub languages: Vec<String>,
    pub position_encoding: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceSet {
    pub id: String,
    pub root_id: String,
    pub languages: Vec<String>,
    pub dependencies: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocumentKey {
    pub source_set_id: String,
    pub language: String,
    pub path: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Document {
    pub key: DocumentKey,
    pub revision_id: String,
    pub content_hash: String,
    pub byte_length: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Revision {
    pub id: String,
    pub source_set_id: String,
    pub documents: Vec<Document>,
    pub toolchain_hash: String,
    pub config_hash: String,
    pub dependency_hash: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Coverage {
    pub producer_id: String,
    pub language: String,
    pub source_set_id: String,
    pub document_path: String,
    pub revision_id: String,
    pub requested: bool,
    pub selected: bool,
    pub state: String,
    pub supported_roles: Vec<String>,
    pub observed_roles: Vec<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub diagnostic: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Provenance {
    pub id: String,
    pub producer_id: String,
    pub document: DocumentKey,
    pub revision_id: String,
    pub content_hash: String,
    pub evidence_kind: String,
    #[serde(deserialize_with = "required_nullable")]
    pub basis: Option<Value>,
    pub freshness: String,
    #[serde(deserialize_with = "required_nullable")]
    pub derived_from: Option<Value>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Range {
    pub start: usize,
    pub end: usize,
}
impl Range {
    fn node(n: Node<'_>) -> Self {
        Self {
            start: n.start_byte(),
            end: n.end_byte(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Signature {
    #[serde(rename = "parameterTypes")]
    pub parameter_types: Vec<String>,
    #[serde(rename = "typeParameterCount")]
    pub type_parameter_count: usize,
    pub variadic: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Key {
    pub kind: String,
    #[serde(deserialize_with = "required_nullable")]
    pub name: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub signature: Option<Signature>,
    pub ordinal: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Parameter {
    #[serde(deserialize_with = "required_nullable")]
    pub name: Option<String>,
    #[serde(rename = "type")]
    #[serde(deserialize_with = "required_nullable")]
    pub type_name: Option<String>,
    pub variadic: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Header {
    pub kind: String,
    #[serde(deserialize_with = "required_nullable")]
    pub name: Option<String>,
    pub modifiers: Vec<String>,
    pub type_parameters: Vec<String>,
    pub parameters: Vec<Parameter>,
    #[serde(deserialize_with = "required_nullable")]
    pub result_type: Option<String>,
    pub bases: Vec<String>,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DeclarationRole {
    Test,
    Production,
}
impl DeclarationRole {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Production => "production",
        }
    }
    pub(crate) fn from_stored(value: &str) -> Result<Self> {
        match value {
            "test" => Ok(Self::Test),
            "production" => Ok(Self::Production),
            _ => anyhow::bail!("store_unavailable: invalid declaration role"),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Declaration {
    pub role: DeclarationRole,
    pub syntax_id: String,
    pub document: DocumentKey,
    pub revision_id: String,
    pub kind: String,
    #[serde(deserialize_with = "required_nullable")]
    pub name: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub lookup_key: Option<String>,
    pub ancestors: Vec<Key>,
    pub key: Key,
    pub range: Range,
    #[serde(deserialize_with = "required_nullable")]
    pub name_range: Option<Range>,
    pub header: Header,
    pub provenance_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Call {
    pub id: String,
    pub owner_syntax_id: String,
    pub ordinal: usize,
    pub document: DocumentKey,
    pub revision_id: String,
    pub range: Range,
    #[serde(deserialize_with = "required_nullable")]
    pub callee_range: Option<Range>,
    #[serde(deserialize_with = "required_nullable")]
    pub spelling: Option<String>,
    pub region_ids: Vec<String>,
    pub provenance_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControlRegion {
    pub id: String,
    pub owner_syntax_id: String,
    pub ordinal: usize,
    pub document: DocumentKey,
    pub revision_id: String,
    pub kind: String,
    pub range: Range,
    #[serde(deserialize_with = "required_nullable")]
    pub parent_id: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub arm: Option<String>,
    pub provenance_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Artifact {
    pub producer: Producer,
    pub source_set: SourceSet,
    pub revision: Revision,
    pub coverage: Vec<Coverage>,
    pub provenance: Vec<Provenance>,
    pub declarations: Vec<Declaration>,
    pub calls: Vec<Call>,
    pub control_regions: Vec<ControlRegion>,
}

/// Canonical evidence checksum for a document version. Revision-scoped proof IDs are
/// deliberately projected at the requested pin rather than stored in this witness.
/// The caller must independently bind source bytes, producer and manifest identity.
pub(crate) fn document_witness(
    document: &Document,
    producer: &Producer,
    coverage: &Coverage,
    declarations: &[Declaration],
    calls: &[Call],
    regions: &[ControlRegion],
) -> Result<String> {
    fn stable<T: Serialize>(items: &[T]) -> Result<Vec<Value>> {
        let mut facts: Vec<Value> = items
            .iter()
            .map(|item| {
                let mut value = serde_json::to_value(item)?;
                if let Some(object) = value.as_object_mut() {
                    object.remove("revisionId");
                    object.remove("provenanceId");
                }
                Ok(value)
            })
            .collect::<Result<_>>()?;
        // Canonicalization walks and sorts nested JSON. Compute each sort key
        // once; recalculating it for every comparison dominates reuse checks.
        facts.sort_by_cached_key(canonical);
        Ok(facts)
    }
    let mut coverage = serde_json::to_value(coverage)?;
    coverage
        .as_object_mut()
        .context("invalid coverage")?
        .remove("revisionId");
    Ok(digest(
        b"trellis.native-version-witness.v1\0",
        &canonical(&json!({
            "documentKey": document.key, "contentHash": document.content_hash,
            "byteLength": document.byte_length, "producerId": producer.id,
            "producerVersion": producer.version,
            "extractionContext": extraction_context(&document.key.language, &[])?,
            "coverage": coverage, "declarations": stable(declarations)?,
            "calls": stable(calls)?, "regions": stable(regions)?,
        })),
    ))
}

pub fn lookup(language: &str, name: &str) -> Result<String> {
    text(name)?;
    Ok(match language {
        "rust" => ComposingNormalizerBorrowed::new_nfc()
            .normalize(name)
            .into_owned(),
        "python" => ComposingNormalizerBorrowed::new_nfkc()
            .normalize(name)
            .into_owned(),
        "java" | "javascript" => name.to_owned(),
        _ => anyhow::bail!("unsupported native language"),
    })
}

// Each absent input is explicit, never indistinguishable from a present empty file.
fn entry(root: &Path, path: &Path, bytes: Option<&[u8]>) -> Result<Value> {
    let rel = path
        .strip_prefix(root)?
        .to_str()
        .context("non-UTF8 native input path")?
        .replace('\\', "/");
    safe_path(&rel)?;
    Ok(match bytes {
        Some(bytes) => {
            json!({"path":rel,"type":"present","bytes":hex::encode(bytes),"byteLength":bytes.len()})
        }
        None => json!({"path":rel,"type":"absent"}),
    })
}

pub fn from_capture(
    capture: &Capture,
    root: &Path,
    root_id: &str,
    cancel: &CancelFlag,
) -> Result<Artifact> {
    from_capture_observed(capture, root, root_id, cancel, |_, _| {})
}

/// Call-scoped evidence from the full native pipeline. `Measured` fires only
/// after extraction of that captured document; `Validated` fires only after
/// the independent full-bundle rebuild and comparison have succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullNativeStage {
    Measured,
    Validated,
}

pub(crate) fn from_capture_observed(
    capture: &Capture,
    root: &Path,
    root_id: &str,
    cancel: &CancelFlag,
    observe: impl Fn(&DocumentKey, FullNativeStage),
) -> Result<Artifact> {
    let artifact = build_native_observed(capture, root, root_id, &|key| {
        observe(key, FullNativeStage::Measured);
    })?;
    artifact.validate(capture, root, root_id, cancel)?;
    for document in &artifact.revision.documents {
        observe(&document.key, FullNativeStage::Validated);
    }
    Ok(artifact)
}

/// The full captured revision header is shared by full and selected measurement.
/// Selecting one file must never change the source-set, producer or revision ID.
fn build_native_header<'a>(
    capture: &'a Capture,
    root: &Path,
    root_id: &str,
) -> Result<(Artifact, Vec<&'a SourceFile>)> {
    text(root_id)?;
    let identity = crate::store::topology::WorkspaceIdentity::discover(Some(root), root)?;
    ensure!(
        identity.record_id == root_id,
        "native source-set root identity mismatch"
    );
    identity.verify()?;
    let source_set_id = format!("source-set:v1:{root_id}");
    let executable_hash = capture
        .executable_digest(capture.executable_path())
        .context("native executable not admitted or replaced")?
        .to_owned();
    let producer = Producer {
        id: PRODUCER.into(),
        version: NATIVE_VERSION.into(),
        executable_hash: executable_hash.clone(),
        kind: "native".into(),
        languages: LANGUAGES.map(str::to_owned).into(),
        position_encoding: "utf8".into(),
    };
    let source_set = SourceSet {
        id: source_set_id.clone(),
        root_id: root_id.into(),
        languages: producer.languages.clone(),
        dependencies: vec![],
    };
    let root_inputs: Vec<_> = crate::capture::ROOT_INPUTS[..22]
        .iter()
        .map(|name| root.join(name))
        .collect();
    let dependencies: Vec<Value> = root_inputs
        .iter()
        .map(|p| entry(root, p, capture.bytes(p)))
        .collect::<Result<_>>()?;
    let mut config = dependencies.clone();
    let selectors: Vec<_> = crate::capture::ROOT_INPUTS[22..]
        .iter()
        .map(|n| root.join(n))
        .collect();
    for p in &selectors {
        config.push(entry(root, p, capture.bytes(p))?);
    }
    config.push(
        json!({"nativeAdmission":{"maxFileBytes":capture.reconcile_options().max_file_bytes}}),
    );
    for (p, b) in capture.admitted_inputs() {
        if p.file_name()
            .is_some_and(|n| n == ".gitignore" || n == ".ignore")
            && p.starts_with(root)
        {
            config.push(entry(root, p, b)?);
        }
    }
    let selectors: Vec<Value> = selectors
        .iter()
        .map(|p| entry(root, p, capture.bytes(p)))
        .collect::<Result<_>>()?;
    let toolchain = json!({"nativeExecutable":executable_hash,"rootRustToolchain":selectors[0],"rootRustToolchainToml":selectors[1]});
    let mut files: Vec<_> = capture.files.iter().collect();
    files.sort_by(|a, b| {
        language_order(&a.language)
            .unwrap()
            .cmp(&language_order(&b.language).unwrap())
            .then(a.path.as_bytes().cmp(b.path.as_bytes()))
    });
    let id = format!(
        "revision:v2:{}",
        native_revision_digest(&source_set_id, &files, &toolchain, &config, &dependencies)
    );
    let documents = files
        .iter()
        .map(|f| Document {
            key: DocumentKey {
                source_set_id: source_set_id.clone(),
                language: f.language.clone(),
                path: f.path.clone(),
            },
            revision_id: id.clone(),
            content_hash: f.hash.clone(),
            byte_length: f.text.len(),
        })
        .collect();
    let revision = Revision {
        id,
        source_set_id,
        documents,
        toolchain_hash: hash(&canonical(&toolchain)),
        config_hash: hash(&canonical(&json!(config))),
        dependency_hash: hash(&canonical(&json!(dependencies))),
    };
    let artifact = Artifact {
        producer,
        source_set,
        revision,
        coverage: vec![],
        provenance: vec![],
        declarations: vec![],
        calls: vec![],
        control_regions: vec![],
    };
    Ok((artifact, files))
}

/// `digest("trellis.native-revision.v2\0", canonical(revision_input))`, where
/// `revision_input` is
/// `{"config","dependencies","documents":[{"byteLength","contentHash","language","path"}],"sourceSetId","toolchain"}`
/// (Decision 0010). Each document enters by its SHA-256 content hash, which
/// capture already computed, not by its bytes, so the cost is per document,
/// not per corpus byte. The canonical bytes are written straight into the
/// hasher in canonical key order.
fn native_revision_digest(
    source_set_id: &str,
    files: &[&SourceFile],
    toolchain: &Value,
    config: &[Value],
    dependencies: &[Value],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"trellis.native-revision.v2\0");
    let out = &mut |bytes: &[u8]| hasher.update(bytes);
    let array = |items: &[Value], out: &mut dyn FnMut(&[u8])| {
        out(b"[");
        for (i, item) in items.iter().enumerate() {
            if i != 0 {
                out(b",");
            }
            crate::native_ids::canonical_into(item, out);
        }
        out(b"]");
    };
    out(b"{\"config\":");
    array(config, out);
    out(b",\"dependencies\":");
    array(dependencies, out);
    out(b",\"documents\":[");
    for (i, file) in files.iter().enumerate() {
        if i != 0 {
            out(b",");
        }
        out(b"{\"byteLength\":");
        out(file.text.len().to_string().as_bytes());
        out(b",\"contentHash\":");
        crate::native_ids::canonical_string_into(&file.hash, out);
        out(b",\"language\":");
        crate::native_ids::canonical_string_into(&file.language, out);
        out(b",\"path\":");
        crate::native_ids::canonical_string_into(&file.path, out);
        out(b"}");
    }
    out(b"],\"sourceSetId\":");
    crate::native_ids::canonical_string_into(source_set_id, out);
    out(b",\"toolchain\":");
    crate::native_ids::canonical_into(toolchain, out);
    out(b"}");
    hex::encode(hasher.finalize())
}

fn build_native(capture: &Capture, root: &Path, root_id: &str) -> Result<Artifact> {
    build_native_observed(capture, root, root_id, &|_| {})
}

fn build_native_observed(
    capture: &Capture,
    root: &Path,
    root_id: &str,
    on_extract: &impl Fn(&DocumentKey),
) -> Result<Artifact> {
    let (mut artifact, files) = build_native_header(capture, root, root_id)?;
    let mut ids = IdentityRegistry::default();
    for (index, f) in files.into_iter().enumerate() {
        // The revision header was built from this exact sorted file sequence.
        // Keep a concrete document witness, rather than scanning the full
        // revision header once per file.
        let document = artifact.revision.documents[index].clone();
        extract_known(&mut artifact, f, &document, &mut ids)?;
        on_extract(&document.key);
    }
    Ok(artifact)
}

/// Native facts for exactly one document of the full admitted revision. This is not
/// an Artifact and cannot be passed to the v8 publisher or full-bundle validator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedDocument {
    pub producer: Producer,
    pub source_set: SourceSet,
    pub revision: Revision,
    pub document: Document,
    pub coverage: Coverage,
    pub provenance: Provenance,
    pub declarations: Vec<Declaration>,
    pub calls: Vec<Call>,
    pub control_regions: Vec<ControlRegion>,
}

/// Extract only the selected admitted source; all header fields retain the full
/// captured snapshot identity. `on_extract` attests successful real native assembly,
/// not a classifier candidate or a full-bundle parse.
pub(crate) fn measure_captured_document(
    capture: &Capture,
    root: &Path,
    root_id: &str,
    path: &str,
    cancel: &CancelFlag,
    mut on_extract: impl FnMut(&DocumentKey),
) -> Result<SelectedDocument> {
    capture.verify(cancel)?;
    let (mut artifact, files) = build_native_header(capture, root, root_id)?;
    let selected: Vec<_> = files.into_iter().filter(|file| file.path == path).collect();
    ensure!(
        selected.len() == 1,
        "selected native source must be unique and admitted"
    );
    let file = selected[0];
    ensure!(
        capture.hashes.get(path) == Some(&file.hash) && file.hash == hash(file.text.as_bytes()),
        "selected native source bytes/hash mismatch"
    );
    let documents: Vec<_> = artifact
        .revision
        .documents
        .iter()
        .filter(|document| document.key.path == path)
        .cloned()
        .collect();
    ensure!(
        documents.len() == 1,
        "selected native revision document must be unique"
    );
    let document = documents[0].clone();
    ensure!(
        document.key.source_set_id == artifact.source_set.id
            && document.key.language == file.language
            && document.content_hash == file.hash
            && document.byte_length == file.text.len()
            && document.revision_id == artifact.revision.id,
        "selected native document not bound to full capture"
    );
    let mut ids = IdentityRegistry::default();
    extract(&mut artifact, file, &mut ids)?;
    ensure!(
        artifact.coverage.len() == 1
            && artifact.provenance.len() == 1
            && artifact.coverage[0].document_path == path
            && artifact.provenance[0].document == document.key
            && artifact
                .declarations
                .iter()
                .all(|row| row.document == document.key)
            && artifact
                .calls
                .iter()
                .all(|row| row.document == document.key)
            && artifact
                .control_regions
                .iter()
                .all(|row| row.document == document.key),
        "selected native facts escaped the admitted document"
    );
    capture.verify(cancel)?;
    on_extract(&document.key);
    Ok(SelectedDocument {
        producer: artifact.producer,
        source_set: artifact.source_set,
        revision: artifact.revision,
        document,
        coverage: artifact.coverage.remove(0),
        provenance: artifact.provenance.remove(0),
        declarations: artifact.declarations,
        calls: artifact.calls,
        control_regions: artifact.control_regions,
    })
}

/// Authenticate a newly measured document on its own. Unchanged rows in the
/// captured revision are already validated immutable rows of this generation;
/// callers carry only their manifest identities, not their occurrence records.
pub(crate) fn validated_changed_artifact(
    capture: &Capture,
    selected: SelectedDocument,
    cancel: &CancelFlag,
) -> Result<Artifact> {
    let file = capture
        .files
        .iter()
        .find(|file| file.path == selected.document.key.path)
        .context("changed native source missing from capture")?;
    let full_documents = selected.revision.documents.clone();
    let mut artifact = Artifact {
        producer: selected.producer,
        source_set: selected.source_set,
        revision: Revision {
            documents: vec![selected.document],
            ..selected.revision
        },
        coverage: vec![selected.coverage],
        provenance: vec![selected.provenance],
        declarations: selected.declarations,
        calls: selected.calls,
        control_regions: selected.control_regions,
    };
    artifact.validate_structure(std::slice::from_ref(file))?;
    capture.verify(cancel)?;
    artifact.revision.documents = full_documents;
    Ok(artifact)
}

/// Assemble a full captured revision from one measured document and independently
/// authenticated immutable prior versions. Rebinding changes ONLY requested-revision
/// coverage/provenance; stable syntax and occurrence IDs are never inferred anew.
#[allow(dead_code)] // Kept for the bounded legacy diagnostic tests; Option C publishes changed-only facts.
pub(crate) fn assemble_selected_revision(
    capture: &Capture,
    root: &Path,
    root_id: &str,
    selected: Vec<SelectedDocument>,
    cancel: &CancelFlag,
) -> Result<Artifact> {
    capture.verify(cancel)?;
    let (mut artifact, files) = build_native_header(capture, root, root_id)?;
    let mut by_path = BTreeMap::new();
    for entry in selected {
        ensure!(
            by_path
                .insert(entry.document.key.path.clone(), entry)
                .is_none(),
            "duplicate selected native document"
        );
    }
    ensure!(
        by_path.len() == files.len(),
        "missing selected native document"
    );
    for file in &files {
        let mut entry = by_path
            .remove(&file.path)
            .context("selected native document absent")?;
        let document = artifact
            .revision
            .documents
            .iter()
            .find(|d| d.key.path == file.path)
            .context("captured document absent")?;
        ensure!(
            entry.producer == artifact.producer
                && entry.source_set == artifact.source_set
                && entry.revision.source_set_id == artifact.revision.source_set_id
                && entry.revision.toolchain_hash == artifact.revision.toolchain_hash
                && entry.revision.config_hash == artifact.revision.config_hash
                && entry.revision.dependency_hash == artifact.revision.dependency_hash
                && entry.document.key == document.key
                && entry.document.content_hash == document.content_hash
                && entry.document.byte_length == document.byte_length
                && file.hash == document.content_hash
                && hash(file.text.as_bytes()) == file.hash,
            "selected native document not authenticated to capture"
        );
        let old_revision = &entry.revision.id;
        let old_proof = &entry.provenance.id;
        ensure!(
            entry.document.revision_id == *old_revision
                && entry.coverage.revision_id == *old_revision
                && entry.provenance.revision_id == *old_revision
                && entry.provenance.document == document.key
                && entry.provenance.content_hash == document.content_hash
                && entry.declarations.iter().all(|d| d.document == document.key
                    && d.revision_id == *old_revision
                    && d.provenance_id == *old_proof)
                && entry.calls.iter().all(|c| c.document == document.key
                    && c.revision_id == *old_revision
                    && c.provenance_id == *old_proof)
                && entry
                    .control_regions
                    .iter()
                    .all(|r| r.document == document.key
                        && r.revision_id == *old_revision
                        && r.provenance_id == *old_proof),
            "selected native facts escaped prior document"
        );
        let new_revision = &artifact.revision.id;
        let proof = format!(
            "native-proof:v1:{}",
            digest(
                b"trellis.native-proof.v1\0",
                &canonical(
                    &json!({"producerId":PRODUCER,"document":document.key,"revisionId":new_revision})
                )
            )
        );
        entry.coverage.revision_id.clone_from(new_revision);
        entry.provenance.revision_id.clone_from(new_revision);
        entry.provenance.id = proof.clone();
        for d in &mut entry.declarations {
            d.revision_id.clone_from(new_revision);
            d.provenance_id.clone_from(&proof);
        }
        for c in &mut entry.calls {
            c.revision_id.clone_from(new_revision);
            c.provenance_id.clone_from(&proof);
        }
        for r in &mut entry.control_regions {
            r.revision_id.clone_from(new_revision);
            r.provenance_id.clone_from(&proof);
        }
        artifact.coverage.push(entry.coverage);
        artifact.provenance.push(entry.provenance);
        artifact.declarations.extend(entry.declarations);
        artifact.calls.extend(entry.calls);
        artifact.control_regions.extend(entry.control_regions);
    }
    artifact.validate_structure(&capture.files)?;
    capture.verify(cancel)?;
    Ok(artifact)
}

fn kind(lang: &str, n: Node<'_>) -> Option<&'static str> {
    match lang {
        "java" => crate::indexer_java::native_kind(n),
        "rust" => crate::indexer_rust::native_kind(n),
        "python" => crate::indexer_python::native_kind(n),
        "javascript" => crate::indexer::native_js_kind(n),
        _ => None,
    }
}
fn is_call(lang: &str, n: Node<'_>) -> bool {
    matches!(
        (lang, n.kind()),
        (
            "java",
            "method_invocation" | "object_creation_expression" | "explicit_constructor_invocation"
        ) | ("rust", "call_expression" | "method_call_expression")
            | ("python", "call")
            | ("javascript", "call_expression" | "new_expression")
    )
}
fn is_region(n: Node<'_>) -> bool {
    matches!(
        n.kind(),
        "if_statement"
            | "if_expression"
            | "else_clause"
            | "elif_clause"
            | "while_statement"
            | "do_statement"
            | "for_statement"
            | "for_in_statement"
            | "for_expression"
            | "loop_expression"
            | "match_expression"
            | "match_statement"
            | "switch_statement"
            | "switch_expression"
            | "switch_case"
            | "switch_default"
            | "try_statement"
            | "catch_clause"
            | "finally_clause"
            | "conditional_expression"
            | "ternary_expression"
    )
}
fn declaration_role(test: bool) -> DeclarationRole {
    if test {
        DeclarationRole::Test
    } else {
        DeclarationRole::Production
    }
}

pub(crate) fn test_path(path: &str) -> bool {
    let (dirs, name) = path.rsplit_once('/').unwrap_or(("", path));
    if dirs
        .split('/')
        .any(|dir| matches!(dir, "tests" | "test" | "__tests__" | "spec"))
    {
        return true;
    }
    // `*.test.*` / `*.spec.*`: any inner dot segment, so multi-dot names such
    // as `component.integration.test.ts` match too.
    let segments: Vec<&str> = name.split('.').collect();
    if segments.len() > 2
        && segments[1..segments.len() - 1]
            .iter()
            .any(|segment| matches!(*segment, "test" | "spec"))
    {
        return true;
    }
    match name.rsplit_once('.') {
        Some((stem, "rs")) => {
            stem == "tests" || stem.ends_with("_tests") || stem.ends_with("_test")
        }
        Some((stem, "py")) => {
            stem.starts_with("test_") || stem.ends_with("_test") || stem == "conftest"
        }
        Some((stem, "java")) => stem.ends_with("Test") || stem.ends_with("Tests"),
        _ => false,
    }
}

// Attribute syntax is inspected only inside parser-recognized attribute nodes. Tokenize
// the small source span, excluding strings and comments, rather than scanning items.
fn attribute_tokens(text: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut offset = 0;
    while offset < text.len() {
        let rest = &text[offset..];
        if rest.starts_with("//") {
            break;
        }
        if rest.starts_with("/*") {
            offset += rest.find("*/").map_or(rest.len(), |end| end + 2);
            continue;
        }
        let ch = rest.chars().next().unwrap();
        if ch == '"' || ch == '\'' {
            let delimiter = ch;
            offset += ch.len_utf8();
            while offset < text.len() {
                let next = text[offset..].chars().next().unwrap();
                offset += next.len_utf8();
                if next == '\\' && offset < text.len() {
                    offset += text[offset..].chars().next().unwrap().len_utf8();
                } else if next == delimiter {
                    break;
                }
            }
            continue;
        }
        if ch.is_ascii_alphabetic() || ch == '_' {
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            tokens.push(&text[offset..offset + end]);
            offset += end;
        } else {
            if !ch.is_whitespace() {
                tokens.push(&text[offset..offset + ch.len_utf8()]);
            }
            offset += ch.len_utf8();
        }
    }
    tokens
}
fn rust_test_attribute(n: Node<'_>, file: &SourceFile) -> bool {
    let Some(text) = file.text.get(n.start_byte()..n.end_byte()) else {
        return false;
    };
    let tokens = attribute_tokens(text);
    let Some(open) = tokens.iter().position(|token| *token == "[") else {
        return false;
    };
    let t = &tokens[open + 1..];
    if t.first() == Some(&"cfg") && t.get(1) == Some(&"(") {
        if t.get(2) == Some(&"test") && t.get(3) == Some(&")") {
            return true;
        }
        if t.get(2) != Some(&"all") || t.get(3) != Some(&"(") {
            return false;
        }
        let mut depth = 0;
        for index in 4..t.len() {
            match t[index] {
                "(" => depth += 1,
                ")" if depth == 0 => break,
                ")" => depth -= 1,
                "test"
                    if depth == 0
                        && matches!(t.get(index.wrapping_sub(1)), Some(&"(") | Some(&","))
                        && matches!(t.get(index + 1), Some(&",") | Some(&")")) =>
                {
                    return true;
                }
                _ => {}
            }
        }
        return false;
    }
    let mut index = 0;
    while t.get(index + 1) == Some(&":") && t.get(index + 2) == Some(&":") {
        index += 3;
    }
    t.get(index) == Some(&"test") && t.get(index + 1) == Some(&"]")
}
fn java_test_method(n: Node<'_>, file: &SourceFile) -> bool {
    if n.kind() != "method_declaration" {
        return false;
    }
    let Some(modifiers) = child_nodes(n).into_iter().find(|c| c.kind() == "modifiers") else {
        return false;
    };
    child_nodes(modifiers)
        .into_iter()
        .filter(|c| matches!(c.kind(), "annotation" | "marker_annotation"))
        .any(|annotation| {
            let Some(name) = annotation
                .child_by_field_name("name")
                .or_else(|| child_nodes(annotation).into_iter().next())
            else {
                return false;
            };
            let Some(value) = file.text.get(name.start_byte()..name.end_byte()) else {
                return false;
            };
            matches!(
                value.rsplit('.').next(),
                Some(
                    "Test" | "ParameterizedTest" | "RepeatedTest" | "TestFactory" | "TestTemplate"
                )
            )
        })
}
fn local_test_marker(n: Node<'_>, file: &SourceFile) -> bool {
    match file.language.as_str() {
        "java" => java_test_method(n, file),
        "python" if test_path(&file.path) => {
            let named = name(n, file).map(|(name, _)| name);
            (matches!(n.kind(), "function_definition")
                && named.as_deref().is_some_and(|s| s.starts_with("test_")))
                || (n.kind() == "class_definition"
                    && named.as_deref().is_some_and(|s| s.starts_with("Test")))
        }
        _ => false,
    }
}

fn child_nodes(n: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = n.walk();
    n.named_children(&mut cursor).collect()
}
fn name(n: Node<'_>, f: &SourceFile) -> Option<(String, Range)> {
    let node = n
        .child_by_field_name("name")
        .or_else(|| {
            (n.kind() == "assignment")
                .then(|| n.child_by_field_name("left"))
                .flatten()
        })
        .or_else(|| {
            (n.kind() == "field_definition")
                .then(|| n.child_by_field_name("property"))
                .flatten()
        })
        .or_else(|| {
            (n.kind() == "parameter" && n.named_child_count() > 0)
                .then(|| n.named_child(0))
                .flatten()
        })
        .or_else(|| {
            matches!(n.kind(), "formal_parameter" | "typed_parameter")
                .then(|| n.named_child(0))
                .flatten()
        })
        .or_else(|| (n.kind() == "identifier").then_some(n))?;
    if !matches!(
        node.kind(),
        "identifier" | "property_identifier" | "field_identifier" | "type_identifier"
    ) {
        return None;
    }
    let value = f.text.get(node.byte_range())?;
    if value.is_empty() {
        return None;
    }
    Some((value.to_owned(), Range::node(node)))
}
fn java_signature(n: Node<'_>, f: &SourceFile) -> Signature {
    let parameters = n
        .child_by_field_name("parameters")
        .map(child_nodes)
        .unwrap_or_default();
    let mut types = vec![];
    let mut variadic = false;
    for parameter in parameters {
        if !parameter.kind().contains("parameter") {
            continue;
        }
        if let Some(t) = parameter
            .child_by_field_name("type")
            .and_then(|part| f.text.get(part.byte_range()))
        {
            types.push(t.to_owned());
        }
        if parameter.kind().contains("spread") {
            variadic = true;
        }
    }
    let type_parameter_count = n
        .child_by_field_name("type_parameters")
        .map(child_nodes)
        .unwrap_or_default()
        .len();
    Signature {
        parameter_types: types,
        type_parameter_count,
        variadic,
    }
}
fn key_of(n: Node<'_>, f: &SourceFile, k: &str) -> Key {
    Key {
        kind: k.into(),
        name: name(n, f).map(|v| v.0),
        signature: if f.language == "java" && (k == "method" || k == "constructor") {
            Some(java_signature(n, f))
        } else {
            None
        },
        ordinal: 0,
    }
}
fn modifier_keyword(value: &str) -> bool {
    matches!(
        value,
        "public"
            | "protected"
            | "private"
            | "static"
            | "abstract"
            | "final"
            | "native"
            | "synchronized"
            | "volatile"
            | "transient"
            | "strictfp"
            | "default"
            | "async"
            | "const"
            | "unsafe"
            | "extern"
            | "override"
            | "get"
            | "set"
            | "readonly"
    )
}

fn header(n: Node<'_>, f: &SourceFile, key: &Key) -> Header {
    let from = |node: Node<'_>| f.text.get(node.byte_range()).map(str::to_owned);
    let field = |field: &str| n.child_by_field_name(field).and_then(from);
    let mut modifier_tokens: Vec<(usize, usize, String)> = Vec::new();
    let mut cursor = n.walk();
    for child in n.children(&mut cursor) {
        match child.kind() {
            "visibility_modifier" => {
                if let Some(value) = from(child) {
                    modifier_tokens.push((child.start_byte(), child.end_byte(), value));
                }
            }
            "modifiers" | "function_modifiers" => {
                let mut nested_cursor = child.walk();
                for token in child.children(&mut nested_cursor) {
                    if let Some(value) = from(token)
                        && (token.is_named() || modifier_keyword(&value))
                    {
                        modifier_tokens.push((token.start_byte(), token.end_byte(), value));
                    }
                }
            }
            _ if !child.is_named() => {
                if let Some(value) = from(child)
                    && modifier_keyword(&value)
                {
                    modifier_tokens.push((child.start_byte(), child.end_byte(), value));
                }
            }
            _ => {}
        }
    }
    modifier_tokens.sort_by_key(|(start, end, _)| (*start, *end));
    let modifiers = modifier_tokens
        .into_iter()
        .map(|(_, _, value)| value)
        .collect();
    let type_parameters = n
        .child_by_field_name("type_parameters")
        .map(child_nodes)
        .unwrap_or_default()
        .into_iter()
        .filter_map(from)
        .collect();
    let parameters = n
        .child_by_field_name("parameters")
        .map(child_nodes)
        .unwrap_or_default()
        .into_iter()
        .filter(|p| {
            p.kind().contains("parameter")
                || matches!(p.kind(), "identifier" | "rest_pattern" | "self_parameter")
        })
        .map(|p| {
            let pname = p
                .child_by_field_name("name")
                .or_else(|| {
                    child_nodes(p)
                        .into_iter()
                        .find(|c| c.kind() == "identifier")
                })
                .and_then(from)
                .or_else(|| (p.kind() == "identifier").then(|| from(p)).flatten());
            let type_name = p.child_by_field_name("type").and_then(from);
            Parameter {
                name: pname,
                type_name,
                variadic: p.kind().contains("spread")
                    || p.kind().contains("rest")
                    || f.text
                        .get(p.byte_range())
                        .is_some_and(|s| s.starts_with('*') || s.starts_with("...")),
            }
        })
        .collect();
    let result_type = field("return_type").or_else(|| {
        if f.language == "java" && matches!(key.kind.as_str(), "method") {
            field("type")
        } else {
            None
        }
    });
    let bases = ["superclass", "interfaces", "super_interfaces", "traits"]
        .into_iter()
        .flat_map(|field| n.child_by_field_name(field))
        .filter_map(from)
        .collect();
    Header {
        kind: key.kind.clone(),
        name: key.name.clone(),
        modifiers,
        type_parameters,
        parameters,
        result_type,
        bases,
    }
}

fn callee(n: Node<'_>, f: &SourceFile) -> (Option<String>, Option<Range>) {
    let expr = n
        .child_by_field_name(match f.language.as_str() {
            "java" => "name",
            "python" => "function",
            "javascript" if n.kind() == "new_expression" => "constructor",
            "javascript" => "function",
            _ => "function",
        })
        .or_else(|| n.child_by_field_name("constructor"));
    let Some(expr) = expr else {
        return (None, None);
    };
    let member = match f.language.as_str() {
        "java" => Some(expr),
        "python" if expr.kind() == "attribute" => expr.child_by_field_name("attribute"),
        "javascript" if expr.kind() == "member_expression" => expr.child_by_field_name("property"),
        "rust" if expr.kind() == "field_expression" => expr.child_by_field_name("field"),
        _ => Some(expr),
    };
    if let Some(member) = member
        && matches!(
            member.kind(),
            "identifier" | "property_identifier" | "field_identifier" | "type_identifier"
        )
        && let Some(token) = f.text.get(member.byte_range())
        && !token.is_empty()
    {
        return (Some(token.into()), Some(Range::node(member)));
    }
    (
        f.text
            .get(expr.byte_range())
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        None,
    )
}
/// Reparse only a selected stored source snapshot. This does not admit or reread
/// workspace files and does not project a second whole-workspace graph.
pub(crate) fn selected_source_witness_with_progress(
    file: &SourceFile,
    producer: Producer,
    source_set: SourceSet,
    revision: Revision,
    progress: Option<&dyn Fn() -> bool>,
) -> Result<Artifact> {
    crate::store::optional_hint_check(progress)?;
    ensure!(
        revision.source_set_id == source_set.id
            && revision.documents.len() == 1
            && revision.documents[0].key.path == file.path
            && revision.documents[0].content_hash == file.hash,
        "selected native source header mismatch"
    );
    // The revision carries only this selected document here. Its ID and hashes
    // are read from the pinned persisted header, never invented or republished.
    let mut artifact = Artifact {
        producer,
        source_set,
        revision,
        coverage: vec![],
        provenance: vec![],
        declarations: vec![],
        calls: vec![],
        control_regions: vec![],
    };
    let document = artifact.revision.documents[0].clone();
    extract_known_with_progress(
        &mut artifact,
        file,
        &document,
        &mut IdentityRegistry::default(),
        progress,
    )?;
    crate::store::optional_hint_check(progress)?;
    Ok(artifact)
}

/// Measure exactly one document. Under `occ:v2` this is strictly document-local: it reads only
/// `f` and the producer/source-set/revision descriptors, never another source document.
fn extract(a: &mut Artifact, f: &SourceFile, ids: &mut IdentityRegistry) -> Result<()> {
    let document = a
        .revision
        .documents
        .iter()
        .find(|document| document.key.path == f.path && document.key.language == f.language)
        .context("native document not in captured revision")?
        .clone();
    extract_known(a, f, &document, ids)
}

fn extract_known(
    a: &mut Artifact,
    f: &SourceFile,
    document: &Document,
    ids: &mut IdentityRegistry,
) -> Result<()> {
    extract_known_with_progress(a, f, document, ids, None)
}
fn extract_known_with_progress(
    a: &mut Artifact,
    f: &SourceFile,
    document: &Document,
    ids: &mut IdentityRegistry,
    progress: Option<&dyn Fn() -> bool>,
) -> Result<()> {
    crate::store::optional_hint_check(progress)?;
    language_order(&f.language)?;
    safe_path(&f.path)?;
    // Occurrence identity binds this captured document version. Authenticate its content hash
    // and extraction context against the captured revision before measuring, so a failed
    // precondition mints no occ:v2 record at all.
    ensure!(
        f.hash
            == if progress.is_some() {
                let mut digest = Sha256::new();
                for chunk in f.text.as_bytes().chunks(64 * 1024) {
                    crate::store::optional_hint_check(progress)?;
                    digest.update(chunk);
                }
                hex::encode(digest.finalize())
            } else {
                hash(f.text.as_bytes())
            },
        "native captured source hash mismatch"
    );
    ensure!(
        document.key.source_set_id == a.source_set.id
            && document.key.language == f.language
            && document.key.path == f.path
            && document.content_hash == f.hash
            && document.byte_length == f.text.len(),
        "native document not in captured revision"
    );
    let extraction_context = native_extraction_context(&a.producer, &f.language, &a.revision)?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&match f.language.as_str() {
        "java" => tree_sitter_java::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        _ => tree_sitter_javascript::LANGUAGE.into(),
    })?;
    let tree = if let Some(expired) = progress {
        let mut cancelled = |_: &tree_sitter::ParseState| expired();
        let tree = parser.parse_with_options(
            &mut |offset, _| &f.text.as_bytes()[offset..],
            None,
            Some(tree_sitter::ParseOptions::new().progress_callback(&mut cancelled)),
        );
        crate::store::optional_hint_check(progress)?;
        tree.context("native parser failed")?
    } else {
        parser
            .parse(&f.text, None)
            .context("native parser failed")?
    };
    let key = DocumentKey {
        source_set_id: a.source_set.id.clone(),
        language: f.language.clone(),
        path: f.path.clone(),
    };
    let proof_id = format!(
        "native-proof:v1:{}",
        digest(
            b"trellis.native-proof.v1\0",
            &canonical(&json!({"producerId":PRODUCER,"document":key,"revisionId":a.revision.id}))
        )
    );
    a.provenance.push(Provenance {
        id: proof_id.clone(),
        producer_id: PRODUCER.into(),
        document: key.clone(),
        revision_id: a.revision.id.clone(),
        content_hash: f.hash.clone(),
        evidence_kind: "measuredSyntax".into(),
        basis: None,
        freshness: "fresh".into(),
        derived_from: None,
    });
    let module_key = Key {
        kind: "module".into(),
        name: None,
        signature: None,
        ordinal: 0,
    };
    let module_id=ids.stable(&json!({"sourceSet":a.source_set.id,"path":f.path,"language":f.language,"ancestors":[],"declaration":module_key}))?;
    a.declarations.push(Declaration {
        syntax_id: module_id.clone(),
        role: declaration_role(test_path(&f.path)),
        document: key.clone(),
        revision_id: a.revision.id.clone(),
        kind: "module".into(),
        name: None,
        lookup_key: None,
        ancestors: vec![],
        key: module_key,
        range: Range::node(tree.root_node()),
        name_range: None,
        header: Header {
            kind: "module".into(),
            name: None,
            modifiers: vec![],
            type_parameters: vec![],
            parameters: vec![],
            result_type: None,
            bases: vec![],
        },
        provenance_id: proof_id.clone(),
    });
    let mut state = ParserState {
        call_start: a.calls.len(),
        control_start: a.control_regions.len(),
        artifact: a,
        file: f,
        ids,
        document: key,
        proof_id,
        extraction_context,
        seen: BTreeSet::new(),
        declaration_ordinals: BTreeMap::new(),
        call_ordinals: BTreeMap::new(),
        control_ordinals: BTreeMap::new(),
        saw_definition: false,
        saw_call: false,
        visits: 0,
        opaque_rust: false,
        progress,
    };
    state.walk(
        tree.root_node(),
        &module_id,
        &[],
        &[],
        0,
        (test_path(&f.path), false),
    )?;
    state.finish_occurrences()?;
    crate::store::optional_hint_check(progress)?;
    let observed_roles = [
        (state.saw_definition, "definition"),
        (state.saw_call, "call"),
    ]
    .into_iter()
    .filter_map(|(present, role)| present.then_some(role.into()))
    .collect();
    state.artifact.coverage.push(Coverage {
        producer_id: PRODUCER.into(),
        language: f.language.clone(),
        source_set_id: state.artifact.source_set.id.clone(),
        document_path: f.path.clone(),
        revision_id: state.artifact.revision.id.clone(),
        requested: true,
        selected: true,
        state: if tree.root_node().has_error() || state.opaque_rust {
            "partial"
        } else {
            "complete"
        }
        .into(),
        supported_roles: vec!["definition".into(), "call".into()],
        observed_roles,
        diagnostic: match (tree.root_node().has_error(), state.opaque_rust) {
            (true, true) => Some("parser recovered from invalid source; opaque Rust macro/attribute token trees skipped".into()),
            (true, false) => Some("parser recovered from invalid source".into()),
            (false, true) => Some("opaque Rust macro/attribute token trees skipped; calls and definitions inside were not measured".into()),
            (false, false) => None,
        },
    });
    Ok(())
}
/// Decision 0003 `occ:v2` input: document version, extraction context and native producer
/// descriptor, never the containing revision.
fn occurrence_input(
    content_hash: &str,
    extraction_context: &str,
    producer: &Producer,
    owner: &str,
    kind: &str,
    ordinal: usize,
) -> Value {
    json!({"contentHash":content_hash,"extractionContext":extraction_context,"nativeProducerId":producer.id,"nativeProducerVersion":producer.version,"ownerSyntaxId":owner,"kind":kind,"ordinal":ordinal})
}
struct ParserState<'a> {
    artifact: &'a mut Artifact,
    call_start: usize,
    control_start: usize,
    file: &'a SourceFile,
    ids: &'a mut IdentityRegistry,
    document: DocumentKey,
    proof_id: String,
    extraction_context: String,
    seen: BTreeSet<(String, String, usize, usize)>,
    declaration_ordinals: BTreeMap<String, usize>,
    call_ordinals: BTreeMap<String, usize>,
    control_ordinals: BTreeMap<String, usize>,
    saw_definition: bool,
    saw_call: bool,
    visits: usize,
    opaque_rust: bool,
    progress: Option<&'a dyn Fn() -> bool>,
}
impl ParserState<'_> {
    fn occurrence_input(&self, owner: &str, kind: &str, ordinal: usize) -> Value {
        occurrence_input(
            &self.file.hash,
            &self.extraction_context,
            &self.artifact.producer,
            owner,
            kind,
            ordinal,
        )
    }
    fn finish_occurrences(&mut self) -> Result<()> {
        let mut replacements = BTreeMap::new();
        let mut by_owner: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (offset, region) in self.artifact.control_regions[self.control_start..]
            .iter()
            .enumerate()
        {
            crate::store::optional_hint_check(self.progress)?;
            ensure!(
                region.document == self.document,
                "foreign native control in current document"
            );
            by_owner
                .entry(region.owner_syntax_id.clone())
                .or_default()
                .push(self.control_start + offset);
        }
        for indexes in by_owner.values_mut() {
            crate::store::optional_hint_check(self.progress)?;
            indexes.sort_by_key(|i| {
                let r = &self.artifact.control_regions[*i].range;
                (r.start, r.end)
            });
            for (ordinal, i) in indexes.iter().enumerate() {
                crate::store::optional_hint_check(self.progress)?;
                let input = self.occurrence_input(
                    &self.artifact.control_regions[*i].owner_syntax_id,
                    "control",
                    ordinal,
                );
                let id = self.ids.occurrence(&input)?;
                let region = &mut self.artifact.control_regions[*i];
                replacements.insert(std::mem::replace(&mut region.id, id.clone()), id);
                region.ordinal = ordinal;
            }
        }
        for region in &mut self.artifact.control_regions[self.control_start..] {
            crate::store::optional_hint_check(self.progress)?;
            if let Some(parent) = &mut region.parent_id {
                *parent = replacements
                    .get(parent)
                    .context("native region parent not measured")?
                    .clone();
            }
        }
        let mut by_owner: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (offset, call) in self.artifact.calls[self.call_start..].iter().enumerate() {
            crate::store::optional_hint_check(self.progress)?;
            ensure!(
                call.document == self.document,
                "foreign native call in current document"
            );
            by_owner
                .entry(call.owner_syntax_id.clone())
                .or_default()
                .push(self.call_start + offset);
        }
        for indexes in by_owner.values_mut() {
            crate::store::optional_hint_check(self.progress)?;
            indexes.sort_by_key(|i| {
                let r = &self.artifact.calls[*i].range;
                (r.start, r.end)
            });
            for (ordinal, i) in indexes.iter().enumerate() {
                crate::store::optional_hint_check(self.progress)?;
                let input = self.occurrence_input(
                    &self.artifact.calls[*i].owner_syntax_id,
                    "call",
                    ordinal,
                );
                let id = self.ids.occurrence(&input)?;
                let call = &mut self.artifact.calls[*i];
                call.ordinal = ordinal;
                call.id = id;
                for region in &mut call.region_ids {
                    *region = replacements
                        .get(region)
                        .context("native call region not measured")?
                        .clone();
                }
            }
        }
        Ok(())
    }

    fn walk(
        &mut self,
        n: Node<'_>,
        owner: &str,
        ancestors: &[Key],
        regions: &[String],
        depth: usize,
        test_flags: (bool, bool),
    ) -> Result<()> {
        crate::store::optional_hint_check(self.progress)?;
        ensure!(
            depth < 128 && self.visits < 1_000_000,
            "native extraction work budget exceeded"
        );
        self.visits += 1;
        // Rust token trees and attributes are opaque source syntax. A call-shaped token
        // inside a macro is not a measured call, and expanding it is outside #22 native
        // extraction. Still count this subtree root against the bounded visit budget.
        if self.file.language == "rust"
            && matches!(
                n.kind(),
                "macro_invocation"
                    | "macro_definition"
                    | "token_tree"
                    | "attribute_item"
                    | "inner_attribute_item"
            )
        {
            self.opaque_rust = true;
            return Ok(());
        }
        let test_context = test_flags.0 || test_flags.1 || local_test_marker(n, self.file);
        let mut owner = owner.to_owned();
        let mut ancestry = ancestors.to_vec();
        let mut regions = regions.to_vec();
        if let Some(k) = kind(&self.file.language, n)
            && (matches!(k, "implementation" | "anonymousFunction") || name(n, self.file).is_some())
        {
            let mut key = key_of(n, self.file, k);
            // Repeated declaration keys are document-local. A cumulative artifact
            // scan would make a multi-file full capture quadratic in its facts.
            // The synthetic module is not a previous measured declaration.
            if k != "module" {
                let ordinal_key =
                    serde_json::to_string(&(&ancestry, &key.kind, &key.name, &key.signature))?;
                let count = self.declaration_ordinals.entry(ordinal_key).or_default();
                key.ordinal = *count;
                *count += 1;
            }
            let id=self.ids.stable(&json!({"sourceSet":self.artifact.source_set.id,"path":self.file.path,"language":self.file.language,"ancestors":ancestry,"declaration":key}))?;
            let named = name(n, self.file);
            self.saw_definition |= k != "module";
            self.artifact.declarations.push(Declaration {
                syntax_id: id.clone(),
                role: declaration_role(test_context),
                document: self.document.clone(),
                revision_id: self.artifact.revision.id.clone(),
                kind: k.into(),
                name: named.as_ref().map(|v| v.0.clone()),
                lookup_key: named
                    .as_ref()
                    .map(|v| lookup(&self.file.language, &v.0))
                    .transpose()?,
                ancestors: ancestry.clone(),
                key: key.clone(),
                range: Range::node(n),
                name_range: named.map(|v| v.1),
                header: header(n, self.file, &key),
                provenance_id: self.proof_id.clone(),
            });
            // A lexical declaration key may nest syntax, but its initializer runs in
            // the enclosing executable scope. Only executable containers own calls.
            if matches!(
                k,
                "module"
                    | "namespace"
                    | "type"
                    | "implementation"
                    | "function"
                    | "method"
                    | "constructor"
                    | "anonymousFunction"
            ) {
                owner = id;
                regions.clear();
            }
            ancestry.push(key);
        }
        if is_region(n) {
            let count = self.control_ordinals.entry(owner.clone()).or_default();
            let ordinal = *count;
            *count += 1;
            let range = Range::node(n);
            ensure!(
                self.seen
                    .insert((owner.clone(), "control".into(), range.start, range.end)),
                "duplicate native control range"
            );
            let id = format!(
                "pending:control:{}:{}:{}:{}",
                self.file.path, owner, range.start, range.end
            );
            self.artifact.control_regions.push(ControlRegion {
                id: id.clone(),
                owner_syntax_id: owner.clone(),
                ordinal,
                document: self.document.clone(),
                revision_id: self.artifact.revision.id.clone(),
                kind: n.kind().into(),
                range,
                parent_id: regions.last().cloned(),
                arm: None,
                provenance_id: self.proof_id.clone(),
            });
            regions.push(id);
        }
        if is_call(&self.file.language, n) {
            let count = self.call_ordinals.entry(owner.clone()).or_default();
            let ordinal = *count;
            *count += 1;
            self.saw_call = true;
            let range = Range::node(n);
            ensure!(
                self.seen
                    .insert((owner.clone(), "call".into(), range.start, range.end)),
                "duplicate native call range"
            );
            let id = String::new();
            let (spelling, callee_range) = callee(n, self.file);
            self.artifact.calls.push(Call {
                id,
                owner_syntax_id: owner.clone(),
                ordinal,
                document: self.document.clone(),
                revision_id: self.artifact.revision.id.clone(),
                range,
                callee_range,
                spelling,
                region_ids: regions.clone(),
                provenance_id: self.proof_id.clone(),
            });
        }
        let mut marked = false;
        for child in child_nodes(n) {
            if self.file.language == "rust" && child.kind() == "attribute_item" {
                marked |= rust_test_attribute(child, self.file);
                self.walk(
                    child,
                    &owner,
                    &ancestry,
                    &regions,
                    depth + 1,
                    (test_context, false),
                )?;
                continue;
            }
            if self.file.language == "rust" && child.kind() == "line_comment" {
                continue;
            }
            self.walk(
                child,
                &owner,
                &ancestry,
                &regions,
                depth + 1,
                (test_context, marked),
            )?;
            marked = false;
        }
        Ok(())
    }
}

impl Artifact {
    /// Validates a selectively assembled revision. The caller already checked each
    /// reused stored witness against captured source and normalized immutable rows;
    /// this checks all resulting IDs/ownership and the full current capture header.
    pub(crate) fn validate_selective(
        &self,
        capture: &Capture,
        root: &Path,
        root_id: &str,
        cancel: &CancelFlag,
    ) -> Result<()> {
        capture.verify(cancel)?;
        let (header, _) = build_native_header(capture, root, root_id)?;
        ensure!(
            self.producer == header.producer
                && self.source_set == header.source_set
                && self.revision == header.revision,
            "selective native header differs from captured revision"
        );
        self.validate_structure(&capture.files)?;
        capture.verify(cancel)
    }
    /// Reconstruct the native syntax from already-admitted immutable buffers; no source file
    /// is opened or hashed again. Comparison forbids well-shaped fabricated evidence.
    /// Honors the caller's cancellation before and after the re-derivation.
    pub fn validate(
        &self,
        capture: &Capture,
        root: &Path,
        root_id: &str,
        cancel: &CancelFlag,
    ) -> Result<()> {
        capture.verify(cancel)?;
        self.validate_structure(&capture.files)?;
        let expected = build_native(capture, root, root_id)?;
        capture.verify(cancel)?;
        expected.validate_structure(&capture.files)?;
        ensure!(
            self == &expected,
            "native evidence differs from captured source or inputs"
        );
        Ok(())
    }
    fn validate_structure(&self, files: &[SourceFile]) -> Result<()> {
        ensure!(
            self.producer.id == PRODUCER
                && self.producer.kind == "native"
                && self.producer.version == NATIVE_VERSION
                && self.producer.position_encoding == "utf8"
                && valid_hash(&self.producer.executable_hash),
            "invalid native producer"
        );
        ensure!(
            self.producer.languages == LANGUAGES.map(str::to_owned)
                && self.source_set.languages == self.producer.languages
                && self.source_set.dependencies.is_empty()
                && self.source_set.id == format!("source-set:v1:{}", self.source_set.root_id),
            "invalid native source set"
        );
        text(&self.source_set.root_id)?;
        ensure!(
            self.revision.source_set_id == self.source_set.id
                && self.revision.id.starts_with("revision:v2:")
                && valid_hash(&self.revision.id[12..])
                && valid_hash(&self.revision.toolchain_hash)
                && valid_hash(&self.revision.config_hash)
                && valid_hash(&self.revision.dependency_hash),
            "invalid native revision"
        );
        ensure!(
            self.revision.documents.len() == files.len()
                && self.coverage.len() == files.len()
                && self.provenance.len() == files.len(),
            "native document tuple cardinality"
        );
        let mut docs = BTreeMap::new();
        let captured_files: BTreeMap<_, _> = files
            .iter()
            .map(|f| ((f.language.as_str(), f.path.as_str()), f))
            .collect();
        ensure!(
            captured_files.len() == files.len(),
            "duplicate captured native path"
        );
        let mut last: Option<(usize, &str)> = None;
        for d in &self.revision.documents {
            safe_path(&d.key.path)?;
            let current = (language_order(&d.key.language)?, d.key.path.as_str());
            ensure!(
                last.is_none_or(|prev| prev < current),
                "unsorted or duplicate native documents"
            );
            last = Some(current);
            let file = captured_files
                .get(&(d.key.language.as_str(), d.key.path.as_str()))
                .context("native document not captured")?;
            ensure!(
                d.key.source_set_id == self.source_set.id
                    && d.revision_id == self.revision.id
                    && d.byte_length == file.text.len()
                    && d.content_hash == file.hash
                    && valid_hash(&d.content_hash),
                "native document bytes/hash mismatch"
            );
            docs.insert(d.key.clone(), file);
        }
        let mut covers = BTreeSet::new();
        for c in &self.coverage {
            let key = DocumentKey {
                source_set_id: c.source_set_id.clone(),
                language: c.language.clone(),
                path: c.document_path.clone(),
            };
            ensure!(
                docs.contains_key(&key)
                    && covers.insert(key)
                    && c.producer_id == PRODUCER
                    && c.revision_id == self.revision.id
                    && c.requested
                    && c.selected
                    && matches!(c.state.as_str(), "complete" | "partial" | "failed")
                    && c.supported_roles == ["definition", "call"]
                    && c.observed_roles
                        .iter()
                        .all(|role| c.supported_roles.contains(role))
                    && c.observed_roles.windows(2).all(|window| c
                        .supported_roles
                        .iter()
                        .position(|r| r == &window[0])
                        < c.supported_roles.iter().position(|r| r == &window[1])),
                "invalid native coverage"
            );
            ensure!(
                (c.state == "complete") == c.diagnostic.is_none(),
                "native diagnostic mismatch"
            );
            if let Some(d) = &c.diagnostic {
                text(d)?;
            }
        }
        let mut proofs = BTreeSet::new();
        for proof in &self.provenance {
            let f = docs
                .get(&proof.document)
                .context("native proof dangling document")?;
            ensure!(
                proofs.insert(proof.id.clone())
                    && proof.producer_id == PRODUCER
                    && proof.revision_id == self.revision.id
                    && proof.content_hash == f.hash
                    && proof.evidence_kind == "measuredSyntax"
                    && proof.basis.is_none()
                    && proof.derived_from.is_none()
                    && proof.freshness == "fresh",
                "invalid native provenance"
            );
            let expected = format!(
                "native-proof:v1:{}",
                digest(
                    b"trellis.native-proof.v1\0",
                    &canonical(
                        &json!({"producerId":PRODUCER,"document":proof.document,"revisionId":self.revision.id})
                    )
                )
            );
            ensure!(proof.id == expected, "native proof digest mismatch");
        }
        let mut declarations = BTreeMap::new();
        let mut identity = IdentityRegistry::default();
        for d in &self.declarations {
            let file = docs
                .get(&d.document)
                .context("native declaration dangling document")?;
            ensure!(
                d.revision_id == self.revision.id
                    && proofs.contains(&d.provenance_id)
                    && matches!(
                        d.kind.as_str(),
                        "module"
                            | "namespace"
                            | "type"
                            | "implementation"
                            | "function"
                            | "method"
                            | "constructor"
                            | "field"
                            | "variable"
                            | "parameter"
                            | "typeParameter"
                            | "alias"
                            | "anonymousFunction"
                    )
                    && d.kind == d.key.kind
                    && d.name == d.key.name
                    && d.header.kind == d.kind
                    && d.header.name == d.name,
                "native declaration mismatch"
            );
            ensure!(
                d.name.is_some() == d.lookup_key.is_some()
                    && d.name.is_some() == d.name_range.is_some(),
                "native name nullability"
            );
            span(
                &file.text,
                &d.range,
                d.kind == "module" && file.text.is_empty(),
            )?;
            if let Some(r) = &d.name_range {
                span(&file.text, r, false)?;
                ensure!(
                    r.start >= d.range.start
                        && r.end <= d.range.end
                        && d.name.as_deref() == file.text.get(r.start..r.end),
                    "native name not source-measured"
                );
            }
            if let Some(n) = &d.name {
                ensure!(
                    d.lookup_key.as_deref() == Some(lookup(&d.document.language, n)?.as_str()),
                    "native lookup normalization mismatch"
                );
            }
            if d.document.language != "java" || !matches!(d.kind.as_str(), "method" | "constructor")
            {
                ensure!(d.key.signature.is_none(), "unexpected native signature");
            }
            if d.kind == "module" {
                ensure!(
                    d.name.is_none() && d.ancestors.is_empty(),
                    "invalid native module"
                );
            }
            let expected=identity.stable(&json!({"sourceSet":self.source_set.id,"path":d.document.path,"language":d.document.language,"ancestors":d.ancestors,"declaration":d.key}))?;
            ensure!(
                d.syntax_id == expected && declarations.insert(d.syntax_id.clone(), d).is_none(),
                "native stable ID collision/duplicate"
            );
        }
        let mut siblings: BTreeMap<Vec<u8>, Vec<&Declaration>> = BTreeMap::new();
        for d in &self.declarations {
            let group = canonical(
                &json!({"document":d.document,"ancestors":d.ancestors,"kind":d.kind,"name":d.name,"signature":d.key.signature}),
            );
            siblings.entry(group).or_default().push(d);
        }
        for group in siblings.values_mut() {
            group.sort_by_key(|d| (d.range.start, d.range.end));
            for (ordinal, d) in group.iter().enumerate() {
                ensure!(d.key.ordinal == ordinal, "native sibling ordinal mismatch");
                if ordinal != 0 {
                    ensure!(
                        group[ordinal - 1].range != d.range,
                        "duplicate sibling range"
                    );
                }
            }
        }
        let mut call_ordinals: BTreeMap<(DocumentKey, String), Vec<&Call>> = BTreeMap::new();
        for c in &self.calls {
            call_ordinals
                .entry((c.document.clone(), c.owner_syntax_id.clone()))
                .or_default()
                .push(c);
        }
        for group in call_ordinals.values_mut() {
            group.sort_by_key(|c| (c.range.start, c.range.end));
            for (ordinal, c) in group.iter().enumerate() {
                ensure!(c.ordinal == ordinal, "native call ordinal mismatch");
                if ordinal != 0 {
                    ensure!(group[ordinal - 1].range != c.range, "duplicate call range");
                }
            }
        }
        let mut control_ordinals: BTreeMap<(DocumentKey, String), Vec<&ControlRegion>> =
            BTreeMap::new();
        for r in &self.control_regions {
            control_ordinals
                .entry((r.document.clone(), r.owner_syntax_id.clone()))
                .or_default()
                .push(r);
        }
        for group in control_ordinals.values_mut() {
            group.sort_by_key(|r| (r.range.start, r.range.end));
            for (ordinal, r) in group.iter().enumerate() {
                ensure!(r.ordinal == ordinal, "native control ordinal mismatch");
                if ordinal != 0 {
                    ensure!(
                        group[ordinal - 1].range != r.range,
                        "duplicate control range"
                    );
                }
            }
        }
        let mut occurrence = IdentityRegistry::default();
        let mut calls = BTreeSet::new();
        for c in &self.calls {
            let file = docs
                .get(&c.document)
                .context("native call dangling document")?;
            let owner = declarations
                .get(&c.owner_syntax_id)
                .context("native call dangling owner")?;
            ensure!(
                owner.document == c.document
                    && c.revision_id == self.revision.id
                    && proofs.contains(&c.provenance_id)
                    && calls.insert(c.id.clone()),
                "native call ownership/duplicate"
            );
            span(&file.text, &c.range, false)?;
            if let Some(r) = &c.callee_range {
                span(&file.text, r, false)?;
                ensure!(
                    r.start >= c.range.start
                        && r.end <= c.range.end
                        && c.spelling.as_deref() == file.text.get(r.start..r.end),
                    "callee not measured in call"
                );
            }
            if let Some(s) = &c.spelling {
                text(s)?;
            }
            let context =
                native_extraction_context(&self.producer, &c.document.language, &self.revision)?;
            ensure!(
                c.id == occurrence.occurrence(&occurrence_input(
                    &file.hash,
                    &context,
                    &self.producer,
                    &c.owner_syntax_id,
                    "call",
                    c.ordinal
                ))?,
                "native call occurrence mismatch"
            );
        }
        let mut regions = BTreeMap::new();
        for region in &self.control_regions {
            let file = docs
                .get(&region.document)
                .context("native control dangling document")?;
            let owner = declarations
                .get(&region.owner_syntax_id)
                .context("native control dangling owner")?;
            ensure!(
                owner.document == region.document
                    && region.revision_id == self.revision.id
                    && proofs.contains(&region.provenance_id),
                "native control ownership"
            );
            span(&file.text, &region.range, false)?;
            let context = native_extraction_context(
                &self.producer,
                &region.document.language,
                &self.revision,
            )?;
            ensure!(
                region.id
                    == occurrence.occurrence(&occurrence_input(
                        &file.hash,
                        &context,
                        &self.producer,
                        &region.owner_syntax_id,
                        "control",
                        region.ordinal
                    ))?
                    && regions.insert(region.id.clone(), region).is_none(),
                "native control occurrence mismatch"
            );
        }
        for region in &self.control_regions {
            if let Some(parent) = &region.parent_id {
                let parent = regions
                    .get(parent)
                    .context("native control dangling parent")?;
                ensure!(
                    parent.owner_syntax_id == region.owner_syntax_id
                        && parent.document == region.document
                        && parent.range.start <= region.range.start
                        && region.range.end <= parent.range.end
                        && parent.id != region.id,
                    "invalid native control parent"
                );
            }
        }
        for c in &self.calls {
            for id in &c.region_ids {
                let r = regions.get(id).context("native call dangling region")?;
                ensure!(
                    r.owner_syntax_id == c.owner_syntax_id
                        && r.document == c.document
                        && r.range.start <= c.range.start
                        && c.range.end <= r.range.end,
                    "native call region mismatch"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod occurrence_identity_tests {
    use super::*;

    fn producer(version: &str) -> Producer {
        Producer {
            id: PRODUCER.into(),
            version: version.into(),
            executable_hash: "e".repeat(64),
            kind: "native".into(),
            languages: LANGUAGES.map(str::to_owned).into(),
            position_encoding: "utf8".into(),
        }
    }
    fn selected(file: &SourceFile, revision_id: &str, version: &str) -> Result<Artifact> {
        let source_set = SourceSet {
            id: "source-set:v1:root".into(),
            root_id: "root".into(),
            languages: LANGUAGES.map(str::to_owned).into(),
            dependencies: vec![],
        };
        let key = DocumentKey {
            source_set_id: source_set.id.clone(),
            language: file.language.clone(),
            path: file.path.clone(),
        };
        let revision = Revision {
            id: revision_id.into(),
            source_set_id: source_set.id.clone(),
            documents: vec![Document {
                key,
                revision_id: revision_id.into(),
                content_hash: file.hash.clone(),
                byte_length: file.text.len(),
            }],
            toolchain_hash: "1".repeat(64),
            config_hash: hash(revision_id.as_bytes()),
            dependency_hash: "3".repeat(64),
        };
        selected_source_witness_with_progress(file, producer(version), source_set, revision, None)
    }
    fn file(text: &str) -> SourceFile {
        SourceFile {
            path: "a.js".into(),
            hash: hash(text.as_bytes()),
            language: "javascript".into(),
            text: text.into(),
        }
    }

    #[test]
    fn native_producer_declares_and_authenticates_an_explicit_empty_inventory() {
        let revision = selected(&file("f();"), "revision:v2:r1", NATIVE_VERSION)
            .unwrap()
            .revision;
        for language in LANGUAGES {
            let measured =
                native_extraction_context(&producer(NATIVE_VERSION), language, &revision).unwrap();
            let reusable =
                declared_selected_extraction_context(PRODUCER, NATIVE_VERSION, language).unwrap();
            assert_eq!(
                measured, reusable,
                "witness and selected reuse used different declared inventory"
            );
            assert_eq!(
                measured,
                extraction_context(language, &[]).unwrap(),
                "current empty-inventory occ:v2 identity changed"
            );
            assert!(
                selected_extraction_context_for_inventory(language, &["config"]).is_err(),
                "synthetic nonempty declaration must not reuse an unauthenticated empty context"
            );
        }
        // An undeclared producer version (e.g. the withdrawn occ:v1 producer) or language has
        // no authenticated inventory, so it fails closed.
        for version in ["native-v2", "native-v3"] {
            assert!(native_extraction_context(&producer(version), "rust", &revision).is_err());
        }
        assert!(native_extraction_context(&producer(NATIVE_VERSION), "go", &revision).is_err());
        let mut captured = revision.clone();
        captured.config_hash = hash(b"captured config");
        assert_eq!(
            authenticated_extraction_context(
                "rust",
                &["config"],
                &[("config", Some(b"captured config"))],
                &captured,
            )
            .unwrap(),
            extraction_context("rust", &[("config".into(), captured.config_hash.clone())]).unwrap()
        );
        for (inventory, observed) in [
            (vec!["config"], vec![]),
            (vec!["config"], vec![("config", None)]),
            (
                vec!["config"],
                vec![("config", Some(b"stale config".as_slice()))],
            ),
            (
                vec![],
                vec![("config", Some(b"captured config".as_slice()))],
            ),
            (
                vec!["settings"],
                vec![("settings", Some(b"captured config".as_slice()))],
            ),
            (
                vec!["config", "config"],
                vec![("config", Some(b"captured config".as_slice()))],
            ),
        ] {
            assert!(
                authenticated_extraction_context("rust", &inventory, &observed, &captured).is_err(),
                "{inventory:?} {observed:?}"
            );
        }
        let mut forged_digest = captured.clone();
        forged_digest.config_hash = "not-a-captured-digest".into();
        assert!(
            authenticated_extraction_context(
                "rust",
                &["config"],
                &[("config", Some(b"captured config"))],
                &forged_digest,
            )
            .is_err()
        );
    }

    #[test]
    fn failed_precondition_mints_no_occurrence() {
        let source = file("function f() { g(); if (x) { h(); } }");
        assert!(selected(&source, "revision:v2:r1", "native-v2").is_err());
        let mut forged = source.clone();
        forged.hash = "0".repeat(64);
        let mut artifact = selected(&source, "revision:v2:r1", NATIVE_VERSION).unwrap();
        artifact.calls.clear();
        artifact.control_regions.clear();
        assert!(extract(&mut artifact, &forged, &mut IdentityRegistry::default()).is_err());
        forged = source.clone();
        forged.text.push_str(" // changed after admission");
        assert!(extract(&mut artifact, &forged, &mut IdentityRegistry::default()).is_err());
        assert!(artifact.calls.is_empty() && artifact.control_regions.is_empty());
    }

    #[test]
    fn cached_witness_sort_matches_legacy_and_preserves_equal_key_order() {
        let values = [
            serde_json::json!({"b": 2, "a": 1}),
            serde_json::json!({"a": 0}),
            serde_json::json!({"a": 1, "b": 2}),
            serde_json::json!({"a": 1, "b": 2}),
        ];
        let mut legacy: Vec<_> = values.iter().cloned().enumerate().collect();
        let mut cached = legacy.clone();
        legacy.sort_by_key(|(_, value)| canonical(value));
        cached.sort_by_cached_key(|(_, value)| canonical(value));
        assert_eq!(cached, legacy);
        assert_eq!(
            cached
                .iter()
                .map(|(ordinal, _)| *ordinal)
                .collect::<Vec<_>>(),
            [1, 0, 2, 3]
        );
        let source = file("function f() { g(); if (x) { h(); } }");
        let a = selected(&source, "revision:v2:r1", NATIVE_VERSION).unwrap();
        let b = selected(&source, "revision:v2:r2", NATIVE_VERSION).unwrap();
        let witness = |artifact: &Artifact| {
            document_witness(
                &artifact.revision.documents[0],
                &artifact.producer,
                &artifact.coverage[0],
                &artifact.declarations,
                &artifact.calls,
                &artifact.control_regions,
            )
            .unwrap()
        };
        assert_eq!(
            witness(&a),
            "699962d67808217642edc089fa7d5d33f4ead9aa9ca2677ae5b2421bf3fdc0af",
            "native-v5 witness includes declaration role"
        );
        assert_eq!(
            witness(&a),
            witness(&b),
            "revision labels do not enter native witness"
        );
        assert!(a.calls.iter().all(|row| row.id.starts_with("occ:v2:")));
    }

    #[test]
    fn document_version_keeps_occurrence_ids_across_revisions() {
        let source = file("function f() { g(); if (x) { h(); } }");
        let r1 = selected(&source, "revision:v2:r1", NATIVE_VERSION).unwrap();
        let r2 = selected(&source, "revision:v2:r2", NATIVE_VERSION).unwrap();
        assert_eq!(r1.calls.len(), 2);
        assert_eq!(r1.control_regions.len(), 1);
        for (a, b) in r1.calls.iter().zip(&r2.calls) {
            assert!(a.id.starts_with("occ:v2:"));
            assert_eq!(a.id, b.id);
            assert_eq!(a.region_ids, b.region_ids);
            assert_eq!(
                (a.revision_id.as_str(), b.revision_id.as_str()),
                ("revision:v2:r1", "revision:v2:r2")
            );
        }
        assert_eq!(r1.control_regions[0].id, r2.control_regions[0].id);
        assert_ne!(r1.provenance[0].id, r2.provenance[0].id);
        let edited = selected(
            &file("function f() { g(); if (x) { h(); } } "),
            "revision:v2:r2",
            NATIVE_VERSION,
        )
        .unwrap();
        assert!(
            r1.calls
                .iter()
                .zip(&edited.calls)
                .all(|(a, b)| a.id != b.id)
        );
        assert_ne!(r1.control_regions[0].id, edited.control_regions[0].id);
    }
}

#[cfg(test)]
mod cached_executable_digest_tests {
    use super::*;
    use crate::{
        indexer::{IndexOptions, index_workspace_bundle},
        store::Store,
    };
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };

    #[test]
    fn admitted_executable_digest_survives_projection_validation_and_paired_publish() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        fs::write(
            workspace.path().join("Types.java"),
            "class A { void go() { helper(); } void helper() {} }\n",
        )
        .unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let root = fs::canonicalize(workspace.path()).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let (graph, native, capture) = index_workspace_bundle(
            &IndexOptions::new(workspace.path().to_owned()),
            store.root_id(),
            &cancel,
            |_| {},
        )
        .unwrap();
        let exe = std::env::current_exe().unwrap();
        assert_eq!(
            native.producer.executable_hash,
            capture.executable_digest(&exe).unwrap()
        );
        assert_eq!(capture.graph_projection_count(), 1);
        assert!(
            capture
                .source_operations
                .values()
                .all(|ops| ops.opens == 1 && ops.complete_reads == 1 && ops.hashes == 1)
        );
        native
            .validate(&capture, &root, store.root_id(), &cancel)
            .unwrap();
        assert_eq!(
            from_capture(&capture, &root, store.root_id(), &cancel).unwrap(),
            native
        );
        let session = store.writer_session().unwrap();
        let pin = store
            .publish_native(
                &graph,
                &capture,
                &native,
                session.writer_guard().unwrap(),
                store.index_baseline().unwrap(),
                &cancel,
            )
            .unwrap();
        assert_eq!(store.status().unwrap().revision, pin);
        let key = &native.revision.documents[0].key;
        let (document, bytes) = store.native_source_at(pin, key).unwrap().unwrap();
        assert_eq!(
            document.content_hash,
            native.revision.documents[0].content_hash
        );
        assert_eq!(bytes.as_slice(), graph.files[0].text.as_bytes());
        let go = store.native_declarations_at(pin, "java", "go").unwrap();
        assert!(!go.is_empty());
        assert!(go.iter().all(|row| {
            native
                .declarations
                .iter()
                .any(|expected| row.syntax_id == expected.syntax_id)
        }));
        drop(session);
    }

    fn all_index_rows(db_path: &Path) -> Vec<(String, Vec<String>)> {
        use rusqlite::{Connection, types::Value};
        let db = Connection::open(db_path).unwrap();
        let tables = db
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        tables
            .into_iter()
            .map(|table| {
                let mut statement = db.prepare(&format!("SELECT * FROM \"{table}\"")).unwrap();
                let count = statement.column_count();
                let mut rows = statement
                    .query_map([], |row| {
                        (0..count)
                            .map(|column| row.get::<_, Value>(column).map(|v| format!("{v:?}")))
                            .collect::<rusqlite::Result<Vec<_>>>()
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap()
                    .into_iter()
                    .map(|row| format!("{row:?}"))
                    .collect::<Vec<_>>();
                rows.sort();
                (table, rows)
            })
            .collect()
    }

    #[test]
    fn rejected_extraction_components_cannot_publish_or_mint_occurrences() {
        use crate::store::topology::WorkspaceIdentity;
        use rusqlite::Connection;
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        fs::write(
            workspace.path().join("Types.java"),
            "class A { int go() { return helper(); } int helper() { return 1; } }\n",
        )
        .unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let session = store.writer_session().unwrap();
        let leader = session.writer_guard().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let options = IndexOptions::new(workspace.path().to_owned());
        let (graph, native, capture) =
            index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
        let first = store
            .publish_native(
                &graph,
                &capture,
                &native,
                leader,
                store.index_baseline().unwrap(),
                &cancel,
            )
            .unwrap();
        let db_path = state
            .path()
            .join("cache/indexes")
            .join(
                WorkspaceIdentity::discover(Some(workspace.path()), workspace.path())
                    .unwrap()
                    .root_key,
            )
            .join("index.db");
        let old_rows = all_index_rows(&db_path);
        let old_source = store
            .native_source_at(first, &native.revision.documents[0].key)
            .unwrap();
        let old_graph = store.graph_at(Some(first)).unwrap();
        let old_class =
            serde_json::to_value(store.classes_at(None, "", Some(first), 0, 100).unwrap()).unwrap();
        let old_occurrences: i64 = Connection::open(&db_path)
            .unwrap()
            .query_row(
                "SELECT (SELECT count(*) FROM native_version_calls WHERE id LIKE 'occ:v2:%') +
                    (SELECT count(*) FROM native_version_control_regions WHERE id LIKE 'occ:v2:%')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(old_occurrences > 0);

        fs::write(
            workspace.path().join("Types.java"),
            "class A { int go() { return helper() + 1; } int helper() { return 1; } }\n",
        )
        .unwrap();
        // Admission and production measurement complete before the injected
        // component fault. Only the publication boundary receives the fault.
        let (next_graph, next_native, next_capture) =
            index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
        assert_ne!(next_native.revision.id, native.revision.id);
        for (fault, diagnostic) in [
            (
                ExtractionAuthFault::Undeclared,
                "undeclared native extraction-input inventory",
            ),
            (
                ExtractionAuthFault::Mismatched,
                "mismatched native extraction component: config",
            ),
        ] {
            let guard = inject_extraction_auth_fault(&next_native.revision.id, "java", fault);
            let error = store
                .publish_native(
                    &next_graph,
                    &next_capture,
                    &next_native,
                    leader,
                    first,
                    &cancel,
                )
                .unwrap_err();
            drop(guard);
            assert!(format!("{error:#}").contains(diagnostic), "{error:#}");
            assert_eq!(store.status().unwrap().revision, first);
            assert_eq!(
                store
                    .native_source_at(first, &native.revision.documents[0].key)
                    .unwrap(),
                old_source
            );
            assert_eq!(store.graph_at(Some(first)).unwrap(), old_graph);
            assert_eq!(
                serde_json::to_value(store.classes_at(None, "", Some(first), 0, 100).unwrap())
                    .unwrap(),
                old_class
            );
            assert_eq!(
                all_index_rows(&db_path),
                old_rows,
                "failed auth changed old or added a new native revision"
            );
            let db = Connection::open(&db_path).unwrap();
            let published: i64 = db
                .query_row("SELECT count(*) FROM native_revisions", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(published, 1, "new revision escaped component refusal");
            let occurrence_count: i64 = db.query_row(
                "SELECT (SELECT count(*) FROM native_version_calls WHERE id LIKE 'occ:v2:%') +
                        (SELECT count(*) FROM native_version_control_regions WHERE id LIKE 'occ:v2:%')",
                [], |row| row.get(0),
            ).unwrap();
            assert_eq!(
                occurrence_count, old_occurrences,
                "new occ:v2 record escaped component refusal"
            );
        }
    }
}

#[cfg(test)]
mod revision_digest_tests {
    use super::*;
    use crate::model::SourceFile;

    /// The construction `native_revision_digest` replaces: the whole input
    /// as one JSON value, canonicalized, then hashed.
    fn materialized(
        source_set_id: &str,
        files: &[&SourceFile],
        toolchain: &Value,
        config: &[Value],
        dependencies: &[Value],
    ) -> String {
        let docs: Vec<Value> = files.iter().map(|f| json!({"language":f.language,"path":f.path,"contentHash":f.hash,"byteLength":f.text.len()})).collect();
        let revision_input = json!({"sourceSetId":source_set_id,"documents":docs,"toolchain":toolchain,"config":config,"dependencies":dependencies});
        digest(b"trellis.native-revision.v2\0", &canonical(&revision_input))
    }

    fn file(language: &str, path: &str, text: String) -> SourceFile {
        SourceFile {
            path: path.into(),
            hash: hex::encode(Sha256::digest(text.as_bytes())),
            language: language.into(),
            text,
        }
    }

    /// The streamed digest is byte-for-byte the materialized one, including
    /// escaped paths, empty and multi-chunk bodies, and an empty corpus.
    /// Decision 0010: the ID commits to every document through its content
    /// hash, path and length; any change to one of them changes the ID.
    #[test]
    fn the_revision_id_changes_with_any_documents_content_or_path() {
        let toolchain = json!({"nativeExecutable":"abc"});
        let id = |files: &[SourceFile]| {
            let refs: Vec<&SourceFile> = files.iter().collect();
            native_revision_digest("source-set:v1:r", &refs, &toolchain, &[], &[])
        };
        let base = [
            file("rust", "src/a.rs", "fn a() {}\n".into()),
            file("java", "B.java", "class B {}\n".into()),
        ];
        assert_eq!(id(&base), id(&base.clone()));
        let mut content = base.clone();
        content[1] = file("java", "B.java", "class B { }\n".into());
        assert_ne!(id(&base), id(&content));
        let mut path = base.clone();
        path[0] = file("rust", "src/b.rs", "fn a() {}\n".into());
        assert_ne!(id(&base), id(&path));
    }

    #[test]
    fn the_streamed_revision_digest_equals_the_materialized_one() {
        let toolchain = json!({"nativeExecutable":"abc","rootRustToolchain":{"path":"rust-toolchain","sha256":null},"rootRustToolchainToml":null});
        let config = vec![
            json!({"path":".gitignore","sha256":"x"}),
            json!({"nativeAdmission":{"maxFileBytes":2097152}}),
        ];
        let dependencies = vec![
            json!({"path":"Cargo.lock","bytes":null}),
            json!([1, "two", {"z": true, "a": false}]),
        ];
        let files = [
            file("rust", "src/a.rs", "fn a() {}\n".into()),
            file(
                "java",
                "dir with \"quotes\"\\and\\backslash.java",
                String::new(),
            ),
            file("python", "ctl\u{1}\u{1f}é漢字.py", "x = '\u{0}'\n".into()),
            file("javascript", "big.js", "€".repeat(9000)),
        ];
        let refs: Vec<&SourceFile> = files.iter().collect();
        for (set, docs) in [
            ("source-set:v1:r", &refs[..]),
            ("s\"et", &refs[..0]),
            ("source-set:v1:r", &refs[3..]),
        ] {
            assert_eq!(
                native_revision_digest(set, docs, &toolchain, &config, &dependencies),
                materialized(set, docs, &toolchain, &config, &dependencies),
                "source set {set}, {} documents",
                docs.len()
            );
        }
    }
}

#[cfg(test)]
mod declaration_role_tests {
    use super::test_path;

    #[test]
    fn typescript_test_path_is_convention_only() {
        assert!(test_path("src/button.integration.test.ts"));
        assert!(test_path("src/button.spec.ts"));
        assert!(!test_path("src/button.ts"));
        assert!(test_path("tests/case.rs"));
        assert!(test_path("src/conftest.py"));
        assert!(!test_path("src/testing.rs"));
    }
}
