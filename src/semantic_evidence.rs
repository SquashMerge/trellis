//! Captured semantic evidence admitted against one validated native snapshot.
use crate::{native_evidence as native, native_ids, store::id_codec};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

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
closed!(Language { Java => "java", Rust => "rust", Python => "python", Javascript => "javascript" });
closed!(PositionEncoding { Utf8 => "utf8", Utf16 => "utf16", UnicodeScalar => "unicodeScalar" });
closed!(Role { Definition => "definition", Read => "read", Write => "write", Call => "call", Type => "type", Import => "import", Alias => "alias" });
closed!(CoverageState { NotRequested => "notRequested", Omitted => "omitted", Unsupported => "unsupported", Failed => "failed", Partial => "partial", Complete => "complete" });
closed!(Freshness { Fresh => "fresh", PossiblyStale => "possiblyStale", Stale => "stale" });
closed!(EvidenceKind { MeasuredSyntax => "measuredSyntax", DeclarationBinding => "declarationBinding", SemanticReference => "semanticReference", TypeRelationship => "typeRelationship" });
closed!(RecordKind { Occurrence => "occurrence", SymbolInformation => "symbolInformation", Relationship => "relationship" });
closed!(SymbolScope { Global => "global", Document => "document" });
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
record!(Coverage { producer_id: String, language: Language, source_set_id: String, document_path: String, revision_id: String, requested: bool, selected: bool, state: CoverageState, supported_roles: Vec<Role>, observed_roles: Vec<Role>, #[serde(deserialize_with = "nullable")] diagnostic: Option<String> });
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
/// Exact captured bytes for the synthetic boundary, never a raw producer envelope.
#[derive(Debug, Clone)]
pub struct CapturedComponents {
    pub producer_executables: BTreeMap<String, Vec<u8>>,
    pub artifacts: BTreeMap<String, Vec<u8>>,
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
fn text(s: &str) -> Result<()> {
    ensure!(!s.is_empty(), "empty semantic text");
    Ok(())
}
fn hash(s: &str) -> Result<()> {
    ensure!(
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid semantic hash"
    );
    Ok(())
}
fn path(s: &str) -> Result<()> {
    text(s)?;
    ensure!(
        !s.starts_with('/')
            && !s.contains(['\\', '\0'])
            && s.split('/').all(|x| !x.is_empty() && x != "." && x != ".."),
        "invalid semantic path"
    );
    Ok(())
}
fn ordered<T: Ord>(items: &[T]) -> Result<()> {
    ensure!(
        items.windows(2).all(|w| w[0] < w[1]),
        "unsorted or duplicate semantic set"
    );
    Ok(())
}
fn uint(n: usize) -> Result<()> {
    ensure!(n <= 9_007_199_254_740_991, "semantic UInt overflow");
    Ok(())
}
fn sid(id: &str) -> Result<()> {
    ensure!(id_codec::encode_sid(id).is_some(), "invalid syntax ID");
    Ok(())
}
fn occ(id: &str) -> Result<()> {
    ensure!(id_codec::encode_occ(id).is_some(), "invalid occurrence ID");
    Ok(())
}
fn span<'a>(bytes: &'a [u8], range: &native::Range) -> Result<&'a str> {
    uint(range.start)?;
    uint(range.end)?;
    let source = std::str::from_utf8(bytes)?;
    ensure!(range.start < range.end, "empty semantic span");
    source
        .get(range.start..range.end)
        .context("invalid converted semantic span")
}
fn key(key: &native::DocumentKey, revision: &native::Revision) -> Result<()> {
    text(&key.source_set_id)?;
    path(&key.path)?;
    ensure!(
        key.source_set_id == revision.source_set_id
            && matches!(
                key.language.as_str(),
                "java" | "rust" | "python" | "javascript"
            ),
        "foreign semantic document"
    );
    Ok(())
}
fn symbol_key(key_: &SymbolKey, revision: &native::Revision) -> Result<()> {
    text(&key_.symbol)?;
    match (&key_.scope, &key_.document) {
        (SymbolScope::Global, None) => {}
        (SymbolScope::Document, Some(d)) => key(d, revision)?,
        _ => anyhow::bail!("symbol scope/document mismatch"),
    }
    Ok(())
}
fn target(
    t: &Target,
    native: &native::Artifact,
    declarations: &BTreeMap<String, &native::Declaration>,
    selected: Option<&BTreeMap<String, &TargetDeclaration>>,
) -> Result<()> {
    match t {
        Target::Internal {
            syntax_id,
            document,
            revision_id,
        } => {
            sid(syntax_id)?;
            ensure!(
                revision_id == &native.revision.id,
                "foreign internal target revision"
            );
            ensure!(
                selected.map_or_else(
                    || declarations
                        .get(syntax_id)
                        .is_some_and(|d| &d.document == document),
                    |items| items
                        .get(syntax_id)
                        .is_some_and(|d| &d.document == document)
                        || (items.get(syntax_id).is_none()
                            && declarations
                                .get(syntax_id)
                                .is_some_and(|d| &d.document == document)),
                ),
                "unmeasured internal target"
            );
        }
        Target::External { symbol } => symbol_key(symbol, &native.revision)?,
    }
    Ok(())
}
fn targets(
    items: &[Target],
    native: &native::Artifact,
    decls: &BTreeMap<String, &native::Declaration>,
    sorted: bool,
    selected: Option<&BTreeMap<String, &TargetDeclaration>>,
) -> Result<()> {
    let mut seen = BTreeSet::new();
    for item in items {
        target(item, native, decls, selected)?;
        let bytes = native_ids::canonical(&serde_json::to_value(item)?);
        ensure!(seen.insert(bytes), "duplicate semantic target");
    }
    if sorted {
        let bytes: Vec<_> = items
            .iter()
            .map(|t| native_ids::canonical(&serde_json::to_value(t).expect("serializable target")))
            .collect();
        ordered(&bytes)?;
    }
    Ok(())
}
fn resolution(kind: Resolution, declared: &Option<Target>, candidates: &[Target]) -> Result<()> {
    ensure!(
        match kind {
            Resolution::Resolved =>
                matches!(declared, Some(Target::Internal { .. })) && candidates.is_empty(),
            Resolution::External =>
                matches!(declared, Some(Target::External { .. })) && candidates.is_empty(),
            Resolution::Ambiguous => declared.is_none() && candidates.len() >= 2,
            Resolution::Unresolved => declared.is_none() && candidates.is_empty(),
        },
        "invalid semantic resolution cardinality"
    );
    Ok(())
}
fn declaration_name_owner<'a>(
    declaration: &'a native::Declaration,
    native: &'a native::Artifact,
) -> Option<&'a str> {
    if declaration.kind == "module" && declaration.ancestors.is_empty() {
        return Some(&declaration.syntax_id);
    }
    let mut parents = native.declarations.iter().filter(|candidate| {
        candidate.document == declaration.document
            && candidate.syntax_id != declaration.syntax_id
            && candidate.range.start <= declaration.range.start
            && candidate.range.end >= declaration.range.end
            && (if declaration.ancestors.is_empty() {
                candidate.kind == "module" && candidate.ancestors.is_empty()
            } else {
                candidate.key == *declaration.ancestors.last().unwrap()
                    && candidate.ancestors
                        == declaration.ancestors[..declaration.ancestors.len() - 1]
            })
    });
    let owner = parents.next().unwrap_or(declaration);
    parents.next().is_none().then_some(owner.syntax_id.as_str())
}

