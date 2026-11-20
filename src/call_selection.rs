//! The producer selection and call-binding core that the call graph and storage's semantic
//! reads share (S12): the per-language default producers and their notes (#483), the
//! snapshot facts a [`GraphSource`] reads, and `call_disposition`, the binding selection
//! (#466). Pure functions over shared records, with no I/O; re-exported at their old paths
//! in `graph_query`.
use crate::{
    model::BoundaryReason,
    native_records::{self as native, DocumentKey},
    semantic_records::{
        self as sem, CallBinding, CoverageState, Dispatch, Freshness, Resolution, Target,
    },
};
use anyhow::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
};

/// Corrupt or contradictory snapshot data. The store reader maps it to its own integrity error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphIntegrity(pub String);

impl fmt::Display for GraphIntegrity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GraphIntegrity {}

pub(crate) fn integrity(message: &str) -> anyhow::Error {
    GraphIntegrity(message.to_string()).into()
}

pub(crate) fn valid_producer_id(id: &str) -> bool {
    !id.is_empty()
}

pub const NO_PRODUCER_NOTE: &str = "no semantic producer published; showing syntax";
pub const LANGUAGE_NOTE: &str = "semantic producer does not cover this language; showing syntax";

/// The note of a language that two or more published producers cover (#483).
pub fn multiple_producers_note(language: &str) -> String {
    format!("multiple semantic producers for {language}; showing syntax")
}

/// The default producer of each language at one revision (owner ruling 2026-10-30, #483,
/// amending #392): a document uses the sole published producer whose profile covers its
/// language. A language that two or more cover shows syntax only. Producers are never merged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DefaultProducers {
    /// The published producers covering each language, in ID order.
    pub by_language: BTreeMap<String, Vec<String>>,
    /// Whether the revision publishes any producer.
    pub published: bool,
    /// The note of each language whose semantic evidence needs re-import or is catching up,
    /// worded as status and describe word that state (#1255). It replaces the coverage note
    /// of a language without a selected producer.
    pub pending: BTreeMap<String, String>,
}

impl DefaultProducers {
    /// The selection over published `(producer, language)` rows. A producer that declares
    /// no language still counts as published.
    pub fn from_published<I>(rows: I) -> Self
    where
        I: IntoIterator<Item = (String, Option<String>)>,
    {
        let mut selection = Self::default();
        for (producer, language) in rows {
            selection.published = true;
            if let Some(language) = language {
                let producers = selection.by_language.entry(language).or_default();
                if !producers.contains(&producer) {
                    producers.push(producer);
                    producers.sort();
                }
            }
        }
        selection
    }

    /// The language's producer, when exactly one published producer covers it.
    pub fn producer(&self, language: &str) -> Option<&str> {
        match self.by_language.get(language).map(Vec::as_slice) {
            Some([producer]) => Some(producer),
            _ => None,
        }
    }

    /// Why the language shows syntax only, or `None` when it has a producer. A language whose
    /// evidence needs re-import or is catching up says so (#1255); "does not cover this
    /// language" is only for a language that no published producer covers.
    pub fn note(&self, language: &str) -> Option<String> {
        if self.producer(language).is_some() {
            return None;
        }
        if let Some(note) = self.pending.get(language) {
            return Some(note.clone());
        }
        match self.by_language.get(language).map(Vec::len) {
            Some(1) => None,
            Some(_) => Some(multiple_producers_note(language)),
            None if !self.published => Some(NO_PRODUCER_NOTE.to_string()),
            None => Some(LANGUAGE_NOTE.to_string()),
        }
    }

