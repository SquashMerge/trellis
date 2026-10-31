//! Semantic bindings shown by the CLI query, the MCP call tools, status and methods (#467).
//!
//! Every read surface selects producers with [`select_producers`] (owner ruling 2026-10-30,
//! #483, amending #392/#409): an explicit published producer for the languages it covers,
//! else, for each document's language, the sole published producer whose profile covers
//! that language. A language that two or more cover shows syntax only, with a note.
//! Producers are never merged. Bindings are selected by the call-graph engine's
//! `call_disposition`, the reader that the call graph, sequence and line navigation share
//! (#466).
use super::*;
use crate::{
    graph_query::{
        self as engine, BoundaryReason, DefaultProducers, DocumentFacts, GraphIntegrity,
        GraphSnapshot, GraphSource, HistoryFacts, SemanticDocument,
    },
    model::SemanticState,
    native_evidence::DocumentKey,
    semantic_evidence as sem,
};

pub use crate::graph_query::{LANGUAGE_NOTE, NO_PRODUCER_NOTE, multiple_producers_note};

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

/// The producers a read uses, by document language (#483).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProducerSelection {
    /// An explicit producer and the languages its profile covers.
    explicit: Option<(String, BTreeSet<String>)>,
    /// The per-language default, when no producer is explicit.
    defaults: DefaultProducers,
}

impl ProducerSelection {
    /// The producer of a document in `language`, if one is selected.
    pub fn producer(&self, language: &str) -> Option<&str> {
        match &self.explicit {
            Some((id, languages)) => languages.contains(language).then_some(id.as_str()),
            None => self.defaults.producer(language),
        }
    }

    /// The producer a read in `language` names: the explicit one, else the selected one.
    pub fn named(&self, language: &str) -> Option<&str> {
        match &self.explicit {
            Some((id, _)) => Some(id),
            None => self.defaults.producer(language),
        }
    }

    /// Why a document in `language` shows syntax only, or `None` when it has a producer.
    pub fn note(&self, language: &str) -> Option<String> {
        match &self.explicit {
            Some((_, languages)) => {
                (!languages.contains(language)).then(|| LANGUAGE_NOTE.to_string())
            }
            None => self.defaults.note(language),
        }
    }

    /// The published producers that cover `language` under the default, in ID order.
    pub fn covering(&self, language: &str) -> &[String] {
        self.defaults
            .by_language
            .get(language)
            .map_or(&[], Vec::as_slice)
    }
}

