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
/// A span decoded with its whole source, as rows did before #477; the oracle of
/// `DocumentIndex::span`.
#[cfg(test)]
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
                "java" | "rust" | "python" | "javascript" | "typescript"
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
        // A Cargo target namespace, named by its Rust crate root (owner ruling 2026-10-30,
        // #394). A local symbol is document-scoped, never target-scoped.
        (SymbolScope::Target, Some(d)) => {
            key(d, revision)?;
            ensure!(
                d.language == "rust" && !key_.symbol.starts_with("local "),
                "symbol scope/document mismatch"
            );
        }
        _ => anyhow::bail!("symbol scope/document mismatch"),
    }
    Ok(())
}
/// Whether an internal target names a measured declaration of its document.
type KnownTarget<'a> = &'a dyn Fn(&str, &native::DocumentKey) -> Result<bool>;

fn target(t: &Target, revision: &native::Revision, known: KnownTarget<'_>) -> Result<()> {
    match t {
        Target::Internal {
            syntax_id,
            document,
            revision_id,
        } => {
            sid(syntax_id)?;
            ensure!(
                revision_id == &revision.id,
                "foreign internal target revision"
            );
            ensure!(known(syntax_id, document)?, "unmeasured internal target");
        }
        Target::External { symbol } => {
            symbol_key(symbol, revision)?;
            // A target-scoped key names a namespace inside the artifact (#394).
            ensure!(
                symbol.scope != SymbolScope::Target,
                "target-scoped external symbol"
            );
        }
    }
    Ok(())
}
fn targets(
    items: &[Target],
    revision: &native::Revision,
    known: KnownTarget<'_>,
    sorted: bool,
) -> Result<()> {
    let mut seen = BTreeSet::new();
    for item in items {
        target(item, revision, known)?;
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

// Validation work counted by the linear-growth test (#477): source bytes decoded and native
// facts or gaps visited.
#[cfg(test)]
thread_local! {
    static VALIDATION_WORK: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
#[inline]
fn count_work(_units: usize) {
    #[cfg(test)]
    VALIDATION_WORK.with(|work| work.set(work.get() + _units as u64));
}

/// What every join and reference row of a document reads from its measured native facts,
/// prepared once per document (#477): the document's source decoded as UTF-8 once, its
/// declaration names, callees, invocations and references ordered by range, and each family's
/// capability support validated once. A row then looks up its own range instead of rescanning
/// the document, so a document's validation work grows with its rows, not rows times facts.
/// Every check is still made on the same inputs; only the repetition goes. The index holds
/// positions into the artifact it was built from, which every lookup passes back.
pub(crate) struct NativeIndex {
    documents: BTreeMap<native::DocumentKey, DocumentIndex>,
}

/// A measured range and the position of the fact that has it.
type Ranged = ((usize, usize), usize);

/// One document's part of a `NativeIndex`. Each list is ordered by range.
#[derive(Default)]
struct DocumentIndex {
    /// The first revision document with this key.
    document: Option<usize>,
    /// Declarations by name range (those with one).
    names: Vec<Ranged>,
    /// Calls by callee range (those with one).
    callees: Vec<Ranged>,
    /// Spelled calls without a callee range, by spelling.
    bare_calls: Vec<usize>,
    /// Calls by invocation range.
    invocations: Vec<Ranged>,
    references: Vec<Ranged>,
    /// Set once the document's whole source has decoded as UTF-8.
    utf8: std::cell::OnceCell<()>,
    /// Each family's capability support once validated, by `JoinKind`.
    support: [std::cell::OnceCell<SupportIndex>; 4],
}

/// One family's capability support, validated once for its document.
struct SupportIndex {
    report: usize,
    family: usize,
    /// The running maximum of gap ends when the gaps are ordered by start; otherwise a row
    /// scans the gaps in order.
    gap_ends: Option<Vec<usize>>,
}

fn ranged(range: &native::Range, position: usize) -> Ranged {
    ((range.start, range.end), position)
}

/// The items whose indexed range equals `range`.
fn at<'n, T>(
    list: &'n [Ranged],
    items: &'n [T],
    range: &native::Range,
) -> impl Iterator<Item = &'n T> + 'n {
    let target = (range.start, range.end);
    let first = list.partition_point(|(r, _)| *r < target);
    list[first..]
        .iter()
        .take_while(move |(r, _)| *r == target)
        .map(|(_, position)| {
            count_work(1);
            &items[*position]
        })
}

impl NativeIndex {
    /// An upper bound of the bytes `new` allocates for `native`, with every family's gap ends
    /// that its rows may add later; admitted to the live budget before the build. Saturates,
    /// so an overflowing bound is refused by the budget.
    ///
    /// - The map: a non-root B-tree node holds at least 5 entries, so `d` documents need at
    ///   most `2 * (d / 5 + 1)` nodes, each at most an internal node (header, 11 keys and
    ///   values, 12 edges); plus each key's cloned strings.
    /// - The lists are pushed (gap ends collected), so each non-empty list holds at most
    ///   `max(2 * len, 4)` slots: twice the entries plus four slots for each of a document's
    ///   five lists (gap ends: for each family).
    ///
    /// It allocates nothing itself, since it runs before the admission (#476): documents are
    /// counted as runs of equal keys over the facts, an upper bound of the distinct ones.
    pub(crate) fn bytes(native: &native::Artifact) -> u64 {
        let mut previous: Option<&native::DocumentKey> = None;
        let (documents, strings) = native
            .revision
            .documents
            .iter()
            .map(|d| &d.key)
            .chain(native.declarations.iter().map(|d| &d.document))
            .chain(native.calls.iter().map(|c| &c.document))
            .chain(native.references.iter().map(|r| &r.document))
            .filter(|key| previous.replace(*key) != Some(*key))
            .fold((0u64, 0u64), |(documents, strings), key| {
                let length = key.source_set_id.len() + key.language.len() + key.path.len();
                (documents + 1, strings.saturating_add(length as u64))
            });
        let word = size_of::<usize>() as u64;
        let node = (2 * word
            + 11 * (size_of::<native::DocumentKey>() + size_of::<DocumentIndex>()) as u64)
            .next_multiple_of(word)
            + 12 * word;
        let map = (2 * (documents / 5 + 1)).saturating_mul(node);
        let entries = (native.declarations.len() as u64)
            .saturating_add((native.calls.len() as u64).saturating_mul(2))
            .saturating_add(native.references.len() as u64);
        let lists = entries
            .saturating_mul(2)
            .saturating_add(documents.saturating_mul(5 * 4))
            .saturating_mul(size_of::<Ranged>() as u64);
        let (gaps, families) = native
            .capability_reports
            .iter()
            .flat_map(|report| &report.families)
            .fold((0u64, 0u64), |(gaps, families), family| {
                (gaps.saturating_add(family.gaps.len() as u64), families + 1)
            });
        let ends = gaps
            .saturating_mul(2)
            .saturating_add(families.saturating_mul(4))
            .saturating_mul(word);
        [map, strings, lists, ends]
            .into_iter()
            .fold(0, u64::saturating_add)
    }

    /// Indexes every document of `native` in one pass over its facts.
    pub(crate) fn new(native: &native::Artifact) -> Self {
        fn entry<'m>(
            documents: &'m mut BTreeMap<native::DocumentKey, DocumentIndex>,
            key: &native::DocumentKey,
        ) -> &'m mut DocumentIndex {
            if !documents.contains_key(key) {
                documents.insert(key.clone(), DocumentIndex::default());
            }
            documents
                .get_mut(key)
                .expect("document entry inserted above")
        }
        let mut documents = BTreeMap::new();
        for (position, d) in native.revision.documents.iter().enumerate() {
            entry(&mut documents, &d.key)
                .document
                .get_or_insert(position);
        }
        for (position, d) in native.declarations.iter().enumerate() {
            count_work(1);
            if let Some(range) = &d.name_range {
                entry(&mut documents, &d.document)
                    .names
                    .push(ranged(range, position));
            }
        }
        for (position, c) in native.calls.iter().enumerate() {
            count_work(1);
            let index = entry(&mut documents, &c.document);
            index.invocations.push(ranged(&c.range, position));
            match &c.callee_range {
                Some(range) => index.callees.push(ranged(range, position)),
                None if c.spelling.is_some() => index.bare_calls.push(position),
                None => {}
            }
        }
        for (position, r) in native.references.iter().enumerate() {
            count_work(1);
            entry(&mut documents, &r.document)
                .references
                .push(ranged(&r.range, position));
        }
        for index in documents.values_mut() {
            index.names.sort_unstable();
            index.callees.sort_unstable();
            index.invocations.sort_unstable();
            index.references.sort_unstable();
            index
                .bare_calls
                .sort_unstable_by_key(|position| (&native.calls[*position].spelling, *position));
        }
        Self { documents }
    }

    /// The index of `document`, or `empty` when the artifact has nothing for it.
    fn document<'i>(
        &'i self,
        document: &native::DocumentKey,
        empty: &'i std::cell::OnceCell<DocumentIndex>,
    ) -> &'i DocumentIndex {
        self.documents
            .get(document)
            .unwrap_or_else(|| empty.get_or_init(DocumentIndex::default))
    }
}

