//! Closed native-only in-memory evidence, derived from the admitted immutable capture.
use crate::{
    capture::Capture,
    model::{CancelFlag, Diagnostic, IndexProgress, IndexStats, SourceFile},
    native_ids::{IdentityRegistry, OccurrenceInputRef, canonical, digest, extraction_context},
    store::admitted_capture::AdmittedCapture,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::os::unix::fs::MetadataExt;
#[cfg(test)]
use std::path::PathBuf;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use tree_sitter::Node;

// The persisted records and their leaf vocabulary live in the shared `native_records`.
pub use crate::native_records::{
    Artifact, Call, CapabilityReport, ControlRegion, Coverage, Declaration, DeclarationRole,
    Document, DocumentKey, Header, Key, MeasurementGap, MeasurementKind, MeasurementSupport,
    Parameter, Producer, Provenance, Range, Reference, Revision, Signature, SourceSet, lookup,
    verified_callee_lookup,
};
pub(crate) use crate::native_records::{LANGUAGES, NATIVE_VERSION, PRODUCER};

mod extract;
mod validate;

use extract::extract_document;

/// The build-time producer hash of this executable's extraction inputs: the
/// listed extraction modules and crates (`build/extractor_inputs.rs`). Each
/// revision records it; an executable rebuild with an equal hash keeps the
/// selected head's validated facts instead of re-extracting (Decision 0005).
pub(crate) fn extractor_hash() -> String {
    // Test-support builds may stand in a different producer hash, so a test
    // can drive a real rebuilt executable through the changed-producer path.
    #[cfg(feature = "test-support")]
    if let Ok(hash) = std::env::var("TRELLIS_TEST_EXTRACTOR_HASH") {
        return hash;
    }
    env!("TRELLIS_EXTRACTOR_HASH").to_owned()
}
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
        ("typescript", &[]),
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
/// The calling thread's injected fault, so parallel workers see the same
/// fault as the sequential code would.
#[cfg(test)]
pub(crate) type ExtractionAuthFaultSnapshot = Option<(String, String, ExtractionAuthFault)>;
#[cfg(test)]
pub(crate) fn extraction_auth_fault_snapshot() -> ExtractionAuthFaultSnapshot {
    EXTRACTION_AUTH_FAULT.with(|slot| slot.borrow().clone())
}
/// Install `snapshot` on this thread until the returned scope drops.
#[cfg(test)]
pub(crate) fn enter_extraction_auth_fault(
    snapshot: &ExtractionAuthFaultSnapshot,
) -> ExtractionAuthFaultScope {
    ExtractionAuthFaultScope(EXTRACTION_AUTH_FAULT.with(|slot| slot.replace(snapshot.clone())))
}
#[cfg(test)]
pub(crate) struct ExtractionAuthFaultScope(ExtractionAuthFaultSnapshot);
#[cfg(test)]
impl Drop for ExtractionAuthFaultScope {
    fn drop(&mut self) {
        EXTRACTION_AUTH_FAULT.with(|slot| *slot.borrow_mut() = self.0.take());
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

impl Range {
    fn node(n: Node<'_>) -> Self {
        Self {
            start: n.start_byte(),
            end: n.end_byte(),
        }
    }
}

fn stable_witness_rows<'a, T: Serialize + 'a>(
    items: impl IntoIterator<Item = &'a T>,
) -> Result<Vec<Value>> {
    let mut facts: Vec<Value> = items
        .into_iter()
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

/// Canonical evidence checksum for a document version. Revision-scoped proof IDs are
/// deliberately projected at the requested pin rather than stored in this witness.
/// The caller must independently bind source bytes, producer and manifest identity.
#[allow(clippy::too_many_arguments)] // Bind each measured family explicitly to the document witness.
pub(crate) fn document_witness<'a>(
    document: &Document,
    producer: &Producer,
    coverage: &Coverage,
    declarations: impl IntoIterator<Item = &'a Declaration>,
    calls: impl IntoIterator<Item = &'a Call>,
    regions: impl IntoIterator<Item = &'a ControlRegion>,
    references: impl IntoIterator<Item = &'a Reference>,
    capability_report: &CapabilityReport,
) -> Result<String> {
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
            "coverage": coverage, "declarations": stable_witness_rows(declarations)?,
            "calls": stable_witness_rows(calls)?, "regions": stable_witness_rows(regions)?,
            "references": stable_witness_rows(references)?, "capabilityReports": stable_witness_rows(std::slice::from_ref(capability_report))?,
        })),
    ))
}

