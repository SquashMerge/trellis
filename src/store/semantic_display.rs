//! Semantic bindings shown by the CLI query, the MCP call tools, status and methods (#467).
//!
//! These reads apply the browser's producer policy (#392/#409): an explicit published
//! producer, else the sole published one. None or several leave syntax only, with a note.
//! Bindings are selected by the call-graph engine's `call_disposition`, the reader that
//! the call graph, sequence and line navigation share (#466).
use super::*;
use crate::{
    graph_query::{
        self as engine, BoundaryReason, DocumentFacts, GraphIntegrity, GraphSnapshot, GraphSource,
        HistoryFacts, SemanticDocument,
    },
    model::SemanticState,
    native_evidence::DocumentKey,
    semantic_evidence as sem,
};

pub const NO_PRODUCER_NOTE: &str = "no semantic producer published; showing syntax";
pub const MULTIPLE_PRODUCERS_NOTE: &str = "multiple semantic producers; showing syntax";
pub const LANGUAGE_NOTE: &str = "semantic producer does not cover this language; showing syntax";

/// An explicitly requested producer that the selected revision does not publish.
#[derive(Debug)]
pub struct ProducerNotPublished;

impl std::fmt::Display for ProducerNotPublished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "producer_unavailable: semantic producer is not published for the selected revision",
        )
    }
}

impl std::error::Error for ProducerNotPublished {}

/// The producer a read uses, or why it shows syntax only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProducerChoice {
    pub producer: Option<String>,
    pub note: Option<&'static str>,
}

/// An explicit producer must be published at this revision. Without one, the sole published
/// producer is selected; none or several select nothing. Producers are never merged.
pub(super) fn choose_producer(
    db: &Connection,
    revision_key: &str,
    explicit: Option<&str>,
) -> Result<ProducerChoice> {
    if let Some(id) = explicit {
        ensure!(
            crate::model::valid_semantic_producer_id(id),
            "invalid_argument: semanticProducerId must contain 1..512 bytes and no NUL"
        );
        let published: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM semantic_revision_producers WHERE revision_id=?1 AND id=?2)",
            params![revision_key, id],
            |row| row.get(0),
        )?;
        ensure!(published, ProducerNotPublished);
        return Ok(ProducerChoice {
            producer: Some(id.to_owned()),
            note: None,
        });
    }
    let mut producers = db
        .prepare_cached(
            "SELECT id FROM semantic_revision_producers WHERE revision_id=?1 ORDER BY id LIMIT 2",
        )?
        .query_map([revision_key], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if producers.iter().any(|id| !engine::valid_producer_id(id)) {
        return Err(SelectedIntegrity("invalid selected semantic producer ID".into()).into());
    }
    Ok(match producers.len() {
        0 => ProducerChoice {
            producer: None,
            note: Some(NO_PRODUCER_NOTE),
        },
        1 => ProducerChoice {
            producer: producers.pop(),
            note: None,
        },
        _ => ProducerChoice {
            producer: None,
            note: Some(MULTIPLE_PRODUCERS_NOTE),
        },
    })
}

/// One measured call's selected binding.
#[derive(Debug, Clone)]
pub struct CallSemantic {
    pub binding: sem::CallBinding,
    pub provenance: sem::Provenance,
    /// `none` when the call graph would traverse the call; otherwise why it stops there.
    pub boundary_reason: BoundaryReason,
    /// The declared internal target of a resolved binding, located exactly in this snapshot
    /// and not stale. A `dispatch` boundary keeps its target: dispatch is not proven direct.
    pub target_id: Option<String>,
    /// The coverage tuple of the call's document.
    pub coverage: sem::Coverage,
    /// Every proof of the call's bindings: one per row of an ambiguous call.
    pub proofs: Vec<sem::Provenance>,
}

/// Only `locate` is read by `call_disposition`; it checks an exact target's location.
struct Locate<'a> {
    db: &'a Connection,
    selected: &'a ReadRevision,
    snapshot: GraphSnapshot,
}