impl DocumentIndex {
    /// `span` over this document's source, decoding the whole source once: a later span checks
    /// its bounds and scalar boundaries and decodes only its own bytes.
    fn span<'s>(&self, bytes: &'s [u8], range: &native::Range) -> Result<&'s str> {
        uint(range.start)?;
        uint(range.end)?;
        if self.utf8.get().is_none() {
            count_work(bytes.len());
            std::str::from_utf8(bytes)?;
            let _ = self.utf8.set(());
        }
        ensure!(range.start < range.end, "empty semantic span");
        // In valid UTF-8, a scalar starts at every byte that is not a continuation byte.
        let boundary =
            |i: usize| i == bytes.len() || bytes.get(i).is_some_and(|b| (*b as i8) >= -0x40);
        bytes
            .get(range.start..range.end)
            .filter(|_| boundary(range.start) && boundary(range.end))
            .and_then(|slice| {
                count_work(slice.len());
                std::str::from_utf8(slice).ok()
            })
            .context("invalid converted semantic span")
    }

    fn revision_document<'n>(&self, native: &'n native::Artifact) -> Option<&'n native::Document> {
        self.document
            .map(|position| &native.revision.documents[position])
    }

    /// The measured ranges of an anchor kind's family.
    fn measured(&self, kind: JoinKind) -> &[Ranged] {
        match kind {
            JoinKind::DeclarationName => &self.names,
            JoinKind::Callee => &self.callees,
            JoinKind::Invocation => &self.invocations,
            JoinKind::Reference => &self.references,
        }
    }

    /// The capability support of an anchor kind's family in this document, validated on first
    /// use: its report and family exist, an unavailable family measured nothing, and every gap
    /// is a valid span with a diagnostic and contains no measured range. The result is kept by
    /// family only: callers pass the artifact's own revision (`validate_join` has checked the
    /// anchor's) and the document's one source.
    fn support<'n>(
        &self,
        native: &'n native::Artifact,
        document: &native::DocumentKey,
        revision_id: &str,
        kind: JoinKind,
        source: &[u8],
    ) -> Result<(&'n native::MeasurementSupport, &SupportIndex)> {
        let (measurement, slot) = match kind {
            JoinKind::DeclarationName => (native::MeasurementKind::DeclarationName, 0),
            JoinKind::Callee => (native::MeasurementKind::Callee, 1),
            JoinKind::Invocation => (native::MeasurementKind::Invocation, 2),
            JoinKind::Reference => (native::MeasurementKind::Reference, 3),
        };
        let cell = &self.support[slot];
        if let Some(index) = cell.get() {
            let support = &native.capability_reports[index.report].families[index.family];
            return Ok((support, index));
        }
        let report = native
            .capability_reports
            .iter()
            .position(|report| {
                count_work(1);
                report.document == *document && report.revision_id == revision_id
            })
            .context("missing native capability report")?;
        let family = native.capability_reports[report]
            .families
            .iter()
            .position(|support| support.kind == measurement)
            .context("missing native measurement family")?;
        let support = &native.capability_reports[report].families[family];
        let measured = self.measured(kind);
        ensure!(
            support.available || measured.is_empty(),
            "unavailable family has measured candidates"
        );
        for gap in &support.gaps {
            self.span(source, &gap.range)?;
            text(&gap.diagnostic)?;
            // A measured range inside the gap starts inside it.
            let first = measured.partition_point(|((start, _), _)| *start < gap.range.start);
            let contradiction = measured[first..]
                .iter()
                .take_while(|((start, _), _)| {
                    count_work(1);
                    *start <= gap.range.end
                })
                .any(|((_, end), _)| *end <= gap.range.end);
            ensure!(
                !contradiction,
                "measured candidate contradicts native capability gap"
            );
        }
        let gap_ends = SupportIndex::gap_ends(&support.gaps);
        let index = cell.get_or_init(|| SupportIndex {
            report,
            family,
            gap_ends,
        });
        Ok((support, index))
    }
}

impl SupportIndex {
    /// The running maximum of `gaps`' ends, when they are ordered by start.
    fn gap_ends(gaps: &[native::MeasurementGap]) -> Option<Vec<usize>> {
        count_work(gaps.len());
        gaps.windows(2)
            .all(|pair| pair[0].range.start <= pair[1].range.start)
            .then(|| {
                gaps.iter()
                    .scan(0, |end, gap| {
                        *end = gap.range.end.max(*end);
                        Some(*end)
                    })
                    .collect()
            })
    }

    /// The first gap of `support`, in its order, that contains `range`.
    fn containing<'n>(
        &self,
        support: &'n native::MeasurementSupport,
        range: &native::Range,
    ) -> Option<&'n native::MeasurementGap> {
        let contains = |gap: &native::MeasurementGap| {
            gap.range.start <= range.start && range.end <= gap.range.end
        };
        match &self.gap_ends {
            // The gaps that start at or before `range` are a prefix, and the first of them to
            // reach `range.end` is where their running maximum end first does.
            Some(ends) => {
                let starts = support
                    .gaps
                    .partition_point(|gap| gap.range.start <= range.start);
                let first = ends[..starts].partition_point(|end| *end < range.end);
                support.gaps[..starts]
                    .get(first)
                    .filter(|gap| contains(gap))
            }
            None => support.gaps.iter().find(|gap| {
                count_work(1);
                contains(gap)
            }),
        }
    }
}