// A declaration's measured spelling stays indexed even if an escaped JavaScript
// identifier cannot decode to a complete standalone token (for example, a
// keyword spelled with escapes). Calls use lookup().ok(), never this raw fallback.
fn declaration_lookup(language: &str, name: &str) -> Result<String> {
    match lookup(language, name) {
        Ok(key) => Ok(key),
        Err(_) if matches!(language, "javascript" | "typescript") && name.contains('\\') => {
            Ok(name.to_owned())
        }
        Err(error) => Err(error),
    }
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

/// Fixture-only entry to the production document extractor. The caller admits the listed
/// immutable snapshot bytes and fixture labels; no checkout discovery or alternate parser runs.
#[cfg(feature = "test-support")]
pub(crate) fn measure_fixture(
    files: &[SourceFile],
    source_set: SourceSet,
    revision: Revision,
) -> Result<(Artifact, FixtureHeaderWitnesses)> {
    let mut artifact = Artifact {
        producer: fixture_producer(),
        source_set,
        revision,
        coverage: vec![],
        provenance: vec![],
        declarations: vec![],
        calls: vec![],
        control_regions: vec![],
        references: vec![],
        capability_reports: vec![],
        search_cards: Vec::new(),
    };
    let mut ids = IdentityRegistry::default();
    let mut witnesses = FixtureHeaderWitnesses::new();
    for file in files {
        let document = artifact
            .revision
            .documents
            .iter()
            .find(|document| {
                document.key.path == file.path && document.key.language == file.language
            })
            .context("native document not in captured revision")?
            .clone();
        extract_known_with_witnesses(
            &mut artifact,
            file,
            &document,
            &mut ids,
            &mut |id, field, range| {
                witnesses
                    .entry(id.to_owned())
                    .or_default()
                    .push((field, range))
            },
        )?;
    }
    // The full publisher validator requires checkout-derived source-set/revision IDs.
    // Fixtures deliberately retain their authored labels; extract authenticates each
    // document hash and extraction context before minting its measured rows.
    Ok((artifact, witnesses))
}

#[cfg(feature = "test-support")]
pub(crate) fn fixture_producer() -> Producer {
    Producer {
        id: PRODUCER.into(),
        version: NATIVE_VERSION.into(),
        // This is the actual compiled extraction build, never a fixture label or override.
        executable_hash: env!("TRELLIS_EXTRACTOR_HASH").into(),
        kind: "native".into(),
        languages: LANGUAGES.map(str::to_owned).into(),
        position_encoding: "utf8".into(),
    }
}

#[cfg(feature = "test-support")]
pub(crate) fn fixture_descriptor() -> Value {
    let inputs: serde_json::Map<String, Value> = LANGUAGES
        .into_iter()
        .map(|language| {
            let inventory = declared_extraction_inputs(PRODUCER, NATIVE_VERSION, language)
                .expect("native producer must declare every supported language");
            (language.into(), json!(inventory))
        })
        .collect();
    json!({"producer":fixture_producer(), "hashKind":"nativeExtractor", "extractionInputs": inputs})
}

pub fn from_capture(
    capture: &Capture,
    root: &Path,
    root_id: &str,
    cancel: &CancelFlag,
) -> Result<Artifact> {
    let artifact = build_native(capture, root, root_id, cancel)?;
    artifact.validate_own_build(capture, cancel)?;
    Ok(artifact)
}

/// Document extractions per root id. Keyed by root, so tests running in
/// parallel on their own temporary roots never see each other's counts.
#[cfg(test)]
static DOCUMENT_EXTRACTIONS: std::sync::Mutex<std::collections::BTreeMap<String, usize>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// How many times this process has extracted a document of `root_id`, on any
/// thread and by any entry point (full build, spool or selected measurement).
#[cfg(test)]
pub(crate) fn document_extractions_for_tests(root_id: &str) -> usize {
    let counts = DOCUMENT_EXTRACTIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    counts.get(root_id).copied().unwrap_or(0)
}

/// Call-scoped evidence from the full native pipeline. `Measured` fires only
/// after the ordered owner writes that document's validated facts to disposable
/// scratch; `Validated` fires after its document-level structural and graph
/// checks, class extraction and identity admission succeed. Neither event means
/// the entire artifact has been validated or published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullNativeStage {
    Measured,
    Validated,
}

/// The captured revision header alone, measuring no document. Used when an
/// executable rebuild with an unchanged extractor hash keeps every selected
/// document's validated facts (Decision 0005).
pub(crate) fn captured_revision_header(
    capture: &dyn AdmittedCapture,
    root: &Path,
    root_id: &str,
) -> Result<(SourceSet, Revision)> {
    let (artifact, _) = build_native_header(capture, root, root_id)?;
    Ok((artifact.source_set, artifact.revision))
}

/// The canonical captured root-input components used in the native revision ID.
/// Store retains these original values for imports after mutable files change.
pub(crate) fn captured_components(
    capture: &dyn AdmittedCapture,
    executable_hash: &str,
) -> Result<(Value, Vec<Value>, Vec<Value>)> {
    let root = capture.physical_root();
    let root_inputs: Vec<_> = crate::capture::ROOT_CONFIG_INPUTS
        .iter()
        .map(|name| root.join(name))
        .collect();
    let dependencies: Vec<Value> = root_inputs
        .iter()
        .map(|p| entry(root, p, capture.bytes(p)))
        .collect::<Result<_>>()?;
    let mut config = dependencies.clone();
    let selectors: Vec<_> = crate::capture::ROOT_TOOLCHAIN_INPUTS
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
        // Present nested config inputs: package roots bind scip-typescript module markers
        // (#443), Cargo manifests scope Rust symbol keys by Cargo target (#394), Python,
        // Maven and Gradle manifests feed app detection (#807, #854), and TypeScript configs
        // are the project references a producer plan follows (#717).
        if b.is_some() && crate::capture::nested_config_input(root, p) {
            config.push(entry(root, p, b)?);
        }
    }
    let selectors: Vec<Value> = selectors
        .iter()
        .map(|p| entry(root, p, capture.bytes(p)))
        .collect::<Result<_>>()?;
    let toolchain = json!({"nativeExecutable":executable_hash,"rootRustToolchain":selectors[0],"rootRustToolchainToml":selectors[1]});
    Ok((toolchain, config, dependencies))
}