fn validate_join(
    join: &Join,
    family: JoinKind,
    native: &native::Artifact,
    sources: &BTreeMap<native::DocumentKey, Vec<u8>>,
    original: Option<&Join>,
) -> Result<()> {
    let a = &join.anchor;
    ensure!(
        a.kind == family || (family == JoinKind::Callee && a.kind == JoinKind::Invocation),
        "wrong join family"
    );
    if let Some(previous) = original {
        ensure!(
            previous.anchor.document == a.document
                && previous.anchor.kind == a.kind
                && previous.anchor.range == a.range
                && previous.status == join.status
                && previous.candidate_ids == join.candidate_ids
                && previous.diagnostic == join.diagnostic,
            "join differs from admitted source association"
        );
        ensure!(
            join.status == JoinStatus::Exact,
            "unwitnessed negative join"
        );
        for id in &join.candidate_ids {
            if family == JoinKind::DeclarationName {
                sid(id)?
            } else {
                occ(id)?
            }
        }
    } else {
        let source = sources.get(&a.document).context("missing join source")?;
        let slice = span(source, &a.range)?;
        let doc = native
            .revision
            .documents
            .iter()
            .find(|d| d.key == a.document)
            .context("missing join document")?;
        ensure!(
            a.revision_id == native.revision.id && a.content_hash == doc.content_hash,
            "foreign join anchor"
        );
        let mut candidates = BTreeSet::new();
        match family {
            JoinKind::DeclarationName => {
                for d in &native.declarations {
                    if d.document == a.document
                        && d.name_range.as_ref() == Some(&a.range)
                        && d.name.as_deref() == Some(slice)
                    {
                        candidates.insert(d.syntax_id.clone());
                    }
                }
            }
            JoinKind::Callee => {
                for call in &native.calls {
                    let r = if a.kind == JoinKind::Invocation {
                        Some(&call.range)
                    } else {
                        call.callee_range.as_ref()
                    };
                    if call.document == a.document
                        && r == Some(&a.range)
                        && (a.kind == JoinKind::Invocation
                            || call.spelling.as_deref() == Some(slice))
                    {
                        candidates.insert(call.id.clone());
                    }
                }
            }
            JoinKind::Reference => {
                for reference in &native.references {
                    if reference.document == a.document
                        && reference.range == a.range
                        && reference.spelling == slice
                        && native::lookup(&a.document.language, slice)? == reference.lookup_key
                    {
                        candidates.insert(reference.id.clone());
                    }
                }
            }
            JoinKind::Invocation => anyhow::bail!("invocation is a call anchor, not a fact family"),
        }
        let kind = match a.kind {
            JoinKind::DeclarationName => native::MeasurementKind::DeclarationName,
            JoinKind::Callee => native::MeasurementKind::Callee,
            JoinKind::Invocation => native::MeasurementKind::Invocation,
            JoinKind::Reference => native::MeasurementKind::Reference,
        };
        let support = native
            .capability_reports
            .iter()
            .find(|report| report.document == a.document && report.revision_id == a.revision_id)
            .context("missing native capability report")?
            .families
            .iter()
            .find(|support| support.kind == kind)
            .context("missing native measurement family")?;
        let contained = |range: &native::Range, within: &native::Range| {
            within.start <= range.start && range.end <= within.end
        };
        let measured_ranges: Vec<_> = match a.kind {
            JoinKind::DeclarationName => native
                .declarations
                .iter()
                .filter(|d| d.document == a.document)
                .filter_map(|d| d.name_range.as_ref())
                .collect(),
            JoinKind::Callee => native
                .calls
                .iter()
                .filter(|c| c.document == a.document)
                .filter_map(|c| c.callee_range.as_ref())
                .collect(),
            JoinKind::Invocation => native
                .calls
                .iter()
                .filter(|c| c.document == a.document)
                .map(|c| &c.range)
                .collect(),
            JoinKind::Reference => native
                .references
                .iter()
                .filter(|r| r.document == a.document)
                .map(|r| &r.range)
                .collect(),
        };
        ensure!(
            support.available || measured_ranges.is_empty(),
            "unavailable family has measured candidates"
        );
        for gap in &support.gaps {
            span(source, &gap.range)?;
            text(&gap.diagnostic)?;
            ensure!(
                !measured_ranges
                    .iter()
                    .any(|range| contained(range, &gap.range)),
                "measured candidate contradicts native capability gap"
            );
        }
        let unsupported = if !support.available {
            Some(
                support
                    .diagnostic
                    .as_deref()
                    .context("unavailable family without diagnostic")?,
            )
        } else {
            ensure!(support.diagnostic.is_none(), "enabled family diagnostic");
            support
                .gaps
                .iter()
                .find(|gap| contained(&a.range, &gap.range))
                .map(|gap| gap.diagnostic.as_str())
        };
        let expected: Vec<_> = candidates.into_iter().collect();
        let status = if unsupported.is_some() && expected.is_empty() {
            JoinStatus::Unsupported
        } else if expected.len() == 1 {
            JoinStatus::Exact
        } else if expected.len() >= 2 {
            JoinStatus::Ambiguous
        } else {
            JoinStatus::Unmatched
        };
        ensure!(
            join.status == status && join.candidate_ids == expected,
            "join differs from measured candidates"
        );
        if let Some(diagnostic) = &join.diagnostic {
            text(diagnostic)?;
        }
        ensure!(
            join.diagnostic.is_some() == (status != JoinStatus::Exact),
            "join diagnostic mismatch"
        );
        if status == JoinStatus::Unsupported {
            ensure!(
                join.diagnostic.as_deref() == unsupported,
                "unsupported join diagnostic differs from native capability"
            );
        }
        for id in &join.candidate_ids {
            if family == JoinKind::DeclarationName {
                sid(id)?
            } else {
                occ(id)?
            }
        }
    }
    let doc = native
        .revision
        .documents
        .iter()
        .find(|d| d.key == a.document)
        .context("missing join document")?;
    ensure!(
        a.revision_id == native.revision.id && a.content_hash == doc.content_hash,
        "foreign join anchor"
    );
    Ok(())
}

fn admitted_proof<'a>(witness: &'a WitnessedDocument, proof: &Provenance) -> Result<&'a str> {
    let derived = proof
        .derived_from
        .as_ref()
        .context("unwitnessed derived proof")?;
    let mut matches = witness.original.provenance.iter().filter(|p| {
        p.document == proof.document
            && p.producer_id == proof.producer_id
            && p.derived_from.as_ref() == Some(derived)
            && p.evidence_kind == proof.evidence_kind
            && p.content_hash == proof.content_hash
    });
    let previous = matches
        .next()
        .context("missing admitted proof association")?;
    ensure!(
        matches.next().is_none(),
        "ambiguous admitted proof association"
    );
    Ok(&previous.id)
}

fn original_join<'a>(
    witness: &'a WitnessedDocument,
    proof: &Provenance,
    family: JoinKind,
) -> Result<&'a Join> {
    let id = admitted_proof(witness, proof)?;
    let matches: Vec<_> = match family {
        JoinKind::DeclarationName => witness
            .original
            .declaration_bindings
            .iter()
            .filter(|b| b.provenance_id == id)
            .map(|b| &b.join)
            .collect(),
        JoinKind::Callee => witness
            .original
            .call_bindings
            .iter()
            .filter(|b| b.provenance_id == id)
            .map(|b| &b.join)
            .collect(),
        _ => anyhow::bail!("invalid witnessed join family"),
    };
    ensure!(matches.len() == 1, "incomplete admitted join association");
    Ok(matches[0])
}

fn original_reference<'a>(
    witness: &'a WitnessedDocument,
    proof: &Provenance,
) -> Result<&'a Reference> {
    let id = admitted_proof(witness, proof)?;
    let mut matches = witness
        .original
        .references
        .iter()
        .filter(|r| r.provenance_id == id);
    let original = matches
        .next()
        .context("missing admitted reference association")?;
    ensure!(
        matches.next().is_none(),
        "fused or ambiguous admitted reference association"
    );
    Ok(original)
}

fn language_rank(language: &str) -> usize {
    match language {
        "java" => 0,
        "rust" => 1,
        "python" => 2,
        "javascript" => 3,
        _ => unreachable!("validated language"),
    }
}

