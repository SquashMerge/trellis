//! Rooted outgoing call-graph answers (contract-v1 "Rooted graph answers").
//!
//! This is a pure core: it reads through [`GraphSource`] and never touches SQLite. The
//! store reader supplies a source over one admitted snapshot; fixtures and response-byte
//! re-evaluation use [`GraphFacts`].
use crate::{
    model::IndexPin,
    native_evidence::{self as native, DocumentKey},
    semantic_evidence::{
        self as sem, CallBinding, CoverageState, Dispatch, Freshness, Resolution, Target,
    },
};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Deserializer, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
};

pub const DEFAULT_DEPTH: u64 = 2;
pub const MAX_DEPTH: u64 = 5;
pub const DEFAULT_MAX_NODES: u64 = 150;
pub const MAX_NODES: u64 = 150;
pub const DEFAULT_MAX_CALLS: u64 = 500;
pub const MAX_CALLS: u64 = 500;

const FACTS_MISS: &str = "store_unavailable: graph facts miss";

fn nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> std::result::Result<Option<T>, D::Error> {
    Option::deserialize(d)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Visit {
    New,
    Seen,
    Boundary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BoundaryReason {
    None,
    External,
    Unresolved,
    Ambiguous,
    Dispatch,
    Stale,
    NodeLimit,
    MissingEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FrontierReason {
    Depth,
    NodeLimit,
    CallLimit,
}

/// Declaration order is the contract enum order and the warning sort order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WarningCode {
    CoverageIncomplete,
    StaleEvidence,
    StaleTarget,
    BindingAmbiguous,
    SyntaxOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GraphErrorCode {
    InvalidRequest,
    InvalidRecord,
    InvalidRange,
    InvalidDigest,
    UnsupportedEncoding,
    SourceSetDenied,
    RevisionUnavailable,
    RootMissing,
    ProducerUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GraphError {
    pub code: GraphErrorCode,
    pub message: String,
    #[serde(deserialize_with = "nullable")]
    pub field: Option<String>,
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

fn failure(code: GraphErrorCode, field: &str, message: &str) -> GraphError {
    GraphError {
        code,
        message: message.to_string(),
        field: Some(field.to_string()),
    }
}

/// Corrupt or contradictory snapshot data. The store reader maps it to its own integrity error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphIntegrity(pub String);

impl fmt::Display for GraphIntegrity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GraphIntegrity {}

fn integrity(message: &str) -> anyhow::Error {
    GraphIntegrity(message.to_string()).into()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GraphRequest {
    pub source_set_id: String,
    pub revision_id: String,
    pub root_syntax_id: String,
    #[serde(deserialize_with = "nullable")]
    pub semantic_producer_id: Option<String>,
    pub depth: u64,
    pub max_nodes: u64,
    pub max_calls: u64,
}

fn is_syntax_id(id: &str) -> bool {
    id.strip_prefix("sid:v1:").is_some_and(|hex| {
        hex.len() == 32 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

impl GraphRequest {
    /// Rejects out-of-range input as `invalidRequest` naming the field; nothing is clamped.
    pub fn validate(&self) -> std::result::Result<(), GraphError> {
        let invalid = |field: &str, message: &str| {
            Err(failure(GraphErrorCode::InvalidRequest, field, message))
        };
        if self.depth > MAX_DEPTH {
            return invalid("depth", "depth must be between 0 and 5");
        }
        if !(1..=MAX_NODES).contains(&self.max_nodes) {
            return invalid("maxNodes", "maxNodes must be between 1 and 150");
        }
        if self.max_calls > MAX_CALLS {
            return invalid("maxCalls", "maxCalls must be between 0 and 500");
        }
        if self.source_set_id.is_empty() {
            return invalid("sourceSetId", "sourceSetId must not be empty");
        }
        if self.revision_id.is_empty() {
            return invalid("revisionId", "revisionId must not be empty");
        }
        if !is_syntax_id(&self.root_syntax_id) {
            return invalid("rootSyntaxId", "rootSyntaxId must be a syntax ID");
        }
        if self.semantic_producer_id.as_deref() == Some("") {
            return invalid("semanticProducerId", "semanticProducerId must not be empty");
        }
        Ok(())
    }
}

/// The caller-supplied part of a request, before the server fills the snapshot identifiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphQuery {
    pub root_syntax_id: String,
    pub semantic_producer_id: Option<String>,
    pub depth: u64,
    pub max_nodes: u64,
    pub max_calls: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GraphNode {
    pub declaration: native::Declaration,
    pub depth: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GraphEdge {
    pub call: native::Call,
    pub from: String,
    #[serde(deserialize_with = "nullable")]
    pub to: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub binding: Option<CallBinding>,
    pub visit: Visit,
    pub boundary_reason: BoundaryReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Frontier {
    pub reason: FrontierReason,
    pub node_id: String,
    #[serde(deserialize_with = "nullable")]
    pub call_id: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub target_id: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub next_ordinal: Option<u64>,
    pub omitted_calls: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Warning {
    pub code: WarningCode,
    pub message: String,
    #[serde(deserialize_with = "nullable")]
    pub provenance_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GraphResult {
    pub request: GraphRequest,
    pub resolved_revision_id: String,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub frontier: Vec<Frontier>,
    pub coverage: Vec<sem::Coverage>,
    pub provenance: Vec<sem::Provenance>,
    pub partial: bool,
    pub truncated: bool,
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphSnapshot {
    pub source_set_id: String,
    pub revision_id: String,
}

/// Semantic evidence of one in-scope document for the selected producer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticDocument {
    pub coverage: sem::Coverage,
    pub provenance: BTreeMap<String, sem::Provenance>,
    pub call_bindings: Vec<CallBinding>,
    pub declaration_bindings: Vec<sem::DeclarationBinding>,
    pub symbols: Vec<sem::Symbol>,
    pub type_relationships: Vec<sem::TypeRelationship>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentFacts {
    pub key: DocumentKey,
    pub declarations: BTreeMap<String, native::Declaration>,
    /// Calls grouped by owner syntax ID.
    pub calls: BTreeMap<String, Vec<native::Call>>,
    pub native_coverage: sem::Coverage,
    pub native_provenance: sem::Provenance,
    /// `Some` only for a document in producer scope whose coverage row exists.
    pub semantic: Option<SemanticDocument>,
}

/// Historical coverage and provenance for one declaration of a document whose current row failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryFacts {
    pub coverage: sem::Coverage,
    pub provenance: Vec<sem::Provenance>,
}

/// Everything the evaluation reads. Errors are read failures, aborts or [`GraphIntegrity`].
pub trait GraphSource {
    fn snapshot(&self) -> &GraphSnapshot;
    /// `None` means the producer is not published for the revision.
    fn producer_languages(&mut self, producer_id: &str) -> Result<Option<BTreeSet<String>>>;
    fn locate(&mut self, syntax_id: &str) -> Result<Option<DocumentKey>>;
    fn document(&mut self, key: &DocumentKey) -> Result<Arc<DocumentFacts>>;
    fn history(&mut self, key: &DocumentKey, syntax_id: &str) -> Result<Option<Arc<HistoryFacts>>>;
    /// Called once per dequeued node; an error aborts the evaluation without a result.
    fn check(&mut self) -> Result<()>;
}

/// In-memory record of every fact one query read.
///
/// In complete mode an absent location, producer or history entry means "not there". In
/// memo mode only a recorded lookup is answered and any other lookup is an error, so a
/// re-evaluation can never need SQL.
#[derive(Debug, Clone)]
pub struct GraphFacts {
    snapshot: GraphSnapshot,
    memo: bool,
    pub producers: BTreeMap<String, BTreeSet<String>>,
    pub locations: BTreeMap<String, Option<DocumentKey>>,
    pub documents: BTreeMap<DocumentKey, Arc<DocumentFacts>>,
    pub history: BTreeMap<(DocumentKey, String), Option<Arc<HistoryFacts>>>,
}

impl GraphFacts {
    fn new(snapshot: GraphSnapshot, memo: bool) -> Self {
        Self {
            snapshot,
            memo,
            producers: BTreeMap::new(),
            locations: BTreeMap::new(),
            documents: BTreeMap::new(),
            history: BTreeMap::new(),
        }
    }

    pub fn complete(snapshot: GraphSnapshot) -> Self {
        Self::new(snapshot, false)
    }

    pub fn memo(snapshot: GraphSnapshot) -> Self {
        Self::new(snapshot, true)
    }
}

impl GraphSource for GraphFacts {
    fn snapshot(&self) -> &GraphSnapshot {
        &self.snapshot
    }

    fn producer_languages(&mut self, producer_id: &str) -> Result<Option<BTreeSet<String>>> {
        match self.producers.get(producer_id) {
            Some(languages) => Ok(Some(languages.clone())),
            None if self.memo => Err(anyhow!(FACTS_MISS)),
            None => Ok(None),
        }
    }

    fn locate(&mut self, syntax_id: &str) -> Result<Option<DocumentKey>> {
        match self.locations.get(syntax_id) {
            Some(location) => Ok(location.clone()),
            None if self.memo => Err(anyhow!(FACTS_MISS)),
            None => Ok(None),
        }
    }

    fn document(&mut self, key: &DocumentKey) -> Result<Arc<DocumentFacts>> {
        self.documents
            .get(key)
            .cloned()
            .ok_or_else(|| anyhow!(FACTS_MISS))
    }

    fn history(&mut self, key: &DocumentKey, syntax_id: &str) -> Result<Option<Arc<HistoryFacts>>> {
        match self.history.get(&(key.clone(), syntax_id.to_string())) {
            Some(history) => Ok(history.clone()),
            None if self.memo => Err(anyhow!(FACTS_MISS)),
            None => Ok(None),
        }
    }

    fn check(&mut self) -> Result<()> {
        Ok(())
    }
}

/// One evaluated query with the facts it read, for re-evaluation without SQL.
#[derive(Debug, Clone)]
pub struct GraphAnswer {
    pub basis: IndexPin,
    pub requested: GraphRequest,
    pub outcome: std::result::Result<GraphResult, GraphError>,
    pub facts: GraphFacts,
}

impl GraphAnswer {
    /// Evaluates the same request over the recorded facts with `maxCalls` replaced.
    pub fn reevaluate(&mut self, max_calls: u64) -> Result<GraphResult> {
        let request = GraphRequest {
            max_calls,
            ..self.requested.clone()
        };
        evaluate(&request, &mut self.facts)?
            .map_err(|e| anyhow!("graph re-evaluation was rejected: {e}"))
    }
}

fn language_name(language: sem::Language) -> &'static str {
    match language {
        sem::Language::Java => "java",
        sem::Language::Rust => "rust",
        sem::Language::Python => "python",
        sem::Language::Javascript => "javascript",
    }
}

type CoverageKey = (String, String, String, String, String);

fn coverage_key(row: &sem::Coverage) -> CoverageKey {
    (
        row.producer_id.clone(),
        row.source_set_id.clone(),
        language_name(row.language).to_string(),
        row.document_path.clone(),
        row.revision_id.clone(),
    )
}

fn selected_incomplete(coverage: &[sem::Coverage]) -> bool {
    coverage.iter().any(|row| {
        row.selected && matches!(row.state, CoverageState::Failed | CoverageState::Partial)
    })
}

/// The producer evidence of a document, or `None` when it carries none by design.
fn semantic<'a>(
    scope: Option<&BTreeSet<String>>,
    doc: &'a DocumentFacts,
) -> Result<Option<&'a SemanticDocument>> {
    let Some(scope) = scope else {
        return Ok(None);
    };
    if !scope.contains(&doc.key.language) {
        return Ok(None);
    }
    match &doc.semantic {
        Some(semantic) => Ok(Some(semantic)),
        None => Err(integrity("missing producer coverage tuple")),
    }
}

fn select_binding<'a>(
    semantic: Option<&'a SemanticDocument>,
    call: &native::Call,
) -> Result<Option<(&'a CallBinding, &'a sem::Provenance)>> {
    let Some(semantic) = semantic else {
        return Ok(None);
    };
    if !semantic.coverage.selected
        || !matches!(
            semantic.coverage.state,
            CoverageState::Complete | CoverageState::Partial
        )
    {
        return Ok(None);
    }
    let mut matches = semantic
        .call_bindings
        .iter()
        .filter(|b| b.call_id.as_deref() == Some(call.id.as_str()))
        .filter(|b| b.join.anchor.document == call.document)
        .filter_map(|b| Some((b, semantic.provenance.get(&b.provenance_id)?)));
    let selected = matches.next();
    if matches.next().is_some() {
        return Err(integrity("multiple call bindings match one call"));
    }
    Ok(selected)
}

fn boundary_reason(binding: &CallBinding, proof: &sem::Provenance) -> BoundaryReason {
    let internal = matches!(binding.declared_target, Some(Target::Internal { .. }));
    if proof.freshness != Freshness::Fresh || (internal && binding.stale_target != Some(false)) {
        BoundaryReason::Stale
    } else if binding.resolution == Resolution::Ambiguous {
        BoundaryReason::Ambiguous
    } else if binding.resolution == Resolution::Unresolved {
        BoundaryReason::Unresolved
    } else if binding.resolution == Resolution::External {
        BoundaryReason::External
    } else if !matches!(binding.dispatch, Dispatch::Direct | Dispatch::Constructor) {
        BoundaryReason::Dispatch
    } else {
        BoundaryReason::None
    }
}

/// Evaluates one rooted graph request. The outer error covers read failures, aborts and
/// [`GraphIntegrity`]; the inner one is a contract failure.
pub fn evaluate(
    request: &GraphRequest,
    source: &mut dyn GraphSource,
) -> Result<std::result::Result<GraphResult, GraphError>> {
    if let Err(e) = request.validate() {
        return Ok(Err(e));
    }
    let snapshot = source.snapshot();
    if request.source_set_id != snapshot.source_set_id {
        return Ok(Err(failure(
            GraphErrorCode::SourceSetDenied,
            "sourceSetId",
            "source set is not admitted for this graph request",
        )));
    }
    if request.revision_id != snapshot.revision_id {
        return Ok(Err(failure(
            GraphErrorCode::RevisionUnavailable,
            "revisionId",
            "revision is not available for this graph request",
        )));
    }
    let Some(root_document) = source.locate(&request.root_syntax_id)? else {
        return Ok(Err(failure(
            GraphErrorCode::RootMissing,
            "rootSyntaxId",
            "Call graph root not found in the admitted snapshot",
        )));
    };
    let scope = match &request.semantic_producer_id {
        None => None,
        Some(producer) => match source.producer_languages(producer)? {
            Some(languages) => Some(languages),
            None => {
                return Ok(Err(failure(
                    GraphErrorCode::ProducerUnavailable,
                    "semanticProducerId",
                    "Semantic producer not published for the admitted revision",
                )));
            }
        },
    };
    let scope = scope.as_ref();

    let root = source
        .document(&root_document)?
        .declarations
        .get(&request.root_syntax_id)
        .cloned()
        .ok_or_else(|| integrity("root declaration missing from located document"))?;
    let mut nodes = vec![GraphNode {
        declaration: root,
        depth: 0,
    }];
    let mut admitted = BTreeSet::from([request.root_syntax_id.clone()]);
    let mut edges: Vec<GraphEdge> = Vec::new();
    let mut frontier: Vec<Frontier> = Vec::new();
    let mut call_cap = false;
    let mut head = 0;
    while head < nodes.len() {
        source.check()?;
        let node_id = nodes[head].declaration.syntax_id.clone();
        let node_depth = nodes[head].depth;
        let doc = source.document(&nodes[head].declaration.document)?;
        head += 1;
        let mut local: Vec<&native::Call> = doc
            .calls
            .get(&node_id)
            .map(|calls| calls.iter().collect())
            .unwrap_or_default();
        if local.is_empty() {
            continue;
        }
        local.sort_by(|a, b| {
            (&a.document.path, a.range.start, a.range.end, &a.id).cmp(&(
                &b.document.path,
                b.range.start,
                b.range.end,
                &b.id,
            ))
        });
        if node_depth == request.depth || call_cap {
            frontier.push(Frontier {
                reason: if node_depth == request.depth {
                    FrontierReason::Depth
                } else {
                    FrontierReason::CallLimit
                },
                node_id,
                call_id: None,
                target_id: None,
                next_ordinal: Some(0),
                omitted_calls: local.len() as u64,
            });
            continue;
        }
        let semantic = semantic(scope, &doc)?;
        for (i, call) in local.iter().enumerate() {
            if edges.len() as u64 == request.max_calls {
                frontier.push(Frontier {
                    reason: FrontierReason::CallLimit,
                    node_id: node_id.clone(),
                    call_id: None,
                    target_id: None,
                    next_ordinal: Some(call.ordinal as u64),
                    omitted_calls: (local.len() - i) as u64,
                });
                call_cap = true;
                break;
            }
            let selected = select_binding(semantic, call)?;
            let mut reason = selected.map_or(BoundaryReason::MissingEvidence, |(b, p)| {
                boundary_reason(b, p)
            });
            let mut to = None;
            let mut visit = Visit::Boundary;
            if reason == BoundaryReason::None {
                let target = selected.and_then(|(b, _)| match &b.declared_target {
                    Some(Target::Internal {
                        syntax_id,
                        document,
                        ..
                    }) => Some((syntax_id, document)),
                    _ => None,
                });
                let located = match target {
                    Some((syntax_id, document)) => {
                        let found = source.locate(syntax_id)?;
                        (found.as_ref() == Some(document)).then_some(syntax_id)
                    }
                    None => None,
                };
                match located {
                    None => reason = BoundaryReason::Stale,
                    Some(target_id) if admitted.contains(target_id) => {
                        to = Some(target_id.clone());
                        visit = Visit::Seen;
                    }
                    Some(target_id) if nodes.len() as u64 == request.max_nodes => {
                        reason = BoundaryReason::NodeLimit;
                        frontier.push(Frontier {
                            reason: FrontierReason::NodeLimit,
                            node_id: node_id.clone(),
                            call_id: Some(call.id.clone()),
                            target_id: Some(target_id.clone()),
                            next_ordinal: None,
                            omitted_calls: 0,
                        });
                    }
                    Some(target_id) => {
                        let (_, document) = target.expect("located implies an internal target");
                        let declaration = source
                            .document(document)?
                            .declarations
                            .get(target_id)
                            .cloned()
                            .ok_or_else(|| integrity("target declaration missing from document"))?;
                        admitted.insert(target_id.clone());
                        nodes.push(GraphNode {
                            declaration,
                            depth: node_depth + 1,
                        });
                        to = Some(target_id.clone());
                        visit = Visit::New;
                    }
                }
            }
            edges.push(GraphEdge {
                call: (*call).clone(),
                from: node_id.clone(),
                to,
                binding: selected.map(|(b, _)| b.clone()),
                visit,
                boundary_reason: reason,
            });
        }
    }

    let (coverage, provenance) = select_evidence(source, scope, &nodes, &edges)?;
    let truncated = !frontier.is_empty();
    let partial = truncated
        || selected_incomplete(&coverage)
        || edges.iter().any(|edge| edge.visit == Visit::Boundary);
    let warnings = warnings_v1(request, &coverage, &provenance, &edges);
    Ok(Ok(GraphResult {
        request: request.clone(),
        resolved_revision_id: request.revision_id.clone(),
        nodes,
        edges,
        frontier,
        coverage,
        provenance,
        partial,
        truncated,
        warnings,
    }))
}

/// Coverage and provenance relevant to the returned nodes and calls, deduplicated and sorted.
fn select_evidence(
    source: &mut dyn GraphSource,
    scope: Option<&BTreeSet<String>>,
    nodes: &[GraphNode],
    edges: &[GraphEdge],
) -> Result<(Vec<sem::Coverage>, Vec<sem::Provenance>)> {
    let mut returned: BTreeMap<DocumentKey, BTreeSet<&str>> = BTreeMap::new();
    for node in nodes {
        returned
            .entry(node.declaration.document.clone())
            .or_default()
            .insert(&node.declaration.syntax_id);
    }
    for edge in edges {
        returned.entry(edge.call.document.clone()).or_default();
    }
    let mut coverage: BTreeMap<CoverageKey, sem::Coverage> = BTreeMap::new();
    let mut provenance: BTreeMap<String, sem::Provenance> = BTreeMap::new();
    let mut add_coverage = |row: &sem::Coverage| {
        coverage.insert(coverage_key(row), row.clone());
    };
    let mut docs = BTreeMap::new();
    for key in returned.keys() {
        docs.insert(key.clone(), source.document(key)?);
    }
    let mut add_proof = |proof: &sem::Provenance| {
        provenance.insert(proof.id.clone(), proof.clone());
    };
    for doc in docs.values() {
        add_coverage(&doc.native_coverage);
        add_proof(&doc.native_provenance);
        if let Some(semantic) = semantic(scope, doc)? {
            add_coverage(&semantic.coverage);
        }
    }
    for edge in edges {
        let Some(binding) = &edge.binding else {
            continue;
        };
        let semantic = semantic(scope, &docs[&edge.call.document])?
            .ok_or_else(|| integrity("binding returned without producer evidence"))?;
        let proof_of = |id: &str| {
            semantic
                .provenance
                .get(id)
                .ok_or_else(|| integrity("binding provenance missing"))
        };
        add_proof(proof_of(&binding.provenance_id)?);
        if binding.resolution == Resolution::Ambiguous {
            for other in semantic
                .call_bindings
                .iter()
                .filter(|b| b.call_id == binding.call_id && b.resolution == Resolution::Ambiguous)
            {
                if let Some(proof) = semantic.provenance.get(&other.provenance_id) {
                    add_proof(proof);
                }
            }
        }
    }
    for (key, ids) in &returned {
        if !nodes.iter().any(|n| &n.declaration.document == key) {
            continue;
        }
        let Some(semantic) = semantic(scope, &docs[key])? else {
            continue;
        };
        if matches!(
            semantic.coverage.state,
            CoverageState::Failed | CoverageState::Omitted
        ) {
            for id in ids {
                if let Some(history) = source.history(key, id)? {
                    add_coverage(&history.coverage);
                    for proof in &history.provenance {
                        add_proof(proof);
                    }
                }
            }
        } else if semantic.coverage.selected {
            let internal_here = |target: &Target| match target {
                Target::Internal {
                    syntax_id,
                    document,
                    ..
                } => document == key && ids.contains(syntax_id.as_str()),
                Target::External { .. } => false,
            };
            let mut named: Vec<&str> = Vec::new();
            named.extend(
                semantic
                    .declaration_bindings
                    .iter()
                    .filter(|b| b.syntax_id.as_deref().is_some_and(|id| ids.contains(id)))
                    .map(|b| b.provenance_id.as_str()),
            );
            named.extend(
                semantic
                    .symbols
                    .iter()
                    .filter(|s| s.declarations.iter().any(internal_here))
                    .map(|s| s.provenance_id.as_str()),
            );
            named.extend(
                semantic
                    .type_relationships
                    .iter()
                    .filter(|r| internal_here(&r.source))
                    .map(|r| r.provenance_id.as_str()),
            );
            for id in named {
                let proof = semantic
                    .provenance
                    .get(id)
                    .ok_or_else(|| integrity("declaration fact provenance missing"))?;
                add_proof(proof);
            }
        }
    }
    Ok((
        coverage.into_values().collect(),
        provenance.into_values().collect(),
    ))
}

/// Exactly the warnings-v1 set, unique by `(code, provenanceId)` and in contract order.
pub fn warnings_v1(
    request: &GraphRequest,
    coverage: &[sem::Coverage],
    provenance: &[sem::Provenance],
    edges: &[GraphEdge],
) -> Vec<Warning> {
    let mut keys: BTreeMap<(WarningCode, Option<String>), &'static str> = BTreeMap::new();
    if request.semantic_producer_id.is_none() {
        keys.insert(
            (WarningCode::SyntaxOnly, None),
            "No semantic producer was selected; only syntax evidence is used.",
        );
    }
    if selected_incomplete(coverage) {
        keys.insert(
            (WarningCode::CoverageIncomplete, None),
            "Selected coverage is failed or partial for a returned document.",
        );
    }
    for proof in provenance {
        match proof.freshness {
            Freshness::Stale => {
                keys.insert(
                    (WarningCode::StaleEvidence, Some(proof.id.clone())),
                    "Returned provenance is stale.",
                );
            }
            Freshness::PossiblyStale => {
                keys.insert(
                    (WarningCode::StaleEvidence, None),
                    "At least one returned provenance is possibly stale.",
                );
            }
            Freshness::Fresh => {}
        }
    }
    for binding in edges.iter().filter_map(|edge| edge.binding.as_ref()) {
        if binding.stale_target == Some(true) {
            keys.insert(
                (
                    WarningCode::StaleTarget,
                    Some(binding.provenance_id.clone()),
                ),
                "A returned binding has a stale target.",
            );
        }
        if binding.resolution == Resolution::Ambiguous {
            keys.insert(
                (
                    WarningCode::BindingAmbiguous,
                    Some(binding.provenance_id.clone()),
                ),
                "A returned binding is ambiguous.",
            );
        }
    }
    keys.into_iter()
        .map(|((code, provenance_id), message)| Warning {
            code,
            message: message.to_string(),
            provenance_id,
        })
        .collect()
}