impl GraphSource for Locate<'_> {
    fn snapshot(&self) -> &GraphSnapshot {
        &self.snapshot
    }
    fn producer_languages(&mut self, _: &str) -> Result<Option<BTreeSet<String>>> {
        anyhow::bail!("store_unavailable: semantic display reads no producer languages")
    }
    fn locate(&mut self, syntax_id: &str) -> Result<Option<DocumentKey>> {
        Ok(v8_owner_scope_for(self.db, syntax_id, self.selected)?.map(|scope| scope.key))
    }
    fn document(&mut self, _: &DocumentKey) -> Result<Arc<DocumentFacts>> {
        anyhow::bail!("store_unavailable: semantic display reads no graph documents")
    }
    fn history(&mut self, _: &DocumentKey, _: &str) -> Result<Option<Arc<HistoryFacts>>> {
        anyhow::bail!("store_unavailable: semantic display reads no history")
    }
    fn check(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Reads the selected producer's binding for measured calls in one selected snapshot.
pub(super) struct SemanticCalls<'a> {
    db: &'a Connection,
    selected: &'a ReadRevision,
    pub choice: ProducerChoice,
    languages: BTreeSet<String>,
    tuples: BTreeMap<(String, String), Option<i64>>,
    locate: Locate<'a>,
}

impl<'a> SemanticCalls<'a> {
    pub(super) fn new(
        db: &'a Connection,
        selected: &'a ReadRevision,
        explicit: Option<&str>,
    ) -> Result<Self> {
        let choice = choose_producer(db, &selected.key, explicit)?;
        let languages = match &choice.producer {
            Some(producer) => db
                .prepare_cached(
                    "SELECT l.language FROM semantic_revision_producers p JOIN semantic_producer_languages l ON l.id=p.id AND l.version=p.version WHERE p.revision_id=?1 AND p.id=?2",
                )?
                .query_map(params![selected.key, producer], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<BTreeSet<_>>>()?,
            None => BTreeSet::new(),
        };
        Ok(Self {
            db,
            selected,
            choice,
            languages,
            tuples: BTreeMap::new(),
            locate: Locate {
                db,
                selected,
                snapshot: GraphSnapshot {
                    source_set_id: String::new(),
                    revision_id: String::new(),
                },
            },
        })
    }

    /// Whether the selected producer covers a language.
    pub(super) fn covers(&self, language: &str) -> bool {
        self.languages.contains(language)
    }

    /// The note for a read whose documents are all in `language`.
    pub(super) fn note_for(&self, language: &str) -> Option<&'static str> {
        match self.choice.producer {
            Some(_) if !self.covers(language) => Some(LANGUAGE_NOTE),
            _ => self.choice.note,
        }
    }

    /// The producer's coverage tuple of an in-scope document; `None` out of scope.
    fn tuple(&mut self, language: &str, path: &str) -> Result<Option<i64>> {
        let Some(producer) = &self.choice.producer else {
            return Ok(None);
        };
        if !self.languages.contains(language) {
            return Ok(None);
        }
        let key = (language.to_owned(), path.to_owned());
        if let Some(tuple) = self.tuples.get(&key) {
            return Ok(*tuple);
        }
        let tuple: Option<i64> = self
            .db
            .query_row(
                "SELECT tuple_id FROM semantic_coverage WHERE revision_id=?1 AND producer_id=?2 AND language=?3 AND path=?4",
                params![self.selected.key, producer, language, path],
                |row| row.get(0),
            )
            .optional()?;
        let tuple = tuple.context(SelectedIntegrity("missing producer coverage tuple".into()))?;
        self.tuples.insert(key, Some(tuple));
        Ok(Some(tuple))
    }

