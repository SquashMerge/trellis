//! The persisted native evidence records and their leaf vocabulary.
//!
//! Plain data, the producer constants and the pure lookup-key functions only: extraction,
//! validation (`impl Artifact`), witnesses and the spool stay in `native_evidence`, which
//! re-exports these names.

use crate::search_card_records::CapturedCard;
use anyhow::{Context, Result, ensure};
use icu_normalizer::ComposingNormalizerBorrowed;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

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
pub(crate) const NATIVE_VERSION: &str = "native-v17";

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