/// The full captured revision header is shared by full and selected measurement.
/// Selecting one file must never change the source-set, producer or revision ID.
fn build_native_header<'a>(
    capture: &'a dyn AdmittedCapture,
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
    let (toolchain, config, dependencies) = captured_components(capture, &executable_hash)?;
    let mut files: Vec<_> = capture.files().iter().collect();
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
        references: vec![],
        capability_reports: vec![],
        search_cards: Vec::new(),
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

/// Rows whose IDs are minted together before their checks run in order.
const MINT_CHUNK: usize = 16 * 1024;

fn build_native(
    capture: &Capture,
    root: &Path,
    root_id: &str,
    cancel: &CancelFlag,
) -> Result<Artifact> {
    build_native_observed(capture, root, root_id, cancel, &|_| {})
}

fn build_native_observed(
    capture: &Capture,
    root: &Path,
    root_id: &str,
    cancel: &CancelFlag,
    on_extract: &impl Fn(&DocumentKey),
) -> Result<Artifact> {
    build_native_with(
        capture,
        root,
        root_id,
        cancel,
        on_extract,
        ExtractionPlan::default(),
        &|_| Ok(()),
    )
}

/// How full extraction is spread over workers: files are extracted in
/// chunks of `chunk`, each on up to `workers` threads, and merged in
/// revision order after each chunk.
#[derive(Clone, Copy, Debug)]
struct ExtractionPlan {
    workers: usize,
    chunk: usize,
}
impl Default for ExtractionPlan {
    fn default() -> Self {
        let workers = crate::parallel::default_workers();
        Self {
            workers,
            chunk: workers * 16,
        }
    }
}

/// One file's facts, measured into its own header-only artifact with its
/// own first-insertion-ordered identity registry.
struct FileFacts {
    shell: Artifact,
    ids: IdentityRegistry,
    result: Result<()>,
}

/// Every file is extracted by the same document-local function as before;
/// results are merged strictly in the revision's sorted order, so the
/// artifact, its minted IDs and the first reported error are those of a
/// sequential build. `before_file` is a test hook; production passes a no-op.
fn build_native_with(
    capture: &Capture,
    root: &Path,
    root_id: &str,
    cancel: &CancelFlag,
    on_extract: &impl Fn(&DocumentKey),
    plan: ExtractionPlan,
    before_file: &(dyn Fn(&SourceFile) -> Result<()> + Sync),
) -> Result<Artifact> {
    let (mut artifact, files) = build_native_header(capture, root, root_id)?;
    // The revision header was built from this exact sorted file sequence, so
    // file i is document i. Workers measure into a header-only shell.
    let documents = std::mem::take(&mut artifact.revision.documents);
    let shell = artifact.clone();
    let pairs: Vec<(&SourceFile, &Document)> = files.into_iter().zip(&documents).collect();
    let mut ids = IdentityRegistry::default();
    for chunk in pairs.chunks(plan.chunk.max(1)) {
        ensure!(
            !cancel.load(std::sync::atomic::Ordering::Acquire),
            "indexing cancelled"
        );
        let measured =
            crate::parallel::map_ordered(chunk, plan.workers, cancel, |_, (f, document)| {
                let mut facts = FileFacts {
                    shell: shell.clone(),
                    ids: IdentityRegistry::logged(),
                    result: Ok(()),
                };
                facts.result = before_file(f)
                    .and_then(|()| extract_known(&mut facts.shell, f, document, &mut facts.ids));
                facts
            });
        for (facts, (_, document)) in measured.into_iter().zip(chunk) {
            let facts = facts.context("indexing cancelled")?;
            // A failed file reports its own extraction error. Successful files
            // enter the global registry in sorted-file, first-insertion order.
            facts.result?;
            ids.absorb(facts.ids)?;
            let shell = facts.shell;
            artifact.coverage.extend(shell.coverage);
            artifact.provenance.extend(shell.provenance);
            artifact.declarations.extend(shell.declarations);
            artifact.calls.extend(shell.calls);
            artifact.control_regions.extend(shell.control_regions);
            artifact.references.extend(shell.references);
            artifact.capability_reports.extend(shell.capability_reports);
            artifact.search_cards.extend(shell.search_cards);
            on_extract(&document.key);
        }
    }
    drop(pairs);
    artifact.revision.documents = documents;
    Ok(artifact)
}

// The private spool omits only reference fields shared by its validated document.
// Public Artifact/Reference JSON remains unchanged.
#[derive(Serialize)]
struct SpoolFactsRef<'a> {
    artifact: &'a Artifact,
    references: SpoolReferencesRef<'a>,
    search_cards: &'a [crate::search_capture::CapturedCard],
}

