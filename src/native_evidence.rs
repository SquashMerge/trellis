//! Closed native-only in-memory evidence, derived from the admitted immutable capture.
use crate::{
    capture::Capture,
    model::{CancelFlag, Diagnostic, IndexProgress, IndexStats, SourceFile},
    native_ids::{IdentityRegistry, OccurrenceInputRef, canonical, digest, extraction_context},
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
    capture: &Capture,
    root: &Path,
    root_id: &str,
) -> Result<(SourceSet, Revision)> {
    let (artifact, _) = build_native_header(capture, root, root_id)?;
    Ok((artifact.source_set, artifact.revision))
}

/// The canonical captured root-input components used in the native revision ID.
/// Store retains these original values for imports after mutable files change.
pub(crate) fn captured_components(
    capture: &Capture,
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
    let (toolchain, config, dependencies) = captured_components(capture, &executable_hash)?;
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
    capture: &Capture,
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
            capture.hashes.get(&file.path) == Some(&file.hash)
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

fn kind(lang: &str, n: Node<'_>, text: &str) -> Option<&'static str> {
    match lang {
        "java" => crate::indexer_java::native_kind(n),
        "rust" => crate::indexer_rust::native_kind(n),
        "python" => crate::indexer_python::native_kind(n),
        "javascript" => crate::indexer::native_js_kind(n),
        "typescript" => crate::indexer_ts::native_ts_kind(n, text),
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
    ) || (lang == "typescript"
        && matches!(n.kind(), "call_expression" | "new_expression")
        && !crate::indexer_ts::in_type_context(n))
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
    // A final-path test attribute may take arguments: `#[tokio::test(flavor = "…")]`.
    t.get(index) == Some(&"test") && matches!(t.get(index + 1), Some(&"]") | Some(&"("))
}
/// An inner `#![cfg(test)]`-style attribute among `n`'s own children marks
/// its enclosing file or module body as test code.
fn rust_inner_test(n: Node<'_>, file: &SourceFile) -> bool {
    child_nodes(n)
        .into_iter()
        .any(|child| child.kind() == "inner_attribute_item" && rust_test_attribute(child, file))
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
        "rust" => match n.kind() {
            "source_file" | "declaration_list" => rust_inner_test(n, file),
            "mod_item" => n
                .child_by_field_name("body")
                .is_some_and(|body| rust_inner_test(body, file)),
            _ => false,
        },
        _ => false,
    }
}

fn child_nodes(n: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = n.walk();
    n.named_children(&mut cursor).collect()
}
fn name(n: Node<'_>, f: &SourceFile) -> Option<(String, Range)> {
    if f.language == "rust"
        && n.kind() == "use_as_clause"
        && n.child_by_field_name("alias")
            .and_then(|alias| f.text.get(alias.byte_range()))
            == Some("_")
    {
        return None;
    }
    let typescript_name = if f.language == "typescript" {
        crate::indexer_ts::name_node(n)
    } else {
        None
    };
    let node = if let Some(decided) = typescript_name {
        decided
    } else if matches!(n.kind(), "import_specifier" | "use_as_clause") {
        n.child_by_field_name("alias")
    } else {
        n.child_by_field_name("name")
    }
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
    .or_else(|| {
        (n.kind() == "spread_parameter")
            .then(|| java_parameter_parts(n).1)
            .flatten()
    })
    .or_else(|| (n.kind() == "identifier").then_some(n))?;
    if !matches!(
        node.kind(),
        "identifier"
            | "property_identifier"
            | "field_identifier"
            | "type_identifier"
            | "string_fragment"
    ) {
        return None;
    }
    let value = f.text.get(node.byte_range())?;
    if value.is_empty() {
        return None;
    }
    Some((value.to_owned(), Range::node(node)))
}
fn java_parameter_parts(n: Node<'_>) -> (Option<Node<'_>>, Option<Node<'_>>) {
    let type_node = n.child_by_field_name("type").or_else(|| {
        (n.kind() == "spread_parameter")
            .then(|| {
                child_nodes(n).into_iter().find(|child| {
                    !matches!(
                        child.kind(),
                        "modifiers" | "annotation" | "marker_annotation" | "variable_declarator"
                    )
                })
            })
            .flatten()
    });
    let name_node = n.child_by_field_name("name").or_else(|| {
        (n.kind() == "spread_parameter")
            .then(|| {
                child_nodes(n)
                    .into_iter()
                    .find(|child| child.kind() == "variable_declarator")
                    .and_then(|declarator| declarator.child_by_field_name("name"))
            })
            .flatten()
    });
    (type_node, name_node)
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
        if let Some(t) = java_parameter_parts(parameter)
            .0
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

/// A TypeScript type annotation measures as the type it wraps, without the `: `.
fn annotated<'t>(f: &SourceFile, node: Node<'t>) -> Node<'t> {
    if f.language == "typescript" {
        crate::indexer_ts::unwrap_annotation(node)
    } else {
        node
    }
}