fn validate_join(
    join: &Join,
    family: JoinKind,
    native: &native::Artifact,
    index: &NativeIndex,
    sources: Sources<'_>,
) -> Result<()> {
    let a = &join.anchor;
    ensure!(
        a.kind == family || (family == JoinKind::Callee && a.kind == JoinKind::Invocation),
        "wrong join family"
    );
    let empty = std::cell::OnceCell::new();
    let measured = index.document(&a.document, &empty);
    let source = sources(&a.document).context("missing join source")?;
    let slice = measured.span(source, &a.range)?;
    let doc = measured
        .revision_document(native)
        .context("missing join document")?;
    ensure!(
        a.revision_id == native.revision.id && a.content_hash == doc.content_hash,
        "foreign join anchor"
    );
    let mut candidates = BTreeSet::new();
    match family {
        JoinKind::DeclarationName => {
            for d in at(&measured.names, &native.declarations, &a.range) {
                if d.document == a.document
                    && d.name_range.as_ref() == Some(&a.range)
                    && d.name.as_deref() == Some(slice)
                {
                    candidates.insert(d.syntax_id.clone());
                }
            }
        }
        JoinKind::Callee => {
            let calls = if a.kind == JoinKind::Invocation {
                &measured.invocations
            } else {
                &measured.callees
            };
            for call in at(calls, &native.calls, &a.range) {
                let r = if a.kind == JoinKind::Invocation {
                    Some(&call.range)
                } else {
                    call.callee_range.as_ref()
                };
                if call.document == a.document
                    && r == Some(&a.range)
                    && (a.kind == JoinKind::Invocation || call.spelling.as_deref() == Some(slice))
                {
                    candidates.insert(call.id.clone());
                }
            }
        }
        JoinKind::Reference => {
            for reference in at(&measured.references, &native.references, &a.range) {
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
    let (support, gaps) = measured.support(native, &a.document, &a.revision_id, a.kind, source)?;
    let unsupported = if !support.available {
        Some(
            support
                .diagnostic
                .as_deref()
                .context("unavailable family without diagnostic")?,
        )
    } else {
        ensure!(support.diagnostic.is_none(), "enabled family diagnostic");
        gaps.containing(support, &a.range)
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
    let doc = measured
        .revision_document(native)
        .context("missing join document")?;
    ensure!(
        a.revision_id == native.revision.id && a.content_hash == doc.content_hash,
        "foreign join anchor"
    );
    Ok(())
}

#[cfg(test)]
fn admitted_proof<'a>(witness: &'a WitnessedDocument, proof: &Provenance) -> Result<&'a str> {
    let derived = proof
        .derived_from
        .as_ref()
        .context("unwitnessed derived proof")?;
    let mut matches = witness
        .index
        .proofs
        .get(&derived.provenance_id)
        .into_iter()
        .flatten()
        .map(|position| &witness.original.provenance[*position])
        .filter(|p| {
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

#[cfg(test)]
fn original_reference<'a>(
    witness: &'a WitnessedDocument,
    proof: &Provenance,
) -> Result<&'a Reference> {
    let id = admitted_proof(witness, proof)?;
    let mut matches = witness.references(id);
    let original = matches
        .next()
        .context("missing admitted reference association")?;
    ensure!(
        matches.next().is_none(),
        "fused or ambiguous admitted reference association"
    );
    Ok(original)
}

pub const LANGUAGE_ORDER: [&str; 5] = ["java", "rust", "python", "javascript", "typescript"];

pub fn language_order(language: &str) -> Option<usize> {
    LANGUAGE_ORDER.iter().position(|name| *name == language)
}

fn language_rank(language: &str) -> usize {
    language_order(language).unwrap_or_else(|| unreachable!("validated language"))
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
    validate_evidence_inner(semantic, native, captured, None, None)
}

/// The whole-publication validator a profile-pinned batch runs, over an in-memory admission of
/// the batch instead of a stored raw batch. Every cross-row check runs; the first producer is
/// the attested profile producer.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn validate_evidence_profile_pinned_for_tests(
    semantic: &SemanticPublication,
    native: &native::Artifact,
    captured: &CapturedComponents,
    inventory: &crate::scip_raw::CapturedScipInventory,
) -> Result<()> {
    let producer = semantic.producers.first().context("no producer")?;
    let attested = crate::store::AttestedProfileProducer::pinned_for_tests(producer);
    let raw = ValidatedRaw {
        producer_id: &producer.id,
        source_set_id: &captured.captured_revision.source_set_id,
        captured_revision: &captured.captured_revision.id,
        binaries: captured
            .artifacts
            .iter()
            .map(|((_, digest), bytes)| (digest.clone(), &bytes[..]))
            .collect(),
        inventory: inventory.proof_identities(),
    };
    validate_evidence_inner(semantic, native, captured, Some(raw), Some(&attested))
}

struct ValidatedRaw<'a> {
    producer_id: &'a str,
    source_set_id: &'a str,
    captured_revision: &'a str,
    /// Every original artifact of the capture, keyed by its SHA-256, so each captured artifact is
    /// compared with its own binary.
    binaries: BTreeMap<String, &'a [u8]>,
    inventory: crate::scip_raw::ProofInventory,
}
#[cfg(feature = "test-support")]
impl<'a> ValidatedRaw<'a> {
    fn from_batch(
        raw: &'a crate::store::SelectedRawBatch,
        inventory: &crate::scip_raw::CapturedScipInventory,
    ) -> Self {
        Self {
            producer_id: &raw.producer_id,
            source_set_id: &raw.source_set_id,
            captured_revision: &raw.captured_revision,
            binaries: raw
                .captures
                .iter()
                .map(|capture| (capture.artifact_hash.clone(), &capture.binary_bytes[..]))
                .collect(),
            inventory: inventory.proof_identities(),
        }
    }
}

/// The whole-publication validator over a stored batch of one or more original artifacts: each
/// captured artifact must equal the stored binary with its own digest.
#[cfg(feature = "test-support")]
pub(crate) fn validate_evidence_with_attested_batch(
    semantic: &SemanticPublication,
    native: &native::Artifact,
    captured: &CapturedComponents,
    raw: &crate::store::SelectedRawBatch,
    inventory: &crate::scip_raw::CapturedScipInventory,
    attested: &crate::store::AttestedProfileProducer,
) -> Result<()> {
    validate_evidence_inner(
        semantic,
        native,
        captured,
        Some(ValidatedRaw::from_batch(raw, inventory)),
        Some(attested),
    )
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct WitnessedDocument {
    original: SemanticSnapshot,
    index: WitnessIndex,
}

/// Row positions in the original tuple, keyed by raw proof and reference provenance.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
struct WitnessIndex {
    proofs: BTreeMap<String, Vec<usize>>,
    references: BTreeMap<String, Vec<usize>>,
}

#[cfg(test)]
fn positions<'a, T>(rows: &'a [T], id: impl Fn(&'a T) -> &'a str) -> BTreeMap<String, Vec<usize>> {
    let mut index: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (position, row) in rows.iter().enumerate() {
        index.entry(id(row).to_owned()).or_default().push(position);
    }
    index
}

#[cfg(test)]
fn rows_at<'a, T>(
    rows: &'a [T],
    index: &'a BTreeMap<String, Vec<usize>>,
    id: &str,
) -> impl Iterator<Item = &'a T> {
    index
        .get(id)
        .into_iter()
        .flatten()
        .map(|position| &rows[*position])
}

