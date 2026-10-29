//! Rooted outgoing call-graph answers (contract-v1 "Rooted graph answers").
//!
//! This is a pure core: it reads through [`GraphSource`] and never touches SQLite. The
//! store reader supplies a source over one admitted snapshot; fixtures and response-byte
//! prefix projection use [`GraphFacts`].
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
    Filtered,
    ResponseBytes,
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
    #[serde(default)]
    pub include_external: bool,
    #[serde(default)]
    pub include_tests: bool,
}

fn is_syntax_id(id: &str) -> bool {
    id.strip_prefix("sid:v1:").is_some_and(|hex| {
        hex.len() == 32 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

pub(crate) fn valid_producer_id(id: &str) -> bool {
    !id.is_empty()
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
        if self
            .semantic_producer_id
            .as_deref()
            .is_some_and(|id| !valid_producer_id(id))
        {
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
    pub include_external: bool,
    pub include_tests: bool,
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
    pub call_site_count: u64,
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
pub struct GraphOmitted {
    pub filtered: u64,
    pub call_limit: u64,
    pub response_bytes: u64,
    pub depth: u64,
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
    pub omitted: GraphOmitted,
    pub hints: Vec<String>,
    /// Evaluation-only filter hints by owner, retained for frozen prefix projection.
    #[serde(skip)]
    pub filtered_hints: BTreeMap<String, BTreeSet<String>>,
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
    /// The role of an exactly located declaration, without requiring its whole document.
    fn declaration_role(
        &mut self,
        key: &DocumentKey,
        syntax_id: &str,
    ) -> Result<native::DeclarationRole> {
        self.document(key)?
            .declarations
            .get(syntax_id)
            .map(|declaration| declaration.role)
            .ok_or_else(|| integrity("target declaration missing from document"))
    }
    fn history(&mut self, key: &DocumentKey, syntax_id: &str) -> Result<Option<Arc<HistoryFacts>>>;
    /// Called once per expanded node; an error aborts without a result.
    fn check(&mut self) -> Result<()>;
    /// Deadline and cancellation check during local scanning, without a node-phase hook.
    fn check_work(&mut self) -> Result<()> {
        Ok(())
    }
}

/// In-memory record of every fact one query read.
///
/// In complete mode an absent location, producer or history entry means "not there". In
/// memo mode only a recorded lookup is answered and any other lookup is an error, so a
/// prefix projection can never need SQL.
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

impl GraphSource for &GraphFacts {
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

impl GraphSource for GraphFacts {
    fn snapshot(&self) -> &GraphSnapshot {
        &self.snapshot
    }
    fn producer_languages(&mut self, id: &str) -> Result<Option<BTreeSet<String>>> {
        (&*self).producer_languages(id)
    }
    fn locate(&mut self, id: &str) -> Result<Option<DocumentKey>> {
        (&*self).locate(id)
    }
    fn document(&mut self, key: &DocumentKey) -> Result<Arc<DocumentFacts>> {
        (&*self).document(key)
    }
    fn history(&mut self, key: &DocumentKey, id: &str) -> Result<Option<Arc<HistoryFacts>>> {
        (&*self).history(key, id)
    }
    fn check(&mut self) -> Result<()> {
        Ok(())
    }
}

/// One bounded query answer with its frozen facts, for prefix projection without SQL.
#[derive(Debug, Clone)]
pub struct GraphAnswer {
    pub basis: IndexPin,
    pub requested: GraphRequest,
    pub outcome: std::result::Result<GraphResult, GraphError>,
    pub facts: GraphFacts,
}

impl GraphAnswer {
    /// Projects an ancestor-closed prefix without traversing or resolving the graph again.
    pub fn prefix(&self, edge_count: usize) -> Result<GraphResult> {
        let full = self
            .outcome
            .as_ref()
            .map_err(|e| anyhow!("graph answer rejected: {e}"))?;
        let count = edge_count.min(full.edges.len());
        if count == full.edges.len() {
            return Ok(full.clone());
        }
        let edges = full.edges[..count].to_vec();
        let mut retained = BTreeSet::from([self.requested.root_syntax_id.clone()]);
        for edge in &edges {
            if let Some(to) = &edge.to {
                retained.insert(to.clone());
            }
        }
        let mut nodes: Vec<_> = full
            .nodes
            .iter()
            .filter(|node| retained.contains(&node.declaration.syntax_id))
            .cloned()
            .collect();
        shortest_depths(&mut nodes, &edges);
        let mut frontier: Vec<_> = full
            .frontier
            .iter()
            .filter(|f| retained.contains(&f.node_id))
            .filter(|f| {
                f.reason != FrontierReason::NodeLimit
                    || edges.iter().any(|e| Some(&e.call.id) == f.call_id.as_ref())
            })
            .cloned()
            .collect();
        for edge in &full.edges[count..] {
            if retained.contains(&edge.from) {
                add_omission(
                    &mut frontier,
                    &edge.from,
                    FrontierReason::ResponseBytes,
                    edge.call_site_count,
                );
            }
        }
        let mut facts = &self.facts;
        let languages = match &self.requested.semantic_producer_id {
            Some(id) => facts.producer_languages(id)?,
            None => None,
        };
        finish(
            &self.requested,
            &mut facts,
            languages.as_ref(),
            nodes,
            edges,
            frontier,
            full.filtered_hints
                .iter()
                .filter(|(owner, _)| retained.contains(*owner))
                .map(|(owner, hints)| (owner.clone(), hints.clone()))
                .collect(),
        )
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
pub(crate) fn semantic<'a>(
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

/// One measured occurrence, before graph grouping or traversal limits.
pub(crate) struct CallDisposition<'a> {
    pub selected: Option<(&'a CallBinding, &'a sem::Provenance)>,
    pub boundary_reason: BoundaryReason,
    pub exact_target: Option<(&'a String, &'a DocumentKey)>,
}

pub(crate) fn call_disposition<'a>(
    semantic: Option<&'a SemanticDocument>,
    call: &native::Call,
    source: &mut dyn GraphSource,
) -> Result<CallDisposition<'a>> {
    let selected = select_binding(semantic, call)?;
    let mut reason = selected.map_or(BoundaryReason::MissingEvidence, |(b, p)| {
        boundary_reason(b, p)
    });
    let target = selected.and_then(|(binding, _)| {
        if binding.resolution != Resolution::Resolved {
            return None;
        }
        match &binding.declared_target {
            Some(Target::Internal {
                syntax_id,
                document,
                ..
            }) => Some((syntax_id, document)),
            _ => None,
        }
    });
    let exact_target = match target {
        Some((id, key)) if source.locate(id)?.as_ref() == Some(key) => Some((id, key)),
        _ => None,
    };
    if reason == BoundaryReason::None && exact_target.is_none() {
        reason = BoundaryReason::Stale;
    }
    Ok(CallDisposition {
        selected,
        boundary_reason: reason,
        exact_target,
    })
}

/// The same pre-grouping eligibility used by rooted expansion, for line navigation.
pub(crate) fn resolved_call_target<'a>(
    semantic: Option<&'a SemanticDocument>,
    call: &native::Call,
) -> Result<Option<(&'a str, &'a DocumentKey)>> {
    let Some((binding, proof)) = select_binding(semantic, call)? else {
        return Ok(None);
    };
    if boundary_reason(binding, proof) != BoundaryReason::None
        || binding.resolution != Resolution::Resolved
    {
        return Ok(None);
    }
    Ok(match &binding.declared_target {
        Some(Target::Internal {
            syntax_id,
            document,
            ..
        }) => Some((syntax_id.as_str(), document)),
        _ => None,
    })
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
    let mut edges = Vec::new();
    let mut frontier = Vec::new();
    let mut filtered_hints: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut pending: Vec<Candidate> = Vec::new();
    let mut head = 0;
    loop {
        // Each admitted owner is inspected once. Only the best remaining edge groups
        // are kept; discarded candidates contribute measured-site omissions immediately.
        while head < nodes.len() {
            source.check()?;
            let node = &nodes[head];
            let node_id = node.declaration.syntax_id.clone();
            let node_depth = node.depth;
            let doc = source.document(&node.declaration.document)?;
            head += 1;
            let mut local: Vec<_> = doc
                .calls
                .get(&node_id)
                .map(|calls| calls.iter().collect())
                .unwrap_or_default();
            local.sort_by_key(|call| {
                (
                    &call.document.path,
                    call.range.start,
                    call.range.end,
                    &call.id,
                )
            });
            if node_depth == request.depth {
                if !local.is_empty() {
                    add_omission(
                        &mut frontier,
                        &node_id,
                        FrontierReason::Depth,
                        local.len() as u64,
                    );
                }
                continue;
            }
            let semantic = semantic(scope, &doc)?;
            for call in local {
                source.check_work()?;
                let disposition = call_disposition(semantic, call, source)?;
                let selected = disposition.selected;
                let reason = disposition.boundary_reason;
                let exact_target = disposition.exact_target;
                let external = selected.is_some_and(|(b, _)| b.resolution == Resolution::External);
                let target_role = exact_target
                    .map(|(id, key)| source.declaration_role(key, id))
                    .transpose()?;
                let test = (node_id != request.root_syntax_id
                    && node.declaration.role == native::DeclarationRole::Test)
                    || target_role == Some(native::DeclarationRole::Test);
                if (external && !request.include_external) || (test && !request.include_tests) {
                    add_omission(&mut frontier, &node_id, FrontierReason::Filtered, 1);
                    let hints = filtered_hints.entry(node_id.clone()).or_default();
                    if external && !request.include_external {
                        hints.insert("includeExternal".to_string());
                    }
                    if test && !request.include_tests {
                        hints.insert("includeTests".to_string());
                    }
                    continue;
                }
                let group = if exact_target.is_some() {
                    selected
                        .map(|(b, proof)| {
                            serde_json::to_string(&(
                                &b.resolution,
                                &b.declared_target,
                                &b.candidates,
                                &b.dispatch,
                                &b.possible_dispatch,
                                b.possible_dispatch_complete,
                                b.stale_target,
                                &b.provenance_id,
                                &b.join.status,
                                &b.join.diagnostic,
                                proof,
                                reason,
                                external,
                                test,
                            ))
                        })
                        .transpose()?
                } else {
                    None
                };
                if let Some(existing) = pending.iter_mut().find(|candidate| {
                    candidate.edge.from == node_id && group.is_some() && candidate.group == group
                }) {
                    existing.edge.call_site_count += 1;
                    continue;
                }
                let rank = if reason == BoundaryReason::None {
                    0
                } else if external {
                    1
                } else {
                    2
                };
                pending.push(Candidate {
                    rank,
                    owner_depth: node_depth,
                    owner_document: doc.key.clone(),
                    group,
                    target: (reason == BoundaryReason::None)
                        .then_some(exact_target)
                        .flatten()
                        .map(|(id, key)| (id.clone(), key.clone())),
                    edge: GraphEdge {
                        call_site_count: 1,
                        call: call.clone(),
                        from: node_id.clone(),
                        to: None,
                        binding: selected.map(|(b, _)| b.clone()),
                        visit: Visit::Boundary,
                        boundary_reason: reason,
                    },
                });
                pending.sort_by(|a, b| a.key().cmp(&b.key()));
                if pending.len() > (request.max_calls as usize).saturating_sub(edges.len()) {
                    let discarded = pending.pop().expect("one pending candidate");
                    add_omission(
                        &mut frontier,
                        &discarded.edge.from,
                        FrontierReason::CallLimit,
                        discarded.edge.call_site_count,
                    );
                }
            }
        }
        if pending.is_empty() {
            break;
        }
        let mut candidate = pending.remove(0);
        if let Some((id, key)) = candidate.target {
            if admitted.contains(&id) {
                candidate.edge.to = Some(id);
                candidate.edge.visit = Visit::Seen;
            } else if nodes.len() as u64 == request.max_nodes {
                candidate.edge.boundary_reason = BoundaryReason::NodeLimit;
                frontier.push(Frontier {
                    reason: FrontierReason::NodeLimit,
                    node_id: candidate.edge.from.clone(),
                    call_id: Some(candidate.edge.call.id.clone()),
                    target_id: Some(id),
                    next_ordinal: None,
                    omitted_calls: 0,
                });
            } else {
                let declaration = source
                    .document(&key)?
                    .declarations
                    .get(&id)
                    .cloned()
                    .ok_or_else(|| integrity("target declaration missing from document"))?;
                admitted.insert(id.clone());
                nodes.push(GraphNode {
                    declaration,
                    depth: candidate.owner_depth + 1,
                });
                candidate.edge.to = Some(id);
                candidate.edge.visit = Visit::New;
            }
        }
        edges.push(candidate.edge);
    }
    shortest_depths(&mut nodes, &edges);
    Ok(Ok(finish(
        request,
        source,
        scope,
        nodes,
        edges,
        frontier,
        filtered_hints,
    )?))
}

struct Candidate {
    rank: u8,
    owner_depth: u64,
    owner_document: DocumentKey,
    group: Option<String>,
    target: Option<(String, DocumentKey)>,
    edge: GraphEdge,
}

impl Candidate {
    fn key(&self) -> (u8, u64, &DocumentKey, &str, &str, usize, usize, &str) {
        (
            self.rank,
            self.owner_depth,
            &self.owner_document,
            &self.edge.from,
            &self.edge.call.document.path,
            self.edge.call.range.start,
            self.edge.call.range.end,
            &self.edge.call.id,
        )
    }
}

fn add_omission(frontier: &mut Vec<Frontier>, node_id: &str, reason: FrontierReason, count: u64) {
    if count == 0 {
        return;
    }
    if let Some(existing) = frontier
        .iter_mut()
        .find(|f| f.node_id == node_id && f.reason == reason)
    {
        existing.omitted_calls += count;
    } else {
        frontier.push(Frontier {
            reason,
            node_id: node_id.to_string(),
            call_id: None,
            target_id: None,
            next_ordinal: (reason == FrontierReason::Depth).then_some(0),
            omitted_calls: count,
        });
    }
}

fn shortest_depths(nodes: &mut [GraphNode], edges: &[GraphEdge]) {
    let mut depths = BTreeMap::from([(nodes[0].declaration.syntax_id.clone(), 0)]);
    loop {
        let mut changed = false;
        for edge in edges {
            if let (Some(from), Some(to)) = (depths.get(&edge.from).copied(), &edge.to)
                && depths.get(to).is_none_or(|depth| *depth > from + 1)
            {
                depths.insert(to.clone(), from + 1);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for node in nodes {
        node.depth = depths[&node.declaration.syntax_id];
    }
}

fn finish(
    request: &GraphRequest,
    source: &mut dyn GraphSource,
    scope: Option<&BTreeSet<String>>,
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    mut frontier: Vec<Frontier>,
    filtered_hints: BTreeMap<String, BTreeSet<String>>,
) -> Result<GraphResult> {
    let order: BTreeMap<_, _> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.declaration.syntax_id.as_str(), i))
        .collect();
    let rank = |reason| match reason {
        FrontierReason::Depth => 0,
        FrontierReason::NodeLimit => 1,
        FrontierReason::CallLimit => 2,
        FrontierReason::Filtered => 3,
        FrontierReason::ResponseBytes => 4,
    };
    frontier.sort_by_key(|f| {
        (
            order[f.node_id.as_str()],
            rank(f.reason),
            f.call_id.clone(),
            f.target_id.clone(),
        )
    });
    let mut omitted = GraphOmitted {
        filtered: 0,
        call_limit: 0,
        response_bytes: 0,
        depth: 0,
    };
    for f in &frontier {
        match f.reason {
            FrontierReason::Depth => omitted.depth += f.omitted_calls,
            FrontierReason::CallLimit => omitted.call_limit += f.omitted_calls,
            FrontierReason::Filtered => omitted.filtered += f.omitted_calls,
            FrontierReason::ResponseBytes => omitted.response_bytes += f.omitted_calls,
            FrontierReason::NodeLimit => {}
        }
    }
    let mut hints = BTreeSet::new();
    if omitted.depth > 0 && request.depth < MAX_DEPTH {
        hints.insert("depth".to_string());
    }
    if omitted.call_limit > 0 && request.max_calls < MAX_CALLS {
        hints.insert("maxCalls".to_string());
    }
    if frontier
        .iter()
        .any(|f| f.reason == FrontierReason::NodeLimit)
        && request.max_nodes < MAX_NODES
    {
        hints.insert("maxNodes".to_string());
    }
    for owner_hints in filtered_hints.values() {
        hints.extend(owner_hints.iter().cloned());
    }
    if frontier
        .iter()
        .any(|f| f.reason != FrontierReason::Filtered)
    {
        hints.insert("rootSyntaxId".to_string());
    }
    let (coverage, provenance) = select_evidence(source, scope, &nodes, &edges)?;
    let truncated = frontier
        .iter()
        .any(|f| f.reason != FrontierReason::Filtered);
    let partial = truncated
        || omitted.filtered > 0
        || selected_incomplete(&coverage)
        || edges.iter().any(|edge| edge.visit == Visit::Boundary);
    let warnings = warnings_v1(request, &coverage, &provenance, &edges);
    Ok(GraphResult {
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
        omitted,
        hints: hints.into_iter().collect(),
        filtered_hints,
    })
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

/// Engine evidence/warnings for ungrouped measured occurrences in another pinned view.
pub(crate) fn occurrence_warnings(
    request: &GraphRequest,
    source: &mut dyn GraphSource,
    scope: Option<&BTreeSet<String>>,
    nodes: &[GraphNode],
    edges: &[GraphEdge],
) -> Result<Vec<Warning>> {
    let (coverage, provenance) = select_evidence(source, scope, nodes, edges)?;
    Ok(warnings_v1(request, &coverage, &provenance, edges))
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