struct SpoolReferencesRef<'a>(&'a [Reference]);
impl Serialize for SpoolReferencesRef<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for reference in self.0 {
            let lookup_override = (reference.lookup_key != reference.spelling)
                .then_some(reference.lookup_key.as_str());
            sequence.serialize_element(&(
                reference.id.as_str(),
                reference.owner_syntax_id.as_str(),
                reference.ordinal,
                reference.range.start,
                reference.range.end,
                reference.spelling.as_str(),
                lookup_override,
            ))?;
        }
        sequence.end()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpoolFacts {
    artifact: Artifact,
    references: Vec<SpoolReference>,
    search_cards: Vec<crate::search_capture::CapturedCard>,
}

#[derive(Deserialize)]
struct SpoolReference(String, String, usize, usize, usize, String, Option<String>);

fn spool_proof<'a>(artifact: &'a Artifact, document: &Document) -> Result<&'a str> {
    ensure!(
        artifact.provenance.len() == 1,
        "native spool proof cardinality"
    );
    let proof = &artifact.provenance[0];
    let expected = format!(
        "native-proof:v1:{}",
        digest(
            b"trellis.native-proof.v1\0",
            &canonical(
                &json!({"producerId":PRODUCER,"document":document.key,"revisionId":artifact.revision.id})
            )
        )
    );
    ensure!(
        proof.id == expected
            && proof.producer_id == PRODUCER
            && proof.document == document.key
            && proof.revision_id == artifact.revision.id
            && proof.content_hash == document.content_hash
            && proof.evidence_kind == "measuredSyntax"
            && proof.basis.is_none()
            && proof.derived_from.is_none()
            && proof.freshness == "fresh",
        "native spool proof/document mismatch"
    );
    Ok(&proof.id)
}

fn encode_spool_facts(mut artifact: Artifact, document: &Document) -> Result<Vec<u8>> {
    ensure!(
        artifact.revision.documents.as_slice() == std::slice::from_ref(document),
        "native spool facts/document mismatch"
    );
    let proof = spool_proof(&artifact, document)?;
    ensure!(
        artifact.references.iter().all(|reference| {
            reference.document == document.key
                && reference.revision_id == artifact.revision.id
                && reference.provenance_id == proof
        }),
        "native spool reference context mismatch"
    );
    let references = std::mem::take(&mut artifact.references);
    artifact.revision.documents.clear();
    serde_json::to_vec(&SpoolFactsRef {
        artifact: &artifact,
        references: SpoolReferencesRef(&references),
        search_cards: &artifact.search_cards,
    })
    .context("encode native spool facts")
}

/// Disposable, disk-backed full extraction. Only the captured header and aggregate
/// counters remain in memory; each measured document is serialized before the next
/// batch is measured. The file is private to this process and removed on drop.
/// The ordered owner admits successful files and validates/projects each file
/// before advancing, so the earliest sorted-file failure is reported.
pub(crate) struct FullDocumentSpool {
    header: Artifact,
    // Drop the connection before the separate file handle: closing another
    // descriptor on a live SQLite file would discard this process's POSIX locks.
    conn: rusqlite::Connection,
    _file: tempfile::NamedTempFile,
    stats: IndexStats,
    diagnostics: Vec<Diagnostic>,
    #[cfg(test)]
    test_read_error: std::cell::RefCell<Option<rusqlite::Error>>,
}