#[cfg(test)]
impl WitnessedDocument {
    fn new(original: SemanticSnapshot) -> Self {
        let mut proofs: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (position, row) in original.provenance.iter().enumerate() {
            if let Some(derived) = &row.derived_from {
                proofs
                    .entry(derived.provenance_id.clone())
                    .or_default()
                    .push(position);
            }
        }
        let index = WitnessIndex {
            proofs,
            references: positions(&original.references, |row| &row.provenance_id),
        };
        Self { original, index }
    }

    fn references<'a>(&'a self, id: &str) -> impl Iterator<Item = &'a Reference> {
        rows_at(&self.original.references, &self.index.references, id)
    }
}

/// Validates one coverage row against the producers and the native revision. Returns the
/// row's language string, which keys the coverage tuple.
/// What a coverage row is checked against: the destination header and its document set.
struct CoverageEnv<'a> {
    revision_id: &'a str,
    source_set_id: &'a str,
    languages: &'a [String],
    has_document: &'a dyn Fn(&str, &str) -> bool,
}

fn validate_coverage_row(
    row: &Coverage,
    producers: &BTreeMap<&str, &Producer>,
    env: &CoverageEnv<'_>,
) -> Result<String> {
    text(&row.producer_id)?;
    path(&row.document_path)?;
    let producer = producers
        .get(row.producer_id.as_str())
        .context("dangling coverage producer")?;
    ensure!(
        row.revision_id == env.revision_id && row.source_set_id == env.source_set_id,
        "foreign coverage tuple"
    );
    let lang = serde_json::to_value(row.language)?
        .as_str()
        .unwrap()
        .to_owned();
    ensure!(
        producer.languages.contains(&row.language)
            && env.languages.contains(&lang)
            && (env.has_document)(&lang, &row.document_path),
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
    if let Some(reasons) = &row.reasons {
        validate_coverage_reasons(row, reasons)?;
    }
    Ok(lang)
}

/// Reasons belong to a `partial` tuple, name at least one cause, stay JSON-safe, and agree with
/// the single diagnostic's precedence: a native gap, then the cap, then an unmeasurable target.
fn validate_coverage_reasons(row: &Coverage, reasons: &CoverageReasons) -> Result<()> {
    ensure!(
        row.state == CoverageState::Partial && !reasons.is_empty(),
        "coverage reasons outside a partial tuple"
    );
    let gaps = reasons.native_gaps();
    ensure!(
        gaps <= 9_007_199_254_740_991,
        "coverage reason count out of range"
    );
    ensure!(
        match row.diagnostic.as_deref() {
            Some("unsupported_native_gap") => gaps > 0,
            Some(
                crate::scip_raw::OCCURRENCE_CAP_EXCEEDED
                | crate::scip_raw::DOCUMENT_CAP_EXCEEDED
                | crate::scip_raw::INVALID_RANGE,
            ) => {
                gaps == 0 && reasons.capped
            }
            Some("unmeasurable_internal_target") => gaps == 0 && !reasons.capped,
            _ => false,
        },
        "coverage reasons contradict the diagnostic"
    );
    Ok(())
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
    pub(crate) artifact_hash: String,
}

/// Resolves `(proof_id, record_kind, fact_digest)` to exactly one raw claim.
pub(crate) type ClaimLookup<'a> = dyn Fn(&str, &str, &str) -> Result<RawClaimDocument> + 'a;

/// Validates a provenance row's derivation: a derived proof resolves through `claims` to
/// exactly one claim with the same kind and digest; any other proof belongs to the captured
/// revision. The lookup is a parameter so an in-memory inventory and an indexed claim table
/// share the rule.
fn validate_provenance_row(
    p: &Provenance,
    raw: Option<(&RawCaptureIdentity<'_>, &ClaimLookup<'_>)>,
    captured: &CapturedComponents,
    revision: &native::Revision,
) -> Result<Option<String>> {
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
        Ok(Some(claim.artifact_hash))
    } else {
        ensure!(
            p.revision_id == captured.captured_revision.id,
            "captured proof revision mismatch"
        );
        Ok(None)
    }
}

/// The artifact digests a proof's basis is checked against.
struct BasisArtifacts<'a> {
    /// The artifacts this row's producer was captured from.
    producer: Option<&'a BTreeSet<String>>,
    /// The derived-from claim's artifact, which the basis must name.
    claim: Option<&'a str>,
    /// The raw inventory's source manifest hash, when it must also match the basis.
    raw_manifest_hash: Option<&'a str>,
}

