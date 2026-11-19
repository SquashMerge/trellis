//! The persisted semantic evidence records and their leaf vocabulary.
//!
//! Plain data, the closed vocabularies, the coverage-reason arithmetic, the language order and
//! the rejoined provenance ID only: validation, the native index and the emission validator
//! stay in `semantic_evidence`, which re-exports the record names.

use crate::native_ids;
use crate::native_records as native;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

fn nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> std::result::Result<Option<T>, D::Error> {
    Option::deserialize(d)
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Language {
    #[serde(rename = "java")]
    Java,
    #[serde(rename = "rust")]
    Rust,
    #[serde(rename = "python")]
    Python,
    #[serde(rename = "javascript")]
    Javascript,
    #[serde(rename = "typescript")]
    Typescript,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PositionEncoding {
    #[serde(rename = "utf8")]
    Utf8,
    #[serde(rename = "utf16")]
    Utf16,
    #[serde(rename = "unicodeScalar")]
    UnicodeScalar,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Role {
    #[serde(rename = "definition")]
    Definition,
    #[serde(rename = "read")]
    Read,
    #[serde(rename = "write")]
    Write,
    #[serde(rename = "call")]
    Call,
    #[serde(rename = "type")]
    Type,
    #[serde(rename = "import")]
    Import,
    #[serde(rename = "alias")]
    Alias,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum CoverageState {
    #[serde(rename = "notRequested")]
    NotRequested,
    #[serde(rename = "omitted")]
    Omitted,
    #[serde(rename = "unsupported")]
    Unsupported,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "partial")]
    Partial,
    #[serde(rename = "complete")]
    Complete,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Freshness {
    #[serde(rename = "fresh")]
    Fresh,
    #[serde(rename = "possiblyStale")]
    PossiblyStale,
    #[serde(rename = "stale")]
    Stale,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum EvidenceKind {
    #[serde(rename = "measuredSyntax")]
    MeasuredSyntax,
    #[serde(rename = "declarationBinding")]
    DeclarationBinding,
    #[serde(rename = "semanticReference")]
    SemanticReference,
    #[serde(rename = "typeRelationship")]
    TypeRelationship,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RecordKind {
    #[serde(rename = "occurrence")]
    Occurrence,
    #[serde(rename = "symbolInformation")]
    SymbolInformation,
    #[serde(rename = "relationship")]
    Relationship,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SymbolScope {
    #[serde(rename = "global")]
    Global,
    #[serde(rename = "document")]
    Document,
    #[serde(rename = "target")]
    Target,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum JoinKind {
    #[serde(rename = "declarationName")]
    DeclarationName,
    #[serde(rename = "callee")]
    Callee,
    #[serde(rename = "invocation")]
    Invocation,
    #[serde(rename = "reference")]
    Reference,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum JoinStatus {
    #[serde(rename = "exact")]
    Exact,
    #[serde(rename = "ambiguous")]
    Ambiguous,
    #[serde(rename = "unmatched")]
    Unmatched,
    #[serde(rename = "unsupported")]
    Unsupported,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Resolution {
    #[serde(rename = "resolved")]
    Resolved,
    #[serde(rename = "external")]
    External,
    #[serde(rename = "ambiguous")]
    Ambiguous,
    #[serde(rename = "unresolved")]
    Unresolved,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ReferenceSite {
    #[serde(rename = "declaration")]
    Declaration,
    #[serde(rename = "use")]
    Use,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Dispatch {
    #[serde(rename = "direct")]
    Direct,
    #[serde(rename = "constructor")]
    Constructor,
    #[serde(rename = "virtual")]
    Virtual,
    #[serde(rename = "interface")]
    Interface,
    #[serde(rename = "dynamic")]
    Dynamic,
    #[serde(rename = "unknown")]
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RelationshipKind {
    #[serde(rename = "extends")]
    Extends,
    #[serde(rename = "implements")]
    Implements,
    #[serde(rename = "overrides")]
    Overrides,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Producer {
    pub id: String,
    pub version: String,
    pub executable_hash: String,
    pub kind: SemanticKind,
    pub languages: Vec<Language>,
    pub position_encoding: PositionEncoding,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SemanticKind {
    #[serde(rename = "semantic")]
    Semantic,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SemanticBasis {
    pub producer_id: String,
    pub producer_version: String,
    pub producer_hash: String,
    pub artifact_hash: String,
    pub language: Language,
    pub source_set_id: String,
    pub revision_id: String,
    pub source_manifest_hash: String,
    pub toolchain_hash: String,
    pub config_hash: String,
    pub dependency_hash: String,
    pub lookup_dependencies: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Coverage {
    pub producer_id: String,
    pub language: Language,
    pub source_set_id: String,
    pub document_path: String,
    pub revision_id: String,
    pub requested: bool,
    pub selected: bool,
    pub state: CoverageState,
    pub supported_roles: Vec<Role>,
    pub observed_roles: Vec<Role>,
    #[serde(deserialize_with = "nullable")]
    pub diagnostic: Option<String>,
    /// Why a `partial` tuple is partial, recorded by the import that derived it (#520). It is
    /// absent on every other state and on rows no import derived, and no graph result returns it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasons: Option<CoverageReasons>,
}
// Every reason a semantic import made one document's coverage `partial` (#520). The single
// `Coverage.diagnostic` keeps only the reason of highest precedence; this keeps them all.
// The four counts are the document's unsupported occurrences by the kind of native gap that
// hid them. The flags are a per-document cap (occurrences, #394, or another wire cap, #634),
// a dependent fact omitted for an unverified internal target, and a diagnostic the captured
// join reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CoverageReasons {
    pub macro_invocation: u64,
    pub attribute_item: u64,
    pub macro_definition: u64,
    pub other: u64,
    pub capped: bool,
    pub omitted: bool,
    pub captured_diagnostics: bool,
}
/// The native gap kinds an unsupported occurrence is counted under, in `CoverageReasons` order.
pub const GAP_KINDS: [&str; 4] = [
    "macroInvocation",
    "attributeItem",
    "macroDefinition",
    "other",
];
impl CoverageReasons {
    /// The index into [`GAP_KINDS`] of the native gap whose diagnostic hid an occurrence. The
    /// native index names the opaque Rust construct it skipped (`opaque Rust <kind> skipped`);
    /// a format string or named format argument is a gap inside a macro invocation. Anything
    /// else, including a wholly unavailable family, is `other`.
    pub fn gap_kind(diagnostic: Option<&str>) -> usize {
        let kind = diagnostic
            .and_then(|text| text.strip_prefix("opaque Rust "))
            .and_then(|text| text.split_once(" skipped"))
            .map(|(kind, _)| kind);
        match kind {
            Some("macro_invocation" | "format string" | "named format argument") => 0,
            Some("attribute_item" | "inner_attribute_item") => 1,
            Some("macro_definition") => 2,
            _ => 3,
        }
    }

    /// The reasons of a `partial` tuple from the operands of its partial decision.
    pub fn new(gaps: [u64; 4], capped: bool, omitted: bool, captured_diagnostics: bool) -> Self {
        Self {
            macro_invocation: gaps[0],
            attribute_item: gaps[1],
            macro_definition: gaps[2],
            other: gaps[3],
            capped,
            omitted,
            captured_diagnostics,
        }
    }

    /// The unsupported occurrence counts in [`GAP_KINDS`] order.
    pub fn gaps(&self) -> [u64; 4] {
        [
            self.macro_invocation,
            self.attribute_item,
            self.macro_definition,
            self.other,
        ]
    }

    /// Unsupported occurrences under any native gap, saturating.
    pub fn native_gaps(&self) -> u64 {
        self.gaps()
            .iter()
            .fold(0u64, |total, count| total.saturating_add(*count))
    }

    pub fn is_empty(&self) -> bool {
        self.native_gaps() == 0 && !self.capped && !self.omitted && !self.captured_diagnostics
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DerivedFrom {
    pub provenance_id: String,
    pub record_kind: RecordKind,
    pub fact_digest: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Provenance {
    pub id: String,
    pub producer_id: String,
    pub document: native::DocumentKey,
    pub revision_id: String,
    pub content_hash: String,
    pub evidence_kind: EvidenceKind,
    #[serde(deserialize_with = "nullable")]
    pub basis: Option<SemanticBasis>,
    pub freshness: Freshness,
    #[serde(deserialize_with = "nullable")]
    pub derived_from: Option<DerivedFrom>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SymbolKey {
    pub scheme: SymbolScheme,
    pub symbol: String,
    pub scope: SymbolScope,
    #[serde(deserialize_with = "nullable")]
    pub document: Option<native::DocumentKey>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SymbolScheme {
    #[serde(rename = "scip")]
    Scip,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Symbol {
    pub key: SymbolKey,
    #[serde(deserialize_with = "nullable")]
    pub display_name: Option<String>,
    pub declarations: Vec<Target>,
    pub provenance_id: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum Target {
    Internal {
        #[serde(rename = "syntaxId")]
        syntax_id: String,
        document: native::DocumentKey,
        #[serde(rename = "revisionId")]
        revision_id: String,
    },
    External {
        symbol: SymbolKey,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MeasuredAnchor {
    pub document: native::DocumentKey,
    pub revision_id: String,
    pub content_hash: String,
    pub range: native::Range,
    pub kind: JoinKind,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Join {
    pub anchor: MeasuredAnchor,
    pub status: JoinStatus,
    pub candidate_ids: Vec<String>,
    #[serde(deserialize_with = "nullable")]
    pub diagnostic: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeclarationBinding {
    #[serde(deserialize_with = "nullable")]
    pub syntax_id: Option<String>,
    pub symbols: Vec<SymbolKey>,
    pub join: Join,
    pub provenance_id: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Reference {
    pub id: String,
    pub owner_syntax_id: String,
    pub ordinal: usize,
    pub document: native::DocumentKey,
    pub revision_id: String,
    pub range: native::Range,
    pub spelling: String,
    pub lookup_key: String,
    pub site: ReferenceSite,
    pub roles: Vec<Role>,
    pub resolution: Resolution,
    #[serde(deserialize_with = "nullable")]
    pub declared_target: Option<Target>,
    pub candidates: Vec<Target>,
    pub provenance_id: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CallBinding {
    #[serde(deserialize_with = "nullable")]
    pub call_id: Option<String>,
    pub join: Join,
    pub resolution: Resolution,
    #[serde(deserialize_with = "nullable")]
    pub declared_target: Option<Target>,
    pub candidates: Vec<Target>,
    pub dispatch: Dispatch,
    pub possible_dispatch: Vec<Target>,
    pub possible_dispatch_complete: bool,
    #[serde(deserialize_with = "nullable")]
    pub stale_target: Option<bool>,
    pub provenance_id: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TypeRelationship {
    pub kind: RelationshipKind,
    pub source: Target,
    pub target: Target,
    pub provenance_id: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SemanticPublication {
    pub producers: Vec<Producer>,
    pub coverage: Vec<Coverage>,
    pub provenance: Vec<Provenance>,
    pub symbols: Vec<Symbol>,
    pub declaration_bindings: Vec<DeclarationBinding>,
    pub references: Vec<Reference>,
    pub call_bindings: Vec<CallBinding>,
    pub type_relationships: Vec<TypeRelationship>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SemanticHead {
    pub producer_id: String,
    pub document: native::DocumentKey,
    pub requested: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SemanticSnapshot {
    pub producers: Vec<Producer>,
    pub coverage: Vec<Coverage>,
    pub provenance: Vec<Provenance>,
    pub symbols: Vec<Symbol>,
    pub declaration_bindings: Vec<DeclarationBinding>,
    pub references: Vec<Reference>,
    pub call_bindings: Vec<CallBinding>,
    pub type_relationships: Vec<TypeRelationship>,
}
/// A read projection of one proof. The stored proof and its basis remain historical;
/// effective freshness belongs to the native-N view, never to a new publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectedEvidence<T> {
    pub evidence: T,
    pub projection: crate::model::SemanticProjection,
    pub historical_freshness: Freshness,
    pub effective_freshness: Freshness,
}
impl<T> ProjectedEvidence<T> {
    pub fn new(
        evidence: T,
        projection: crate::model::SemanticProjection,
        historical_freshness: Freshness,
    ) -> Self {
        let effective_freshness = projection.effective_freshness(historical_freshness);
        Self {
            evidence,
            projection,
            historical_freshness,
            effective_freshness,
        }
    }
}

/// Exact captured bytes for the synthetic boundary, never a raw producer envelope.
#[derive(Debug, Clone)]
pub struct CapturedComponents {
    pub producer_executables: BTreeMap<String, Vec<u8>>,
    /// Captured artifact bytes keyed by `(producer_id, artifact_hash)`, one entry per capture.
    pub artifacts: BTreeMap<(String, String), Vec<u8>>,
    pub sources: BTreeMap<native::DocumentKey, Vec<u8>>,
    pub captured_revision: native::Revision,
    /// Canonical native revision input components, captured with the native header.
    pub native_toolchain: serde_json::Value,
    pub native_config: Vec<serde_json::Value>,
    pub native_dependencies: Vec<serde_json::Value>,
    /// Requested descriptors are independent of captured producer descriptors.
    pub requested_producers: Vec<Producer>,
    pub toolchain: Vec<u8>,
    pub config: Vec<u8>,
    pub dependency: Vec<u8>,
    /// Authenticated native extraction inputs, keyed by component name. The current
    /// native producer declares an empty per-document inventory.
    pub native_extraction_components: BTreeMap<String, Vec<u8>>,
}

pub const LANGUAGE_ORDER: [&str; 5] = ["java", "rust", "python", "javascript", "typescript"];

pub fn language_order(language: &str) -> Option<usize> {
    LANGUAGE_ORDER.iter().position(|name| *name == language)
}

/// The provenance ID of a rejoined fact: a digest of the raw proof, the destination revision
/// and the evidence kind, so it needs no lookup table.
pub(crate) fn rejoin_provenance_id(raw_id: &str, revision_id: &str, kind: EvidenceKind) -> String {
    native_ids::digest(
        b"trellis.semantic-rejoin.v1\0",
        &native_ids::canonical(
            &serde_json::json!({"proof":raw_id,"revision":revision_id,"kind":kind}),
        ),
    )
}