    /// The selected binding of one measured call, or `None` for syntax-only evidence.
    pub(super) fn call(
        &mut self,
        call: &crate::native_evidence::Call,
    ) -> Result<Option<CallSemantic>> {
        let Some(tuple) = self.tuple(&call.document.language, &call.document.path)? else {
            return Ok(None);
        };
        let encoded = id_codec::encode_occ(&call.id)
            .context(SelectedIntegrity("invalid selected call ID".into()))?;
        let facts = read_semantic_tuple_for_call(
            self.db,
            &self.selected.key,
            Some(tuple),
            None,
            true,
            Some(&encoded),
        )?;
        let [coverage] = <[_; 1]>::try_from(facts.coverage)
            .map_err(|_| SelectedIntegrity("corrupt selected semantic coverage".into()))?;
        let semantic = SemanticDocument {
            coverage: coverage.clone(),
            provenance: facts
                .provenance
                .iter()
                .map(|proof| (proof.id.clone(), proof.clone()))
                .collect(),
            call_bindings: facts.call_bindings,
            declaration_bindings: vec![],
            symbols: vec![],
            type_relationships: vec![],
        };
        let disposition = engine::call_disposition(Some(&semantic), call, &mut self.locate)
            .map_err(|error| match error.downcast::<GraphIntegrity>() {
                Ok(integrity) => SelectedIntegrity(integrity.0).into(),
                Err(error) => error,
            })?;
        let Some((binding, proof)) = disposition.selected else {
            return Ok(None);
        };
        let bound: BTreeSet<&str> = semantic
            .call_bindings
            .iter()
            .filter(|b| b.call_id.as_deref() == Some(call.id.as_str()))
            .map(|b| b.provenance_id.as_str())
            .collect();
        Ok(Some(CallSemantic {
            binding: binding.clone(),
            provenance: proof.clone(),
            boundary_reason: disposition.boundary_reason,
            target_id: disposition
                .exact_target
                .filter(|_| disposition.boundary_reason != BoundaryReason::Stale)
                .map(|(id, _)| id.clone()),
            coverage,
            proofs: facts
                .provenance
                .into_iter()
                .filter(|proof| bound.contains(proof.id.as_str()))
                .collect(),
        }))
    }

    /// The selected producer's state for one document: fresh or stale when its selected
    /// coverage tuple is complete or partial, otherwise unavailable.
    pub(super) fn document_state(&mut self, language: &str, path: &str) -> Result<SemanticState> {
        let Some(tuple) = self.tuple(language, path)? else {
            return Ok(SemanticState::Unavailable);
        };
        let (usable, stale): (bool, bool) = self.db.query_row(
            "SELECT c.selected=1 AND c.state IN ('complete','partial'),
                    EXISTS(SELECT 1 FROM semantic_provenance p WHERE p.tuple_id=c.tuple_id AND p.freshness<>'fresh')
             FROM semantic_coverage c WHERE c.tuple_id=?1",
            [tuple],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(match (usable, stale) {
            (false, _) => SemanticState::Unavailable,
            (true, true) => SemanticState::Stale,
            (true, false) => SemanticState::Fresh,
        })
    }
}

/// Replace status's publication-time semantic counts with the selected producer's bindings:
/// internal, external and ambiguous calls, with the rest unresolved.
pub(super) fn status_semantics(
    db: &Connection,
    status: &mut crate::model::IndexStatus,
) -> Result<()> {
    let key = semantic_pin_key(status.revision);
    let choice = choose_producer(db, &key, None)?;
    status.semantic_producer_id = choice.producer.clone();
    status.semantic_note = choice.note.map(str::to_owned);
    let stats = &mut status.stats;
    let Some(producer) = choice.producer else {
        stats.semantic_state = SemanticState::Unavailable;
        return Ok(());
    };
    // Rows of one ambiguous call share its call ID; each call counts once.
    let mut statement = db.prepare_cached(
        "SELECT CASE WHEN b.resolution='resolved' AND b.declared_kind='internal' THEN 0
                     WHEN b.resolution='external' OR b.declared_kind='external' THEN 1
                     WHEN b.resolution='ambiguous' THEN 2 ELSE 3 END AS kind,
                count(DISTINCT b.call_id)
         FROM semantic_coverage c JOIN semantic_call_bindings b ON b.tuple_id=c.tuple_id
         WHERE c.revision_id=?1 AND c.producer_id=?2 AND c.selected=1
           AND c.state IN ('complete','partial') AND b.call_id IS NOT NULL
         GROUP BY kind",
    )?;
    let mut counts = [0_usize; 4];
    for row in statement.query_map(params![key, producer], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })? {
        let (kind, count) = row?;
        counts[kind as usize] = count as usize;
    }
    let (usable, stale): (bool, bool) = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM semantic_coverage WHERE revision_id=?1 AND producer_id=?2 AND selected=1 AND state IN ('complete','partial')),
                EXISTS(SELECT 1 FROM semantic_coverage c JOIN semantic_provenance p ON p.tuple_id=c.tuple_id
                       WHERE c.revision_id=?1 AND c.producer_id=?2 AND c.selected=1
                         AND c.state IN ('complete','partial') AND p.freshness<>'fresh')",
        params![key, producer],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    stats.semantic_state = match (usable, stale) {
        (false, _) => SemanticState::Unavailable,
        (true, true) => SemanticState::Stale,
        (true, false) => SemanticState::Fresh,
    };
    stats.internal = counts[0];
    stats.external = counts[1];
    stats.ambiguous = counts[2];
    stats.unresolved = stats
        .calls
        .saturating_sub(counts[0] + counts[1] + counts[2]);
    Ok(())
}