/// Validates a proof's captured basis against `artifacts`.
fn validate_basis(
    p: &Provenance,
    basis: &SemanticBasis,
    producer: &Producer,
    captured: &CapturedComponents,
    artifacts: BasisArtifacts<'_>,
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
        artifacts
            .producer
            .context("missing captured semantic artifact")?
            .contains(&basis.artifact_hash)
            && artifacts
                .claim
                .is_none_or(|artifact_hash| basis.artifact_hash == artifact_hash)
            && artifacts
                .raw_manifest_hash
                .is_none_or(|source_manifest_hash| source_manifest_hash == manifest_hash)
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

/// The source bytes of a document, when the validator holds them.
type Sources<'a> = &'a dyn Fn(&native::DocumentKey) -> Option<&'a [u8]>;

/// Read-only inputs shared by the per-row association validators. A row needs only its own
/// document's measured facts: `native` carries the document's declarations, calls, references
/// and capability report (the whole artifact for the whole-publication validator), and the
/// lookups answer by syntax or occurrence ID. `index` is `native`'s per-document index.
#[derive(Clone, Copy)]
struct RowEnv<'a> {
    native: &'a native::Artifact,
    index: &'a NativeIndex,
    revision: &'a native::Revision,
    sources: Sources<'a>,
    declaration: &'a dyn Fn(&str) -> Option<&'a native::Declaration>,
    reference: &'a dyn Fn(&str) -> Option<&'a native::Reference>,
    known: KnownTarget<'a>,
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
        index,
        revision,
        sources,
        ..
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
    validate_join(&b.join, JoinKind::DeclarationName, native, index, sources)?;
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
    references: &mut BTreeSet<(&'a str, &'a str)>,
) -> Result<()> {
    let RowEnv {
        native,
        index,
        revision,
        sources,
        declaration,
        reference,
        known,
    } = *env;
    let empty = std::cell::OnceCell::new();
    let measured = index.document(&r.document, &empty);
    ensure!(r.revision_id == revision.id, "foreign reference revision");
    sid(&r.owner_syntax_id)?;
    occ(&r.id)?;
    uint(r.ordinal)?;
    ensure!(
        reference(r.id.as_str()).is_some_and(|candidate| {
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
        declaration(&r.owner_syntax_id).is_some_and(|d| d.document == r.document
            && d.range.start <= r.range.start
            && r.range.end <= d.range.end),
        "unmeasured reference owner"
    );
    let bytes = sources(&r.document).context("missing reference source")?;
    ensure!(
        measured.span(bytes, &r.range)? == r.spelling,
        "reference spelling mismatch"
    );
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
    if r.roles.contains(&Role::Call) {
        let invocation = |call: &native::Call| {
            call.document == r.document
                && call.owner_syntax_id == r.owner_syntax_id
                && call.range.start <= r.range.start
                && r.range.end <= call.range.end
                && call
                    .callee_range
                    .as_ref()
                    .is_none_or(|callee| callee == &r.range)
                && call.spelling.as_deref() == Some(&r.spelling)
        };
        // Such a call has the reference's range as its callee range, or none and the
        // reference's spelling.
        let spelling = |position: &usize| native.calls[*position].spelling.as_deref();
        let bare = &measured.bare_calls;
        let first = bare.partition_point(|position| spelling(position) < Some(&r.spelling));
        ensure!(
            at(&measured.callees, &native.calls, &r.range).any(invocation)
                || bare[first..]
                    .iter()
                    .take_while(|position| spelling(position) == Some(&r.spelling))
                    .any(|position| {
                        count_work(1);
                        invocation(&native.calls[*position])
                    }),
            "call role without measured invocation"
        );
    }
    ensure!(
        (r.site == ReferenceSite::Declaration) == r.roles.contains(&Role::Definition),
        "reference site/role mismatch"
    );
    if r.site == ReferenceSite::Declaration {
        ensure!(
            at(&measured.names, &native.declarations, &r.range).any(|d| {
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
        target(t, revision, known)?;
    }
    targets(&r.candidates, revision, known, true)?;
    ensure!(
        references.insert((p.producer_id.as_str(), r.id.as_str())),
        "duplicate reference ID"
    );
    Ok(())
}

/// Validates one call binding against its resolved provenance row.
fn validate_call_binding_row(
    b: &CallBinding,
    proof: &Provenance,
    env: &RowEnv<'_>,
    call_facts: &mut BTreeSet<(String, native::DocumentKey, String, Vec<u8>)>,
    call_resolutions: &mut BTreeMap<(String, String), Vec<TargetClaim>>,
) -> Result<()> {
    let RowEnv {
        native,
        index,
        revision,
        sources,
        known,
        ..
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
    validate_join(&b.join, JoinKind::Callee, native, index, sources)?;
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
        target(t, revision, known)?;
    }
    targets(&b.candidates, revision, known, true)?;
    targets(&b.possible_dispatch, revision, known, true)?;
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
        revision, known, ..
    } = *env;
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
    target(&r.source, revision, known)?;
    target(&r.target, revision, known)?;
    Ok(())
}

/// Validates one symbol against its resolved provenance row.
fn validate_symbol_row(
    s: &Symbol,
    p: &Provenance,
    env: &RowEnv<'_>,
    symbols: &mut BTreeSet<(String, String, Vec<u8>)>,
) -> Result<()> {
    let RowEnv {
        revision, known, ..
    } = *env;
    symbol_key(&s.key, revision)?;
    if let Some(name) = &s.display_name {
        text(name)?;
    }
    match (&s.key.scope, &s.key.document) {
        (SymbolScope::Document, Some(doc)) => {
            ensure!(doc == &p.document, "symbol evidence document mismatch")
        }
        (SymbolScope::Target, Some(_)) => ensure!(
            p.document.language == "rust",
            "symbol evidence document mismatch"
        ),
        _ => {}
    }
    targets(&s.declarations, revision, known, false)?;
    let artifact = p
        .basis
        .as_ref()
        .map(|basis| basis.artifact_hash.clone())
        .unwrap_or_default();
    ensure!(
        symbols.insert((
            p.producer_id.clone(),
            artifact.clone(),
            native_ids::canonical(&serde_json::to_value(&s.key)?)
        )),
        "duplicate semantic symbol key"
    );
    // A key unique per Cargo target (#394): a global key, from a document whose target is not
    // determined, still conflicts with the same key in any target. Tagged entries never equal
    // a canonical key, which starts with `{`.
    let tagged = |scope: &str| {
        let mut entry = format!("\0{scope}\0").into_bytes();
        entry.extend_from_slice(s.key.symbol.as_bytes());
        (p.producer_id.clone(), artifact.clone(), entry)
    };
    match s.key.scope {
        SymbolScope::Global => {
            ensure!(
                !symbols.contains(&tagged("target")),
                "duplicate semantic symbol key"
            );
            symbols.insert(tagged("global"));
        }
        SymbolScope::Target => {
            ensure!(
                !symbols.contains(&tagged("global")),
                "duplicate semantic symbol key"
            );
            symbols.insert(tagged("target"));
        }
        SymbolScope::Document => {}
    }
    Ok(())
}

fn validate_evidence_inner(
    semantic: &SemanticPublication,
    native: &native::Artifact,
    captured: &CapturedComponents,
    raw: Option<ValidatedRaw<'_>>,
    attested: Option<&crate::store::AttestedProfileProducer>,
) -> Result<()> {
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
    for doc in &captured.captured_revision.documents {
        ensure!(
            !sources.contains_key(&doc.key),
            "duplicate native manifest document"
        );
        let bytes = captured
            .sources
            .get(&doc.key)
            .context("absent captured source")?;
        ensure!(
            doc.content_hash == native_ids::digest(b"", bytes) && doc.byte_length == bytes.len(),
            "source digest/length mismatch"
        );
        sources.insert(doc.key.clone(), bytes.clone());
    }
    ensure!(
        captured.sources.len() == sources.len(),
        "extra captured source or witness"
    );
    let manifest_hash = validate_captured_header(captured)?;
    if captured.captured_revision.id == revision.id {
        ensure!(
            captured.captured_revision == *revision,
            "same native revision has different captured snapshot"
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
            && captured
                .artifacts
                .keys()
                .map(|(producer_id, _)| producer_id)
                .collect::<BTreeSet<_>>()
                .len()
                == producers.len(),
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
        let lang = validate_coverage_row(
            row,
            &producers,
            &CoverageEnv {
                revision_id: &revision.id,
                source_set_id: &revision.source_set_id,
                languages: &native.source_set.languages,
                has_document: &|language, path| {
                    revision.documents.iter().any(|d| {
                        d.key.source_set_id == revision.source_set_id
                            && d.key.language == language
                            && d.key.path == path
                    })
                },
            },
        )?;
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
    let decls: BTreeMap<_, _> = native
        .declarations
        .iter()
        .map(|d| (d.syntax_id.clone(), d))
        .collect();
    ensure!(
        decls.len() == native.declarations.len(),
        "duplicate measured declaration ID"
    );
    let mut artifact_hashes: BTreeMap<&String, BTreeSet<String>> = BTreeMap::new();
    for ((producer_id, keyed_hash), bytes) in &captured.artifacts {
        let digest = native_ids::digest(b"", bytes);
        ensure!(&digest == keyed_hash, "captured artifact key mismatch");
        if let Some(raw) = &raw {
            ensure!(
                raw.producer_id == producer_id
                    && raw.binaries.get(&digest).copied() == Some(bytes.as_slice()),
                "captured artifact differs from admitted raw binary"
            );
            ensure!(
                raw.inventory.artifact_hashes.contains(&digest),
                "raw binary identity mismatch"
            );
        }
        artifact_hashes
            .entry(producer_id)
            .or_default()
            .insert(digest);
    }
    // One proof-ID index for the whole loop, not a scan of every raw claim per proof (#494).
    let raw_proofs = raw.as_ref().map(|raw| raw.inventory.proof_lookup());
    let lookup = |proof: &str, kind: &str, digest: &str| -> Result<RawClaimDocument> {
        let claim = raw_proofs
            .as_ref()
            .context("derived provenance requires authenticated raw inventory")?
            .lookup(proof, kind, digest)?;
        Ok(RawClaimDocument {
            language: claim.language.clone(),
            path: claim.path.clone(),
            artifact_hash: claim.artifact_hash.clone(),
        })
    };
    let identity = raw.as_ref().map(|raw| RawCaptureIdentity {
        producer_id: raw.producer_id,
        source_set_id: raw.source_set_id,
        captured_revision: raw.captured_revision,
    });
    let captured_documents = document_index(&captured.captured_revision);
    let requested_documents = document_index(revision);
    let proof = validate_provenance_rows(
        semantic,
        &coverage,
        &ProofEnv {
            captured,
            revision,
            captured_documents: &captured_documents,
            requested_documents: &requested_documents,
            producers: &producers,
            requested_producers: &requested_producers,
            artifact_hashes: &artifact_hashes,
            manifest_hash: &manifest_hash,
            requested_manifest_hash: &requested_manifest_hash(revision),
            raw: identity
                .as_ref()
                .map(|identity| (identity, &lookup as &ClaimLookup<'_>)),
            raw_manifest_hash: None,
        },
    )?;
    let native_references: BTreeMap<_, _> = native
        .references
        .iter()
        .map(|r| (r.id.as_str(), r))
        .collect();
    ensure!(
        native_references.len() == native.references.len(),
        "duplicate native reference ID"
    );
    let known = |syntax_id: &str, document: &native::DocumentKey| -> Result<bool> {
        Ok(decls
            .get(syntax_id)
            .is_some_and(|d| &d.document == document))
    };
    // The whole publication's facts are already materialized; the index adds a range-ordered
    // position per fact, bounded by the artifact the request bounds already admitted.
    let index = NativeIndex::new(native);
    let env = RowEnv {
        native,
        index: &index,
        revision,
        sources: &|key| sources.get(key).map(Vec::as_slice),
        declaration: &|id| decls.get(id).copied(),
        reference: &|id| native_references.get(id).copied(),
        known: &known,
    };
    validate_fact_rows(semantic, &proof, &coverage, &env)
}

/// Checks the captured manifest's order and revision identity and its component hashes, and
/// returns the manifest hash the captured bases name.
fn validate_captured_header(captured: &CapturedComponents) -> Result<String> {
    let mut manifest = Vec::new();
    for doc in &captured.captured_revision.documents {
        key(&doc.key, &captured.captured_revision)?;
        ensure!(
            doc.revision_id == captured.captured_revision.id,
            "foreign native document revision"
        );
        manifest.push(json!({"document":doc.key,"contentHash":doc.content_hash}));
    }
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
    Ok(native_ids::digest(
        b"",
        &native_ids::canonical(&serde_json::Value::Array(manifest)),
    ))
}

/// A revision's documents by key.
fn document_index(
    revision: &native::Revision,
) -> BTreeMap<&native::DocumentKey, &native::Document> {
    revision.documents.iter().map(|d| (&d.key, d)).collect()
}

/// The coverage of a publication by producer, language and path.
type CoverageIndex<'a> = BTreeMap<(&'a str, String, &'a str), &'a Coverage>;

/// What a provenance row is checked against.
struct ProofEnv<'a> {
    captured: &'a CapturedComponents,
    revision: &'a native::Revision,
    captured_documents: &'a BTreeMap<&'a native::DocumentKey, &'a native::Document>,
    requested_documents: &'a BTreeMap<&'a native::DocumentKey, &'a native::Document>,
    producers: &'a BTreeMap<&'a str, &'a Producer>,
    requested_producers: &'a BTreeMap<&'a str, &'a Producer>,
    artifact_hashes: &'a BTreeMap<&'a String, BTreeSet<String>>,
    manifest_hash: &'a str,
    requested_manifest_hash: &'a str,
    raw: Option<(&'a RawCaptureIdentity<'a>, &'a ClaimLookup<'a>)>,
    raw_manifest_hash: Option<&'a str>,
}

/// Validates every provenance row and returns them by ID.
fn validate_provenance_rows<'a>(
    semantic: &'a SemanticPublication,
    coverage: &CoverageIndex<'a>,
    env: &ProofEnv<'_>,
) -> Result<BTreeMap<&'a str, &'a Provenance>> {
    let mut proof = BTreeMap::new();
    for p in &semantic.provenance {
        text(&p.id)?;
        key(&p.document, env.revision)?;
        hash(&p.content_hash)?;
        let claim_artifact = validate_provenance_row(p, env.raw, env.captured, env.revision)?;
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
        let producer = env
            .producers
            .get(p.producer_id.as_str())
            .context("unknown proof producer")?;
        validate_basis(
            p,
            basis,
            producer,
            env.captured,
            BasisArtifacts {
                producer: env.artifact_hashes.get(&p.producer_id),
                claim: claim_artifact.as_deref(),
                raw_manifest_hash: env.raw_manifest_hash,
            },
            env.manifest_hash,
        )?;
        let document = env
            .captured_documents
            .get(&p.document)
            .context("missing captured provenance document")?;
        ensure!(
            p.content_hash == document.content_hash,
            "captured proof document hash mismatch"
        );
        let requested = env.requested_documents.get(&p.document);
        let requested_producer = env.requested_producers.get(p.producer_id.as_str());
        let revision = env.revision;
        let freshness = if requested.is_none_or(|d| d.content_hash != p.content_hash) {
            Freshness::Stale
        } else if basis.revision_id != revision.id
            || basis.source_manifest_hash != env.requested_manifest_hash
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
    Ok(proof)
}

/// Validates every fact row of a publication against its provenance row and `env`, then each
/// document's observed roles.
fn validate_fact_rows(
    semantic: &SemanticPublication,
    proof: &BTreeMap<&str, &Provenance>,
    coverage: &CoverageIndex<'_>,
    env: &RowEnv<'_>,
) -> Result<()> {
    let revision = env.revision;
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
    let mut symbols = BTreeSet::new();
    for s in &semantic.symbols {
        let p = fact(&s.provenance_id, None, EvidenceKind::DeclarationBinding)?;
        validate_symbol_row(s, p, env, &mut symbols)?;
    }
    let mut binding_facts = BTreeSet::new();
    for b in &semantic.declaration_bindings {
        let p = fact(
            &b.provenance_id,
            Some(&b.join.anchor.document),
            EvidenceKind::DeclarationBinding,
        )?;
        validate_declaration_binding_row(b, p, env, &mut binding_facts)?;
    }
    let mut references = BTreeSet::new();
    for r in &semantic.references {
        let p = fact(
            &r.provenance_id,
            Some(&r.document),
            EvidenceKind::SemanticReference,
        )?;
        validate_reference_row(r, p, env, &mut references)?;
    }
    let mut call_facts = BTreeSet::new();
    let mut call_resolutions: BTreeMap<(String, String), Vec<TargetClaim>> = BTreeMap::new();
    for b in &semantic.call_bindings {
        let proof = fact(
            &b.provenance_id,
            Some(&b.join.anchor.document),
            EvidenceKind::SemanticReference,
        )?;
        validate_call_binding_row(b, proof, env, &mut call_facts, &mut call_resolutions)?;
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
        validate_type_relationship_row(r, proof, env, &mut relationships)?;
    }
    // Each document's observed roles, from one pass over the admitted rows (#494): the roles of
    // its references, a definition for a bound declaration, a call for a bound call and a type
    // for a type relationship, each attributed to the document of the row's own proof.
    let mut observed: BTreeMap<(&str, &str, &str), BTreeSet<Role>> = BTreeMap::new();
    let mut observe = |id: &str, roles: &[Role]| {
        if let Some(p) = proof.get(id) {
            observed
                .entry((
                    p.producer_id.as_str(),
                    p.document.language.as_str(),
                    p.document.path.as_str(),
                ))
                .or_default()
                .extend(roles.iter().copied());
        }
    };
    for r in &semantic.references {
        observe(&r.provenance_id, &r.roles);
    }
    for b in &semantic.declaration_bindings {
        if b.syntax_id.is_some() {
            observe(&b.provenance_id, &[Role::Definition]);
        }
    }
    for b in &semantic.call_bindings {
        if b.call_id.is_some() {
            observe(&b.provenance_id, &[Role::Call]);
        }
    }
    for r in &semantic.type_relationships {
        observe(&r.provenance_id, &[Role::Type]);
    }
    let none = BTreeSet::new();
    for ((producer, lang, path), row) in coverage {
        let roles = observed
            .get(&(*producer, lang.as_str(), *path))
            .unwrap_or(&none);
        for role in &[
            Role::Definition,
            Role::Read,
            Role::Write,
            Role::Call,
            Role::Type,
            Role::Import,
            Role::Alias,
        ] {
            ensure!(
                row.observed_roles.contains(role) == roles.contains(role),
                "coverage observed roles differ from admitted evidence"
            );
        }
    }
    Ok(())
}

/// Per-row validation for rows a streaming join emits. Every row runs the row-local validators
/// the whole-publication validator calls, against the measured facts of its own document and
/// the batch header instead of a materialized revision. Checks that compare rows with each
/// other are the join's closure queries.
pub(crate) struct EmissionValidator<'a> {
    producer: &'a Producer,
    captured: CapturedComponents,
    revision: native::Revision,
    languages: Vec<String>,
    artifact_hashes: BTreeSet<String>,
    manifest_hash: String,
    producers: BTreeMap<&'a str, &'a Producer>,
}

/// The header values an emission validator needs.
pub(crate) struct EmissionHeader<'a> {
    pub producer: &'a Producer,
    pub captured_revision: native::Revision,
    pub destination_revision: native::Revision,
    pub source_set_languages: Vec<String>,
    pub artifact_hashes: BTreeSet<String>,
    pub source_manifest_hash: String,
}

impl<'h> EmissionValidator<'h> {
    pub(crate) fn new(header: EmissionHeader<'h>) -> Self {
        let captured = CapturedComponents {
            producer_executables: BTreeMap::new(),
            artifacts: BTreeMap::new(),
            sources: BTreeMap::new(),
            captured_revision: header.captured_revision,
            native_toolchain: serde_json::Value::Null,
            native_config: vec![],
            native_dependencies: vec![],
            requested_producers: vec![],
            toolchain: vec![],
            config: vec![],
            dependency: vec![],
            native_extraction_components: BTreeMap::new(),
        };
        Self {
            producers: BTreeMap::from([(header.producer.id.as_str(), header.producer)]),
            producer: header.producer,
            captured,
            revision: header.destination_revision,
            languages: header.source_set_languages,
            artifact_hashes: header.artifact_hashes,
            manifest_hash: header.source_manifest_hash,
        }
    }

    pub(crate) fn coverage(
        &self,
        row: &Coverage,
        has_document: &dyn Fn(&str, &str) -> bool,
    ) -> Result<()> {
        validate_coverage_row(
            row,
            &self.producers,
            &CoverageEnv {
                revision_id: &self.revision.id,
                source_set_id: &self.revision.source_set_id,
                languages: &self.languages,
                has_document,
            },
        )
        .map(|_| ())
    }

    /// Validates a derived proof. `claims` resolves `(proof, kind, digest)` to exactly one
    /// staged claim, so a forged derivation fails before it can be published.
    pub(crate) fn provenance(&self, p: &Provenance, claims: &ClaimLookup<'_>) -> Result<()> {
        text(&p.id)?;
        key(&p.document, &self.revision)?;
        hash(&p.content_hash)?;
        let identity = RawCaptureIdentity {
            producer_id: &self.producer.id,
            source_set_id: &self.captured.captured_revision.source_set_id,
            captured_revision: &self.captured.captured_revision.id,
        };
        let claim_artifact =
            validate_provenance_row(p, Some((&identity, claims)), &self.captured, &self.revision)?;
        let basis = p
            .basis
            .as_ref()
            .context("semantic proof requires captured basis")?;
        validate_basis(
            p,
            basis,
            self.producer,
            &self.captured,
            BasisArtifacts {
                producer: Some(&self.artifact_hashes),
                claim: claim_artifact.as_deref(),
                raw_manifest_hash: None,
            },
            &self.manifest_hash,
        )
    }

    /// Runs `check` over the same row-local validators the whole-publication validator uses,
    /// with `facts` standing for the one document the row belongs to.
    fn with_env<'a, T>(
        &'a self,
        facts: &DocumentFacts<'a>,
        check: impl FnOnce(&RowEnv<'a>) -> Result<T>,
    ) -> Result<T> {
        check(&RowEnv {
            native: facts.native,
            index: facts.index,
            revision: &self.revision,
            sources: facts.sources,
            declaration: facts.declaration,
            reference: facts.reference,
            known: facts.known,
        })
    }

    /// The proof a staged fact row stands on: the measured identity of its derivation, which is
    /// all the row-local validators read from it.
    pub(crate) fn fact_proof(
        &self,
        id: &str,
        document: native::DocumentKey,
        evidence_kind: EvidenceKind,
        content_hash: &str,
    ) -> Provenance {
        Provenance {
            id: id.to_owned(),
            producer_id: self.producer.id.clone(),
            document,
            revision_id: self.revision.id.clone(),
            content_hash: content_hash.to_owned(),
            evidence_kind,
            basis: None,
            freshness: Freshness::Fresh,
            derived_from: None,
        }
    }

    pub(crate) fn symbol<'a>(
        &'a self,
        row: &Symbol,
        proof: &Provenance,
        facts: &DocumentFacts<'a>,
    ) -> Result<()> {
        text(&row.provenance_id)?;
        self.with_env(facts, |env| {
            validate_symbol_row(row, proof, env, &mut BTreeSet::new())
        })
    }

    pub(crate) fn declaration_binding<'a>(
        &'a self,
        row: &DeclarationBinding,
        proof: &Provenance,
        facts: &DocumentFacts<'a>,
    ) -> Result<()> {
        self.with_env(facts, |env| {
            validate_declaration_binding_row(row, proof, env, &mut BTreeSet::new())
        })
    }

    pub(crate) fn reference<'a>(
        &'a self,
        row: &Reference,
        proof: &Provenance,
        facts: &DocumentFacts<'a>,
    ) -> Result<()> {
        self.with_env(facts, |env| {
            validate_reference_row(row, proof, env, &mut BTreeSet::new())
        })
    }

    pub(crate) fn call_binding<'a>(
        &'a self,
        row: &CallBinding,
        proof: &Provenance,
        facts: &DocumentFacts<'a>,
    ) -> Result<()> {
        self.with_env(facts, |env| {
            validate_call_binding_row(row, proof, env, &mut BTreeSet::new(), &mut BTreeMap::new())
        })
    }

    pub(crate) fn type_relationship<'a>(
        &'a self,
        row: &TypeRelationship,
        proof: &Provenance,
        facts: &DocumentFacts<'a>,
    ) -> Result<()> {
        self.with_env(facts, |env| {
            validate_type_relationship_row(row, proof, env, &mut BTreeSet::new())
        })
    }
}