impl FullDocumentSpool {
    pub(crate) fn header(&self) -> &Artifact {
        &self.header
    }
    pub(crate) fn len(&self) -> usize {
        self.header.revision.documents.len()
    }
    pub(crate) fn stats(&self) -> &IndexStats {
        &self.stats
    }
    pub(crate) fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }
    /// Read one document with full revision identity but only its own document row.
    /// This is repeatable; no decoded document is cached between reads.
    pub(crate) fn read(&self, index: usize) -> Result<(Artifact, crate::model::Graph)> {
        let document = self
            .header
            .revision
            .documents
            .get(index)
            .context("native spool document index out of range")?;
        #[cfg(test)]
        if let Some(error) = self.test_read_error.borrow_mut().take() {
            return Err(error.into());
        }
        let (facts, graph): (Vec<u8>, Vec<u8>) = self.conn.query_row(
            "SELECT facts, graph FROM documents WHERE ordinal = ?1",
            rusqlite::params![i64::try_from(index)?],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let SpoolFacts {
            mut artifact,
            references,
            search_cards,
        } = serde_json::from_slice(&facts)?;
        artifact.search_cards = search_cards;
        ensure!(
            artifact.references.is_empty()
                && artifact.revision.documents.is_empty()
                && artifact.producer == self.header.producer
                && artifact.source_set == self.header.source_set
                && artifact.revision.id == self.header.revision.id
                && artifact.coverage.len() == 1
                && artifact.coverage[0].document_path == document.key.path,
            "native spool document/header mismatch"
        );
        let proof_id = spool_proof(&artifact, document)?.to_owned();
        artifact.references = references
            .into_iter()
            .map(
                |SpoolReference(
                    id,
                    owner_syntax_id,
                    ordinal,
                    start,
                    end,
                    spelling,
                    lookup_override,
                )| {
                    Reference {
                        id,
                        owner_syntax_id,
                        ordinal,
                        document: document.key.clone(),
                        revision_id: artifact.revision.id.clone(),
                        range: Range { start, end },
                        lookup_key: lookup_override.unwrap_or_else(|| spelling.clone()),
                        spelling,
                        provenance_id: proof_id.clone(),
                    }
                },
            )
            .collect();
        artifact.revision.documents.push(document.clone());
        let graph: crate::model::Graph = serde_json::from_slice(&graph)?;
        ensure!(
            graph.files.len() == 1 && graph.files[0].path == document.key.path,
            "native spool graph/document mismatch"
        );
        Ok((artifact, graph))
    }
    #[cfg(test)]
    pub(crate) fn corrupt_graph_json_for_tests(&self, index: usize) {
        assert_eq!(
            self.conn
                .execute(
                    "UPDATE documents SET graph=X'00' WHERE ordinal=?1",
                    [i64::try_from(index).unwrap()],
                )
                .unwrap(),
            1
        );
    }
    #[cfg(test)]
    pub(crate) fn corrupt_extraction_json_for_tests(&self, index: usize) {
        assert_eq!(
            self.conn
                .execute(
                    "UPDATE extractions SET payload=X'00' WHERE ordinal=?1",
                    [i64::try_from(index).unwrap()],
                )
                .unwrap(),
            1
        );
    }
    #[cfg(test)]
    pub(crate) fn inject_sqlite_corrupt_read_for_tests(&self) {
        *self.test_read_error.borrow_mut() = Some(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            None,
        ));
    }
    fn write_extraction(
        &self,
        index: usize,
        extraction: &crate::model::FileExtraction,
    ) -> Result<()> {
        ensure!(
            index < self.len(),
            "native spool extraction index out of range"
        );
        ensure!(
            extraction.path == self.header.revision.documents[index].key.path,
            "native spool extraction path mismatch"
        );
        self.conn.execute(
            "INSERT INTO extractions (ordinal, payload) VALUES (?1, ?2)",
            rusqlite::params![i64::try_from(index)?, serde_json::to_vec(extraction)?],
        )?;
        Ok(())
    }
    pub(crate) fn read_extraction(&self, index: usize) -> Result<crate::model::FileExtraction> {
        ensure!(
            index < self.len(),
            "native spool extraction index out of range"
        );
        let payload: Vec<u8> = self.conn.query_row(
            "SELECT payload FROM extractions WHERE ordinal = ?1",
            rusqlite::params![i64::try_from(index)?],
            |row| row.get(0),
        )?;
        let extraction: crate::model::FileExtraction = serde_json::from_slice(&payload)?;
        ensure!(
            extraction.path == self.header.revision.documents[index].key.path,
            "native spool extraction path mismatch"
        );
        Ok(extraction)
    }
}

/// Validate the exact one-document view while retaining the full revision ID
/// and captured config/toolchain/dependency binding. The full document list
/// stays only in the spool header, not in each projected document.
fn validate_spooled_document(
    facts: &mut Artifact,
    document: &Document,
    file: &SourceFile,
) -> Result<()> {
    ensure!(
        facts.revision.documents.is_empty() && document.key.path == file.path,
        "native spool facts/document mismatch"
    );
    facts.revision.documents.push(document.clone());
    facts.validate_structure(std::slice::from_ref(file))
}

/// One document's spool rows, prepared on a worker.
struct PreparedSpoolDocument {
    facts: Vec<u8>,
    graph: Vec<u8>,
    extraction: Option<crate::model::FileExtraction>,
    stats: IndexStats,
    diagnostics: Vec<Diagnostic>,
}

/// Validate, project and encode one extracted document. Pure per-document
/// work: it touches no shared registry and no SQLite handle.
#[allow(clippy::too_many_arguments)] // The worker's whole per-document context.
fn prepare_spooled_document(
    options: &crate::indexer::IndexOptions,
    capture: &Capture,
    mut facts: Artifact,
    document: &Document,
    file: &SourceFile,
    cancel: &CancelFlag,
    before_validate: &(dyn Fn(&Document, &mut Artifact) -> Result<()> + Sync),
    after_class: &(dyn Fn(&Document, &crate::model::FileExtraction) -> Result<()> + Sync),
) -> Result<PreparedSpoolDocument> {
    before_validate(document, &mut facts)?;
    validate_spooled_document(&mut facts, document, file)?;
    let graph = crate::indexer::project_native_document(
        options,
        capture,
        &facts,
        &document.key.path,
        cancel,
        &|_| {},
    )?;
    crate::indexer::validate_native_graph_records(&graph, &facts, cancel)?;
    let extraction = if crate::classes::supports_language(&file.language) {
        let symbols: Vec<_> = graph
            .nodes
            .iter()
            .filter(|node| {
                matches!(
                    node.kind,
                    crate::model::SymbolKind::Class
                        | crate::model::SymbolKind::Method
                        | crate::model::SymbolKind::Function
                )
            })
            .cloned()
            .collect();
        let extraction = crate::model::FileExtraction::extract_file(
            file,
            &symbols,
            cancel,
            crate::classes::Limits::default(),
        )?;
        after_class(document, &extraction)?;
        Some(extraction)
    } else {
        None
    };
    Ok(PreparedSpoolDocument {
        facts: encode_spool_facts(facts, document)?,
        graph: serde_json::to_vec(&graph)?,
        extraction,
        stats: graph.stats.clone(),
        diagnostics: graph.diagnostics.clone(),
    })
}