fn native_revision_id(captured: &CapturedComponents) -> String {
    let documents: Vec<_> = captured.captured_revision.documents.iter().map(|d| json!({"byteLength":d.byte_length,"contentHash":d.content_hash,"language":d.key.language,"path":d.key.path})).collect();
    format!(
        "revision:v2:{}",
        native_ids::digest(
            b"trellis.native-revision.v2\0",
            &native_ids::canonical(&json!({
                "sourceSetId":captured.captured_revision.source_set_id,
                "documents":documents,
                "toolchain":captured.native_toolchain,
                "config":captured.native_config,
                "dependencies":captured.native_dependencies,
            }))
        )
    )
}

fn requested_manifest_hash(revision: &native::Revision) -> String {
    let rows = revision
        .documents
        .iter()
        .map(|d| json!({"document":d.key,"contentHash":d.content_hash}))
        .collect();
    native_ids::digest(b"", &native_ids::canonical(&serde_json::Value::Array(rows)))
}

/// Validate one fully normalized synthetic semantic artifact against the measured native input.
/// The Store invokes this before inserting any normalized row.
pub fn validate_evidence(
    semantic: &SemanticPublication,
    native: &native::Artifact,
    captured: &CapturedComponents,
) -> Result<()> {
    validate_evidence_with_raw(semantic, native, captured, None)
}

struct ValidatedRaw<'a> {
    producer_id: &'a str,
    source_set_id: &'a str,
    captured_revision: &'a str,
    binary_bytes: &'a [u8],
    inventory: crate::scip_raw::ProofInventory,
}
impl<'a> ValidatedRaw<'a> {
    fn from_decoded(raw: &'a crate::store::SemanticRawCapture) -> Self {
        Self {
            producer_id: &raw.producer_id,
            source_set_id: &raw.source_set_id,
            captured_revision: &raw.captured_revision,
            binary_bytes: &raw.binary_bytes,
            inventory: raw.inventory.proof_identities(),
        }
    }
    fn from_witnessed(raw: &'a crate::store::WitnessedRawCapture) -> Self {
        Self {
            producer_id: &raw.producer_id,
            source_set_id: &raw.source_set_id,
            captured_revision: &raw.captured_revision,
            binary_bytes: &raw.binary_bytes,
            inventory: raw.inventory.clone(),
        }
    }
}

pub fn validate_evidence_with_raw(
    semantic: &SemanticPublication,
    native: &native::Artifact,
    captured: &CapturedComponents,
    raw: Option<&crate::store::SemanticRawCapture>,
) -> Result<()> {
    validate_evidence_inner(
        semantic,
        native,
        captured,
        raw.map(ValidatedRaw::from_decoded),
        None,
        None,
    )
}

pub(crate) fn validate_evidence_with_attested_raw(
    semantic: &SemanticPublication,
    native: &native::Artifact,
    captured: &CapturedComponents,
    raw: &crate::store::SemanticRawCapture,
    attested: &crate::store::AttestedProfileProducer,
) -> Result<()> {
    validate_evidence_inner(
        semantic,
        native,
        captured,
        Some(ValidatedRaw::from_decoded(raw)),
        Some(attested),
        None,
    )
}

#[derive(Debug, Clone)]
pub(crate) struct WitnessedDocument {
    pub source_hash: String,
    pub wire_digest: Option<String>,
    pub captured_version: String,
    pub current_version: String,
    pub original: SemanticSnapshot,
}

#[derive(Debug, Clone)]
pub(crate) struct TargetDeclaration {
    pub syntax_id: String,
    pub document: native::DocumentKey,
    pub name: String,
    pub name_range: native::Range,
}

pub(crate) fn validate_evidence_with_witnessed_raw(
    semantic: &SemanticPublication,
    native: &native::Artifact,
    captured: &CapturedComponents,
    raw: &crate::store::WitnessedRawCapture,
    attested: &crate::store::AttestedProfileProducer,
    witnesses: &BTreeMap<native::DocumentKey, WitnessedDocument>,
    current_declarations: &[TargetDeclaration],
) -> Result<()> {
    validate_evidence_inner(
        semantic,
        native,
        captured,
        Some(ValidatedRaw::from_witnessed(raw)),
        Some(attested),
        Some((witnesses, current_declarations)),
    )
}

/// Validates one coverage row against the producers and the native revision. Returns the
/// row's language string, which keys the coverage tuple.
fn validate_coverage_row(
    row: &Coverage,
    producers: &BTreeMap<&str, &Producer>,
    native: &native::Artifact,
) -> Result<String> {
    let revision = &native.revision;
    text(&row.producer_id)?;
    path(&row.document_path)?;
    let producer = producers
        .get(row.producer_id.as_str())
        .context("dangling coverage producer")?;
    ensure!(
        row.revision_id == revision.id && row.source_set_id == revision.source_set_id,
        "foreign coverage tuple"
    );
    let lang = serde_json::to_value(row.language)?
        .as_str()
        .unwrap()
        .to_owned();
    ensure!(
        producer.languages.contains(&row.language)
            && native.source_set.languages.contains(&lang)
            && revision
                .documents
                .iter()
                .any(|d| d.key.source_set_id == row.source_set_id
                    && d.key.language == lang
                    && d.key.path == row.document_path),
        "unsupported coverage document"
    );
    ordered(&row.supported_roles)?;
    ordered(&row.observed_roles)?;
    ensure!(
        row.observed_roles
            .iter()
            .all(|r| row.supported_roles.contains(r)),
        "observed role not supported"
    );
    if row.language == Language::Java {
        ensure!(
            !row.supported_roles.contains(&Role::Alias)
                && !row.observed_roles.contains(&Role::Alias),
            "Java alias not applicable"
        );
    }
    ensure!(
        match row.state {
            CoverageState::NotRequested => !row.requested && !row.selected,
            CoverageState::Omitted | CoverageState::Unsupported => row.requested && !row.selected,
            CoverageState::Failed | CoverageState::Partial | CoverageState::Complete =>
                row.requested && row.selected,
        },
        "invalid coverage request/selection state"
    );
    if let Some(diagnostic) = &row.diagnostic {
        text(diagnostic)?;
    }
    ensure!(
        row.diagnostic.is_some()
            == !matches!(
                row.state,
                CoverageState::NotRequested | CoverageState::Complete
            ),
        "coverage diagnostic mismatch"
    );
    if !matches!(row.state, CoverageState::Complete | CoverageState::Partial) {
        ensure!(
            row.observed_roles.is_empty(),
            "unselected coverage observed roles"
        );
    }
    Ok(lang)
}

/// The capture identity a derived proof must agree with.
struct RawCaptureIdentity<'a> {
    producer_id: &'a str,
    source_set_id: &'a str,
    captured_revision: &'a str,
}

/// The document a raw claim belongs to.
pub(crate) struct RawClaimDocument {
    pub(crate) language: String,
    pub(crate) path: String,
}

/// Resolves `(proof_id, record_kind, fact_digest)` to exactly one raw claim.
type ClaimLookup<'a> = dyn Fn(&str, &str, &str) -> Result<RawClaimDocument> + 'a;

/// Validates a provenance row's derivation: a derived proof resolves through `claims` to
/// exactly one claim with the same kind and digest; any other proof belongs to the captured
/// revision. The lookup is a parameter so an in-memory inventory and an indexed claim table
/// share the rule.
fn validate_provenance_row(
    p: &Provenance,
    raw: Option<(&RawCaptureIdentity<'_>, &ClaimLookup<'_>)>,
    captured: &CapturedComponents,
    revision: &native::Revision,
) -> Result<()> {
    if let Some(derived) = &p.derived_from {
        ensure!(
            p.revision_id == revision.id,
            "derived proof revision mismatch"
        );
        let (raw, claims) =
            raw.context("derived provenance requires authenticated raw inventory")?;
        let kind = serde_json::to_value(derived.record_kind)?;
        let claim = claims(
            &derived.provenance_id,
            kind.as_str().context("invalid raw record kind")?,
            &derived.fact_digest,
        )?;
        ensure!(
            p.producer_id == raw.producer_id
                && p.document.source_set_id == raw.source_set_id
                && p.document.language == claim.language
                && p.document.path == claim.path
                && raw.captured_revision == captured.captured_revision.id,
            "derived provenance raw capture identity mismatch"
        );
    } else {
        ensure!(
            p.revision_id == captured.captured_revision.id,
            "captured proof revision mismatch"
        );
    }
    Ok(())
}