/// Original, validated source rows under an identical immutable selected native version.
/// Targets are deliberately checked against the current destination, not the original answer.
pub(crate) enum WitnessFact<'a> {
    Symbol(&'a Symbol, &'a Symbol),
    Declaration(&'a DeclarationBinding, &'a DeclarationBinding),
    Reference(&'a Reference, &'a Reference),
    Call(&'a CallBinding, &'a CallBinding),
    Relationship(&'a TypeRelationship, &'a TypeRelationship),
}

impl EmissionValidator<'_> {
    pub(crate) fn witnessed_fact(
        &self,
        row: WitnessFact<'_>,
        raw: &str,
        id: &str,
        document: &native::DocumentKey,
        content_hash: &str,
        known: &dyn Fn(&str, &native::DocumentKey) -> Result<bool>,
    ) -> Result<()> {
        key(document, &self.revision)?;
        hash(content_hash)?;
        text(id)?;
        let anchor = |a: &MeasuredAnchor, b: &MeasuredAnchor| -> Result<()> {
            ensure!(
                a.document == *document
                    && b.document == *document
                    && a.content_hash == content_hash
                    && b.content_hash == content_hash
                    && b.revision_id == self.revision.id
                    && a.range == b.range
                    && a.kind == b.kind,
                "witnessed source anchor differs"
            );
            Ok(())
        };
        let proof = |original: &str, current: &str| -> Result<()> {
            ensure!(
                original == raw && current == id,
                "witnessed source proof differs"
            );
            Ok(())
        };
        let current_targets = |resolution_: Resolution,
                               declared: &Option<Target>,
                               candidates: &[Target]|
         -> Result<()> {
            resolution(resolution_, declared, candidates)?;
            if let Some(value) = declared {
                target(value, &self.revision, known)?;
            }
            targets(candidates, &self.revision, known, true)
        };
        match row {
            WitnessFact::Symbol(original, row) => {
                proof(&original.provenance_id, &row.provenance_id)?;
                ensure!(
                    original.key == row.key && original.display_name == row.display_name,
                    "witnessed symbol source differs"
                );
                symbol_key(&row.key, &self.revision)?;
                targets(&row.declarations, &self.revision, known, false)?;
            }
            WitnessFact::Declaration(original, row) => {
                proof(&original.provenance_id, &row.provenance_id)?;
                anchor(&original.join.anchor, &row.join.anchor)?;
                ensure!(
                    original.syntax_id == row.syntax_id
                        && original.symbols == row.symbols
                        && original.join.status == row.join.status
                        && original.join.candidate_ids == row.join.candidate_ids
                        && original.join.diagnostic == row.join.diagnostic,
                    "witnessed declaration source differs"
                );
                for value in &row.symbols {
                    symbol_key(value, &self.revision)?;
                }
            }
            WitnessFact::Reference(original, row) => {
                proof(&original.provenance_id, &row.provenance_id)?;
                ensure!(
                    row.document == *document
                        && row.revision_id == self.revision.id
                        && original.id == row.id
                        && original.document == row.document
                        && original.owner_syntax_id == row.owner_syntax_id
                        && original.ordinal == row.ordinal
                        && original.range == row.range
                        && original.spelling == row.spelling
                        && original.lookup_key == row.lookup_key
                        && original.site == row.site
                        && original.roles == row.roles,
                    "witnessed reference source differs"
                );
                current_targets(row.resolution, &row.declared_target, &row.candidates)?;
            }
            WitnessFact::Call(original, row) => {
                proof(&original.provenance_id, &row.provenance_id)?;
                anchor(&original.join.anchor, &row.join.anchor)?;
                ensure!(
                    original.call_id == row.call_id
                        && original.join.status == row.join.status
                        && original.join.candidate_ids == row.join.candidate_ids
                        && original.join.diagnostic == row.join.diagnostic,
                    "witnessed call source differs"
                );
                current_targets(row.resolution, &row.declared_target, &row.candidates)?;
                targets(&row.possible_dispatch, &self.revision, known, true)?;
                ensure!(
                    !row.possible_dispatch_complete
                        && row.stale_target
                            == if matches!(row.declared_target, Some(Target::Internal { .. })) {
                                Some(false)
                            } else {
                                None
                            },
                    "witnessed call target flags differ"
                );
            }
            WitnessFact::Relationship(original, row) => {
                proof(&original.provenance_id, &row.provenance_id)?;
                ensure!(
                    original.kind == row.kind,
                    "witnessed relationship kind differs"
                );
                let same = |a: &Target, b: &Target| match (a, b) {
                    (
                        Target::Internal {
                            syntax_id: a,
                            document: ad,
                            ..
                        },
                        Target::Internal {
                            syntax_id: b,
                            document: bd,
                            ..
                        },
                    ) => a == b && ad == bd,
                    (Target::External { symbol: a }, Target::External { symbol: b }) => a == b,
                    _ => false,
                };
                ensure!(
                    same(&original.source, &row.source) && same(&original.target, &row.target),
                    "witnessed relationship endpoint differs"
                );
                target(&row.source, &self.revision, known)?;
                target(&row.target, &self.revision, known)?;
            }
        }
        Ok(())
    }
}

/// One document's measured facts, as the row-local validators read them.
pub(crate) struct DocumentFacts<'a> {
    pub native: &'a native::Artifact,
    /// `NativeIndex::new(native)` of this very `native` (it holds positions into it), built
    /// once for the document and shared by its rows.
    pub index: &'a NativeIndex,
    pub sources: &'a dyn Fn(&native::DocumentKey) -> Option<&'a [u8]>,
    pub declaration: &'a dyn Fn(&str) -> Option<&'a native::Declaration>,
    pub reference: &'a dyn Fn(&str) -> Option<&'a native::Reference>,
    pub known: &'a dyn Fn(&str, &native::DocumentKey) -> Result<bool>,
}

#[cfg(test)]
#[path = "semantic_evidence_witness_tests.rs"]
mod witness_tests;

#[cfg(test)]
#[path = "semantic_evidence_native_index_tests.rs"]
mod native_index_tests;

#[cfg(test)]
#[path = "semantic_evidence_coverage_reason_tests.rs"]
mod coverage_reason_tests;