/// Admit an immutable capture without retaining its full artifact or graph.
/// The global digest registry is resident; only facts and graphs live in the
/// disposable SQLite spool. A failure drops it without publication.
#[allow(clippy::too_many_arguments)] // Keep capture, observer and scratch-owner bindings explicit.
pub(crate) fn spool_full_capture(
    options: &crate::indexer::IndexOptions,
    capture: &Capture,
    root: &Path,
    root_id: &str,
    cancel: &CancelFlag,
    progress: &(impl Fn(IndexProgress) + Sync),
    observe: impl Fn(&DocumentKey, FullNativeStage),
    spool_dir: &Path,
) -> Result<FullDocumentSpool> {
    spool_full_capture_with(
        options,
        capture,
        root,
        root_id,
        cancel,
        progress,
        observe,
        ExtractionPlan::default(),
        &|_, _| Ok(()),
        &|_, _| Ok(()),
        &|_, _, _| Ok(()),
        &|_, _| Ok(()),
        spool_dir,
    )
}

#[allow(clippy::too_many_arguments)] // The test injection seam preserves the production argument order.
fn spool_full_capture_with(
    options: &crate::indexer::IndexOptions,
    capture: &Capture,
    root: &Path,
    root_id: &str,
    cancel: &CancelFlag,
    progress: &(impl Fn(IndexProgress) + Sync),
    observe: impl Fn(&DocumentKey, FullNativeStage),
    plan: ExtractionPlan,
    after_extract: &(dyn Fn(&SourceFile, &IdentityRegistry) -> Result<()> + Sync),
    before_validate: &(dyn Fn(&Document, &mut Artifact) -> Result<()> + Sync),
    before_admit: &dyn Fn(&Document, &mut IdentityRegistry, &IdentityRegistry) -> Result<()>,
    after_class: &(dyn Fn(&Document, &crate::model::FileExtraction) -> Result<()> + Sync),
    spool_dir: &Path,
) -> Result<FullDocumentSpool> {
    capture.verify(cancel)?;
    let (header, files) = build_native_header(capture, root, root_id)?;
    // The store supplies its already-prepared managed index directory, including
    // isolated test topology. Never select a production home independently.
    let file = tempfile::Builder::new()
        .prefix("native-spool-")
        .suffix(".db")
        .tempfile_in(spool_dir)?;
    let conn = rusqlite::Connection::open(file.path())?;
    // SQLite keeps its own descriptor. Unlink before any schema writes so a
    // crash cannot strand the disposable spool. OFF mode creates no journal.
    std::fs::remove_file(file.path()).context("remove private native spool pathname")?;
    conn.execute_batch(
        "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA cache_size=-2048;
         CREATE TABLE documents (ordinal INTEGER PRIMARY KEY, facts BLOB NOT NULL, graph BLOB);
         CREATE TABLE extractions (ordinal INTEGER PRIMARY KEY, payload BLOB NOT NULL);",
    )?;
    let mut spool = FullDocumentSpool {
        header,
        conn,
        _file: file,
        stats: IndexStats::default(),
        diagnostics: Vec::new(),
        #[cfg(test)]
        test_read_error: std::cell::RefCell::new(None),
    };
    // The full revision document vector remains in the header. Worker shells
    // carry only revision ID and inputs, never the complete document vector.
    let shell = Artifact {
        producer: spool.header.producer.clone(),
        source_set: spool.header.source_set.clone(),
        revision: Revision {
            documents: Vec::new(),
            id: spool.header.revision.id.clone(),
            source_set_id: spool.header.revision.source_set_id.clone(),
            toolchain_hash: spool.header.revision.toolchain_hash.clone(),
            config_hash: spool.header.revision.config_hash.clone(),
            dependency_hash: spool.header.revision.dependency_hash.clone(),
        },
        coverage: Vec::new(),
        provenance: Vec::new(),
        declarations: Vec::new(),
        calls: Vec::new(),
        control_regions: Vec::new(),
        references: Vec::new(),
        capability_reports: Vec::new(),
        search_cards: Vec::new(),
    };
    let documents = &spool.header.revision.documents;
    let pairs: Vec<(&SourceFile, &Document)> = files.into_iter().zip(documents).collect();
    // Only one graph projection is allowed for this capture. Per-document
    // projection does not claim again. Keep source/producer checks before work.
    capture.claim_graph_projection()?;
    let mut ids = IdentityRegistry::default();
    spool.conn.execute_batch("BEGIN IMMEDIATE")?;
    for (chunk_index, chunk) in pairs.chunks(plan.chunk.max(1)).enumerate() {
        ensure!(
            !cancel.load(std::sync::atomic::Ordering::Acquire),
            "indexing cancelled"
        );
        // Each worker does all of one document's own work: extraction,
        // structure and graph validation, class extraction and encoding. The
        // ordered owner below only admits identities and writes the spool, in
        // sorted order, so the earliest sorted file's failure is still the one
        // reported whatever order the workers finish in.
        let prepared =
            crate::parallel::map_ordered(chunk, plan.workers, cancel, |_, (file, document)| {
                let mut ids = IdentityRegistry::logged();
                let mut facts = shell.clone();
                let result = extract_known(&mut facts, file, document, &mut ids)
                    .and_then(|()| after_extract(file, &ids))
                    .and_then(|()| {
                        prepare_spooled_document(
                            options,
                            capture,
                            facts,
                            document,
                            file,
                            cancel,
                            before_validate,
                            after_class,
                        )
                    });
                (ids, result)
            });
        for (offset, (prepared, (_, document))) in prepared.into_iter().zip(chunk).enumerate() {
            let (local_ids, prepared) = prepared.context("indexing cancelled")?;
            let prepared = prepared?;
            let index = chunk_index * plan.chunk.max(1) + offset;
            spool.conn.execute(
                "INSERT INTO documents (ordinal, facts, graph) VALUES (?1, ?2, ?3)",
                rusqlite::params![i64::try_from(index)?, prepared.facts, prepared.graph],
            )?;
            observe(&document.key, FullNativeStage::Measured);
            // Scratch facts are disposable; collision checking must precede
            // any index.db publication, not this scratch write.
            before_admit(document, &mut ids, &local_ids)?;
            ids.absorb(local_ids)?;
            observe(&document.key, FullNativeStage::Validated);
            if let Some(extraction) = &prepared.extraction {
                spool.write_extraction(index, extraction)?;
            }
            spool.stats.files += prepared.stats.files;
            spool.stats.symbols += prepared.stats.symbols;
            spool.stats.calls += prepared.stats.calls;
            spool.stats.regions += prepared.stats.regions;
            spool.stats.unresolved += prepared.stats.unresolved;
            spool.stats.parse_error_files += prepared.stats.parse_error_files;
            spool.diagnostics.extend(prepared.diagnostics);
            progress(IndexProgress {
                phase: "parse".into(),
                completed: index + 1,
                total: spool.len(),
            });
        }
    }
    spool.conn.execute_batch("COMMIT")?;
    drop(shell);
    capture.verify(cancel)?;
    progress(IndexProgress {
        phase: "complete".into(),
        completed: spool.len(),
        total: spool.len(),
    });
    Ok(spool)
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
    pub references: Vec<Reference>,
    pub capability_report: CapabilityReport,
    pub search_cards: Vec<crate::search_capture::CapturedCard>,
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
            && artifact.capability_reports.len() == 1
            && artifact.capability_reports[0].document == document.key
            && artifact
                .references
                .iter()
                .all(|row| row.document == document.key)
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
        references: artifact.references,
        capability_report: artifact.capability_reports.remove(0),
        search_cards: artifact.search_cards,
    })
}

