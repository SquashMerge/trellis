//! The persisted native evidence records and their leaf vocabulary.
//!
//! Plain data, the producer constants, the pure lookup-key functions, the declared
//! extraction-input inventory with the extraction contexts, and the document witness:
//! extraction, validation (`impl Artifact`), revision headers and the spool stay in
//! `native_evidence`, which re-exports the record names.

use crate::native_ids::{canonical, digest, extraction_context};
use crate::search_card_records::CapturedCard;
use anyhow::{Context, Result, ensure};
use icu_normalizer::ComposingNormalizerBorrowed;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

fn required_nullable<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

pub(crate) const LANGUAGES: [&str; 5] = ["java", "rust", "python", "javascript", "typescript"];
pub(crate) const PRODUCER: &str = "trellis.native.syntax";
/// Native producer descriptor version. Any change that can alter a native measured field or
/// projection, or that starts reading another input, must change it (Decision 0003).
pub(crate) const NATIVE_VERSION: &str = "native-v20";

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
pub struct Reference {
    pub id: String,
    pub owner_syntax_id: String,
    pub ordinal: usize,
    pub document: DocumentKey,
    pub revision_id: String,
    pub range: Range,
    pub spelling: String,
    pub lookup_key: String,
    pub provenance_id: String,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum MeasurementKind {
    DeclarationName,
    Callee,
    Invocation,
    Reference,
}
impl MeasurementKind {
    pub const ALL: [Self; 4] = [
        Self::DeclarationName,
        Self::Callee,
        Self::Invocation,
        Self::Reference,
    ];
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MeasurementGap {
    pub range: Range,
    pub diagnostic: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MeasurementSupport {
    pub kind: MeasurementKind,
    pub available: bool,
    #[serde(deserialize_with = "required_nullable")]
    pub diagnostic: Option<String>,
    pub gaps: Vec<MeasurementGap>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapabilityReport {
    pub document: DocumentKey,
    pub revision_id: String,
    pub families: Vec<MeasurementSupport>,
    /// Outermost allowlisted Rust expression-macro invocations measured as expressions
    /// (Decision 0019), sorted and disjoint. Facts inside them take ordinals after the
    /// owner's other facts, so measurement outside them is unchanged. Empty elsewhere.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub macro_expansions: Vec<Range>,
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
    pub references: Vec<Reference>,
    pub capability_reports: Vec<CapabilityReport>,
    /// Search-only capture; never part of the native evidence wire shape.
    #[serde(skip)]
    pub search_cards: Vec<CapturedCard>,
}

pub fn lookup(language: &str, name: &str) -> Result<String> {
    ensure!(!name.is_empty(), "empty text");
    Ok(match language {
        "rust" => ComposingNormalizerBorrowed::new_nfc()
            .normalize(name.strip_prefix("r#").unwrap_or(name))
            .into_owned(),
        "python" => ComposingNormalizerBorrowed::new_nfkc()
            .normalize(name)
            .into_owned(),
        "java" => name.to_owned(),
        "javascript" | "typescript" => javascript_identifier(name)?,
        _ => anyhow::bail!("unsupported native language"),
    })
}

// Only decode escapes within a parser-measured identifier. Reparse the decoded
// token to reject invalid code points or a split expression after decoding.
fn javascript_identifier(name: &str) -> Result<String> {
    if !name.contains('\\') {
        return Ok(name.to_owned());
    }
    let mut decoded = String::new();
    let mut chars = name.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            decoded.push(ch);
            continue;
        }
        ensure!(
            chars.next() == Some('u'),
            "invalid JavaScript identifier escape"
        );
        let code = if chars.peek() == Some(&'{') {
            chars.next();
            let mut code = 0u32;
            let mut saw_digit = false;
            loop {
                match chars.next() {
                    Some('}') => break,
                    Some(c) if c.is_ascii_hexdigit() => {
                        saw_digit = true;
                        let digit = c
                            .to_digit(16)
                            .context("invalid JavaScript identifier escape")?;
                        code = code
                            .checked_mul(16)
                            .and_then(|n| n.checked_add(digit))
                            .filter(|n| *n <= 0x10FFFF)
                            .context("invalid JavaScript identifier escape")?;
                    }
                    _ => anyhow::bail!("invalid JavaScript identifier escape"),
                }
            }
            ensure!(saw_digit, "invalid JavaScript identifier escape");
            code
        } else {
            let digits: String = chars.by_ref().take(4).collect();
            ensure!(digits.len() == 4, "invalid JavaScript identifier escape");
            u32::from_str_radix(&digits, 16)?
        };
        decoded.push(char::from_u32(code).context("invalid JavaScript identifier code point")?);
    }
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_javascript::LANGUAGE.into())?;
    let tree = parser
        .parse(&decoded, None)
        .context("JavaScript identifier parse failed")?;
    let root = tree.root_node();
    let token = root
        .named_child(0)
        .context("missing JavaScript identifier")?;
    ensure!(
        !root.has_error()
            && root.named_child_count() == 1
            && token.kind() == "expression_statement"
            && token.named_child_count() == 1
            && token
                .named_child(0)
                .is_some_and(|n| n.kind() == "identifier" && n.byte_range() == (0..decoded.len())),
        "invalid decoded JavaScript identifier"
    );
    Ok(decoded)
}
/// A name-match key requires the entire parser-measured callee token and its
/// exact selected captured bytes; expressions cannot supply a guessed suffix.
pub fn verified_callee_lookup(language: &str, call: &Call, source: &str) -> Option<String> {
    let range = call.callee_range.as_ref()?;
    let spelling = call.spelling.as_deref()?;
    let token = source.get(range.start..range.end)?;
    if language == "java" && token.contains('\\') {
        return None;
    }
    (token == spelling)
        .then(|| lookup(language, token).ok())
        .flatten()
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
pub(crate) fn declared_extraction_inputs(
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
pub(crate) fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
pub(crate) fn valid_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The canonical JSON array of `items` without their proof labels, ordered by each
/// row's canonical bytes. Each row is canonicalized once and the sorted bytes are
/// spliced: the same output as canonicalizing the array of rows sorted that way.
fn stable_witness_bytes<'a, T: Serialize + 'a>(
    items: impl IntoIterator<Item = &'a T>,
) -> Result<Vec<u8>> {
    let mut facts: Vec<Vec<u8>> = items
        .into_iter()
        .map(|item| {
            let mut value = serde_json::to_value(item)?;
            if let Some(object) = value.as_object_mut() {
                object.remove("revisionId");
                object.remove("provenanceId");
            }
            Ok(canonical(&value))
        })
        .collect::<Result<_>>()?;
    // Rows with equal bytes are interchangeable, so an unstable sort is exact.
    facts.sort_unstable();
    let mut out = Vec::with_capacity(facts.iter().map(|fact| fact.len() + 1).sum::<usize>() + 2);
    out.push(b'[');
    for (i, fact) in facts.iter().enumerate() {
        if i != 0 {
            out.push(b',');
        }
        out.extend_from_slice(fact);
    }
    out.push(b']');
    Ok(out)
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
    // The canonical bytes of the object with these members, built in one pass: each
    // member's value is canonicalized once and spliced in canonical key order.
    let mut members: [(&str, Vec<u8>); 12] = [
        (
            "documentKey",
            canonical(&serde_json::to_value(&document.key)?),
        ),
        ("contentHash", canonical(&json!(document.content_hash))),
        ("byteLength", canonical(&json!(document.byte_length))),
        ("producerId", canonical(&json!(producer.id))),
        ("producerVersion", canonical(&json!(producer.version))),
        (
            "extractionContext",
            canonical(&json!(extraction_context(&document.key.language, &[])?)),
        ),
        ("coverage", canonical(&coverage)),
        ("declarations", stable_witness_bytes(declarations)?),
        ("calls", stable_witness_bytes(calls)?),
        ("regions", stable_witness_bytes(regions)?),
        ("references", stable_witness_bytes(references)?),
        (
            "capabilityReports",
            stable_witness_bytes(std::slice::from_ref(capability_report))?,
        ),
    ];
    members.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut bytes = Vec::with_capacity(
        members
            .iter()
            .map(|(key, value)| key.len() + value.len() + 4)
            .sum::<usize>()
            + 2,
    );
    bytes.push(b'{');
    for (i, (key, value)) in members.iter().enumerate() {
        if i != 0 {
            bytes.push(b',');
        }
        crate::native_ids::canonical_string_into(key, &mut |part| bytes.extend_from_slice(part));
        bytes.push(b':');
        bytes.extend_from_slice(value);
    }
    bytes.push(b'}');
    Ok(digest(b"trellis.native-version-witness.v1\0", &bytes))
}

#[cfg(test)]
#[path = "native_records_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "native_records_borrowed_witness_tests.rs"]
mod borrowed_witness_tests;