    /// Every language with a selected producer.
    pub fn languages(&self) -> BTreeSet<String> {
        self.by_language
            .iter()
            .filter(|(_, producers)| producers.len() == 1)
            .map(|(language, _)| language.clone())
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphSnapshot {
    pub source_set_id: String,
    pub revision_id: String,
}

/// Semantic evidence of one in-scope document for the selected producer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticDocument {
    pub projection: Option<crate::model::SemanticProjection>,
    pub coverage: sem::Coverage,
    pub provenance: BTreeMap<String, sem::Provenance>,
    pub call_bindings: Vec<CallBinding>,
    pub declaration_bindings: Vec<sem::DeclarationBinding>,
    pub symbols: Vec<sem::Symbol>,
    pub type_relationships: Vec<sem::TypeRelationship>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentFacts {
    pub projection: Option<crate::model::SemanticProjection>,
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
    /// The per-language default of a request without a producer (#483). `None`, as for a
    /// fixture without one, leaves the whole answer syntax only.
    fn default_producers(&mut self) -> Result<Option<DefaultProducers>> {
        Ok(None)
    }
    fn locate(&mut self, syntax_id: &str) -> Result<Option<DocumentKey>>;
    /// Checks the recorded basis at every historical semantic hop, not by name.
    fn projection_endpoint(
        &mut self,
        projection: &crate::model::SemanticProjection,
        producer: &str,
        document: &DocumentKey,
    ) -> Result<Option<BoundaryReason>> {
        let target = self.document(document)?;
        let eligible = target.projection.as_ref().is_some_and(|other| {
            other.semantic_status == crate::model::SemanticStatus::OlderBasis
                && other.semantic_basis_pin == projection.semantic_basis_pin
        }) && target
            .semantic
            .as_ref()
            .is_some_and(|semantic| semantic.coverage.producer_id == producer);
        Ok((!eligible).then(|| endpoint_reason(target.projection.as_ref())))
    }
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
    /// Whether `call`'s measured callee lookup key names a declaration of the call's language
    /// in the admitted snapshot: the Decision 0012 name relation, read in the other
    /// direction (#778). It only orders unbound boundaries; it never targets, expands or filters.
    fn names_declaration(&mut self, _call: &native::Call) -> Result<bool> {
        Ok(false)
    }
    /// Called once per expanded node; an error aborts without a result.
    fn check(&mut self) -> Result<()>;
    /// Deadline and cancellation check during local scanning, without a node-phase hook.
    fn check_work(&mut self) -> Result<()> {
        Ok(())
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
    let matches: Vec<_> = semantic
        .call_bindings
        .iter()
        .filter(|b| b.call_id.as_deref() == Some(call.id.as_str()))
        .filter(|b| b.join.anchor.document == call.document)
        .filter_map(|b| Some((b, semantic.provenance.get(&b.provenance_id)?)))
        .collect();
    // A call has one row per producer fact that joins it (contract-v1: "their separate
    // provenance is preserved"). Publication admits two shapes. Rows that assert at most one
    // distinct target between them agree: scip-typescript gives one occurrence per declaration
    // of a merged library symbol, so `new Set()` is four unresolved rows (#1102). Rows that
    // assert more are contradictory evidence, published as one ambiguous row per provenance,
    // each with the full candidate union (#466). Anything else was never published.
    if matches.len() > 1 {
        let mut asserted = Vec::<&Target>::new();
        for target in matches
            .iter()
            .flat_map(|(b, _)| b.declared_target.iter().chain(&b.candidates))
        {
            if !asserted.contains(&target) {
                asserted.push(target);
            }
        }
        if asserted.len() > 1
            && !matches
                .iter()
                .all(|(b, _)| b.resolution == Resolution::Ambiguous)
        {
            return Err(integrity("contradictory call bindings match one call"));
        }
    }
    Ok(matches
        .into_iter()
        .min_by_key(|(binding, proof)| representative_key(binding.resolution, proof)))
}

/// Orders the rows of one call; the least represents it. A row that asserts the call's target
/// comes before an unresolved one, which asserts none and so contradicts nothing. Equal rows
/// follow their raw producer fact's digest, which depends only on the imported evidence: a
/// minted provenance ID also hashes the native revision, so it changes with the build. The ID
/// only separates rows the digest cannot. `store::semantic_display` orders its rows the same way.
fn representative_key(
    resolution: Resolution,
    proof: &sem::Provenance,
) -> (bool, Option<&str>, &str) {
    (
        resolution == Resolution::Unresolved,
        proof
            .derived_from
            .as_ref()
            .map(|raw| raw.fact_digest.as_str()),
        proof.id.as_str(),
    )
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
    let projection = semantic.and_then(|semantic| semantic.projection.as_ref());
    let mut reason = selected.map_or(BoundaryReason::MissingEvidence, |(b, p)| {
        if projection.is_some() && p.freshness != Freshness::Stale && b.stale_target != Some(true) {
            if b.resolution == Resolution::Ambiguous {
                BoundaryReason::Ambiguous
            } else if b.resolution == Resolution::Unresolved {
                BoundaryReason::Unresolved
            } else if b.resolution == Resolution::External {
                BoundaryReason::External
            } else if !matches!(b.dispatch, Dispatch::Direct | Dispatch::Constructor) {
                BoundaryReason::Dispatch
            } else {
                BoundaryReason::None
            }
        } else {
            boundary_reason(b, p)
        }
    });
    if let (Some(projection), Some((binding, proof))) = (projection, selected) {
        let mut pending = if projection.semantic_status == crate::model::SemanticStatus::OlderBasis
        {
            source.projection_endpoint(projection, &proof.producer_id, &call.document)?
        } else {
            Some(BoundaryReason::pending(projection.semantic_status))
        };
        for target in binding
            .declared_target
            .iter()
            .chain(&binding.candidates)
            .chain(&binding.possible_dispatch)
        {
            if let Target::Internal {
                document,
                revision_id,
                ..
            } = target
                && pending.is_none()
            {
                pending = if revision_id == &projection.basis_native_revision_id {
                    source.projection_endpoint(projection, &proof.producer_id, document)?
                } else {
                    Some(BoundaryReason::CatchingUp)
                };
            }
        }
        if let Some(pending) = pending {
            reason = pending;
        }
    }
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
        Some((id, key))
            if !matches!(
                reason,
                BoundaryReason::CatchingUp | BoundaryReason::NeedsReimport
            ) && source.locate(id)?.as_ref() == Some(key) =>
        {
            Some((id, key))
        }
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

/// An ineligible endpoint's boundary: its own pending status, else catching up.
pub(crate) fn endpoint_reason(target: Option<&crate::model::SemanticProjection>) -> BoundaryReason {
    target
        .filter(|projection| projection.semantic_status.is_pending())
        .map_or(BoundaryReason::CatchingUp, |projection| {
            BoundaryReason::pending(projection.semantic_status)
        })
}