/// Measure the current changed documents once against one full revision header.
/// Unchanged documents retain their validated stored facts; an empty selection
/// represents a deletion-only publication.
pub(crate) fn measure_captured_documents(
    capture: &dyn AdmittedCapture,
    root: &Path,
    root_id: &str,
    paths: &BTreeSet<String>,
    cancel: &CancelFlag,
    mut on_extract: impl FnMut(&DocumentKey),
) -> Result<Artifact> {
    capture.verify(cancel)?;
    let (mut artifact, files) = build_native_header(capture, root, root_id)?;
    let full_documents = std::mem::take(&mut artifact.revision.documents);
    artifact.revision.documents = full_documents
        .iter()
        .filter(|document| paths.contains(&document.key.path))
        .cloned()
        .collect();
    ensure!(
        artifact.revision.documents.len() == paths.len(),
        "selected native source must be unique and admitted"
    );
    let selected: Vec<_> = files
        .into_iter()
        .filter(|file| paths.contains(&file.path))
        .collect();
    let mut ids = IdentityRegistry::default();
    let documents = std::mem::take(&mut artifact.revision.documents);
    for (file, document) in selected.iter().zip(&documents) {
        ensure!(
            file.path == document.key.path,
            "selected native source path mismatch"
        );
        ensure!(
            !cancel.load(std::sync::atomic::Ordering::Acquire),
            "indexing cancelled"
        );
        ensure!(
            capture.hashes().get(&file.path) == Some(&file.hash)
                && file.hash == hash(file.text.as_bytes()),
            "selected native source bytes/hash mismatch"
        );
        extract_known(&mut artifact, file, document, &mut ids)?;
        on_extract(&document.key);
    }
    artifact.revision.documents = documents;
    let selected_files: Vec<_> = selected.into_iter().cloned().collect();
    artifact.validate_structure(&selected_files)?;
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
                && entry.capability_report.document == document.key
                && entry.capability_report.revision_id == *old_revision
                && entry.references.iter().all(|r| r.document == document.key
                    && r.revision_id == *old_revision
                    && r.provenance_id == *old_proof)
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
        entry.capability_report.revision_id.clone_from(new_revision);
        for r in &mut entry.references {
            r.revision_id.clone_from(new_revision);
            r.provenance_id.clone_from(&proof);
        }
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
        artifact.references.extend(entry.references);
        artifact.capability_reports.push(entry.capability_report);
        artifact.search_cards.extend(entry.search_cards);
    }
    artifact.validate_structure(&capture.files)?;
    capture.verify(cancel)?;
    Ok(artifact)
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
            // A line comment ends at its newline; the attribute may continue after it.
            offset += rest.find('\n').map_or(rest.len(), |end| end + 1);
            continue;
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
/// Whether an attribute marks test code under Decision 0011: `cfg(test)`, a direct `test`
/// operand of `cfg(all(..))`, or a final-path `test` attribute.
pub(crate) fn rust_test_attribute(n: Node<'_>, file: &SourceFile) -> bool {
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
    // A final-path test attribute may take arguments: `#[tokio::test(flavor = "…")]`.
    t.get(index) == Some(&"test") && matches!(t.get(index + 1), Some(&"]") | Some(&"("))
}