fn header_with_witnesses(
    n: Node<'_>,
    f: &SourceFile,
    key: &Key,
    _witness: &mut impl FnMut(String, Range),
) -> Header {
    let from = |node: Node<'_>| f.text.get(node.byte_range()).map(str::to_owned);
    macro_rules! measured {
        ($field:expr, $node:expr) => {{
            let node = $node;
            let _field = || -> String { $field };
            from(node).inspect(|_value| {
                #[cfg(feature = "test-support")]
                _witness(_field(), Range::node(node));
            })
        }};
    }
    let mut modifier_tokens = Vec::new();
    let mut cursor = n.walk();
    for child in n.children(&mut cursor) {
        match child.kind() {
            "visibility_modifier" | "accessibility_modifier" | "override_modifier" => {
                if let Some(value) = from(child) {
                    modifier_tokens.push((child, value));
                }
            }
            "modifiers" | "function_modifiers" => {
                let mut nested_cursor = child.walk();
                for token in child.children(&mut nested_cursor) {
                    if let Some(value) = from(token)
                        && (token.is_named() || modifier_keyword(&value))
                    {
                        modifier_tokens.push((token, value));
                    }
                }
            }
            _ if !child.is_named() => {
                if let Some(value) = from(child)
                    && modifier_keyword(&value)
                {
                    modifier_tokens.push((child, value));
                }
            }
            _ => {}
        }
    }
    modifier_tokens.sort_by_key(|(node, _)| (node.start_byte(), node.end_byte()));
    let modifiers = modifier_tokens
        .into_iter()
        .enumerate()
        .filter_map(|(_i, (node, _))| measured!(format!("header.modifiers[{_i}]"), node))
        .collect();
    let type_parameters = n
        .child_by_field_name("type_parameters")
        .map(child_nodes)
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .filter_map(|(_i, node)| measured!(format!("header.typeParameters[{_i}]"), node))
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
        .enumerate()
        .map(|(_i, p)| {
            let pname = (if f.language == "java" {
                java_parameter_parts(p).1
            } else {
                p.child_by_field_name("name")
            })
            .or_else(|| {
                child_nodes(p)
                    .into_iter()
                    .find(|c| c.kind() == "identifier")
            })
            .or_else(|| (p.kind() == "identifier").then_some(p))
            .and_then(|node| measured!(format!("header.parameters[{_i}].name"), node));
            let type_name = (if f.language == "java" {
                java_parameter_parts(p).0
            } else {
                p.child_by_field_name("type")
            })
            .map(|node| annotated(f, node))
            .and_then(|node| measured!(format!("header.parameters[{_i}].type"), node));
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
    let result_type = n
        .child_by_field_name("return_type")
        .or_else(|| {
            (f.language == "java" && key.kind == "method")
                .then(|| n.child_by_field_name("type"))
                .flatten()
        })
        .map(|node| annotated(f, node))
        .and_then(|node| measured!("header.resultType".into(), node));
    let bases = [
        "superclass",
        "interfaces",
        "super_interfaces",
        "traits",
        "trait",
    ]
    .into_iter()
    .flat_map(|field| n.child_by_field_name(field))
    .enumerate()
    .filter_map(|(_i, node)| measured!(format!("header.bases[{_i}]"), node))
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

fn callee(n: Node<'_>, f: &SourceFile) -> (Option<String>, Option<Range>) {
    let expr = n
        .child_by_field_name(match f.language.as_str() {
            "java" => "name",
            "python" => "function",
            "javascript" | "typescript" if n.kind() == "new_expression" => "constructor",
            "javascript" | "typescript" => "function",
            _ => "function",
        })
        .or_else(|| n.child_by_field_name("constructor"));
    let Some(expr) = expr else {
        return (None, None);
    };
    // The `await f<T>(x)` misparse names the awaited callee `f` (#958).
    let expr = match f.language.as_str() {
        "javascript" | "typescript" => awaited_callee(n).unwrap_or(expr),
        _ => expr,
    };
    let token_expr = if f.language == "rust" && expr.kind() == "generic_function" {
        expr.child_by_field_name("function").unwrap_or(expr)
    } else {
        expr
    };
    let member = match f.language.as_str() {
        "java" => Some(expr),
        "python" if expr.kind() == "attribute" => expr.child_by_field_name("attribute"),
        "javascript" | "typescript" if expr.kind() == "member_expression" => {
            expr.child_by_field_name("property")
        }
        "rust" if token_expr.kind() == "field_expression" => {
            token_expr.child_by_field_name("field")
        }
        "rust" if token_expr.kind() == "scoped_identifier" => {
            token_expr.child_by_field_name("name")
        }
        "rust" => Some(token_expr),
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

/// `expand_macros: false` keeps every Rust macro opaque, as before Decision 0019; only tests
/// use it, to show that expansion leaves every fact outside the expanded macros unchanged.
fn extract_document(
    a: &mut Artifact,
    f: &SourceFile,
    document: &Document,
    ids: &mut IdentityRegistry,
    header_witness: &mut dyn FnMut(&str, String, Range),
    expand_macros: bool,
) -> Result<()> {
    #[cfg(test)]
    {
        *DOCUMENT_EXTRACTIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(a.source_set.root_id.clone())
            .or_default() += 1;
    }
    language_order(&f.language)?;
    safe_path(&f.path)?;
    // Occurrence identity binds this captured document version. Authenticate its content hash
    // and extraction context against the captured revision before measuring, so a failed
    // precondition mints no occ:v2 record at all.
    ensure!(
        f.hash == hash(f.text.as_bytes()),
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
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "typescript" => crate::indexer_ts::grammar(&f.path),
        _ => anyhow::bail!("unsupported native language"),
    })?;
    let tree = parser
        .parse(&f.text, None)
        .context("native parser failed")?;
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
    // A file-level inner `#![cfg(test)]` makes the whole document test code.
    let file_test = test_path(&f.path) || local_test_marker(tree.root_node(), f);
    let mut module_range = Range::node(tree.root_node());
    if !f.text.is_empty() && module_range.start == module_range.end {
        module_range = Range {
            start: 0,
            end: f.text.len(),
        };
    }
    a.declarations.push(Declaration {
        syntax_id: module_id.clone(),
        role: declaration_role(file_test),
        document: key.clone(),
        revision_id: a.revision.id.clone(),
        kind: "module".into(),
        name: None,
        lookup_key: None,
        ancestors: vec![],
        key: module_key,
        range: module_range,
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
    let declaration_start = a.declarations.len() - 1;
    let mut state = ParserState {
        search_nodes: BTreeMap::new(),
        call_start: a.calls.len(),
        control_start: a.control_regions.len(),
        reference_start: a.references.len(),
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
        gaps: Vec::new(),
        bound_gaps: Vec::new(),
        bounded: false,
        module_only: None,
        name_owners: BTreeMap::new(),
        lexical_owners: BTreeMap::from([(canonical(&json!([])), module_id.clone())]),
        header_witness,
        expand_macros,
        expanding: false,
        macro_expansions: Vec::new(),
        measured_macros: BTreeMap::new(),
        expansion_bytes: 0,
        macro_parser: None,
        visit_ceiling: VISIT_LIMIT,
        reserved_until: 0,
        declaration_only: f.language == "typescript"
            && crate::indexer_ts::declaration_only(&f.path),
    };
    let mut macros = Vec::new();
    let skipped = crate::skipped_source::skipped(&f.language, &f.text);
    if skipped.is_none() {
        state.walk(
            tree.root_node(),
            &module_id,
            &[],
            &[],
            0,
            (file_test, false),
            &mut macros,
        )?;
    }
    // Allowlisted expression macros are measured only after every ordinary fact, so their
    // declarations take later sibling ordinals and everything outside them is unchanged.
    state.expanding = true;
    for site in macros {
        let range = Range::node(site.node);
        if state.measure_macro(site)? {
            state.macro_expansions.push(range);
        }
    }
    state.finish_bound_gaps(declaration_start);
    state
        .gaps
        .sort_by_key(|gap| (gap.range.start, gap.range.end));
    state.finish_occurrences()?;
    match skipped {
        Some(skip) => state.keep_file_module(declaration_start, skip.diagnostic()),
        None => state.keep_within_budget(declaration_start),
    }
    state
        .artifact
        .search_cards
        .extend(crate::search_capture::capture(
            tree.root_node(),
            f,
            &state.artifact.declarations[declaration_start..],
            &state.artifact.calls[state.call_start..],
            &state.search_nodes,
            &state.measured_macros,
        )?);
    state.artifact.capability_reports.push(CapabilityReport {
        document: state.document.clone(),
        revision_id: state.artifact.revision.id.clone(),
        families: MeasurementKind::ALL
            .into_iter()
            .map(|kind| MeasurementSupport {
                kind,
                available: true,
                diagnostic: None,
                gaps: state.gaps.clone(),
            })
            .collect(),
        macro_expansions: std::mem::take(&mut state.macro_expansions),
    });
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
        state: if tree.root_node().has_error()
            || state.opaque_rust
            || state.bounded
            || state.module_only.is_some()
        {
            "partial"
        } else {
            "complete"
        }
        .into(),
        supported_roles: vec!["definition".into(), "call".into()],
        observed_roles,
        diagnostic: match (
            tree.root_node().has_error(),
            state.opaque_rust,
            state.bounded,
        ) {
            _ if state.module_only.is_some() => state.module_only.map(str::to_owned),
            (true, true, false) => Some("parser recovered from invalid source; opaque Rust macro/attribute token trees skipped".into()),
            (true, false, false) => Some("parser recovered from invalid source".into()),
            (false, true, false) => Some("opaque Rust macro/attribute token trees skipped; calls and definitions inside were not measured".into()),
            (false, false, false) => None,
            (true, _, true) => Some(format!("parser recovered from invalid source; {BOUND_DIAGNOSTIC}")),
            (false, _, true) => Some(BOUND_DIAGNOSTIC.into()),
        },
    });
    Ok(())
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
struct ParserState<'a> {
    search_nodes: BTreeMap<usize, String>,
    artifact: &'a mut Artifact,
    call_start: usize,
    control_start: usize,
    reference_start: usize,
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
    gaps: Vec<MeasurementGap>,
    /// Subtrees skipped at the walk bounds, at most [`BOUND_GAPS`], and whether any was.
    bound_gaps: Vec<Range>,
    bounded: bool,
    /// Why only the document's file module was kept: its facts exceeded the stored budgets
    /// (#564), or its source was not read (#576).
    module_only: Option<&'static str>,
    name_owners: BTreeMap<(usize, usize), String>,
    lexical_owners: BTreeMap<Vec<u8>, String>,
    header_witness: &'a mut dyn FnMut(&str, String, Range),
    expand_macros: bool,
    /// Set once the ordinary walk is done: allowlisted macros then expand where found.
    expanding: bool,
    macro_expansions: Vec<Range>,
    /// Every measured macro's re-parse, nested ones included, by invocation byte range. Kept
    /// for the search capture, which reads the declarations recorded from these trees.
    measured_macros: crate::search_capture::MeasuredMacros,
    expansion_bytes: usize,
    macro_parser: Option<tree_sitter::Parser>,
    /// Visit ceiling for the expansion being measured, and the visit count its reservation
    /// runs to. A nested expansion must leave room for the rest of its enclosing one.
    visit_ceiling: usize,
    reserved_until: usize,
    /// A `.d.ts`-style document: no calls, no control regions, and function bodies are
    /// skipped without a gap (Q2, Q7).
    declaration_only: bool,
}
/// Per-document walk bounds. The walk skips what lies past them and leaves a gap (#554). A
/// macro expansion measures completely or not at all, so it reserves its extent first and
/// stays opaque rather than cross them.
const VISIT_LIMIT: usize = 1_000_000;
const DEPTH_LIMIT: usize = 128;
/// A pathological file can have a skipped subtree per node: past this many it is partial
/// without a further range.
const BOUND_GAPS: usize = 1024;
const BOUND_DIAGNOSTIC: &str =
    "syntax beyond native walk bounds skipped; calls and definitions inside were not measured";
/// An allowlisted Rust expression macro, measured after the ordinary walk in its own context.
struct MacroSite<'t> {
    node: Node<'t>,
    form: crate::native_rust_macros::MacroForm,
    owner: String,
    ancestry: Vec<Key>,
    regions: Vec<String>,
    depth: usize,
    test: bool,
}
/// Whether `range` lies inside one of a document's sorted, disjoint macro expansions. Facts
/// there order after the owner's other facts (Decision 0019), so the others keep their ordinals.
fn in_macro_expansion(expansions: &[Range], range: &Range) -> bool {
    let i = expansions.partition_point(|e| e.start <= range.start);
    i > 0 && range.end <= expansions[i - 1].end
}
impl ParserState<'_> {
    fn finish_occurrences(&mut self) -> Result<()> {
        let mut replacements = BTreeMap::new();
        let mut by_owner: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (offset, region) in self.artifact.control_regions[self.control_start..]
            .iter()
            .enumerate()
        {
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
            indexes.sort_by_key(|i| {
                let r = &self.artifact.control_regions[*i].range;
                (
                    in_macro_expansion(&self.macro_expansions, r),
                    r.start,
                    r.end,
                )
            });
            for (ordinal, i) in indexes.iter().enumerate() {
                let input = occurrence_input(
                    &self.file.hash,
                    &self.extraction_context,
                    &self.artifact.producer,
                    &self.artifact.control_regions[*i].owner_syntax_id,
                    "control",
                    ordinal,
                );
                let id = self.ids.occurrence_ref(&input)?;
                let region = &mut self.artifact.control_regions[*i];
                replacements.insert(std::mem::replace(&mut region.id, id.clone()), id);
                region.ordinal = ordinal;
            }
        }
        for region in &mut self.artifact.control_regions[self.control_start..] {
            if let Some(parent) = &mut region.parent_id {
                *parent = replacements
                    .get(parent)
                    .context("native region parent not measured")?
                    .clone();
            }
        }
        let mut by_owner: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (offset, call) in self.artifact.calls[self.call_start..].iter().enumerate() {
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
            indexes.sort_by_key(|i| {
                let r = &self.artifact.calls[*i].range;
                (
                    in_macro_expansion(&self.macro_expansions, r),
                    r.start,
                    r.end,
                )
            });
            for (ordinal, i) in indexes.iter().enumerate() {
                let input = occurrence_input(
                    &self.file.hash,
                    &self.extraction_context,
                    &self.artifact.producer,
                    &self.artifact.calls[*i].owner_syntax_id,
                    "call",
                    ordinal,
                );
                let id = self.ids.occurrence_ref(&input)?;
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
        let mut by_owner: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (offset, reference) in self.artifact.references[self.reference_start..]
            .iter()
            .enumerate()
        {
            by_owner
                .entry(reference.owner_syntax_id.clone())
                .or_default()
                .push(self.reference_start + offset);
        }
        for indexes in by_owner.values_mut() {
            indexes.sort_by_key(|i| {
                let r = &self.artifact.references[*i].range;
                (
                    in_macro_expansion(&self.macro_expansions, r),
                    r.start,
                    r.end,
                )
            });
            for (ordinal, i) in indexes.iter().enumerate() {
                let input = occurrence_input(
                    &self.file.hash,
                    &self.extraction_context,
                    &self.artifact.producer,
                    &self.artifact.references[*i].owner_syntax_id,
                    "reference",
                    ordinal,
                );
                let id = self.ids.occurrence_ref(&input)?;
                let reference = &mut self.artifact.references[*i];
                reference.ordinal = ordinal;
                reference.id = id;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)] // The walk's lexical context plus its macro queue.
    fn walk<'t>(
        &mut self,
        n: Node<'t>,
        owner: &str,
        ancestors: &[Key],
        regions: &[String],
        depth: usize,
        test_flags: (bool, bool),
        macros: &mut Vec<MacroSite<'t>>,
    ) -> Result<()> {
        if depth >= DEPTH_LIMIT || self.visits >= VISIT_LIMIT {
            ensure!(!self.expanding, "native expansion exceeded its reservation");
            self.bounded = true;
            if self.bound_gaps.len() < BOUND_GAPS && n.start_byte() < n.end_byte() {
                self.bound_gaps.push(Range::node(n));
            }
            return Ok(());
        }
        if self.declaration_only && crate::indexer_ts::is_executable_body(n) {
            return Ok(());
        }
        self.visits += 1;
        // Rust token trees and attributes are opaque source syntax: a call-shaped token
        // inside one is not a measured call. Only allowlisted expression macros are
        // measured, after the ordinary walk (Decision 0019). Still count this subtree root
        // against the bounded visit budget.
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
            if self.expand_macros
                && n.kind() == "macro_invocation"
                && let Some(form) = crate::native_rust_macros::expression_macro(n, &self.file.text)
            {
                let site = MacroSite {
                    node: n,
                    form,
                    owner: owner.to_owned(),
                    ancestry: ancestors.to_vec(),
                    regions: regions.to_vec(),
                    depth,
                    test: test_flags.0 || test_flags.1,
                };
                if self.expanding {
                    self.measure_macro(site)?;
                } else {
                    macros.push(site);
                }
                return Ok(());
            }
            self.opaque_gap(
                n,
                format!(
                    "opaque Rust {} skipped; contents require macro/attribute interpretation",
                    n.kind()
                ),
            );
            return Ok(());
        }
        if self.file.language == "rust" && matches!(n.kind(), "lifetime" | "label" | "metavariable")
        {
            return Ok(());
        }
        let test_context = test_flags.0 || test_flags.1 || local_test_marker(n, self.file);
        let mut owner = owner.to_owned();
        // Most nodes declare nothing, and a key can carry a whole Java signature.
        let mut ancestry = std::borrow::Cow::Borrowed(ancestors);
        let mut regions = regions.to_vec();
        if let Some(k) = kind(&self.file.language, n, &self.file.text)
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
            if let Some((_, range)) = &named {
                let parent = self
                    .lexical_owners
                    .get(&canonical(&json!(ancestry)))
                    .context("native reference declaration parent missing")?;
                self.name_owners
                    .insert((range.start, range.end), parent.clone());
            }
            let mut lexical_ancestry = ancestry.to_vec();
            lexical_ancestry.push(key.clone());
            self.lexical_owners
                .insert(canonical(&json!(lexical_ancestry)), id.clone());
            #[cfg(feature = "test-support")]
            if key.signature.is_some() {
                for (i, parameter) in n
                    .child_by_field_name("parameters")
                    .map(child_nodes)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|p| p.kind().contains("parameter"))
                    .filter_map(|p| java_parameter_parts(p).0)
                    .enumerate()
                {
                    (self.header_witness)(
                        &id,
                        format!("signature.parameterTypes[{i}]"),
                        Range::node(parameter),
                    );
                }
            }
            self.saw_definition |= k != "module";
            // Search capture walks the same trees by node ID: measured macros' stay alive.
            self.search_nodes.insert(n.id(), id.clone());
            self.artifact.declarations.push(Declaration {
                syntax_id: id.clone(),
                role: declaration_role(test_context),
                document: self.document.clone(),
                revision_id: self.artifact.revision.id.clone(),
                kind: k.into(),
                name: named.as_ref().map(|v| v.0.clone()),
                lookup_key: named
                    .as_ref()
                    .map(|v| declaration_lookup(&self.file.language, &v.0))
                    .transpose()?,
                ancestors: ancestry.to_vec(),
                key: key.clone(),
                range: Range::node(n),
                name_range: named.map(|v| v.1),
                header: header_with_witnesses(n, self.file, &key, &mut |field, range| {
                    (self.header_witness)(&id, field, range);
                }),
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
            ancestry.to_mut().push(key);
        }
        if !self.declaration_only && is_region(n) {
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
        if !self.declaration_only && is_call(&self.file.language, n) {
            let count = self.call_ordinals.entry(owner.clone()).or_default();
            let ordinal = *count;
            *count += 1;
            self.saw_call = true;
            let range = match self.file.language.as_str() {
                "javascript" | "typescript" => js_call_range(n),
                _ => Range::node(n),
            };
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
        let reference_kind = match self.file.language.as_str() {
            "java" => matches!(n.kind(), "identifier" | "type_identifier"),
            "rust" => matches!(
                n.kind(),
                "identifier" | "type_identifier" | "field_identifier"
            ),
            "python" => n.kind() == "identifier",
            "javascript" => matches!(
                n.kind(),
                "identifier"
                    | "property_identifier"
                    | "shorthand_property_identifier"
                    | "shorthand_property_identifier_pattern"
            ),
            "typescript" => matches!(
                n.kind(),
                "identifier"
                    | "property_identifier"
                    | "shorthand_property_identifier"
                    | "shorthand_property_identifier_pattern"
                    | "type_identifier"
            ),
            _ => false,
        };
        if reference_kind
            && n.named_child_count() == 0
            && let Some(spelling) = self.file.text.get(n.byte_range())
            && let Ok(lookup_key) = lookup(&self.file.language, spelling)
        {
            let range = Range::node(n);
            let reference_owner = self
                .name_owners
                .get(&(range.start, range.end))
                .unwrap_or(&owner)
                .clone();
            if self.seen.insert((
                reference_owner.clone(),
                "reference".into(),
                range.start,
                range.end,
            )) {
                self.artifact.references.push(Reference {
                    id: String::new(),
                    owner_syntax_id: reference_owner,
                    ordinal: 0,
                    document: self.document.clone(),
                    revision_id: self.artifact.revision.id.clone(),
                    range,
                    spelling: spelling.to_owned(),
                    lookup_key,
                    provenance_id: self.proof_id.clone(),
                });
            }
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
                    macros,
                )?;
                continue;
            }
            if self.file.language == "rust"
                && matches!(child.kind(), "line_comment" | "block_comment")
            {
                self.walk(
                    child,
                    &owner,
                    &ancestry,
                    &regions,
                    depth + 1,
                    (test_context, false),
                    macros,
                )?;
                continue;
            }
            self.walk(
                child,
                &owner,
                &ancestry,
                &regions,
                depth + 1,
                (test_context, marked),
                macros,
            )?;
            marked = false;
        }
        Ok(())
    }

    /// Publish the skipped subtrees as gaps. An enclosing node records its name, callee and
    /// invocation before walking its children, so a skipped child can span one of those
    /// measured candidates; a gap must not claim a measured candidate, so that child is not
    /// one. The document stays partial either way.
    fn finish_bound_gaps(&mut self, declaration_start: usize) {
        if self.bound_gaps.is_empty() {
            return;
        }
        let artifact = &*self.artifact;
        let mut candidates: Vec<_> = artifact.declarations[declaration_start..]
            .iter()
            .filter_map(|d| d.name_range.as_ref())
            .chain(
                artifact.calls[self.call_start..]
                    .iter()
                    .flat_map(|c| std::iter::once(&c.range).chain(c.callee_range.as_ref())),
            )
            .chain(
                artifact.references[self.reference_start..]
                    .iter()
                    .map(|r| &r.range),
            )
            .map(|range| (range.start, range.end))
            .collect();
        candidates.sort_unstable();
        for range in std::mem::take(&mut self.bound_gaps) {
            let first = candidates.partition_point(|&(start, _)| start < range.start);
            let claimed = candidates[first..]
                .iter()
                .take_while(|&&(start, _)| start < range.end)
                .any(|&(_, end)| end <= range.end);
            if !claimed {
                self.gaps.push(MeasurementGap {
                    range,
                    diagnostic: format!("{BOUND_DIAGNOSTIC}; nesting exceeds {DEPTH_LIMIT} levels or {VISIT_LIMIT} visits"),
                });
            }
        }
    }

    /// One document past the stored budgets publishes as a coverage gap, whatever its
    /// language or shape of evidence (#564). What reads can't admit must not be published:
    /// nothing is left to fail a read, or to send the index back to reconciliation.
    fn keep_within_budget(&mut self, declaration_start: usize) {
        let artifact = &*self.artifact;
        let facts = crate::evidence_budget::Facts {
            declarations: &artifact.declarations[declaration_start..],
            calls: &artifact.calls[self.call_start..],
            control_regions: &artifact.control_regions[self.control_start..],
            references: &artifact.references[self.reference_start..],
            gaps: &self.gaps,
            macro_expansions: self.macro_expansions.len(),
        };
        if crate::evidence_budget::exceeded(self.file, &artifact.source_set.id, facts) {
            self.keep_file_module(declaration_start, crate::evidence_budget::DIAGNOSTIC);
        }
    }

    /// Keep only the file module, with a whole-file gap that says why.
    fn keep_file_module(&mut self, declaration_start: usize, diagnostic: &'static str) {
        self.module_only = Some(diagnostic);
        self.artifact.declarations.truncate(declaration_start + 1);
        self.artifact.calls.truncate(self.call_start);
        self.artifact.control_regions.truncate(self.control_start);
        self.artifact.references.truncate(self.reference_start);
        self.macro_expansions.clear();
        self.measured_macros.clear();
        self.search_nodes.clear();
        self.saw_definition = false;
        self.saw_call = false;
        self.gaps = vec![MeasurementGap {
            range: Range {
                start: 0,
                end: self.file.text.len(),
            },
            diagnostic: diagnostic.into(),
        }];
    }

    fn opaque_gap(&mut self, n: Node<'_>, diagnostic: String) {
        self.opaque_rust = true;
        self.gaps.push(MeasurementGap {
            range: Range::node(n),
            diagnostic,
        });
    }

    /// Measure one allowlisted expression macro in its recorded context (Decision 0019).
    /// Returns whether it was measured; otherwise the whole invocation is an opaque gap, as
    /// for any other macro. The parse is bounded by a per-document byte budget, and the
    /// measured extent is reserved against the walk's visit and depth bounds before any fact
    /// is recorded, so an expansion either measures completely or not at all.
    fn measure_macro(&mut self, site: MacroSite<'_>) -> Result<bool> {
        use crate::native_rust_macros as macros;
        let file = self.file;
        if self.macro_parser.is_none() {
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
            self.macro_parser = Some(parser);
        }
        let Some(tree) = macros::token_tree(site.node).and_then(|tt| {
            let (start, end) = (tt.start_byte(), tt.end_byte());
            if self.expansion_bytes.saturating_add(end - start)
                > macros::byte_budget(file.text.len())
            {
                return None;
            }
            self.expansion_bytes += end - start;
            let parser = self.macro_parser.as_mut()?;
            // Measurement reads byte ranges only, so it needs no line table.
            let position = |byte| tree_sitter::Point::new(0, byte);
            macros::parse(
                parser,
                file.text.as_bytes(),
                site.form,
                start,
                end,
                &position,
                None,
            )
            .map(|tree| (tree, start, end))
        }) else {
            self.unmeasured_macro(site.node);
            return Ok(false);
        };
        let (tree, start, end) = tree;
        let Some(arguments) = macros::arguments(&tree, site.form, start, end) else {
            self.unmeasured_macro(site.node);
            return Ok(false);
        };
        // Reserve the whole expansion before recording anything. A nested expansion's own
        // parse can hold more nodes than the tokens its enclosing reservation counted, so it
        // must also leave the rest of that reservation available. The path, `!` and wrapper
        // add a few visits beyond the argument subtree.
        let ceiling = self
            .visit_ceiling
            .saturating_sub(self.reserved_until.saturating_sub(self.visits));
        let available = ceiling.saturating_sub(self.visits).saturating_sub(16);
        let Some((nodes, height)) = macros::extent(arguments, available) else {
            self.unmeasured_macro(site.node);
            return Ok(false);
        };
        if site.depth + 2 + height >= DEPTH_LIMIT {
            self.unmeasured_macro(site.node);
            return Ok(false);
        }
        let enclosing = (self.visit_ceiling, self.reserved_until);
        self.visit_ceiling = ceiling;
        self.reserved_until = self.visits + nodes + 16;
        let measured = self.measure_arguments(&site, arguments);
        (self.visit_ceiling, self.reserved_until) = enclosing;
        measured?;
        self.measured_macros.insert(
            (site.node.start_byte(), site.node.end_byte()),
            macros::Expansion::new(tree, site.form, start, end),
        );
        Ok(true)
    }

    fn measure_arguments(&mut self, site: &MacroSite<'_>, arguments: Node<'_>) -> Result<()> {
        use crate::native_rust_macros as macros;
        let file = self.file;
        let flags = (site.test, false);
        let (owner, ancestry, regions) = (&site.owner, &site.ancestry, &site.regions);
        if let Some(path) = site.node.child_by_field_name("macro") {
            self.walk(
                path,
                owner,
                ancestry,
                regions,
                site.depth + 1,
                flags,
                &mut Vec::new(),
            )?;
        }
        let format = match site.form {
            macros::MacroForm::Arguments { format } => format,
            macros::MacroForm::Array => None,
        };
        let mut index = 0;
        let mut cursor = arguments.walk();
        for child in arguments.named_children(&mut cursor) {
            let expression = !matches!(
                child.kind(),
                "line_comment" | "block_comment" | "attribute_item"
            );
            let position = index;
            index += usize::from(expression);
            if expression
                && Some(position) == format
                && matches!(child.kind(), "string_literal" | "raw_string_literal")
            {
                // The format string stays opaque; it is a gap only where it names arguments.
                if file.text.get(child.byte_range()).is_some_and(|literal| {
                    macros::names_arguments(literal, child.kind() == "raw_string_literal")
                }) {
                    self.opaque_gap(
                        child,
                        "opaque Rust format string skipped; implicit captures and named arguments require format interpretation".into(),
                    );
                }
                continue;
            }
            if expression
                && format.is_some_and(|format| position > format)
                && child.kind() == "assignment_expression"
                && let Some(name) = child.child_by_field_name("left")
                && name.kind() == "identifier"
                && let Some(value) = child.child_by_field_name("right")
            {
                // `name = value` names a format argument; only the value is an expression.
                self.opaque_gap(
                    name,
                    "opaque Rust named format argument skipped; argument names require format interpretation".into(),
                );
                self.walk(
                    value,
                    owner,
                    ancestry,
                    regions,
                    site.depth + 3,
                    flags,
                    &mut Vec::new(),
                )?;
                continue;
            }
            self.walk(
                child,
                owner,
                ancestry,
                regions,
                site.depth + 2,
                flags,
                &mut Vec::new(),
            )?;
        }
        Ok(())
    }

    fn unmeasured_macro(&mut self, n: Node<'_>) {
        self.opaque_gap(
            n,
            "opaque Rust macro_invocation skipped; its token tree is not measurable as Rust expressions within native bounds".into(),
        );
    }
}

#[cfg(test)]
#[path = "native_rust_macros_tests.rs"]
mod macro_expansion_tests;

impl Artifact {
    /// Validates an assembled revision against the full current capture header
    /// and checks all resulting IDs, ownership and structure without re-extraction.
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
        capture: &Capture,
        root: &Path,
        root_id: &str,
        cancel: &CancelFlag,
    ) -> Result<()> {
        self.validate_selective(capture, root, root_id, cancel)
    }
    /// The structural checks, one section at a time. The sections run in this
    /// order and each stops at its first failure, so the order decides which
    /// error an artifact with several faults reports.
    fn validate_structure(&self, files: &[SourceFile]) -> Result<()> {
        self.check_header(files)?;
        let docs = self.index_documents(files)?;
        self.check_coverage(&docs)?;
        let proofs = self.index_proofs(&docs)?;
        // The pure part of every ID (input checks, canonical bytes, digest) is
        // minted on workers, a chunk at a time. Each row's checks still run
        // here in order, and the minted ID is admitted at the point where it
        // was computed before, so errors and their order are unchanged. A
        // one-document view is already running on a document worker, so it
        // mints inline instead of starting threads of its own.
        let workers = if files.len() == 1 {
            1
        } else {
            crate::parallel::default_workers()
        };
        let declarations = self.check_declarations(&docs, &proofs, workers)?;
        self.check_ordinal_sections()?;
        let check = StructureCheck {
            artifact: self,
            docs: &docs,
            proofs: &proofs,
            declarations: &declarations,
            contexts: self.extraction_contexts(&docs),
            workers,
        };
        let mut occurrence = IdentityRegistry::default();
        check.check_calls(&mut occurrence)?;
        check.check_references(&mut occurrence)?;
        check.check_capability_reports(&self.measured_candidates())?;
        let regions = check.check_control_regions(&mut occurrence)?;
        self.check_region_parents(&regions)?;
        self.check_region_links(&regions)
    }
    /// Section 1: producer, source set, revision and the per-document tuple counts.
    fn check_header(&self, files: &[SourceFile]) -> Result<()> {
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
                && self.provenance.len() == files.len()
                && self.capability_reports.len() == files.len(),
            "native document tuple cardinality"
        );
        Ok(())
    }
    /// Section 2: every revision document, sorted and matching its captured file.
    fn index_documents<'a>(&self, files: &'a [SourceFile]) -> Result<Docs<'a>> {
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
            docs.insert(d.key.clone(), *file);
        }
        Ok(docs)
    }
    /// Section 3: one coverage row per document.
    fn check_coverage(&self, docs: &Docs<'_>) -> Result<()> {
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
        Ok(())
    }
    /// Section 4: one provenance proof per document, with its digest.
    fn index_proofs(&self, docs: &Docs<'_>) -> Result<Proofs<'_>> {
        let mut proofs = BTreeMap::new();
        for proof in &self.provenance {
            let f = docs
                .get(&proof.document)
                .context("native proof dangling document")?;
            ensure!(
                proofs.insert(proof.id.as_str(), &proof.document).is_none()
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
        Ok(proofs)
    }
    /// Section 5: every declaration row, then its stable ID.
    fn check_declarations(
        &self,
        docs: &Docs<'_>,
        proofs: &Proofs<'_>,
        workers: usize,
    ) -> Result<BTreeMap<String, &Declaration>> {
        let never = std::sync::atomic::AtomicBool::new(false);
        let stable_input = |d: &Declaration| {
            crate::native_ids::mint_stable(
                &json!({"sourceSet":self.source_set.id,"path":d.document.path,"language":d.document.language,"ancestors":d.ancestors,"declaration":d.key}),
            )
        };
        let mut declarations = BTreeMap::new();
        let mut identity = IdentityRegistry::default();
        for chunk in self.declarations.chunks(MINT_CHUNK) {
            let mut minted =
                crate::parallel::map_ordered(chunk, workers, &never, |_, d| stable_input(d))
                    .into_iter();
            for d in chunk {
                let file = docs
                    .get(&d.document)
                    .context("native declaration dangling document")?;
                ensure!(
                    d.revision_id == self.revision.id
                        && proofs.get(d.provenance_id.as_str()) == Some(&&d.document)
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
                        d.lookup_key.as_deref()
                            == Some(declaration_lookup(&d.document.language, n)?.as_str()),
                        "native lookup normalization mismatch"
                    );
                }
                if d.document.language != "java"
                    || !matches!(d.kind.as_str(), "method" | "constructor")
                {
                    ensure!(d.key.signature.is_none(), "unexpected native signature");
                }
                if d.kind == "module" {
                    ensure!(
                        d.name.is_none() && d.ancestors.is_empty(),
                        "invalid native module"
                    );
                }
                let expected =
                    identity.admit(minted.next().flatten().unwrap_or_else(|| stable_input(d))?)?;
                ensure!(
                    d.syntax_id == expected
                        && declarations.insert(d.syntax_id.clone(), d).is_none(),
                    "native stable ID collision/duplicate"
                );
            }
        }
        Ok(declarations)
    }
    /// Sections 6 to 9: sibling, call, reference and control-region ordinals, in that order.
    fn check_ordinal_sections(&self) -> Result<()> {
        // Facts inside a document's measured macro expansions take the owner's later ordinals.
        let expansions: BTreeMap<&DocumentKey, &[Range]> = self
            .capability_reports
            .iter()
            .map(|report| (&report.document, report.macro_expansions.as_slice()))
            .collect();
        let late = |document: &DocumentKey, range: &Range| {
            expansions
                .get(document)
                .is_some_and(|expansions| in_macro_expansion(expansions, range))
        };
        check_ordinals(
            &self.declarations,
            |d| {
                canonical(
                    &json!({"document":d.document,"ancestors":d.ancestors,"kind":d.kind,"name":d.name,"signature":d.key.signature}),
                )
            },
            late,
            |d| (&d.document, &d.range, d.key.ordinal),
            "native sibling ordinal mismatch",
            "duplicate sibling range",
        )?;
        check_ordinals(
            &self.calls,
            |c| (c.document.clone(), c.owner_syntax_id.clone()),
            late,
            |c| (&c.document, &c.range, c.ordinal),
            "native call ordinal mismatch",
            "duplicate call range",
        )?;
        check_ordinals(
            &self.references,
            |r| (&r.document, r.owner_syntax_id.as_str()),
            late,
            |r| (&r.document, &r.range, r.ordinal),
            "native reference ordinal mismatch",
            "duplicate reference range",
        )?;
        check_ordinals(
            &self.control_regions,
            |r| (r.document.clone(), r.owner_syntax_id.clone()),
            late,
            |r| (&r.document, &r.range, r.ordinal),
            "native control ordinal mismatch",
            "duplicate control range",
        )
    }
    // Keep authentication results local to this producer/revision validation.
    // Failures are surfaced at the existing occurrence-mint boundary.
    fn extraction_contexts<'a>(
        &self,
        docs: &'a Docs<'_>,
    ) -> BTreeMap<&'a DocumentKey, Result<String>> {
        docs.keys()
            .map(|document| {
                (
                    document,
                    native_extraction_context(&self.producer, &document.language, &self.revision),
                )
            })
            .collect()
    }
    /// The ranges each capability family measured, per document, sorted for
    /// the gap check of section 12.
    fn measured_candidates(&self) -> BTreeMap<(DocumentKey, MeasurementKind), Vec<&Range>> {
        let mut candidates: BTreeMap<(DocumentKey, MeasurementKind), Vec<&Range>> = BTreeMap::new();
        for d in &self.declarations {
            if let Some(range) = &d.name_range {
                candidates
                    .entry((d.document.clone(), MeasurementKind::DeclarationName))
                    .or_default()
                    .push(range);
            }
        }
        for c in &self.calls {
            candidates
                .entry((c.document.clone(), MeasurementKind::Invocation))
                .or_default()
                .push(&c.range);
            if let Some(range) = &c.callee_range {
                candidates
                    .entry((c.document.clone(), MeasurementKind::Callee))
                    .or_default()
                    .push(range);
            }
        }
        for r in &self.references {
            candidates
                .entry((r.document.clone(), MeasurementKind::Reference))
                .or_default()
                .push(&r.range);
        }
        for ranges in candidates.values_mut() {
            ranges.sort_by_key(|range| (range.start, range.end));
        }
        candidates
    }
    /// Section 14: a control region's parent has the same owner and encloses it.
    fn check_region_parents(&self, regions: &BTreeMap<String, &ControlRegion>) -> Result<()> {
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
        Ok(())
    }
    /// Section 15: a call's regions have the same owner and enclose it.
    fn check_region_links(&self, regions: &BTreeMap<String, &ControlRegion>) -> Result<()> {
        for c in &self.calls {
            let mut seen_regions = BTreeSet::new();
            for id in &c.region_ids {
                ensure!(seen_regions.insert(id), "duplicate native call region");
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

type Docs<'a> = BTreeMap<DocumentKey, &'a SourceFile>;
type Proofs<'a> = BTreeMap<&'a str, &'a DocumentKey>;

/// One ordinal section of `validate_structure`. Groups `facts` by `group_of`
/// and visits the groups in key order. Each group is ordered by
/// `(late, start, end)` of `place`'s document and range; its facts must then
/// carry the ordinals 0, 1, 2, … and no two neighbours the same range. Per
/// fact the ordinal is checked before the range, and the first failure ends
/// the section.
fn check_ordinals<'a, T, K: Ord>(
    facts: &'a [T],
    group_of: impl Fn(&'a T) -> K,
    late: impl Fn(&DocumentKey, &Range) -> bool,
    place: impl Fn(&T) -> (&DocumentKey, &Range, usize),
    mismatch: &'static str,
    duplicate: &'static str,
) -> Result<()> {
    let mut groups: BTreeMap<K, Vec<&T>> = BTreeMap::new();
    for fact in facts {
        groups.entry(group_of(fact)).or_default().push(fact);
    }
    for group in groups.values_mut() {
        group.sort_by_key(|fact| {
            let (document, range, _) = place(fact);
            (late(document, range), range.start, range.end)
        });
        for (ordinal, fact) in group.iter().enumerate() {
            let (_, range, own) = place(fact);
            ensure!(own == ordinal, mismatch);
            if ordinal != 0 {
                ensure!(place(group[ordinal - 1]).1 != range, duplicate);
            }
        }
    }
    Ok(())
}

/// What the fact sections of `validate_structure` share: the artifact and the
/// indexes the earlier sections built from it.
struct StructureCheck<'a> {
    artifact: &'a Artifact,
    docs: &'a Docs<'a>,
    proofs: &'a Proofs<'a>,
    declarations: &'a BTreeMap<String, &'a Declaration>,
    contexts: BTreeMap<&'a DocumentKey, Result<String>>,
    workers: usize,
}

impl<'a> StructureCheck<'a> {
    fn occurrence_of(
        &self,
        document: &DocumentKey,
        owner: &str,
        kind: &str,
        ordinal: usize,
    ) -> Result<crate::native_ids::Minted> {
        let Self {
            artifact,
            docs,
            contexts,
            ..
        } = self;
        let file = docs
            .get(document)
            .context("native occurrence dangling document")?;
        let context = contexts
            .get(document)
            .context("native occurrence dangling context")?
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error:#}"))?;
        crate::native_ids::mint_occurrence_ref(&occurrence_input(
            &file.hash,
            context,
            &artifact.producer,
            owner,
            kind,
            ordinal,
        ))
    }
    fn call_occurrence(&self, c: &Call) -> Result<crate::native_ids::Minted> {
        self.occurrence_of(&c.document, &c.owner_syntax_id, "call", c.ordinal)
    }
    fn region_occurrence(&self, r: &ControlRegion) -> Result<crate::native_ids::Minted> {
        self.occurrence_of(&r.document, &r.owner_syntax_id, "control", r.ordinal)
    }
    /// Section 10: every call row, then its occurrence ID.
    fn check_calls(&self, occurrence: &mut IdentityRegistry) -> Result<()> {
        let Self {
            artifact,
            docs,
            proofs,
            declarations,
            workers,
            ..
        } = *self;
        let never = std::sync::atomic::AtomicBool::new(false);
        let mut calls = BTreeSet::new();
        for chunk in artifact.calls.chunks(MINT_CHUNK) {
            let mut minted = crate::parallel::map_ordered(chunk, workers, &never, |_, c| {
                self.call_occurrence(c)
            })
            .into_iter();
            for c in chunk {
                let file = docs
                    .get(&c.document)
                    .context("native call dangling document")?;
                let owner = declarations
                    .get(&c.owner_syntax_id)
                    .context("native call dangling owner")?;
                ensure!(
                    owner.document == c.document
                        && c.revision_id == artifact.revision.id
                        && proofs.get(c.provenance_id.as_str()) == Some(&&c.document)
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
                let minted = minted
                    .next()
                    .flatten()
                    .unwrap_or_else(|| self.call_occurrence(c))?;
                ensure!(
                    c.id == occurrence.admit(minted)?,
                    "native call occurrence mismatch"
                );
            }
        }
        Ok(())
    }
    /// Section 11: every reference row, then its occurrence ID.
    fn check_references(&self, occurrence: &mut IdentityRegistry) -> Result<()> {
        let Self {
            artifact,
            docs,
            proofs,
            declarations,
            ..
        } = *self;
        let mut references: BTreeSet<&str> = BTreeSet::new();
        for r in &artifact.references {
            let file = docs
                .get(&r.document)
                .context("native reference dangling document")?;
            let owner = declarations
                .get(&r.owner_syntax_id)
                .context("native reference dangling owner")?;
            ensure!(
                owner.document == r.document
                    && r.revision_id == artifact.revision.id
                    && proofs.get(r.provenance_id.as_str()) == Some(&&r.document)
                    && references.insert(r.id.as_str()),
                "native reference ownership/duplicate"
            );
            span(&file.text, &r.range, false)?;
            ensure!(
                file.text.get(r.range.start..r.range.end) == Some(r.spelling.as_str())
                    && r.lookup_key == lookup(&r.document.language, &r.spelling)?,
                "native reference not source-measured"
            );
            ensure!(
                r.id == occurrence.admit(self.occurrence_of(
                    &r.document,
                    &r.owner_syntax_id,
                    "reference",
                    r.ordinal
                )?)?,
                "native reference occurrence mismatch"
            );
        }
        Ok(())
    }
    /// Section 12: one capability report per document: macro expansions,
    /// families and their gaps.
    fn check_capability_reports(
        &self,
        candidates: &BTreeMap<(DocumentKey, MeasurementKind), Vec<&Range>>,
    ) -> Result<()> {
        let Self { artifact, docs, .. } = *self;
        let mut reports = BTreeSet::new();
        for report in &artifact.capability_reports {
            let file = docs
                .get(&report.document)
                .context("native capability dangling document")?;
            ensure!(
                reports.insert(report.document.clone())
                    && report.revision_id == artifact.revision.id,
                "native capability document/revision mismatch"
            );
            let mut previous_end = None;
            for range in &report.macro_expansions {
                span(&file.text, range, false)?;
                ensure!(
                    report.document.language == "rust"
                        && previous_end.is_none_or(|end| end <= range.start),
                    "invalid native macro expansions"
                );
                previous_end = Some(range.end);
            }
            let mut families = BTreeSet::new();
            for family in &report.families {
                ensure!(
                    families.insert(family.kind) && family.available && family.diagnostic.is_none(),
                    "invalid native capability convention"
                );
                let mut last = None;
                for gap in &family.gaps {
                    span(&file.text, &gap.range, false)?;
                    text(&gap.diagnostic)?;
                    let range_key = (gap.range.start, gap.range.end);
                    ensure!(
                        last.is_none_or(|previous| previous < range_key),
                        "duplicate or unsorted native capability gaps"
                    );
                    last = Some(range_key);
                    let ranges = candidates
                        .get(&(report.document.clone(), family.kind))
                        .map(Vec::as_slice)
                        .unwrap_or_default();
                    let first = ranges.partition_point(|range| range.start < gap.range.start);
                    let contradiction = ranges[first..]
                        .iter()
                        .take_while(|range| range.start < gap.range.end)
                        .any(|range| range.end <= gap.range.end);
                    ensure!(
                        !contradiction,
                        "native capability gap contradicts measured candidate"
                    );
                }
            }
            ensure!(
                families == MeasurementKind::ALL.into_iter().collect(),
                "native capability family cardinality"
            );
        }
        Ok(())
    }
    /// Section 13: every control-region row, then its occurrence ID.
    fn check_control_regions(
        &self,
        occurrence: &mut IdentityRegistry,
    ) -> Result<BTreeMap<String, &'a ControlRegion>> {
        let Self {
            artifact,
            docs,
            proofs,
            declarations,
            workers,
            ..
        } = *self;
        let never = std::sync::atomic::AtomicBool::new(false);
        let mut regions = BTreeMap::new();
        for chunk in artifact.control_regions.chunks(MINT_CHUNK) {
            let mut minted = crate::parallel::map_ordered(chunk, workers, &never, |_, r| {
                self.region_occurrence(r)
            })
            .into_iter();
            for region in chunk {
                let file = docs
                    .get(&region.document)
                    .context("native control dangling document")?;
                let owner = declarations
                    .get(&region.owner_syntax_id)
                    .context("native control dangling owner")?;
                ensure!(
                    owner.document == region.document
                        && region.revision_id == artifact.revision.id
                        && proofs.get(region.provenance_id.as_str()) == Some(&&region.document),
                    "native control ownership"
                );
                span(&file.text, &region.range, false)?;
                let minted = minted
                    .next()
                    .flatten()
                    .unwrap_or_else(|| self.region_occurrence(region))?;
                ensure!(
                    region.id == occurrence.admit(minted)?
                        && regions.insert(region.id.clone(), region).is_none(),
                    "native control occurrence mismatch"
                );
            }
        }
        Ok(regions)
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
