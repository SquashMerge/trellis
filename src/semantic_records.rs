//! The persisted semantic evidence records and their leaf vocabulary.
//!
//! Plain data, the closed vocabularies, the coverage-reason arithmetic and the language order
//! only: validation, the native index and the emission validator stay in `semantic_evidence`,
//! which re-exports these names.

use crate::native_records as native;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

fn nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> std::result::Result<Option<T>, D::Error> {
    Option::deserialize(d)
}
macro_rules! closed {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        pub enum $name { $(#[serde(rename = $value)] $variant),+ }
    };
}
closed!(Language { Java => "java", Rust => "rust", Python => "python", Javascript => "javascript", Typescript => "typescript" });
closed!(PositionEncoding { Utf8 => "utf8", Utf16 => "utf16", UnicodeScalar => "unicodeScalar" });
closed!(Role { Definition => "definition", Read => "read", Write => "write", Call => "call", Type => "type", Import => "import", Alias => "alias" });
closed!(CoverageState { NotRequested => "notRequested", Omitted => "omitted", Unsupported => "unsupported", Failed => "failed", Partial => "partial", Complete => "complete" });
closed!(Freshness { Fresh => "fresh", PossiblyStale => "possiblyStale", Stale => "stale" });
closed!(EvidenceKind { MeasuredSyntax => "measuredSyntax", DeclarationBinding => "declarationBinding", SemanticReference => "semanticReference", TypeRelationship => "typeRelationship" });
closed!(RecordKind { Occurrence => "occurrence", SymbolInformation => "symbolInformation", Relationship => "relationship" });
closed!(SymbolScope { Global => "global", Document => "document", Target => "target" });
closed!(JoinKind { DeclarationName => "declarationName", Callee => "callee", Invocation => "invocation", Reference => "reference" });
closed!(JoinStatus { Exact => "exact", Ambiguous => "ambiguous", Unmatched => "unmatched", Unsupported => "unsupported" });
closed!(Resolution { Resolved => "resolved", External => "external", Ambiguous => "ambiguous", Unresolved => "unresolved" });
closed!(ReferenceSite { Declaration => "declaration", Use => "use" });
closed!(Dispatch { Direct => "direct", Constructor => "constructor", Virtual => "virtual", Interface => "interface", Dynamic => "dynamic", Unknown => "unknown" });
closed!(RelationshipKind { Extends => "extends", Implements => "implements", Overrides => "overrides" });
macro_rules! record {
    ($name:ident { $($(#[$attr:meta])* $field:ident : $ty:ty),* $(,)? }) => {
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        pub struct $name { $($(#[$attr])* pub $field: $ty),* }
    };
}
record!(Producer { id: String, version: String, executable_hash: String, kind: SemanticKind, languages: Vec<Language>, position_encoding: PositionEncoding });
closed!(SemanticKind { Semantic => "semantic" });
record!(SemanticBasis { producer_id: String, producer_version: String, producer_hash: String, artifact_hash: String, language: Language, source_set_id: String, revision_id: String, source_manifest_hash: String, toolchain_hash: String, config_hash: String, dependency_hash: String, lookup_dependencies: Vec<String> });
record!(Coverage { producer_id: String, language: Language, source_set_id: String, document_path: String, revision_id: String, requested: bool, selected: bool, state: CoverageState, supported_roles: Vec<Role>, observed_roles: Vec<Role>, #[serde(deserialize_with = "nullable")] diagnostic: Option<String>,
    /// Why a `partial` tuple is partial, recorded by the import that derived it (#520). It is
    /// absent on every other state and on rows no import derived, and no graph result returns it.
    #[serde(default, skip_serializing_if = "Option::is_none")] reasons: Option<CoverageReasons> });
// Every reason a semantic import made one document's coverage `partial` (#520). The single
// `Coverage.diagnostic` keeps only the reason of highest precedence; this keeps them all.
// The four counts are the document's unsupported occurrences by the kind of native gap that
// hid them. The flags are a per-document cap (occurrences, #394, or another wire cap, #634),
// a dependent fact omitted for an unverified internal target, and a diagnostic the captured
// join reported.
record!(CoverageReasons {
    macro_invocation: u64,
    attribute_item: u64,
    macro_definition: u64,
    other: u64,
    capped: bool,
    omitted: bool,
    captured_diagnostics: bool
});
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
record!(DerivedFrom {
    provenance_id: String,
    record_kind: RecordKind,
    fact_digest: String
});
record!(Provenance { id: String, producer_id: String, document: native::DocumentKey, revision_id: String, content_hash: String, evidence_kind: EvidenceKind, #[serde(deserialize_with = "nullable")] basis: Option<SemanticBasis>, freshness: Freshness, #[serde(deserialize_with = "nullable")] derived_from: Option<DerivedFrom> });
record!(SymbolKey { scheme: SymbolScheme, symbol: String, scope: SymbolScope, #[serde(deserialize_with = "nullable")] document: Option<native::DocumentKey> });
closed!(SymbolScheme { Scip => "scip" });
record!(Symbol { key: SymbolKey, #[serde(deserialize_with = "nullable")] display_name: Option<String>, declarations: Vec<Target>, provenance_id: String });
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
record!(MeasuredAnchor {
    document: native::DocumentKey,
    revision_id: String,
    content_hash: String,
    range: native::Range,
    kind: JoinKind
});
record!(Join { anchor: MeasuredAnchor, status: JoinStatus, candidate_ids: Vec<String>, #[serde(deserialize_with = "nullable")] diagnostic: Option<String> });
record!(DeclarationBinding { #[serde(deserialize_with = "nullable")] syntax_id: Option<String>, symbols: Vec<SymbolKey>, join: Join, provenance_id: String });
record!(Reference { id: String, owner_syntax_id: String, ordinal: usize, document: native::DocumentKey, revision_id: String, range: native::Range, spelling: String, lookup_key: String, site: ReferenceSite, roles: Vec<Role>, resolution: Resolution, #[serde(deserialize_with = "nullable")] declared_target: Option<Target>, candidates: Vec<Target>, provenance_id: String });
record!(CallBinding { #[serde(deserialize_with = "nullable")] call_id: Option<String>, join: Join, resolution: Resolution, #[serde(deserialize_with = "nullable")] declared_target: Option<Target>, candidates: Vec<Target>, dispatch: Dispatch, possible_dispatch: Vec<Target>, possible_dispatch_complete: bool, #[serde(deserialize_with = "nullable")] stale_target: Option<bool>, provenance_id: String });
record!(TypeRelationship {
    kind: RelationshipKind,
    source: Target,
    target: Target,
    provenance_id: String
});
record!(SemanticPublication { producers: Vec<Producer>, coverage: Vec<Coverage>, provenance: Vec<Provenance>, symbols: Vec<Symbol>, declaration_bindings: Vec<DeclarationBinding>, references: Vec<Reference>, call_bindings: Vec<CallBinding>, type_relationships: Vec<TypeRelationship> });
record!(SemanticHead {
    producer_id: String,
    document: native::DocumentKey,
    requested: bool
});
record!(SemanticSnapshot { producers: Vec<Producer>, coverage: Vec<Coverage>, provenance: Vec<Provenance>, symbols: Vec<Symbol>, declaration_bindings: Vec<DeclarationBinding>, references: Vec<Reference>, call_bindings: Vec<CallBinding>, type_relationships: Vec<TypeRelationship> });
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