#[cfg(feature = "test-support")]
pub(crate) type FixtureHeaderWitnesses = BTreeMap<String, Vec<(String, Range)>>;

/// tree-sitter-typescript reads `await f<T>(x)` as a call of `await f`: the call's function
/// is an `await_expression` around the real callee (#958). This is that callee.
pub(crate) fn awaited_callee(n: Node<'_>) -> Option<Node<'_>> {
    if n.kind() != "call_expression" {
        return None;
    }
    let function = n.child_by_field_name("function")?;
    (function.kind() == "await_expression")
        .then(|| function.named_child(0))
        .flatten()
}

/// The measured range of a JavaScript or TypeScript call node. The call that `await f<T>(x)`
/// awaits is `f<T>(x)`, as it is for `await f(x)`, so its range starts at the callee.
pub(crate) fn js_call_range(n: Node<'_>) -> Range {
    Range {
        start: awaited_callee(n).map_or(n.start_byte(), |callee| callee.start_byte()),
        end: n.end_byte(),
    }
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
    extract_known_with_witnesses(a, f, document, ids, &mut |_, _, _| {})
}

fn extract_known_with_witnesses(
    a: &mut Artifact,
    f: &SourceFile,
    document: &Document,
    ids: &mut IdentityRegistry,
    header_witness: &mut dyn FnMut(&str, String, Range),
) -> Result<()> {
    extract_document(a, f, document, ids, header_witness, true)
}

/// Decision 0003 `occ:v2` input: document version, extraction context and native producer
/// descriptor, never the containing revision.
fn occurrence_input<'a>(
    content_hash: &'a str,
    extraction_context: &'a str,
    producer: &'a Producer,
    owner: &'a str,
    kind: &'a str,
    ordinal: usize,
) -> OccurrenceInputRef<'a> {
    OccurrenceInputRef {
        content_hash,
        extraction_context,
        native_producer_id: &producer.id,
        native_producer_version: &producer.version,
        owner_syntax_id: owner,
        kind,
        ordinal: ordinal as u64,
    }
}
/// Whether `range` lies inside one of a document's sorted, disjoint macro expansions. Facts
/// there order after the owner's other facts (Decision 0019), so the others keep their ordinals.
fn in_macro_expansion(expansions: &[Range], range: &Range) -> bool {
    let i = expansions.partition_point(|e| e.start <= range.start);
    i > 0 && range.end <= expansions[i - 1].end
}

#[cfg(test)]
#[path = "native_rust_macros_tests.rs"]
mod macro_expansion_tests;

impl Artifact {
    /// Validates an assembled revision against the full current capture header
    /// and checks all resulting IDs, ownership and structure without re-extraction.
    pub(crate) fn validate_selective(
        &self,
        capture: &dyn AdmittedCapture,
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
        self.validate_structure(capture.files())?;
        capture.verify(cancel)
    }
    /// The artifact is this process's own `build_native` output for exactly
    /// this capture, so rebuilding it would only re-check determinism, which
    /// the `native_build_is_deterministic` verify test covers. Every
    /// structural check and the capture freshness check still run. Only
    /// `from_capture` calls this.
    fn validate_own_build(&self, capture: &Capture, cancel: &CancelFlag) -> Result<()> {
        capture.verify(cancel)?;
        self.validate_structure(&capture.files)
    }
    /// Validate the captured header and native structure once, without re-extracting
    /// facts. Determinism is covered by `native_build_is_deterministic` instead.
    /// Honors the caller's cancellation before and after validation.
    pub fn validate(
        &self,
        capture: &dyn AdmittedCapture,
        root: &Path,
        root_id: &str,
        cancel: &CancelFlag,
    ) -> Result<()> {
        self.validate_selective(capture, root, root_id, cancel)
    }
}

#[cfg(test)]
#[path = "native_evidence_borrowed_witness_tests.rs"]
mod borrowed_witness_tests;

#[cfg(test)]
#[path = "native_evidence_occurrence_identity_tests.rs"]
mod occurrence_identity_tests;

#[cfg(test)]
#[path = "native_evidence_cached_executable_digest_tests.rs"]
mod cached_executable_digest_tests;

#[cfg(test)]
#[path = "native_evidence_revision_digest_tests.rs"]
mod revision_digest_tests;

#[cfg(test)]
#[path = "native_evidence_declaration_role_tests.rs"]
mod declaration_role_tests;

#[cfg(test)]
#[path = "native_evidence_parallel_extraction_tests.rs"]
mod parallel_extraction_tests;

#[cfg(test)]
#[path = "native_evidence_full_document_spool_tests.rs"]
mod full_document_spool_tests;