/// Validates a proof's captured basis. `expected_artifact_hash` is the digest of the artifact
/// this row's producer was captured from; `raw_binding` is the raw inventory's
/// `(artifact_hash, source_manifest_hash)` when it must also match the basis.
fn validate_basis(
    p: &Provenance,
    basis: &SemanticBasis,
    producer: &Producer,
    captured: &CapturedComponents,
    expected_artifact_hash: Option<&String>,
    raw_binding: Option<(&str, &str)>,
    manifest_hash: &str,
) -> Result<()> {
    for field in [
        &basis.producer_hash,
        &basis.artifact_hash,
        &basis.source_manifest_hash,
        &basis.toolchain_hash,
        &basis.config_hash,
        &basis.dependency_hash,
    ] {
        hash(field)?;
    }
    ensure!(
        basis.producer_id == p.producer_id
            && basis.producer_version == producer.version
            && basis.producer_hash == producer.executable_hash
            && basis.revision_id == captured.captured_revision.id
            && basis.source_set_id == captured.captured_revision.source_set_id
            && basis.language == serde_json::from_value::<Language>(json!(p.document.language))?,
        "captured basis identity mismatch"
    );
    ensure!(
        basis.artifact_hash
            == *expected_artifact_hash.context("missing captured semantic artifact")?
            && raw_binding.is_none_or(|(artifact_hash, source_manifest_hash)| {
                basis.artifact_hash == artifact_hash && source_manifest_hash == manifest_hash
            })
            && basis.source_manifest_hash == manifest_hash
            && basis.toolchain_hash == captured.captured_revision.toolchain_hash
            && basis.config_hash == captured.captured_revision.config_hash
            && basis.dependency_hash == captured.captured_revision.dependency_hash,
        "captured basis digest mismatch"
    );
    ordered(&basis.lookup_dependencies)?;
    for lookup in &basis.lookup_dependencies {
        text(lookup)?;
        ensure!(
            id_codec::encode_sid(lookup).is_none() && id_codec::encode_occ(lookup).is_none(),
            "syntax ID is not a lookup key"
        );
    }
    Ok(())
}

/// Read-only inputs shared by the per-row association validators.
#[derive(Clone, Copy)]
struct RowEnv<'a> {
    native: &'a native::Artifact,
    captured: &'a CapturedComponents,
    revision: &'a native::Revision,
    sources: &'a BTreeMap<native::DocumentKey, Vec<u8>>,
    decls: &'a BTreeMap<String, &'a native::Declaration>,
    selected: Option<&'a BTreeMap<String, &'a TargetDeclaration>>,
    witnesses: Option<&'a BTreeMap<native::DocumentKey, WitnessedDocument>>,
}

type TargetClaim = (Resolution, Option<Vec<u8>>, Vec<Vec<u8>>);

/// Validates one declaration binding against its resolved provenance row.
fn validate_declaration_binding_row<'a>(
    b: &'a DeclarationBinding,
    p: &'a Provenance,
    env: &RowEnv<'_>,
    binding_facts: &mut BTreeSet<(String, native::DocumentKey, String, Vec<u8>)>,
) -> Result<()> {
    let RowEnv {
        native,
        captured,
        revision,
        sources,
        decls: _,
        selected: _,
        witnesses,
    } = *env;
    ensure!(
        binding_facts.insert((
            p.producer_id.clone(),
            p.document.clone(),
            b.provenance_id.clone(),
            native_ids::canonical(&serde_json::to_value(&b.join.anchor)?)
        )),
        "duplicate declaration binding fact"
    );
    let admitted = witnesses
        .and_then(|items| items.get(&p.document))
        .map(|w| original_join(w, p, JoinKind::DeclarationName))
        .transpose()?;
    validate_join(
        &b.join,
        JoinKind::DeclarationName,
        native,
        sources,
        admitted,
    )?;
    if let Some(old) = admitted {
        ensure!(
            old.anchor.revision_id == captured.captured_revision.id
                && old.anchor.content_hash == p.content_hash,
            "foreign admitted declaration anchor"
        );
        ensure!(
            witnesses
                .and_then(|items| items.get(&p.document))
                .is_some_and(|w| {
                    let Ok(id) = admitted_proof(w, p) else {
                        return false;
                    };
                    w.original.declaration_bindings.iter().any(|previous| {
                        previous.provenance_id == id
                            && previous.join == *old
                            && previous.syntax_id == b.syntax_id
                            && previous.symbols == b.symbols
                    })
                }),
            "binding differs from admitted association"
        );
    }
    ensure!(
        p.document == b.join.anchor.document,
        "foreign declaration binding"
    );
    ensure!(
        b.syntax_id.as_ref()
            == (b.join.status == JoinStatus::Exact).then(|| &b.join.candidate_ids[0]),
        "declaration binding ID mismatch"
    );
    ensure!(
        b.join.status != JoinStatus::Exact || !b.symbols.is_empty(),
        "exact binding without symbol"
    );
    let mut seen = BTreeSet::new();
    for key_ in &b.symbols {
        symbol_key(key_, revision)?;
        ensure!(
            seen.insert(native_ids::canonical(&serde_json::to_value(key_)?)),
            "duplicate binding symbol"
        );
    }
    Ok(())
}