/// Every published producer of the revision with the languages its profile declares.
pub(super) fn default_producers(db: &Connection, revision_key: &str) -> Result<DefaultProducers> {
    let rows = db
        .prepare_cached(
            "SELECT p.id,l.language FROM semantic_revision_producers p
             LEFT JOIN semantic_producer_languages l ON l.id=p.id AND l.version=p.version
             WHERE p.revision_id=?1 ORDER BY p.id,l.language",
        )?
        .query_map([revision_key], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.iter().any(|(id, _)| !engine::valid_producer_id(id)) {
        return Err(SelectedIntegrity("invalid selected semantic producer ID".into()).into());
    }
    Ok(DefaultProducers::from_published(rows))
}

/// The one producer selection every read surface uses (#483). An explicit producer must
/// be published at this revision and applies to the languages it covers; another language
/// shows syntax only. Without one, each language uses its sole covering producer.
pub(super) fn select_producers(
    db: &Connection,
    revision_key: &str,
    explicit: Option<&str>,
) -> Result<ProducerSelection> {
    let Some(id) = explicit else {
        return Ok(ProducerSelection {
            explicit: None,
            defaults: default_producers(db, revision_key)?,
        });
    };
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
    let languages = db
        .prepare_cached(
            "SELECT l.language FROM semantic_revision_producers p JOIN semantic_producer_languages l ON l.id=p.id AND l.version=p.version WHERE p.revision_id=?1 AND p.id=?2",
        )?
        .query_map(params![revision_key, id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    Ok(ProducerSelection {
        explicit: Some((id.to_owned(), languages)),
        defaults: DefaultProducers::default(),
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

/// Reads each document language's selected producer binding for measured calls in one
/// selected snapshot (#483).
pub(super) struct SemanticCalls<'a> {
    db: &'a Connection,
    selected: &'a ReadRevision,
    pub selection: ProducerSelection,
    tuples: BTreeMap<(String, String), Option<i64>>,
    locate: Locate<'a>,
}

impl<'a> SemanticCalls<'a> {
    pub(super) fn new(
        db: &'a Connection,
        selected: &'a ReadRevision,
        explicit: Option<&str>,
    ) -> Result<Self> {
        let selection = select_producers(db, &selected.key, explicit)?;
        Ok(Self {
            db,
            selected,
            selection,
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

    /// The selected producer's coverage tuple of a document; `None` when its language has
    /// no selected producer.
    fn tuple(&mut self, language: &str, path: &str) -> Result<Option<i64>> {
        let Some(producer) = self.selection.producer(language) else {
            return Ok(None);
        };
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

/// Replace status's publication-time semantic counts with each language's selected producer
/// bindings (#483): internal, external and ambiguous calls, with the rest unresolved.
/// `semanticLanguages` reports every indexed language; `stats` is their combined total.
pub(super) fn status_semantics(
    db: &Connection,
    status: &mut crate::model::IndexStatus,
) -> Result<()> {
    let key = semantic_pin_key(status.revision);
    let defaults = default_producers(db, &key)?;
    status.semantic_languages.clear();
    if !defaults.published {
        status.semantic_producer_id = None;
        status.semantic_note = Some(NO_PRODUCER_NOTE.into());
        status.stats.semantic_state = SemanticState::Unavailable;
        return Ok(());
    }
    // Measured calls of each indexed language at this revision.
    let languages = db
        .prepare_cached(
            "SELECT r.language,count(c.id) FROM revision_documents r
             LEFT JOIN native_version_calls c ON c.vid=r.vid
             WHERE r.revision_id=?1 GROUP BY r.language ORDER BY r.language",
        )?
        .query_map([&key], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut selected = BTreeSet::new();
    let mut notes: Vec<String> = Vec::new();
    for (language, calls) in languages {
        let producer = defaults.producer(&language).map(str::to_owned);
        let mut row = crate::model::LanguageSemantic {
            semantic_note: defaults.note(&language),
            language,
            semantic_producer_id: producer.clone(),
            semantic_state: SemanticState::Unavailable,
            calls,
            internal: 0,
            external: 0,
            ambiguous: 0,
            unresolved: calls,
        };
        if let Some(producer) = producer {
            language_counts(db, &key, &producer, &mut row)?;
            selected.insert(producer);
        } else if let Some(note) = &row.semantic_note
            && !notes.contains(note)
        {
            notes.push(note.clone());
        }
        status.semantic_languages.push(row);
    }
    let rows = &status.semantic_languages;
    let stats = &mut status.stats;
    if selected.is_empty() {
        status.semantic_producer_id = None;
        status.semantic_note = Some(if notes.is_empty() {
            LANGUAGE_NOTE.into()
        } else {
            notes.join("; ")
        });
        stats.semantic_state = SemanticState::Unavailable;
        return Ok(());
    }
    // A language no published producer covers is ordinary; only conflicts are noted.
    notes.retain(|note| note != LANGUAGE_NOTE);
    status.semantic_note = (!notes.is_empty()).then(|| notes.join("; "));
    status.semantic_producer_id = (selected.len() == 1)
        .then(|| selected.pop_first())
        .flatten();
    let states: Vec<SemanticState> = rows.iter().map(|row| row.semantic_state).collect();
    stats.semantic_state = if states.contains(&SemanticState::Stale) {
        SemanticState::Stale
    } else if states.contains(&SemanticState::Fresh) {
        SemanticState::Fresh
    } else {
        SemanticState::Unavailable
    };
    stats.internal = rows.iter().map(|row| row.internal).sum();
    stats.external = rows.iter().map(|row| row.external).sum();
    stats.ambiguous = rows.iter().map(|row| row.ambiguous).sum();
    stats.unresolved = stats
        .calls
        .saturating_sub(stats.internal + stats.external + stats.ambiguous);
    Ok(())
}

/// One language's selected bindings and state under its producer.
fn language_counts(
    db: &Connection,
    key: &str,
    producer: &str,
    row: &mut crate::model::LanguageSemantic,
) -> Result<()> {
    // Rows of one ambiguous call share its call ID; each call counts once.
    let mut statement = db.prepare_cached(
        "SELECT CASE WHEN b.resolution='resolved' AND b.declared_kind='internal' THEN 0
                     WHEN b.resolution='external' OR b.declared_kind='external' THEN 1
                     WHEN b.resolution='ambiguous' THEN 2 ELSE 3 END AS kind,
                count(DISTINCT b.call_id)
         FROM semantic_coverage c JOIN semantic_call_bindings b ON b.tuple_id=c.tuple_id
         WHERE c.revision_id=?1 AND c.producer_id=?2 AND c.language=?3 AND c.selected=1
           AND c.state IN ('complete','partial') AND b.call_id IS NOT NULL
         GROUP BY kind",
    )?;
    let mut counts = [0_usize; 4];
    for result in statement.query_map(params![key, producer, row.language], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })? {
        let (kind, count) = result?;
        counts[kind as usize] = count as usize;
    }
    let (usable, stale): (bool, bool) = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM semantic_coverage WHERE revision_id=?1 AND producer_id=?2 AND language=?3 AND selected=1 AND state IN ('complete','partial')),
                EXISTS(SELECT 1 FROM semantic_coverage c JOIN semantic_provenance p ON p.tuple_id=c.tuple_id
                       WHERE c.revision_id=?1 AND c.producer_id=?2 AND c.language=?3 AND c.selected=1
                         AND c.state IN ('complete','partial') AND p.freshness<>'fresh')",
        params![key, producer, row.language],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    row.semantic_state = match (usable, stale) {
        (false, _) => SemanticState::Unavailable,
        (true, true) => SemanticState::Stale,
        (true, false) => SemanticState::Fresh,
    };
    row.internal = counts[0];
    row.external = counts[1];
    row.ambiguous = counts[2];
    row.unresolved = row.calls.saturating_sub(counts[0] + counts[1] + counts[2]);
    Ok(())
}

/// A CLI query's traversal: its nodes, calls, regions and selected bindings.
pub(super) struct ViewWalk {
    pub nodes: Vec<crate::model::Symbol>,
    pub calls: Vec<crate::model::CallSite>,
    pub regions: Vec<crate::model::ControlRegion>,
    pub truncated: bool,
    pub omitted_nodes: usize,
    pub semantic: crate::model::ViewSemantic,
}

/// Walk a CLI query from its seed (#467, #483). The seed's measured calls are always
/// returned. Below the query depth, a call whose selected binding the call graph would
/// traverse (`boundaryReason: none`) adds its exact target as a node, whose own calls are
/// inspected while the depth allows. Each document uses its language's selected producer.
pub(super) fn view_walk(
    store: &Store,
    db: &Connection,
    selected: &ReadRevision,
    query: &crate::model::ViewQuery,
    seed_document: SelectedDocument,
    seed: crate::model::Symbol,
) -> Result<ViewWalk> {
    let mut reader = SemanticCalls::new(db, selected, query.semantic_producer_id.as_deref())?;
    let seed_language = seed_document.file.language.clone();
    let mut semantic = crate::model::ViewSemantic {
        producer_id: reader.selection.named(&seed_language).map(str::to_owned),
        note: reader.selection.note(&seed_language),
        bindings: vec![],
    };
    let excluded = |path: &str| query.exclude_paths.iter().any(|p| path.starts_with(p));
    let mut documents = BTreeMap::from([(seed.path.clone(), seed_document)]);
    let mut nodes = vec![seed];
    let mut calls = Vec::new();
    let mut region_ids: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut truncated = false;
    let mut omitted_nodes = 0;
    let mut level = vec![0_usize];
    let mut depth = 0;
    while !level.is_empty() && !truncated {
        let mut next = Vec::new();
        for index in level {
            let node = nodes[index].clone();
            let language = documents[&node.path].file.language.clone();
            let bound = reader.selection.producer(&language).is_some();
            if excluded(&node.path) {
                continue;
            }
            // A call's owner belongs to the node's document. Sort its calls, then take the
            // remaining budget before filtering excluded paths.
            let mut ordered: Vec<crate::model::CallSite> = documents[&node.path]
                .graph_calls
                .iter()
                .filter(|call| call.caller == node.id)
                .cloned()
                .collect();
            ordered.sort_by(crate::indexer::compare_reader_calls);
            let native: BTreeMap<String, crate::native_evidence::Call> = if bound {
                let scope = v8_owner_scope_for(db, &node.id, selected)?.ok_or_else(|| {
                    SelectedIntegrity("incompatible_index: selected view node missing".into())
                })?;
                read_native_calls_v8(db, &scope, Some(&node.id))?
                    .into_iter()
                    .map(|call| (call.id.clone(), call))
                    .collect()
            } else {
                BTreeMap::new()
            };
            let remaining = query.max_calls.saturating_sub(calls.len());
            for mut call in ordered.into_iter().take(remaining + 1) {
                if excluded(&call.path) {
                    continue;
                }
                if calls.len() >= query.max_calls {
                    truncated = true;
                    break;
                }
                region_ids
                    .entry(call.path.clone())
                    .or_default()
                    .extend(call.regions.iter().cloned());
                if bound {
                    let measured = native
                        .get(&call.id)
                        .ok_or_else(|| SelectedIntegrity("selected view call missing".into()))?;
                    if let Some(binding) = reader.call(measured)? {
                        call.provenance.semantic = match binding.provenance.freshness {
                            sem::Freshness::Fresh => SemanticState::Fresh,
                            _ => SemanticState::Stale,
                        };
                        if depth < query.depth
                            && binding.boundary_reason == BoundaryReason::None
                            && let Some(target) = &binding.target_id
                            && !nodes.iter().any(|n| &n.id == target)
                        {
                            match add_view_node(store, db, selected, &mut documents, target)? {
                                Some(symbol) if excluded(&symbol.path) => {}
                                Some(_) if nodes.len() >= query.max_nodes => omitted_nodes += 1,
                                Some(symbol) => {
                                    next.push(nodes.len());
                                    nodes.push(symbol);
                                }
                                None => {}
                            }
                        }
                        semantic.bindings.push(crate::model::ViewCallBinding {
                            call_id: call.id.clone(),
                            boundary_reason: binding.boundary_reason,
                            target_id: binding.target_id,
                            freshness: binding.provenance.freshness,
                            binding: binding.binding,
                        });
                    }
                }
                calls.push(call);
            }
            if truncated {
                break;
            }
        }
        depth += 1;
        // Calls of nodes past the seed are inspected only while the depth allows.
        level = if depth < query.depth { next } else { vec![] };
    }
    // Each node's state is its document language's selected producer's.
    for node in &mut nodes {
        let language = &documents[&node.path].file.language;
        if reader.selection.producer(language).is_some() {
            node.provenance.semantic = reader.document_state(language, &node.path)?;
        }
    }
    let mut regions = BTreeMap::new();
    for (path, mut ids) in region_ids {
        let document = &documents[&path];
        while let Some(id) = ids.pop_first() {
            if regions.contains_key(&id) {
                continue;
            }
            let region = document
                .graph_regions
                .iter()
                .find(|region| region.id == id)
                .cloned();
            if let Some(region) = region {
                if let Some(parent) = &region.parent {
                    ids.insert(parent.clone());
                }
                regions.insert(id, region);
            }
        }
    }
    Ok(ViewWalk {
        nodes,
        calls,
        regions: regions.into_values().collect(),
        truncated,
        omitted_nodes,
        semantic,
    })
}

/// The symbol of an exact binding target, reading its document once.
fn add_view_node(
    store: &Store,
    db: &Connection,
    selected: &ReadRevision,
    documents: &mut BTreeMap<String, SelectedDocument>,
    target: &str,
) -> Result<Option<crate::model::Symbol>> {
    let Some(scope) = v8_owner_scope_for(db, target, selected)? else {
        return Ok(None);
    };
    let path = scope.key.path;
    if !documents.contains_key(&path) {
        let document = store.selected_graph_document_for(db, &path, selected)?;
        documents.insert(path.clone(), document);
    }
    Ok(documents[&path]
        .nodes
        .iter()
        .find(|symbol| symbol.id == target)
        .cloned())
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
    /// Why calls are syntax only: each distinct reason of a returned language without a
    /// producer (#483).
    pub note: Option<String>,
    pub calls: Vec<Option<CallSemantic>>,
}

impl McpSemantic {
    /// Syntax-only evidence for `calls` calls, with the reason.
    pub fn syntax_only(note: &str, calls: usize) -> Self {
        Self {
            producer_id: None,
            note: Some(note.to_owned()),
            calls: (0..calls).map(|_| None).collect(),
        }
    }

    /// The selected binding of the call at `index`, if any.
    pub fn call(&self, index: usize) -> Option<&CallSemantic> {
        self.calls.get(index).and_then(Option::as_ref)
    }
}

/// One language's selected imported evidence at the admitted head (#528): its default
/// producer (#483), that producer's coverage tuple counts and its worst proof freshness.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LanguageTier {
    pub producer_id: Option<String>,
    /// Why the language shows syntax only, when no producer is selected.
    pub note: Option<String>,
    /// Tuple counts `[selected, complete, partial, failed, unselected]`, as native ones.
    pub counts: [u64; 5],
    /// `fresh`, `possiblyStale` or `stale` over the usable tuples' proofs, else `unavailable`.
    pub freshness: &'static str,
}

impl LanguageTier {
    /// Semantic evidence is selected, fresh and covers at least one document (complete or
    /// partial). Partial coverage stays visible in `counts`; it does not demote the tier.
    pub fn semantic(&self) -> bool {
        self.producer_id.is_some()
            && self.freshness == "fresh"
            && self.counts[1] + self.counts[2] > 0
    }
}

/// The four MCP languages in #22 order.
pub const MCP_LANGUAGES: [&str; 4] = ["java", "rust", "python", "javascript"];

impl EvidenceResponse {
    /// Each MCP language's selected semantic evidence at the admitted head, in #22 order
    /// (#528). Uses the per-language default producer, as status and the call tools do.
    pub fn mcp_language_tiers(&self) -> Result<[LanguageTier; 4]> {
        let selected = self.selected(None)?;
        let defaults = default_producers(&self.db, &selected.key)?;
        let mut tiers: [LanguageTier; 4] = Default::default();
        for (tier, language) in tiers.iter_mut().zip(MCP_LANGUAGES) {
            tier.freshness = "unavailable";
            tier.note = defaults.note(language);
            let Some(producer) = defaults.producer(language) else {
                continue;
            };
            tier.producer_id = Some(producer.to_owned());
            // Closed enums decode strictly: a malformed stored value is typed corruption,
            // never healthy availability (#528 review).
            let corrupt = |what: &str| -> anyhow::Error {
                SelectedIntegrity(format!(
                    "store_unavailable: invalid selected semantic {what}"
                ))
                .into()
            };
            let mut statement = self.db.prepare_cached(
                "SELECT selected,state,count(*) FROM semantic_coverage
                 WHERE revision_id=?1 AND producer_id=?2 AND language=?3 GROUP BY selected,state",
            )?;
            for row in statement.query_map(params![selected.key, producer, language], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })? {
                let (chosen, state, count) = row?;
                let count = u64::try_from(count).map_err(|_| corrupt("coverage count"))?;
                if !matches!(chosen, 0 | 1)
                    || !matches!(
                        state.as_str(),
                        "notRequested"
                            | "omitted"
                            | "unsupported"
                            | "failed"
                            | "partial"
                            | "complete"
                    )
                {
                    return Err(corrupt("coverage state"));
                }
                // Unselected tuples, and selected ones not requested or unsupported, count as
                // unselected, as native `scopeCoverage` does.
                let bucket = match (chosen, state.as_str()) {
                    (1, "complete") => Some(1),
                    (1, "partial") => Some(2),
                    (1, "failed") => Some(3),
                    _ => None,
                };
                match bucket {
                    Some(bucket) => {
                        tier.counts[0] += count;
                        tier.counts[bucket] += count;
                    }
                    None => tier.counts[4] += count,
                }
            }
            if tier.counts[1] + tier.counts[2] == 0 {
                continue;
            }
            // Publication derives a proof's freshness from its document and that document's
            // captured basis only, so every proof of one tuple shares it: one proof per
            // tuple is exact for what Trellis published (#261), and avoids scanning the
            // hundreds of thousands of proofs a language can hold on every read. A tuple
            // without proofs (an empty fact set) is valid and adds nothing.
            let mut statement = self.db.prepare_cached(
                "SELECT (SELECT p.freshness FROM semantic_provenance p
                         WHERE p.tuple_id=c.tuple_id LIMIT 1)
                 FROM semantic_coverage c
                 WHERE c.revision_id=?1 AND c.producer_id=?2 AND c.language=?3 AND c.selected=1
                   AND c.state IN ('complete','partial')",
            )?;
            let mut worst = 0;
            for freshness in statement
                .query_map(params![selected.key, producer, language], |row| {
                    row.get::<_, Option<String>>(0)
                })?
            {
                worst = worst.max(match freshness?.as_deref() {
                    None | Some("fresh") => 0,
                    Some("possiblyStale") => 1,
                    Some("stale") => 2,
                    Some(_) => return Err(corrupt("proof freshness")),
                });
            }
            tier.freshness = match worst {
                2 => "stale",
                1 => "possiblyStale",
                _ => "fresh",
            };
        }
        Ok(tiers)
    }

    /// Bindings for measured calls of the admitted head, each under its document language's
    /// producer (#483). `language` is the selected declaration's, which names the producer
    /// and, without calls, the note.
    pub(crate) fn mcp_call_semantics<'c>(
        &self,
        explicit: Option<&str>,
        language: &str,
        calls: impl IntoIterator<Item = &'c crate::native_evidence::Call>,
    ) -> Result<McpSemantic> {
        let selected = self.selected(None)?;
        let mut reader = SemanticCalls::new(&self.db, &selected, explicit)?;
        let calls: Vec<_> = calls.into_iter().collect();
        let mut languages: BTreeSet<&str> = calls
            .iter()
            .map(|call| call.document.language.as_str())
            .collect();
        if languages.is_empty() {
            languages.insert(language);
        }
        let mut notes: Vec<String> = Vec::new();
        for note in languages
            .into_iter()
            .filter_map(|l| reader.selection.note(l))
        {
            if !notes.contains(&note) {
                notes.push(note);
            }
        }
        let mut semantic = Vec::new();
        for call in calls {
            semantic.push(reader.call(call)?);
        }
        Ok(McpSemantic {
            producer_id: reader.selection.named(language).map(str::to_owned),
            note: (!notes.is_empty()).then(|| notes.join("; ")),
            calls: semantic,
        })
    }
}