/// Attach the selected bindings of a CLI query's calls and mark their semantic state.
pub(super) fn view_semantics(
    db: &Connection,
    selected: &ReadRevision,
    explicit: Option<&str>,
    seed: &mut crate::model::Symbol,
    language: &str,
    calls: &mut [crate::model::CallSite],
) -> Result<crate::model::ViewSemantic> {
    let mut reader = SemanticCalls::new(db, selected, explicit)?;
    let mut view = crate::model::ViewSemantic {
        producer_id: reader.choice.producer.clone(),
        note: reader.note_for(language).map(str::to_owned),
        bindings: vec![],
    };
    if view.note.is_some() {
        return Ok(view);
    }
    seed.provenance.semantic = reader.document_state(language, &seed.path)?;
    let scope = v8_owner_scope_for(db, &seed.id, selected)?.ok_or_else(|| {
        SelectedIntegrity("incompatible_index: selected view seed missing".into())
    })?;
    let native: BTreeMap<String, crate::native_evidence::Call> =
        read_native_calls_v8(db, &scope, Some(&seed.id))?
            .into_iter()
            .map(|call| (call.id.clone(), call))
            .collect();
    for call in calls {
        let Some(measured) = native.get(&call.id) else {
            return Err(SelectedIntegrity("selected view call missing".into()).into());
        };
        let Some(semantic) = reader.call(measured)? else {
            continue;
        };
        call.provenance.semantic = match semantic.provenance.freshness {
            sem::Freshness::Fresh => SemanticState::Fresh,
            _ => SemanticState::Stale,
        };
        view.bindings.push(crate::model::ViewCallBinding {
            call_id: call.id.clone(),
            boundary_reason: semantic.boundary_reason,
            target_id: semantic.target_id,
            freshness: semantic.provenance.freshness,
            binding: semantic.binding,
        });
    }
    Ok(view)
}

/// The selected producer's state of every symbol in one document (methods API).
pub(super) fn mark_document_symbols(
    db: &Connection,
    selected: &ReadRevision,
    language: &str,
    path: &str,
    symbols: &mut [crate::model::Symbol],
) -> Result<()> {
    let mut reader = SemanticCalls::new(db, selected, None)?;
    let state = reader.document_state(language, path)?;
    for symbol in symbols {
        symbol.provenance.semantic = state;
    }
    Ok(())
}

/// The selected bindings of MCP call items, aligned with the calls (#467).
pub struct McpSemantic {
    pub producer_id: Option<String>,
    /// Why every call is syntax only, when it is.
    pub note: Option<&'static str>,
    pub calls: Vec<Option<CallSemantic>>,
}

impl McpSemantic {
    /// Syntax-only evidence for `calls` calls, with the reason.
    pub fn syntax_only(note: &'static str, calls: usize) -> Self {
        Self {
            producer_id: None,
            note: Some(note),
            calls: (0..calls).map(|_| None).collect(),
        }
    }

    /// The selected binding of the call at `index`, if any.
    pub fn call(&self, index: usize) -> Option<&CallSemantic> {
        self.calls.get(index).and_then(Option::as_ref)
    }
}

impl EvidenceResponse {
    /// Bindings for measured calls of the admitted head, under the producer policy.
    pub(crate) fn mcp_call_semantics<'c>(
        &self,
        explicit: Option<&str>,
        language: &str,
        calls: impl IntoIterator<Item = &'c crate::native_evidence::Call>,
    ) -> Result<McpSemantic> {
        let selected = self.selected(None)?;
        let mut reader = SemanticCalls::new(&self.db, &selected, explicit)?;
        let note = reader.note_for(language);
        let mut semantic = Vec::new();
        for call in calls {
            semantic.push(if note.is_some() {
                None
            } else {
                reader.call(call)?
            });
        }
        Ok(McpSemantic {
            producer_id: reader.choice.producer.clone(),
            note,
            calls: semantic,
        })
    }
}