/// Validates one reference against its resolved provenance row and the native reference
/// candidates.
fn validate_reference_row<'a>(
    r: &'a Reference,
    p: &'a Provenance,
    env: &RowEnv<'_>,
    native_references: &BTreeMap<&str, &native::Reference>,
    references: &mut BTreeSet<(&'a str, &'a str)>,
) -> Result<()> {
    let RowEnv {
        native,
        captured,
        revision,
        sources,
        decls,
        selected,
        witnesses,
    } = *env;
    ensure!(r.revision_id == revision.id, "foreign reference revision");
    let admitted = witnesses
        .and_then(|items| items.get(&r.document))
        .map(|w| original_reference(w, p))
        .transpose()?;
    if let Some(original) = admitted {
        ensure!(
            original.revision_id == captured.captured_revision.id,
            "foreign admitted reference revision"
        );
        ensure!(
            original.id == r.id
                && original.owner_syntax_id == r.owner_syntax_id
                && original.ordinal == r.ordinal
                && original.document == r.document
                && original.range == r.range
                && original.spelling == r.spelling
                && original.lookup_key == r.lookup_key
                && original.site == r.site
                && original.roles == r.roles,
            "reference differs from admitted source association"
        );
    }
    sid(&r.owner_syntax_id)?;
    occ(&r.id)?;
    uint(r.ordinal)?;
    ensure!(
        admitted.is_some()
            || native_references
                .get(r.id.as_str())
                .is_some_and(|candidate| {
                    candidate.owner_syntax_id == r.owner_syntax_id
                        && candidate.ordinal == r.ordinal
                        && candidate.document == r.document
                        && candidate.revision_id == r.revision_id
                        && candidate.range == r.range
                        && candidate.spelling == r.spelling
                        && candidate.lookup_key == r.lookup_key
                }),
        "reference differs from native reference candidate"
    );
    ensure!(
        admitted.is_some()
            || decls
                .get(&r.owner_syntax_id)
                .is_some_and(|d| d.document == r.document
                    && d.range.start <= r.range.start
                    && r.range.end <= d.range.end),
        "unmeasured reference owner"
    );
    if admitted.is_none() {
        let bytes = sources
            .get(&r.document)
            .context("missing reference source")?;
        ensure!(
            span(bytes, &r.range)? == r.spelling,
            "reference spelling mismatch"
        );
    }
    ensure!(!r.spelling.is_empty(), "empty reference spelling");
    ensure!(
        native::lookup(&r.document.language, &r.spelling)? == r.lookup_key,
        "reference lookup mismatch"
    );
    ordered(&r.roles)?;
    ensure!(
        !r.roles.is_empty() && (r.document.language != "java" || !r.roles.contains(&Role::Alias)),
        "invalid reference roles"
    );
    ensure!(
        !r.roles.contains(&Role::Alias) || r.roles.contains(&Role::Definition),
        "alias without definition"
    );
    if r.roles.contains(&Role::Call) && admitted.is_none() {
        ensure!(
            native.calls.iter().any(|call| call.document == r.document
                && call.owner_syntax_id == r.owner_syntax_id
                && call.range.start <= r.range.start
                && r.range.end <= call.range.end
                && call
                    .callee_range
                    .as_ref()
                    .is_none_or(|callee| callee == &r.range)
                && call.spelling.as_deref() == Some(&r.spelling)),
            "call role without measured invocation"
        );
    }
    ensure!(
        (r.site == ReferenceSite::Declaration) == r.roles.contains(&Role::Definition),
        "reference site/role mismatch"
    );
    if r.site == ReferenceSite::Declaration && admitted.is_none() {
        ensure!(
            native.declarations.iter().any(|d| {
                d.document == r.document
                    && d.name_range.as_ref() == Some(&r.range)
                    && d.name.as_deref() == Some(&r.spelling)
                    && declaration_name_owner(d, native) == Some(r.owner_syntax_id.as_str())
            }),
            "declaration reference site not measured"
        );
    }
    resolution(r.resolution, &r.declared_target, &r.candidates)?;
    if let Some(t) = &r.declared_target {
        target(t, native, decls, selected)?;
    }
    targets(&r.candidates, native, decls, true, selected)?;
    ensure!(
        references.insert((p.producer_id.as_str(), r.id.as_str())),
        "duplicate reference ID"
    );
    Ok(())
}

/// Validates one call binding against its resolved provenance row. `references` is the
/// publication's reference rows, which a witnessed call must agree with.
fn validate_call_binding_row(
    b: &CallBinding,
    proof: &Provenance,
    references: &[Reference],
    env: &RowEnv<'_>,
    call_facts: &mut BTreeSet<(String, native::DocumentKey, String, Vec<u8>)>,
    call_resolutions: &mut BTreeMap<(String, String), Vec<TargetClaim>>,
) -> Result<()> {
    let RowEnv {
        native,
        captured,
        revision: _,
        sources,
        decls,
        selected,
        witnesses,
    } = *env;
    ensure!(
        call_facts.insert((
            proof.producer_id.clone(),
            proof.document.clone(),
            b.provenance_id.clone(),
            native_ids::canonical(&serde_json::to_value(&b.join.anchor)?)
        )),
        "duplicate call binding fact"
    );
    let admitted = witnesses
        .and_then(|items| items.get(&proof.document))
        .map(|w| original_join(w, proof, JoinKind::Callee))
        .transpose()?;
    validate_join(&b.join, JoinKind::Callee, native, sources, admitted)?;
    if let Some(old) = admitted {
        ensure!(
            old.anchor.revision_id == captured.captured_revision.id
                && old.anchor.content_hash == proof.content_hash,
            "foreign admitted call anchor"
        );
        let witness = witnesses
            .and_then(|items| items.get(&proof.document))
            .context("missing call witness")?;
        let id = admitted_proof(witness, proof)?;
        ensure!(
            witness
                .original
                .call_bindings
                .iter()
                .any(|previous| previous.provenance_id == id
                    && previous.join == *old
                    && previous.call_id == b.call_id),
            "call differs from admitted source association"
        );
        let call = b.call_id.as_ref().context("unwitnessed call ID")?;
        let owner = &witness
            .original
            .references
            .iter()
            .find(|old| old.provenance_id == id)
            .context("missing admitted call reference")?
            .owner_syntax_id;
        ensure!(
            references.iter().any(|r| r.document == proof.document
                && r.provenance_id == b.provenance_id
                && r.roles.contains(&Role::Call)
                && (r.range == b.join.anchor.range
                    || (b.join.anchor.kind == JoinKind::Invocation
                        && b.join.anchor.range.start <= r.range.start
                        && r.range.end <= b.join.anchor.range.end))
                && &r.owner_syntax_id == owner),
            "call/reference witness mismatch"
        );
        occ(call)?;
    }
    ensure!(
        b.call_id.as_ref()
            == (b.join.status == JoinStatus::Exact).then(|| &b.join.candidate_ids[0]),
        "call binding ID mismatch"
    );
    resolution(b.resolution, &b.declared_target, &b.candidates)?;
    if let Some(call_id) = &b.call_id {
        let key = (proof.producer_id.clone(), call_id.clone());
        let target = b
            .declared_target
            .as_ref()
            .map(|t| native_ids::canonical(&serde_json::to_value(t).expect("target serializes")));
        let candidates = b
            .candidates
            .iter()
            .map(|t| native_ids::canonical(&serde_json::to_value(t).expect("target serializes")))
            .collect();
        call_resolutions
            .entry(key)
            .or_default()
            .push((b.resolution, target, candidates));
    }
    if let Some(t) = &b.declared_target {
        target(t, native, decls, selected)?;
    }
    targets(&b.candidates, native, decls, true, selected)?;
    targets(&b.possible_dispatch, native, decls, true, selected)?;
    ensure!(
        !b.possible_dispatch_complete && b.stale_target != Some(true),
        "unsupported dispatch completeness/stale target"
    );
    ensure!(
        b.stale_target
            == if matches!(b.declared_target, Some(Target::Internal { .. })) {
                Some(false)
            } else {
                None
            },
        "stale target mismatch"
    );
    Ok(())
}

/// Validates one type relationship against its resolved provenance row.
fn validate_type_relationship_row(
    r: &TypeRelationship,
    proof: &Provenance,
    env: &RowEnv<'_>,
    relationships: &mut BTreeSet<(String, native::DocumentKey, String, Vec<u8>)>,
) -> Result<()> {
    let RowEnv {
        native,
        decls,
        selected,
        witnesses,
        ..
    } = *env;
    if let Some(witness) = witnesses.and_then(|items| items.get(&proof.document)) {
        let id = admitted_proof(witness, proof)?;
        ensure!(
            witness.original.type_relationships.iter().any(|old| {
                old.provenance_id == id
                    && old.kind == r.kind
                    && matches!((&old.source, &r.source),
                        (Target::Internal { syntax_id: old_id, document: old_doc, .. },
                         Target::Internal { syntax_id: new_id, document: new_doc, .. })
                        if old_id == new_id && old_doc == new_doc)
            }),
            "relationship differs from admitted source association"
        );
    }
    ensure!(
        matches!(&r.source, Target::Internal { document, .. } if document == &proof.document),
        "relationship source/evidence document mismatch"
    );
    ensure!(
        relationships.insert((
            proof.producer_id.clone(),
            proof.document.clone(),
            r.provenance_id.clone(),
            native_ids::canonical(&serde_json::to_value((&r.kind, &r.source, &r.target))?)
        )),
        "duplicate relationship fact"
    );
    target(&r.source, native, decls, selected)?;
    target(&r.target, native, decls, selected)?;
    Ok(())
}

