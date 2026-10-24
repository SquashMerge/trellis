//! Captured semantic evidence admitted against one validated native snapshot.
use crate::{
    native_evidence as native,
    native_ids::{self, IdentityRegistry},
    store::id_codec,
};
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
                declarations
                    .get(syntax_id)
                    .is_some_and(|d| &d.document == document),
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
) -> Result<()> {
    let mut seen = BTreeSet::new();
    for item in items {
        target(item, native, decls)?;
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
fn validate_join(
    join: &Join,
    family: JoinKind,
    native: &native::Artifact,
    sources: &BTreeMap<native::DocumentKey, Vec<u8>>,
) -> Result<()> {
    let a = &join.anchor;
    ensure!(
        a.kind == family || (family == JoinKind::Callee && a.kind == JoinKind::Invocation),
        "wrong join family"
    );
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
    // Native publication has no authenticated per-family unavailability witness yet.
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
                    && (a.kind == JoinKind::Invocation || call.spelling.as_deref() == Some(slice))
                {
                    candidates.insert(call.id.clone());
                }
            }
        }
        _ => anyhow::bail!("unsupported join family"),
    }
    let expected: Vec<_> = candidates.into_iter().collect();
    let status = if expected.len() == 1 {
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
    for id in &join.candidate_ids {
        if family == JoinKind::DeclarationName {
            sid(id)?
        } else {
            occ(id)?
        }
    }
    Ok(())
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
        let bytes = captured
            .sources
            .get(&doc.key)
            .context("absent captured source")?;
        ensure!(
            doc.content_hash == native_ids::digest(b"", bytes) && doc.byte_length == bytes.len(),
            "source digest/length mismatch"
        );
        manifest.push(json!({"document":doc.key,"contentHash":doc.content_hash}));
        ensure!(
            sources.insert(doc.key.clone(), bytes.clone()).is_none(),
            "duplicate native manifest document"
        );
    }
    ensure!(
        captured.sources.len() == sources.len(),
        "extra captured source"
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
        ensure!(
            producers.insert(p.id.as_str(), p).is_none(),
            "duplicate semantic producer"
        );
    }
    ensure!(
        captured.producer_executables.len() == producers.len()
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
                CoverageState::Omitted | CoverageState::Unsupported =>
                    row.requested && !row.selected,
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
    let mut proof = BTreeMap::new();
    for p in &semantic.provenance {
        text(&p.id)?;
        key(&p.document, revision)?;
        hash(&p.content_hash)?;
        ensure!(
            p.revision_id == captured.captured_revision.id,
            "captured proof revision mismatch"
        );
        ensure!(
            p.derived_from.is_none(),
            "derived provenance is not admitted in this stage"
        );
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
                && basis.language
                    == serde_json::from_value::<Language>(json!(p.document.language))?,
            "captured basis identity mismatch"
        );
        ensure!(
            basis.artifact_hash
                == native_ids::digest(
                    b"",
                    captured
                        .artifacts
                        .get(&p.producer_id)
                        .context("missing captured semantic artifact")?
                )
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
        targets(&s.declarations, native, &decls, false)?;
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
        ensure!(
            binding_facts.insert((
                p.producer_id.clone(),
                p.document.clone(),
                b.provenance_id.clone(),
                native_ids::canonical(&serde_json::to_value(&b.join.anchor)?)
            )),
            "duplicate declaration binding fact"
        );
        validate_join(&b.join, JoinKind::DeclarationName, native, &sources)?;
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
    }
    let mut references = BTreeSet::new();
    let mut grouped: BTreeMap<(&str, &native::DocumentKey, &str), Vec<&Reference>> =
        BTreeMap::new();
    for r in &semantic.references {
        let p = fact(
            &r.provenance_id,
            Some(&r.document),
            EvidenceKind::SemanticReference,
        )?;
        ensure!(r.revision_id == revision.id, "foreign reference revision");
        sid(&r.owner_syntax_id)?;
        occ(&r.id)?;
        uint(r.ordinal)?;
        ensure!(
            decls
                .get(&r.owner_syntax_id)
                .is_some_and(|d| d.document == r.document
                    && d.range.start <= r.range.start
                    && r.range.end <= d.range.end),
            "unmeasured reference owner"
        );
        let bytes = sources
            .get(&r.document)
            .context("missing reference source")?;
        ensure!(
            span(bytes, &r.range)? == r.spelling && !r.spelling.is_empty(),
            "reference spelling mismatch"
        );
        ensure!(
            native::lookup(&r.document.language, &r.spelling)? == r.lookup_key,
            "reference lookup mismatch"
        );
        ordered(&r.roles)?;
        ensure!(
            !r.roles.is_empty()
                && (r.document.language != "java" || !r.roles.contains(&Role::Alias)),
            "invalid reference roles"
        );
        ensure!(
            !r.roles.contains(&Role::Alias) || r.roles.contains(&Role::Definition),
            "alias without definition"
        );
        if r.roles.contains(&Role::Call) {
            ensure!(
                native.calls.iter().any(|call| call.document == r.document
                    && call.owner_syntax_id == r.owner_syntax_id
                    && call.callee_range.as_ref() == Some(&r.range)
                    && call.spelling.as_deref() == Some(&r.spelling)),
                "call role without measured invocation"
            );
        }
        ensure!(
            (r.site == ReferenceSite::Declaration) == r.roles.contains(&Role::Definition),
            "reference site/role mismatch"
        );
        if r.site == ReferenceSite::Declaration {
            ensure!(
                decls
                    .get(&r.owner_syntax_id)
                    .is_some_and(|d| d.name_range.as_ref() == Some(&r.range)
                        && d.name.as_deref() == Some(&r.spelling)),
                "declaration reference site not measured"
            );
        }
        resolution(r.resolution, &r.declared_target, &r.candidates)?;
        if let Some(t) = &r.declared_target {
            target(t, native, &decls)?;
        }
        targets(&r.candidates, native, &decls, true)?;
        ensure!(
            references.insert((p.producer_id.as_str(), r.id.as_str())),
            "duplicate reference ID"
        );
        grouped
            .entry((&p.producer_id, &r.document, &r.owner_syntax_id))
            .or_default()
            .push(r);
    }
    let mut registry = IdentityRegistry::default();
    for ((_producer, document, owner), mut group) in grouped {
        group.sort_by_key(|r| (r.range.start, r.range.end));
        let content_hash = &revision
            .documents
            .iter()
            .find(|d| &d.key == document)
            .context("missing reference document")?
            .content_hash;
        let context =
            native::native_extraction_context(&native.producer, &document.language, revision)?;
        for (i, r) in group.iter().enumerate() {
            ensure!(
                i == r.ordinal && (i == 0 || group[i - 1].range != r.range),
                "reference ordinal/range mismatch"
            );
            let id = registry.occurrence(&json!({"contentHash":content_hash,"extractionContext":context,"nativeProducerId":native.producer.id,"nativeProducerVersion":native.producer.version,"ownerSyntaxId":owner,"kind":"reference","ordinal":r.ordinal}))?;
            ensure!(id == r.id, "noncanonical reference ID");
        }
    }
    let mut call_facts = BTreeSet::new();
    type TargetClaim = (Resolution, Option<Vec<u8>>, Vec<Vec<u8>>);
    let mut call_resolutions: BTreeMap<(String, String), Vec<TargetClaim>> = BTreeMap::new();
    for b in &semantic.call_bindings {
        let proof = fact(
            &b.provenance_id,
            Some(&b.join.anchor.document),
            EvidenceKind::SemanticReference,
        )?;
        ensure!(
            call_facts.insert((
                proof.producer_id.clone(),
                proof.document.clone(),
                b.provenance_id.clone(),
                native_ids::canonical(&serde_json::to_value(&b.join.anchor)?)
            )),
            "duplicate call binding fact"
        );
        validate_join(&b.join, JoinKind::Callee, native, &sources)?;
        ensure!(
            b.call_id.as_ref()
                == (b.join.status == JoinStatus::Exact).then(|| &b.join.candidate_ids[0]),
            "call binding ID mismatch"
        );
        resolution(b.resolution, &b.declared_target, &b.candidates)?;
        if let Some(call_id) = &b.call_id {
            let key = (proof.producer_id.clone(), call_id.clone());
            let target = b.declared_target.as_ref().map(|t| {
                native_ids::canonical(&serde_json::to_value(t).expect("target serializes"))
            });
            let candidates = b
                .candidates
                .iter()
                .map(|t| {
                    native_ids::canonical(&serde_json::to_value(t).expect("target serializes"))
                })
                .collect();
            call_resolutions
                .entry(key)
                .or_default()
                .push((b.resolution, target, candidates));
        }
        if let Some(t) = &b.declared_target {
            target(t, native, &decls)?;
        }
        targets(&b.candidates, native, &decls, true)?;
        targets(&b.possible_dispatch, native, &decls, true)?;
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
        target(&r.source, native, &decls)?;
        target(&r.target, native, &decls)?;
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