fn validate_evidence_inner(
    semantic: &SemanticPublication,
    native: &native::Artifact,
    captured: &CapturedComponents,
    raw: Option<ValidatedRaw<'_>>,
    attested: Option<&crate::store::AttestedProfileProducer>,
    witnessed: Option<(
        &BTreeMap<native::DocumentKey, WitnessedDocument>,
        &[TargetDeclaration],
    )>,
) -> Result<()> {
    let witnesses = witnessed.map(|(documents, _)| documents);
    ensure!(
        captured.native_extraction_components.is_empty(),
        "undeclared native extraction components"
    );
    let revision = &native.revision;
    ensure!(
        revision.source_set_id == native.source_set.id,
        "foreign native source set"
    );
    ensure!(
        captured.captured_revision.source_set_id == native.source_set.id,
        "unadmitted captured source set"
    );
    let mut sources = BTreeMap::new();
    let mut manifest = Vec::new();
    for doc in &captured.captured_revision.documents {
        key(&doc.key, &captured.captured_revision)?;
        ensure!(
            doc.revision_id == captured.captured_revision.id,
            "foreign native document revision"
        );
        ensure!(
            !sources.contains_key(&doc.key),
            "duplicate native manifest document"
        );
        if let Some(witness) = witnesses.and_then(|items| items.get(&doc.key)) {
            if let Some(wire_digest) = &witness.wire_digest {
                hash(wire_digest)?;
            } else {
                ensure!(
                    !semantic
                        .producers
                        .iter()
                        .any(|p| p
                            .languages
                            .iter()
                            .any(|language| serde_json::to_value(language)
                                .ok()
                                .and_then(|v| v.as_str().map(str::to_owned))
                                .as_deref()
                                == Some(&doc.key.language)))
                        && witness.original.producers.is_empty()
                        && witness.original.coverage.is_empty()
                        && witness.original.provenance.is_empty()
                        && witness.original.symbols.is_empty()
                        && witness.original.declaration_bindings.is_empty()
                        && witness.original.references.is_empty()
                        && witness.original.call_bindings.is_empty()
                        && witness.original.type_relationships.is_empty()
                        && raw.as_ref().is_some_and(|raw| raw
                            .inventory
                            .claims
                            .iter()
                            .all(|claim| claim.language != doc.key.language
                                || claim.path != doc.key.path)),
                    "missing producer-covered document witness"
                );
            }
            ensure!(
                !captured.sources.contains_key(&doc.key)
                    && witness.source_hash == doc.content_hash
                    && witness.captured_version == witness.current_version
                    && revision
                        .documents
                        .iter()
                        .any(|current| current.key == doc.key
                            && current.content_hash == doc.content_hash
                            && current.byte_length == doc.byte_length),
                "invalid unchanged source witness"
            );
        } else {
            let bytes = captured
                .sources
                .get(&doc.key)
                .context("absent captured source")?;
            #[cfg(feature = "test-support")]
            if witnessed.is_some() {
                crate::semantic_import::note_original_source_hash(&doc.key);
            }
            ensure!(
                doc.content_hash == native_ids::digest(b"", bytes)
                    && doc.byte_length == bytes.len(),
                "source digest/length mismatch"
            );
            sources.insert(doc.key.clone(), bytes.clone());
        }
        manifest.push(json!({"document":doc.key,"contentHash":doc.content_hash}));
    }
    ensure!(
        captured.sources.len() == sources.len()
            && witnesses.is_none_or(|items| items.keys().all(|key| captured
                .captured_revision
                .documents
                .iter()
                .any(|doc| &doc.key == key))),
        "extra captured source or witness"
    );
    let mut ordered_docs = captured
        .captured_revision
        .documents
        .iter()
        .map(|d| (language_rank(&d.key.language), d.key.path.as_bytes()))
        .collect::<Vec<_>>();
    let original = ordered_docs.clone();
    ordered_docs.sort();
    ensure!(
        original == ordered_docs && original.windows(2).all(|w| w[0] != w[1]),
        "unsorted native document manifest"
    );
    ensure!(
        captured.captured_revision.id == native_revision_id(captured),
        "unauthenticated captured native revision ID"
    );
    ensure!(
        captured.toolchain == native_ids::canonical(&captured.native_toolchain)
            && captured.config == native_ids::canonical(&json!(captured.native_config))
            && captured.dependency == native_ids::canonical(&json!(captured.native_dependencies)),
        "captured native component input mismatch"
    );
    if captured.captured_revision.id == revision.id {
        ensure!(
            captured.captured_revision == *revision,
            "same native revision has different captured snapshot"
        );
    }
    let manifest_hash = native_ids::digest(
        b"",
        &native_ids::canonical(&serde_json::Value::Array(manifest)),
    );
    for (declared, bytes) in [
        (
            &captured.captured_revision.toolchain_hash,
            &captured.toolchain,
        ),
        (&captured.captured_revision.config_hash, &captured.config),
        (
            &captured.captured_revision.dependency_hash,
            &captured.dependency,
        ),
    ] {
        hash(declared)?;
        ensure!(
            declared == &native_ids::digest(b"", bytes),
            "captured revision component mismatch"
        );
    }
    let mut producers = BTreeMap::new();
    for p in &semantic.producers {
        text(&p.id)?;
        text(&p.version)?;
        hash(&p.executable_hash)?;
        ordered(&p.languages)?;
        ensure!(!p.languages.is_empty(), "empty producer languages");
        if let Some(attested) = attested {
            ensure!(
                attested.matches(p),
                "authenticated profile producer mismatch"
            );
        } else {
            ensure!(
                p.executable_hash
                    == native_ids::digest(
                        b"",
                        captured
                            .producer_executables
                            .get(&p.id)
                            .context("missing semantic executable")?
                    ),
                "semantic executable mismatch"
            );
        }
        ensure!(
            producers.insert(p.id.as_str(), p).is_none(),
            "duplicate semantic producer"
        );
    }
    ensure!(
        captured.producer_executables.len()
            == if attested.is_some() {
                0
            } else {
                producers.len()
            }
            && captured.artifacts.len() == producers.len(),
        "unclaimed semantic bytes"
    );
    let mut requested_producers = BTreeMap::new();
    for p in &captured.requested_producers {
        text(&p.id)?;
        text(&p.version)?;
        hash(&p.executable_hash)?;
        ordered(&p.languages)?;
        ensure!(
            !p.languages.is_empty() && requested_producers.insert(p.id.as_str(), p).is_none(),
            "invalid requested semantic producer"
        );
    }
    let mut coverage = BTreeMap::new();
    for row in &semantic.coverage {
        let lang = validate_coverage_row(row, &producers, native)?;
        ensure!(
            coverage
                .insert(
                    (row.producer_id.as_str(), lang, row.document_path.as_str()),
                    row
                )
                .is_none(),
            "duplicate coverage tuple"
        );
    }
    for p in &semantic.producers {
        for d in &revision.documents {
            if p.languages.iter().any(|l| {
                serde_json::to_value(l)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .as_deref()
                    == Some(&d.key.language)
            }) {
                ensure!(
                    coverage.contains_key(&(
                        p.id.as_str(),
                        d.key.language.clone(),
                        d.key.path.as_str()
                    )),
                    "missing producer coverage tuple"
                );
            }
        }
    }
    if let Some(documents) = witnesses {
        ensure!(
            native
                .declarations
                .iter()
                .all(|d| !documents.contains_key(&d.document))
                && native
                    .references
                    .iter()
                    .all(|r| !documents.contains_key(&r.document))
                && native
                    .calls
                    .iter()
                    .all(|c| !documents.contains_key(&c.document))
                && native
                    .capability_reports
                    .iter()
                    .all(|r| !documents.contains_key(&r.document)),
            "witness overlaps measured native document"
        );
    }
    let decls: BTreeMap<_, _> = native
        .declarations
        .iter()
        .map(|d| (d.syntax_id.clone(), d))
        .collect();
    ensure!(
        decls.len() == native.declarations.len(),
        "duplicate measured declaration ID"
    );
    let selected: Option<BTreeMap<String, &TargetDeclaration>> =
        witnessed.map(|(_, rows)| rows.iter().map(|d| (d.syntax_id.clone(), d)).collect());
    if let Some((_, rows)) = witnessed {
        let selected = selected.as_ref().expect("selected declarations");
        ensure!(
            selected.len() == rows.len(),
            "duplicate current target declaration"
        );
        for d in rows {
            sid(&d.syntax_id)?;
            key(&d.document, revision)?;
            text(&d.name)?;
            ensure!(
                d.name_range.start < d.name_range.end,
                "invalid target name range"
            );
            let document = revision
                .documents
                .iter()
                .find(|doc| doc.key == d.document)
                .context("target declaration without current document")?;
            ensure!(
                d.name_range.end <= document.byte_length,
                "target name exceeds current document"
            );
            if let Some(measured) = decls.get(&d.syntax_id) {
                ensure!(
                    measured.document == d.document
                        && measured.name.as_deref() == Some(&d.name)
                        && measured.name_range.as_ref() == Some(&d.name_range),
                    "current target contradicts measurement"
                );
            }
        }
    }
    let mut artifact_hashes = BTreeMap::new();
    for (producer_id, bytes) in &captured.artifacts {
        let artifact_hash = if let Some(raw) = &raw {
            ensure!(
                raw.producer_id == producer_id && raw.binary_bytes == bytes.as_slice(),
                "captured artifact differs from admitted raw binary"
            );
            let digest = native_ids::digest(b"", raw.binary_bytes);
            ensure!(
                raw.inventory.artifact_hash == digest,
                "raw binary identity mismatch"
            );
            digest
        } else {
            native_ids::digest(b"", bytes)
        };
        artifact_hashes.insert(producer_id, artifact_hash);
    }
    let mut proof = BTreeMap::new();
    for p in &semantic.provenance {
        text(&p.id)?;
        key(&p.document, revision)?;
        hash(&p.content_hash)?;
        if let Some(witness) = witnesses.and_then(|items| items.get(&p.document)) {
            ensure!(p.derived_from.is_some(), "unwitnessed captured proof");
            admitted_proof(witness, p)?;
        }
        let identity = raw.as_ref().map(|raw| RawCaptureIdentity {
            producer_id: raw.producer_id,
            source_set_id: raw.source_set_id,
            captured_revision: raw.captured_revision,
        });
        let lookup = |proof: &str, kind: &str, digest: &str| -> Result<RawClaimDocument> {
            let claim = raw
                .as_ref()
                .context("derived provenance requires authenticated raw inventory")?
                .inventory
                .lookup(proof, kind, digest)?;
            Ok(RawClaimDocument {
                language: claim.language.clone(),
                path: claim.path.clone(),
            })
        };
        validate_provenance_row(
            p,
            identity
                .as_ref()
                .map(|identity| (identity, &lookup as &ClaimLookup<'_>)),
            captured,
            revision,
        )?;
        let row = coverage
            .get(&(
                p.producer_id.as_str(),
                p.document.language.clone(),
                p.document.path.as_str(),
            ))
            .context("provenance without coverage")?;
        ensure!(
            matches!(row.state, CoverageState::Complete | CoverageState::Partial),
            "proof on non-fact-bearing coverage"
        );
        let basis = p
            .basis
            .as_ref()
            .context("semantic proof requires captured basis")?;
        let producer = producers
            .get(p.producer_id.as_str())
            .context("unknown proof producer")?;
        validate_basis(
            p,
            basis,
            producer,
            captured,
            artifact_hashes.get(&p.producer_id),
            raw.as_ref().filter(|_| witnessed.is_some()).map(|raw| {
                (
                    raw.inventory.artifact_hash.as_str(),
                    raw.inventory.source_manifest_hash.as_str(),
                )
            }),
            &manifest_hash,
        )?;
        let document = captured
            .captured_revision
            .documents
            .iter()
            .find(|d| d.key == p.document)
            .context("missing captured provenance document")?;
        ensure!(
            p.content_hash == document.content_hash,
            "captured proof document hash mismatch"
        );
        let requested = revision.documents.iter().find(|d| d.key == p.document);
        let requested_producer = requested_producers.get(p.producer_id.as_str());
        let freshness = if requested.is_none_or(|d| d.content_hash != p.content_hash) {
            Freshness::Stale
        } else if basis.revision_id != revision.id
            || basis.source_manifest_hash != requested_manifest_hash(revision)
            || basis.toolchain_hash != revision.toolchain_hash
            || basis.config_hash != revision.config_hash
            || basis.dependency_hash != revision.dependency_hash
            || basis.source_set_id != revision.source_set_id
            || requested_producer.is_none_or(|current| {
                current.version != basis.producer_version
                    || current.executable_hash != basis.producer_hash
                    || !current.languages.contains(&basis.language)
            })
        {
            Freshness::PossiblyStale
        } else {
            Freshness::Fresh
        };
        ensure!(
            p.freshness == freshness,
            "claimed freshness differs from captured basis"
        );
        ensure!(
            p.evidence_kind != EvidenceKind::MeasuredSyntax,
            "native proof in semantic producer"
        );
        ensure!(
            proof.insert(p.id.as_str(), p).is_none(),
            "duplicate semantic provenance"
        );
    }
    let fact =
        |id: &str, doc: Option<&native::DocumentKey>, kind: EvidenceKind| -> Result<&Provenance> {
            let p = *proof.get(id).context("dangling semantic fact provenance")?;
            ensure!(
                p.evidence_kind == kind
                    && doc.is_none_or(|d| d == &p.document)
                    && p.revision_id == revision.id
                    && coverage.contains_key(&(
                        p.producer_id.as_str(),
                        p.document.language.clone(),
                        p.document.path.as_str()
                    )),
                "semantic fact/provenance tuple mismatch"
            );
            Ok(p)
        };
    for (doc, witness) in witnesses.into_iter().flat_map(|items| items.iter()) {
        let producer_id = &raw.as_ref().context("missing raw witness")?.producer_id;
        ensure!(
            witness
                .original
                .provenance
                .iter()
                .all(|p| p.document == *doc),
            "witness contains foreign proof document"
        );
        let admitted_ids: BTreeSet<_> = witness
            .original
            .provenance
            .iter()
            .map(|p| p.id.as_str())
            .collect();
        let admitted: BTreeSet<_> = witness
            .original
            .provenance
            .iter()
            .filter(|p| p.document == *doc && p.producer_id == *producer_id)
            .filter_map(|p| {
                p.derived_from.as_ref().map(|d| {
                    (
                        &d.provenance_id,
                        d.record_kind,
                        &d.fact_digest,
                        p.evidence_kind,
                    )
                })
            })
            .collect();
        ensure!(
            admitted_ids.len() == witness.original.provenance.len()
                && admitted.len() == witness.original.provenance.len(),
            "duplicate or non-derived admitted proof"
        );
        ensure!(
            witness.original.references.iter().all(|r| witness
                .original
                .provenance
                .iter()
                .any(|p| p.id == r.provenance_id))
                && witness.original.declaration_bindings.iter().all(|b| witness
                    .original
                    .provenance
                    .iter()
                    .any(|p| p.id == b.provenance_id))
                && witness.original.call_bindings.iter().all(|b| witness
                    .original
                    .provenance
                    .iter()
                    .any(|p| p.id == b.provenance_id)),
            "foreign admitted association proof"
        );
        let output: BTreeSet<_> = semantic
            .provenance
            .iter()
            .filter(|p| p.document == *doc && p.producer_id == *producer_id)
            .filter_map(|p| {
                p.derived_from.as_ref().map(|d| {
                    (
                        &d.provenance_id,
                        d.record_kind,
                        &d.fact_digest,
                        p.evidence_kind,
                    )
                })
            })
            .collect();
        ensure!(
            admitted == output
                && output.len()
                    == semantic
                        .provenance
                        .iter()
                        .filter(|p| p.document == *doc && p.producer_id == *producer_id)
                        .count(),
            "incomplete witnessed document proof inventory"
        );
    }
    let env = RowEnv {
        native,
        captured,
        revision,
        sources: &sources,
        decls: &decls,
        selected: selected.as_ref(),
        witnesses,
    };
    let mut symbols = BTreeSet::new();
    for s in &semantic.symbols {
        let p = fact(&s.provenance_id, None, EvidenceKind::DeclarationBinding)?;
        symbol_key(&s.key, revision)?;
        if let Some(name) = &s.display_name {
            text(name)?;
        }
        if let Some(doc) = &s.key.document {
            ensure!(doc == &p.document, "symbol evidence document mismatch");
        }
        targets(&s.declarations, native, &decls, false, selected.as_ref())?;
        if let Some(witness) = witnesses.and_then(|items| items.get(&p.document)) {
            let id = admitted_proof(witness, p)?;
            ensure!(
                witness
                    .original
                    .symbols
                    .iter()
                    .any(|old| old.provenance_id == id
                        && old.key == s.key
                        && old.display_name == s.display_name
                        && old.declarations.len() == s.declarations.len()
                        && old.declarations.iter().zip(&s.declarations).all(
                            |(before, after)| match (before, after) {
                                (
                                    Target::Internal { document: a, .. },
                                    Target::Internal { document: b, .. },
                                ) => a == b,
                                (
                                    Target::External { symbol: a },
                                    Target::External { symbol: b },
                                ) => a == b,
                                _ => false,
                            }
                        )),
                "symbol differs from admitted association"
            );
        }
        ensure!(
            symbols.insert((
                p.producer_id.clone(),
                native_ids::canonical(&serde_json::to_value(&s.key)?)
            )),
            "duplicate semantic symbol key"
        );
    }
    let mut binding_facts = BTreeSet::new();
    for b in &semantic.declaration_bindings {
        let p = fact(
            &b.provenance_id,
            Some(&b.join.anchor.document),
            EvidenceKind::DeclarationBinding,
        )?;
        validate_declaration_binding_row(b, p, &env, &mut binding_facts)?;
    }
    let mut references = BTreeSet::new();
    let native_references: BTreeMap<_, _> = native
        .references
        .iter()
        .map(|r| (r.id.as_str(), r))
        .collect();
    ensure!(
        native_references.len() == native.references.len(),
        "duplicate native reference ID"
    );
    for r in &semantic.references {
        let p = fact(
            &r.provenance_id,
            Some(&r.document),
            EvidenceKind::SemanticReference,
        )?;
        validate_reference_row(r, p, &env, &native_references, &mut references)?;
    }
    let mut call_facts = BTreeSet::new();
    let mut call_resolutions: BTreeMap<(String, String), Vec<TargetClaim>> = BTreeMap::new();
    for b in &semantic.call_bindings {
        let proof = fact(
            &b.provenance_id,
            Some(&b.join.anchor.document),
            EvidenceKind::SemanticReference,
        )?;
        validate_call_binding_row(
            b,
            proof,
            &semantic.references,
            &env,
            &mut call_facts,
            &mut call_resolutions,
        )?;
    }
    for proofs in call_resolutions.values() {
        let mut asserted = BTreeSet::new();
        for (_, target, candidates) in proofs {
            if let Some(target) = target {
                asserted.insert(target.clone());
            }
            asserted.extend(candidates.iter().cloned());
        }
        if asserted.len() > 1 {
            let all: Vec<_> = asserted.into_iter().collect();
            ensure!(
                proofs
                    .iter()
                    .all(
                        |(resolution, declared, candidates)| *resolution == Resolution::Ambiguous
                            && declared.is_none()
                            && *candidates == all
                    ),
                "contradictory resolved call targets"
            );
        }
    }
    let mut relationships = BTreeSet::new();
    for r in &semantic.type_relationships {
        let proof = fact(&r.provenance_id, None, EvidenceKind::TypeRelationship)?;
        validate_type_relationship_row(r, proof, &env, &mut relationships)?;
    }
    for ((producer, lang, path), row) in &coverage {
        let observed = |id: &str| {
            proof.get(id).is_some_and(|p| {
                p.producer_id == *producer
                    && p.document.language == *lang
                    && p.document.path == *path
            })
        };
        for role in &[
            Role::Definition,
            Role::Read,
            Role::Write,
            Role::Call,
            Role::Type,
            Role::Import,
            Role::Alias,
        ] {
            let from_reference = semantic
                .references
                .iter()
                .any(|r| observed(&r.provenance_id) && r.roles.contains(role));
            let from_binding = *role == Role::Definition
                && semantic
                    .declaration_bindings
                    .iter()
                    .any(|b| observed(&b.provenance_id) && b.syntax_id.is_some());
            let from_call = *role == Role::Call
                && semantic
                    .call_bindings
                    .iter()
                    .any(|b| observed(&b.provenance_id) && b.call_id.is_some());
            let from_type = *role == Role::Type
                && semantic
                    .type_relationships
                    .iter()
                    .any(|r| observed(&r.provenance_id));
            ensure!(
                row.observed_roles.contains(role)
                    == (from_reference || from_binding || from_call || from_type),
                "coverage observed roles differ from admitted evidence"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod witness_tests {
    use super::*;

    #[test]
    fn admitted_reference_requires_exact_proof_and_one_original_association() {
        let document = native::DocumentKey {
            source_set_id: "set".into(),
            language: "python".into(),
            path: "a.py".into(),
        };
        let derived = DerivedFrom {
            provenance_id: "proof".into(),
            record_kind: RecordKind::Occurrence,
            fact_digest: "f".repeat(64),
        };
        let proof = Provenance {
            id: "new".into(),
            producer_id: "producer".into(),
            document: document.clone(),
            revision_id: "revision:new".into(),
            content_hash: "a".repeat(64),
            evidence_kind: EvidenceKind::SemanticReference,
            basis: None,
            freshness: Freshness::PossiblyStale,
            derived_from: Some(derived.clone()),
        };
        let previous = Provenance {
            id: "old".into(),
            revision_id: "revision:old".into(),
            ..proof.clone()
        };
        let reference = Reference {
            id: "occ:v2:test".into(),
            owner_syntax_id: "sid:v2:test".into(),
            ordinal: 0,
            document,
            revision_id: "revision:old".into(),
            range: native::Range { start: 0, end: 1 },
            spelling: "a".into(),
            lookup_key: "a".into(),
            site: ReferenceSite::Use,
            roles: vec![Role::Read],
            resolution: Resolution::Unresolved,
            declared_target: None,
            candidates: vec![],
            provenance_id: "old".into(),
        };
        let mut original = SemanticSnapshot {
            producers: vec![],
            coverage: vec![],
            provenance: vec![previous],
            symbols: vec![],
            declaration_bindings: vec![],
            references: vec![reference],
            call_bindings: vec![],
            type_relationships: vec![],
        };
        let mut witness = WitnessedDocument {
            source_hash: "a".repeat(64),
            wire_digest: Some("b".repeat(64)),
            captured_version: "v".into(),
            current_version: "v".into(),
            original: original.clone(),
        };
        assert_eq!(original_reference(&witness, &proof).unwrap().spelling, "a");
        original.references.push(original.references[0].clone());
        witness.original = original;
        assert!(
            original_reference(&witness, &proof)
                .unwrap_err()
                .to_string()
                .contains("fused or ambiguous")
        );
        witness.original.references.pop();
        let mut wrong = proof;
        wrong.derived_from.as_mut().unwrap().fact_digest = "0".repeat(64);
        assert!(
            original_reference(&witness, &wrong)
                .unwrap_err()
                .to_string()
                .contains("missing admitted proof")
        );
    }
}
