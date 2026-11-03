//! Semantic bindings shown by the CLI query, the MCP call tools, status and methods (#467).
//!
//! Every read surface selects producers with [`select_producers`] (owner ruling 2026-10-30,
//! #483, amending #392/#409): an explicit published producer for the languages it covers,
//! else, for each document's language, the sole published producer whose profile covers
//! that language. A language that two or more cover shows syntax only, with a note.
//! Producers are never merged. Bindings are selected by the call-graph engine's
//! `call_disposition`, the reader that the call graph, sequence and line navigation share
//! (#466).
use super::native_first::DocumentSemanticOrigin;
use super::*;
use crate::{
    graph_query::{
        self as engine, BoundaryReason, DefaultProducers, DocumentFacts, GraphIntegrity,
        GraphSnapshot, GraphSource, HistoryFacts, SemanticDocument,
    },
    model::{
        DocumentSemanticStatus, SemanticAvailability, SemanticAvailabilityCounts, SemanticState,
    },
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
    selected_native_revision: String,
    pub selection: ProducerSelection,
    origins: BTreeMap<(String, String), DocumentSemanticOrigin>,
    excluded_proofs: BTreeMap<i64, BTreeSet<String>>,
    locate: Locate<'a>,
}

impl<'a> SemanticCalls<'a> {
    pub(super) fn new(
        db: &'a Connection,
        selected: &'a ReadRevision,
        explicit: Option<&str>,
    ) -> Result<Self> {
        let selection = select_producers(db, &selected.key, explicit)?;
        let selected_native_revision = semantic_comparison_revision(db, selected)?;
        Ok(Self {
            db,
            selected,
            selected_native_revision,
            selection,
            origins: BTreeMap::new(),
            excluded_proofs: BTreeMap::new(),
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

    /// The direct semantic origin; native-first descriptors never stand in for facts.
    fn origin(&mut self, language: &str, path: &str) -> Result<Option<&DocumentSemanticOrigin>> {
        let Some(producer) = self.selection.producer(language) else {
            return Ok(None);
        };
        let key = (language.to_owned(), path.to_owned());
        if !self.origins.contains_key(&key) {
            let origin =
                semantic_document_origin(self.db, self.selected, producer, language, path)?;
            self.origins.insert(key.clone(), origin);
        }
        Ok(self.origins.get(&key))
    }

    fn document_status(
        &self,
        language: &str,
        path: &str,
    ) -> Result<crate::model::DocumentSemanticStatus> {
        selected_document_status(self.db, self.selected, &self.selection, language, path)
    }

    /// The selected binding of one measured call, or `None` for syntax-only evidence.
    pub(super) fn call(
        &mut self,
        call: &crate::native_evidence::Call,
    ) -> Result<Option<CallSemantic>> {
        let Some(origin) = self.origin(&call.document.language, &call.document.path)? else {
            return Ok(None);
        };
        if !origin.source_eligible && origin.catching_up {
            return Ok(None);
        }
        let tuple = origin
            .tuple_id
            .context(SelectedIntegrity("missing producer coverage tuple".into()))?;
        let origin_key = origin.origin_key.clone();
        let origin_native_revision = origin.native_revision_id.clone();
        let encoded = id_codec::encode_occ(&call.id)
            .context(SelectedIntegrity("invalid selected call ID".into()))?;
        let mut facts = read_semantic_tuple_for_call(
            self.db,
            &origin_key,
            Some(tuple),
            None,
            true,
            Some(&encoded),
        )?;
        if !self.excluded_proofs.contains_key(&tuple) {
            self.excluded_proofs.insert(
                tuple,
                semantic_ineligible_provenance(self.db, self.selected, tuple)?,
            );
        }
        filter_semantic_facts(&mut facts, &self.excluded_proofs[&tuple]);
        project_origin_freshness(
            &mut facts,
            &origin_native_revision,
            &self.selected_native_revision,
        );
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
        let Some(origin) = self.origin(language, path)? else {
            return Ok(SemanticState::Unavailable);
        };
        if !origin.source_eligible && origin.catching_up {
            return Ok(SemanticState::Unavailable);
        }
        let tuple = origin
            .tuple_id
            .context(SelectedIntegrity("missing producer coverage tuple".into()))?;
        let origin_native_revision = origin.native_revision_id.clone();
        let comparison = semantic_comparison_freshness(
            sem::Freshness::Fresh,
            &origin_native_revision,
            &self.selected_native_revision,
        );
        let (usable, stored_stale): (bool, bool) = self.db.query_row(
            "SELECT c.selected=1 AND c.state IN ('complete','partial'),
                    EXISTS(SELECT 1 FROM semantic_provenance p
                           WHERE p.tuple_id=c.tuple_id AND p.freshness<>'fresh'
                             AND NOT EXISTS(SELECT 1 FROM semantic_ineligible_proofs e
                                            WHERE e.revision_id=?2 AND e.origin_tuple_id=p.tuple_id
                                              AND e.provenance_id=p.provenance_id))
             FROM semantic_coverage c WHERE c.tuple_id=?1",
            params![tuple, self.selected.key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let stale = stored_stale || comparison != sem::Freshness::Fresh;
        Ok(match (usable, stale) {
            (false, _) => SemanticState::Unavailable,
            (true, true) => SemanticState::Stale,
            (true, false) => SemanticState::Fresh,
        })
    }
}

/// Keep the original origin identities and drop associations excluded at publication.
pub(super) fn filter_semantic_snapshot(
    db: &Connection,
    selected: &ReadRevision,
    origin_tuple: i64,
    snapshot: &mut sem::SemanticSnapshot,
) -> Result<()> {
    let excluded = semantic_ineligible_provenance(db, selected, origin_tuple)?;
    filter_semantic_facts(snapshot, &excluded);
    let origin_revision: String = db.query_row(
        "SELECT r.native_revision_id FROM semantic_coverage c JOIN native_revisions r ON r.id=c.revision_id WHERE c.tuple_id=?1",
        [origin_tuple], |row| row.get(0),
    )?;
    let selected_revision = semantic_comparison_revision(db, selected)?;
    project_origin_freshness(snapshot, &origin_revision, &selected_revision);
    Ok(())
}

fn project_origin_freshness(
    snapshot: &mut sem::SemanticSnapshot,
    origin_revision: &str,
    selected_revision: &str,
) {
    for proof in &mut snapshot.provenance {
        proof.freshness =
            semantic_comparison_freshness(proof.freshness, origin_revision, selected_revision);
    }
}

fn filter_semantic_facts(snapshot: &mut sem::SemanticSnapshot, excluded: &BTreeSet<String>) {
    snapshot
        .symbols
        .retain(|fact| !excluded.contains(&fact.provenance_id));
    snapshot
        .declaration_bindings
        .retain(|fact| !excluded.contains(&fact.provenance_id));
    snapshot
        .references
        .retain(|fact| !excluded.contains(&fact.provenance_id));
    snapshot
        .call_bindings
        .retain(|fact| !excluded.contains(&fact.provenance_id));
    snapshot
        .type_relationships
        .retain(|fact| !excluded.contains(&fact.provenance_id));
    snapshot
        .provenance
        .retain(|proof| !excluded.contains(&proof.id));
}

/// Availability is independent from the freshness attached to admitted origin proofs.
fn selected_document_status(
    db: &Connection,
    selected: &ReadRevision,
    selection: &ProducerSelection,
    language: &str,
    path: &str,
) -> Result<DocumentSemanticStatus> {
    match selection.producer(language) {
        Some(producer) => semantic_document_status(db, selected, producer, language, path),
        None => Ok(DocumentSemanticStatus {
            producer_id: selection.named(language).map(str::to_owned),
            reason: selection.note(language),
            ..Default::default()
        }),
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
    let read = ReadRevision::new(status.revision, key.clone());
    let defaults = default_producers(db, &key)?;
    status.semantic_languages.clear();
    status.semantic_availability = SemanticAvailabilityCounts::default();
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
            semantic_availability: SemanticAvailabilityCounts::default(),
            calls,
            internal: 0,
            external: 0,
            ambiguous: 0,
            unresolved: calls,
            coverage: None,
        };
        if let Some(producer) = producer {
            language_counts(db, &read, &producer, &mut row)?;
            selected.insert(producer);
        } else if let Some(note) = &row.semantic_note
            && !notes.contains(note)
        {
            notes.push(note.clone());
        }
        if row.semantic_producer_id.is_none() {
            let unavailable: i64 = db.query_row(
                "SELECT count(*) FROM revision_documents WHERE revision_id=?1 AND language=?2",
                params![key, row.language],
                |row| row.get(0),
            )?;
            row.semantic_availability.unavailable = u64::try_from(unavailable)
                .context(SelectedIntegrity("corrupt semantic document count".into()))?;
        }
        status.semantic_availability.fresh += row.semantic_availability.fresh;
        status.semantic_availability.catching_up += row.semantic_availability.catching_up;
        status.semantic_availability.unavailable += row.semantic_availability.unavailable;
        status.semantic_languages.push(row);
    }
    let rows = &status.semantic_languages;
    let stats = &mut status.stats;
    if selected.is_empty() {
        status.semantic_producer_id = None;
        status.semantic_note = Some(if !defaults.published {
            NO_PRODUCER_NOTE.into()
        } else if notes.is_empty() {
            LANGUAGE_NOTE.into()
        } else {
            notes.join("; ")
        });
        stats.semantic_state = SemanticState::Unavailable;
        stats.internal = 0;
        stats.external = 0;
        stats.ambiguous = 0;
        stats.unresolved = stats.calls;
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
    selected: &ReadRevision,
    producer: &str,
    row: &mut crate::model::LanguageSemantic,
) -> Result<()> {
    let paths = db.prepare_cached(
        "SELECT path FROM revision_documents WHERE revision_id=?1 AND language=?2 ORDER BY path",
    )?.query_map(params![selected.key, row.language], |row| row.get::<_, String>(0))?
      .collect::<rusqlite::Result<Vec<_>>>()?;
    let selected_revision = semantic_comparison_revision(db, selected)?;
    let mut counts = [0_usize; 4];
    let mut usable = false;
    let mut stale = false;
    let mut coverage = crate::model::SemanticCoverageSummary::default();
    for path in paths {
        row.semantic_availability
            .add(semantic_document_status(db, selected, producer, &row.language, &path)?.state);
        let origin = semantic_document_origin(db, selected, producer, &row.language, &path)?;
        if !origin.source_eligible && origin.catching_up {
            continue;
        }
        let tuple = origin
            .tuple_id
            .context(SelectedIntegrity("missing producer coverage tuple".into()))?;
        add_tuple_coverage(db, tuple, &mut coverage)?;
        let selected_usable: bool = db.query_row(
            "SELECT selected=1 AND state IN ('complete','partial') FROM semantic_coverage WHERE tuple_id=?1",
            [tuple], |row| row.get(0),
        )?;
        if !selected_usable {
            continue;
        }
        usable = true;
        stale |= semantic_comparison_freshness(
            sem::Freshness::Fresh,
            &origin.native_revision_id,
            &selected_revision,
        ) != sem::Freshness::Fresh;
        stale |= db.query_row(
            "SELECT EXISTS(SELECT 1 FROM semantic_provenance p
                           WHERE p.tuple_id=?1 AND p.freshness<>'fresh'
                             AND NOT EXISTS(SELECT 1 FROM semantic_ineligible_proofs e
                                            WHERE e.revision_id=?2 AND e.origin_tuple_id=p.tuple_id
                                              AND e.provenance_id=p.provenance_id))",
            params![tuple, selected.key],
            |row| row.get::<_, bool>(0),
        )?;
        // Aggregate eligible bindings in SQLite; never scan every proof in a status read.
        let mut statement = db.prepare_cached(
            "SELECT CASE WHEN b.resolution='resolved' AND b.declared_kind='internal' THEN 0
                         WHEN b.resolution='external' OR b.declared_kind='external' THEN 1
                         WHEN b.resolution='ambiguous' THEN 2 ELSE 3 END AS kind,
                    count(DISTINCT b.call_id)
             FROM semantic_call_bindings b
             WHERE b.tuple_id=?1 AND b.call_id IS NOT NULL
               AND NOT EXISTS(SELECT 1 FROM semantic_ineligible_proofs e
                              WHERE e.revision_id=?2 AND e.origin_tuple_id=b.tuple_id
                                AND e.provenance_id=b.provenance_id)
             GROUP BY kind",
        )?;
        for result in statement.query_map(params![tuple, selected.key], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })? {
            let (kind, count) = result?;
            let kind = usize::try_from(kind)
                .context(SelectedIntegrity("corrupt semantic binding kind".into()))?;
            let count = usize::try_from(count)
                .context(SelectedIntegrity("corrupt semantic binding count".into()))?;
            let total = counts
                .get_mut(kind)
                .context(SelectedIntegrity("corrupt semantic binding kind".into()))?;
            *total = total
                .checked_add(count)
                .context(SelectedIntegrity("semantic binding count overflow".into()))?;
        }
    }
    row.semantic_state = match (usable, stale) {
        (false, _) => SemanticState::Unavailable,
        (true, true) => SemanticState::Stale,
        (true, false) => SemanticState::Fresh,
    };
    row.internal = counts[0];
    row.external = counts[1];
    row.ambiguous = counts[2];
    row.unresolved = row.calls.saturating_sub(counts[0] + counts[1] + counts[2]);
    coverage.summary = coverage.describe();
    row.coverage = Some(coverage);
    Ok(())
}

/// Add coverage metadata from the direct origin without reading any fact rows.
fn add_tuple_coverage(
    db: &Connection,
    tuple: i64,
    summary: &mut crate::model::SemanticCoverageSummary,
) -> Result<()> {
    let state: String = db.query_row(
        "SELECT state FROM semantic_coverage WHERE tuple_id=?1",
        [tuple],
        |row| row.get(0),
    )?;
    match state.as_str() {
        "complete" => summary.complete += 1,
        "failed" => summary.failed += 1,
        "partial" => {
            summary.partial += 1;
            add_tuple_reasons(db, tuple, &mut summary.partial_reasons)?;
        }
        "notRequested" | "omitted" | "unsupported" => summary.unselected += 1,
        _ => return Err(SelectedIntegrity("corrupt semantic coverage state".into()).into()),
    }
    Ok(())
}

fn add_tuple_reasons(
    db: &Connection,
    tuple: i64,
    totals: &mut crate::model::PartialReasons,
) -> Result<()> {
    let reasons = db.query_row(
        "SELECT macro_invocation,attribute_item,macro_definition,other,capped,omitted,captured_diagnostics
         FROM semantic_coverage_reasons WHERE tuple_id=?1", [tuple], |row| {
            Ok([row.get::<_, i64>(0)?, row.get(1)?, row.get(2)?, row.get(3)?,
                row.get(4)?, row.get(5)?, row.get(6)?])
        },
    ).optional()?;
    let Some(
        [
            invocation,
            attribute,
            definition,
            other,
            capped,
            omitted,
            diagnostics,
        ],
    ) = reasons
    else {
        totals.unrecorded += 1;
        return Ok(());
    };
    let invocation = u64::try_from(invocation).context(SelectedIntegrity(
        "corrupt semantic occurrence count".into(),
    ))?;
    let attribute = u64::try_from(attribute).context(SelectedIntegrity(
        "corrupt semantic occurrence count".into(),
    ))?;
    let definition = u64::try_from(definition).context(SelectedIntegrity(
        "corrupt semantic occurrence count".into(),
    ))?;
    let other = u64::try_from(other).context(SelectedIntegrity(
        "corrupt semantic occurrence count".into(),
    ))?;
    let capped = usize::try_from(capped)
        .context(SelectedIntegrity("corrupt semantic partial count".into()))?;
    let omitted = usize::try_from(omitted)
        .context(SelectedIntegrity("corrupt semantic partial count".into()))?;
    let diagnostics = usize::try_from(diagnostics)
        .context(SelectedIntegrity("corrupt semantic partial count".into()))?;
    let gaps = [invocation, attribute, definition, other]
        .iter()
        .any(|count| *count > 0);
    totals.native_gaps += usize::from(gaps);
    totals.native_gaps_only += usize::from(gaps && capped == 0 && omitted == 0 && diagnostics == 0);
    totals.capped += capped;
    totals.omitted += omitted;
    totals.captured_diagnostics += diagnostics;
    totals.unsupported_occurrences.macro_invocation += invocation;
    totals.unsupported_occurrences.attribute_item += attribute;
    totals.unsupported_occurrences.macro_definition += definition;
    totals.unsupported_occurrences.other += other;
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
    pub semantic_documents: Vec<crate::model::DocumentSemantic>,
    pub partial: bool,
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
    let source_set_id: String = db.query_row(
        "SELECT source_set_id FROM native_revisions WHERE id=?1",
        [&selected.key],
        |row| row.get(0),
    )?;
    let semantic_documents = documents
        .values()
        .map(|document| {
            let language = &document.file.language;
            let path = &document.file.path;
            Ok(crate::model::DocumentSemantic {
                document: DocumentKey {
                    source_set_id: source_set_id.clone(),
                    language: language.clone(),
                    path: path.clone(),
                },
                semantic_status: reader.document_status(language, path)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let partial = truncated
        || semantic_documents
            .iter()
            .any(|document| document.semantic_status.state == SemanticAvailability::CatchingUp);
    Ok(ViewWalk {
        nodes,
        calls,
        regions: regions.into_values().collect(),
        truncated,
        omitted_nodes,
        semantic,
        semantic_documents,
        partial,
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
    pub documents: Vec<crate::model::DocumentSemantic>,
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
            documents: Vec::new(),
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
    pub semantic_availability: SemanticAvailabilityCounts,
    /// Why the language shows syntax only, when no producer is selected.
    pub note: Option<String>,
    /// Tuple counts `[selected, complete, partial, failed, unselected]`, as native ones.
    pub counts: [u64; 5],
    /// `fresh`, `possiblyStale` or `stale` over the usable tuples' proofs, else `unavailable`.
    pub freshness: &'static str,
    /// Why the producer's partial tuples are partial (#520), when any is.
    pub partial_reasons: Option<crate::model::SemanticCoverageSummary>,
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
        let selected_revision = semantic_comparison_revision(&self.db, &selected)?;
        let selection = select_producers(&self.db, &selected.key, None)?;
        let mut tiers: [LanguageTier; 4] = Default::default();
        for (tier, language) in tiers.iter_mut().zip(MCP_LANGUAGES) {
            tier.freshness = "unavailable";
            tier.note = selection.note(language);
            tier.producer_id = selection.producer(language).map(str::to_owned);
            let paths = self.db.prepare_cached(
                "SELECT path FROM revision_documents WHERE revision_id=?1 AND language=?2 ORDER BY path",
            )?.query_map(params![selected.key, language], |row| row.get::<_, String>(0))?
              .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut coverage = crate::model::SemanticCoverageSummary::default();
            let mut worst = 0;
            let mut usable = false;
            for path in paths {
                tier.semantic_availability.add(
                    selected_document_status(&self.db, &selected, &selection, language, &path)?
                        .state,
                );
                let Some(producer) = selection.producer(language) else {
                    continue;
                };
                let origin =
                    semantic_document_origin(&self.db, &selected, producer, language, &path)?;
                if !origin.source_eligible && origin.catching_up {
                    continue;
                }
                let tuple = origin
                    .tuple_id
                    .context(SelectedIntegrity("missing producer coverage tuple".into()))?;
                add_tuple_coverage(&self.db, tuple, &mut coverage)?;
                let (chosen, state): (i64, String) = self.db.query_row(
                    "SELECT selected,state FROM semantic_coverage WHERE tuple_id=?1",
                    [tuple],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                if !matches!(chosen, 0 | 1) {
                    return Err(SelectedIntegrity(
                        "invalid selected semantic coverage state".into(),
                    )
                    .into());
                }
                let bucket = match (chosen, state.as_str()) {
                    (1, "complete") => Some(1),
                    (1, "partial") => Some(2),
                    (1, "failed") => Some(3),
                    _ => None,
                };
                match bucket {
                    Some(bucket) => {
                        tier.counts[0] += 1;
                        tier.counts[bucket] += 1;
                    }
                    None => tier.counts[4] += 1,
                }
                if !matches!(bucket, Some(1 | 2)) {
                    continue;
                }
                usable = true;
                worst = worst.max(
                    match semantic_comparison_freshness(
                        sem::Freshness::Fresh,
                        &origin.native_revision_id,
                        &selected_revision,
                    ) {
                        sem::Freshness::Fresh => 0,
                        sem::Freshness::PossiblyStale => 1,
                        sem::Freshness::Stale => 2,
                    },
                );
                // Tuple proofs share publication freshness. Availability is separate.
                let freshness: Option<String> = self
                    .db
                    .query_row(
                        "SELECT p.freshness FROM semantic_provenance p WHERE p.tuple_id=?1
                       AND NOT EXISTS(SELECT 1 FROM semantic_ineligible_proofs e
                                      WHERE e.revision_id=?2 AND e.origin_tuple_id=p.tuple_id
                                        AND e.provenance_id=p.provenance_id)
                     LIMIT 1",
                        params![tuple, selected.key],
                        |row| row.get(0),
                    )
                    .optional()?;
                worst = worst.max(match freshness.as_deref() {
                    None | Some("fresh") => 0,
                    Some("possiblyStale") => 1,
                    Some("stale") => 2,
                    Some(_) => {
                        return Err(SelectedIntegrity(
                            "invalid selected semantic proof freshness".into(),
                        )
                        .into());
                    }
                });
            }
            if coverage.partial > 0 {
                coverage.summary = coverage.describe();
                tier.partial_reasons = Some(coverage);
            }
            if usable {
                tier.freshness = match worst {
                    2 => "stale",
                    1 => "possiblyStale",
                    _ => "fresh",
                };
            }
        }
        Ok(tiers)
    }

    /// Per-document semantic availability at one selected native pin, in MCP language order.
    pub fn semantic_availability_at(
        &self,
        pin: Option<IndexPin>,
    ) -> Result<[SemanticAvailabilityCounts; 4]> {
        self.semantic_availability_for_producer_at(pin, None)
    }

    /// Counts the selected producer's document availability over the pinned search scope.
    pub fn semantic_availability_for_producer_at(
        &self,
        pin: Option<IndexPin>,
        explicit: Option<&str>,
    ) -> Result<[SemanticAvailabilityCounts; 4]> {
        let selected = self.selected(pin)?;
        let selection = select_producers(&self.db, &selected.key, explicit)?;
        let mut counts = [SemanticAvailabilityCounts::default(); 4];
        let mut statement = self.db.prepare_cached(
            "SELECT language,path FROM revision_documents WHERE revision_id=?1 ORDER BY language,path",
        )?;
        for row in statement.query_map([&selected.key], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (language, path) = row?;
            let Some(index) = MCP_LANGUAGES.iter().position(|l| *l == language) else {
                continue;
            };
            counts[index].add(
                selected_document_status(&self.db, &selected, &selection, &language, &path)?.state,
            );
        }
        Ok(counts)
    }

    /// Availability of the indexed document at `path` under its default selected producer.
    pub fn document_semantic_status_at(
        &self,
        path: &str,
        pin: Option<IndexPin>,
    ) -> Result<DocumentSemanticStatus> {
        let selected = self.selected(pin)?;
        let language: Option<String> = self
            .db
            .query_row(
                "SELECT language FROM revision_documents WHERE revision_id=?1 AND path=?2",
                params![selected.key, path],
                |row| row.get(0),
            )
            .optional()?;
        let language = language.context("not_found: indexed document not found")?;
        let selection = select_producers(&self.db, &selected.key, None)?;
        selected_document_status(&self.db, &selected, &selection, &language, path)
    }

    /// Bindings for measured calls of the admitted head, each under its document language's
    /// producer (#483). The owner document is included even when it has no calls.
    pub(crate) fn mcp_call_semantics<'c>(
        &self,
        explicit: Option<&str>,
        owner: &DocumentKey,
        calls: impl IntoIterator<Item = &'c crate::native_evidence::Call>,
    ) -> Result<McpSemantic> {
        let language = owner.language.as_str();
        let selected = self.selected(None)?;
        let mut reader = SemanticCalls::new(&self.db, &selected, explicit)?;
        let calls: Vec<_> = calls.into_iter().collect();
        let mut languages: BTreeSet<&str> = calls
            .iter()
            .map(|call| call.document.language.as_str())
            .collect();
        languages.insert(language);
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
        for call in &calls {
            semantic.push(reader.call(call)?);
        }
        let mut documents = BTreeMap::from([(
            (owner.language.clone(), owner.path.clone()),
            crate::model::DocumentSemantic {
                document: owner.clone(),
                semantic_status: reader.document_status(&owner.language, &owner.path)?,
            },
        )]);
        for call in &calls {
            let document = &call.document;
            documents.insert(
                (document.language.clone(), document.path.clone()),
                crate::model::DocumentSemantic {
                    document: document.clone(),
                    semantic_status: reader.document_status(&document.language, &document.path)?,
                },
            );
        }
        Ok(McpSemantic {
            producer_id: reader.selection.named(language).map(str::to_owned),
            documents: documents.into_values().collect(),
            note: (!notes.is_empty()).then(|| notes.join("; ")),
            calls: semantic,
        })
    }
}

/// At most this many semantic rows are admitted into one architecture snapshot (#523).
const ARCHITECTURE_SEMANTIC_ROWS: usize = 2_000_000;

/// Fresh bindings of each document under its language's default producer (#483), for
/// architecture references and rules (#523). Only rows of a selected, complete or partial
/// coverage tuple whose proof is fresh are read; anything else stays syntax-only. An
/// internal call target must also be confirmed current (`stale_target = 0`), and an
/// internal target outside the snapshot's documents is left out.
pub(super) fn architecture_semantic(
    db: &Connection,
    selected: &ReadRevision,
    documents: &mut [crate::references::DocumentInput],
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    use crate::references::{SemanticAnchor, SemanticOutcome, SemanticRelationship};
    let selected_revision = semantic_comparison_revision(db, selected)?;
    let defaults = default_producers(db, &selected.key)?;
    // Snapshot documents by language, then path.
    // A revision has one source set; its documents are keyed by language and path.
    let mut index: HashMap<String, HashMap<String, usize>> = HashMap::new();
    for (i, doc) in documents.iter().enumerate() {
        ensure!(
            index
                .entry(doc.key.language.clone())
                .or_default()
                .insert(doc.key.path.clone(), i)
                .is_none(),
            SelectedIntegrity("duplicate architecture document".into())
        );
    }
    let locate = |language: Option<&str>, path: Option<&str>| {
        index
            .get(language?)
            .and_then(|paths| paths.get(path?))
            .copied()
    };
    let mut rows = 0usize;
    let mut admit = || -> Result<()> {
        rows += 1;
        ensure!(
            rows <= ARCHITECTURE_SEMANTIC_ROWS,
            "bounded_work: architecture semantic evidence"
        );
        Ok(())
    };
    let sid = |bytes: Option<&[u8]>| -> Result<Option<String>> {
        bytes
            .map(|bytes| {
                id_codec::decode_sid(bytes).map_err(|_| {
                    anyhow::Error::from(SelectedIntegrity("invalid semantic target ID".into()))
                })
            })
            .transpose()
    };
    // Stored offsets are nonnegative and ordered; anything else is a corrupt row.
    let range = |start: i64, end: i64| -> Result<crate::native_evidence::Range> {
        match (usize::try_from(start), usize::try_from(end)) {
            (Ok(start), Ok(end)) if start <= end => {
                Ok(crate::native_evidence::Range { start, end })
            }
            _ => Err(SelectedIntegrity("corrupt semantic offset".into()).into()),
        }
    };
    // The closed dispatch enum: direct and constructor calls are direct.
    let direct = |dispatch: &str| -> Result<bool> {
        match dispatch {
            "direct" | "constructor" => Ok(true),
            "virtual" | "interface" | "dynamic" | "unknown" => Ok(false),
            _ => Err(SelectedIntegrity("corrupt semantic call dispatch".into()).into()),
        }
    };
    let mut tuples = Vec::new();
    for (i, document) in documents.iter_mut().enumerate() {
        let language = &document.key.language;
        let path = &document.key.path;
        let Some(producer) = defaults.producer(language) else {
            document.semantic.semantic_status = DocumentSemanticStatus {
                reason: defaults.note(language),
                ..Default::default()
            };
            continue;
        };
        check()?;
        document.semantic.semantic_status =
            semantic_document_status(db, selected, producer, language, path)?;
        let origin = semantic_document_origin(db, selected, producer, language, path)?;
        if !origin.source_eligible && origin.catching_up {
            continue;
        }
        if semantic_comparison_freshness(
            sem::Freshness::Fresh,
            &origin.native_revision_id,
            &selected_revision,
        ) != sem::Freshness::Fresh
        {
            continue;
        }
        let tuple = origin
            .tuple_id
            .context(SelectedIntegrity("missing producer coverage tuple".into()))?;
        let (usable, complete): (bool, bool) = db.query_row(
            "SELECT selected=1 AND state IN ('complete','partial'),state='complete'
             FROM semantic_coverage WHERE tuple_id=?1",
            [tuple],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if usable {
            tuples.push((tuple, i, producer.to_owned(), complete));
        }
    }
    let mut calls = db.prepare_cached(
        "SELECT b.call_id,b.join_start,b.join_end,b.resolution,b.declared_kind,
                b.declared_internal_syntax_id,b.declared_internal_language,b.declared_internal_path,
                b.dispatch,b.stale_target,b.provenance_id
         FROM semantic_call_bindings b
         JOIN semantic_provenance p ON p.tuple_id=b.tuple_id AND p.provenance_id=b.provenance_id
         WHERE b.tuple_id=?1 AND b.call_id IS NOT NULL AND p.freshness='fresh'
           AND b.join_document_language=?2 AND b.join_document_path=?3
           AND NOT EXISTS(SELECT 1 FROM semantic_ineligible_proofs e
                          WHERE e.revision_id=?4 AND e.origin_tuple_id=b.tuple_id
                            AND e.provenance_id=b.provenance_id)",
    )?;
    let mut references = db.prepare_cached(
        "SELECT r.start_byte,r.end_byte,r.resolution,r.declared_internal_syntax_id,
                r.declared_internal_language,r.declared_internal_path,r.provenance_id,
                EXISTS(SELECT 1 FROM semantic_reference_roles o
                       WHERE o.tuple_id=r.tuple_id AND o.reference_id=r.reference_id AND o.role='type')
         FROM semantic_references r
         JOIN semantic_provenance p ON p.tuple_id=r.tuple_id AND p.provenance_id=r.provenance_id
         WHERE r.tuple_id=?1 AND r.site='use' AND p.freshness='fresh'
           AND ((r.resolution='resolved' AND r.declared_kind='internal') OR r.resolution='external')
           AND NOT EXISTS(SELECT 1 FROM semantic_ineligible_proofs e
                          WHERE e.revision_id=?2 AND e.origin_tuple_id=r.tuple_id
                            AND e.provenance_id=r.provenance_id)",
    )?;
    let mut relationships = db.prepare_cached(
        "SELECT t.kind,t.source_internal_syntax_id,t.target_kind,t.target_internal_syntax_id,
                t.target_internal_language,t.target_internal_path,t.provenance_id
         FROM semantic_type_relationships t
         JOIN semantic_provenance p ON p.tuple_id=t.tuple_id AND p.provenance_id=t.provenance_id
         WHERE t.tuple_id=?1 AND p.freshness='fresh' AND t.source_kind='internal'
           AND t.kind IN ('extends','implements')
           AND NOT EXISTS(SELECT 1 FROM semantic_ineligible_proofs e
                          WHERE e.revision_id=?2 AND e.origin_tuple_id=t.tuple_id
                            AND e.provenance_id=t.provenance_id)
         ORDER BY t.ordinal",
    )?;
    for (tuple, i, producer, complete) in tuples {
        check()?;
        let (language, path) = (
            documents[i].key.language.clone(),
            documents[i].key.path.clone(),
        );
        // One binding per measured call: the lowest provenance, as the call graph selects.
        // The lowest provenance represents a call, as in the call graph; an unusable one
        // (stale, or outside this snapshot) leaves the call without a binding.
        let mut by_call: BTreeMap<String, (String, Option<SemanticAnchor>)> = BTreeMap::new();
        let mut cursor = calls.query(params![tuple, language, path, selected.key])?;
        while let Some(row) = cursor.next()? {
            admit()?;
            let call_id = id_codec::decode_occ(row.get_ref(0)?.as_blob()?)
                .map_err(|_| SelectedIntegrity("invalid semantic call ID".into()))?;
            let provenance_id: String = row.get(10)?;
            // Every row's offsets and dispatch decode strictly, used or not.
            let anchor_range = range(row.get(1)?, row.get(2)?)?;
            let is_direct = direct(row.get_ref(8)?.as_str()?)?;
            if by_call
                .get(&call_id)
                .is_some_and(|(kept, _)| *kept <= provenance_id)
            {
                continue;
            }
            let outcome = match (
                row.get_ref(3)?.as_str()?,
                row.get_ref(4)?.as_str_or_null()?,
                sid(row.get_ref(5)?.as_blob_or_null()?)?,
            ) {
                ("resolved", Some("internal"), Some(target)) => {
                    let target_doc = locate(
                        row.get_ref(6)?.as_str_or_null()?,
                        row.get_ref(7)?.as_str_or_null()?,
                    );
                    // A target not confirmed current, or outside this snapshot, is stale.
                    match (row.get::<_, Option<i64>>(9)?, target_doc) {
                        (Some(0), Some(target_doc)) => {
                            SemanticOutcome::Internal(target_doc, target)
                        }
                        _ => {
                            by_call.insert(call_id, (provenance_id, None));
                            continue;
                        }
                    }
                }
                ("external", ..) => SemanticOutcome::External,
                ("ambiguous", ..) => SemanticOutcome::Ambiguous,
                ("unresolved", ..) => SemanticOutcome::Unresolved,
                _ => {
                    return Err(SelectedIntegrity("corrupt semantic call binding".into()).into());
                }
            };
            let anchor = SemanticAnchor {
                range: anchor_range,
                call_id: Some(call_id.clone()),
                type_role: false,
                direct: is_direct,
                outcome,
                provenance_id: provenance_id.clone(),
            };
            by_call.insert(call_id, (provenance_id, Some(anchor)));
        }
        drop(cursor);
        let mut anchors: Vec<SemanticAnchor> = by_call
            .into_values()
            .filter_map(|(_, anchor)| anchor)
            .collect();
        // Resolved and external use-site references: the names a site spells.
        let mut cursor = references.query(params![tuple, selected.key])?;
        while let Some(row) = cursor.next()? {
            admit()?;
            let provenance_id: String = row.get(6)?;
            let outcome = match (
                row.get_ref(2)?.as_str()?,
                sid(row.get_ref(3)?.as_blob_or_null()?)?,
            ) {
                ("resolved", Some(target)) => match locate(
                    row.get_ref(4)?.as_str_or_null()?,
                    row.get_ref(5)?.as_str_or_null()?,
                ) {
                    Some(target_doc) => SemanticOutcome::Internal(target_doc, target),
                    None => continue,
                },
                ("external", _) => SemanticOutcome::External,
                _ => return Err(SelectedIntegrity("corrupt semantic reference".into()).into()),
            };
            anchors.push(SemanticAnchor {
                range: range(row.get(0)?, row.get(1)?)?,
                call_id: None,
                type_role: row.get(7)?,
                direct: false,
                outcome,
                provenance_id,
            });
        }
        drop(cursor);
        // A stable order independent of the storage plan.
        anchors.sort_by(|a, b| {
            (a.range.start, a.range.end, &a.provenance_id).cmp(&(
                b.range.start,
                b.range.end,
                &b.provenance_id,
            ))
        });
        let mut found = Vec::new();
        let mut cursor = relationships.query(params![tuple, selected.key])?;
        while let Some(row) = cursor.next()? {
            admit()?;
            let provenance_id: String = row.get(6)?;
            let source = sid(row.get_ref(1)?.as_blob_or_null()?)?.context(SelectedIntegrity(
                "corrupt semantic type relationship".into(),
            ))?;
            let target = match (
                row.get_ref(2)?.as_str()?,
                sid(row.get_ref(3)?.as_blob_or_null()?)?,
            ) {
                ("internal", Some(target)) => locate(
                    row.get_ref(4)?.as_str_or_null()?,
                    row.get_ref(5)?.as_str_or_null()?,
                )
                .map(|target_doc| (target_doc, target)),
                _ => None,
            };
            found.push(SemanticRelationship {
                kind: row.get(0)?,
                source_syntax_id: source,
                target,
                provenance_id,
            });
        }
        drop(cursor);
        let semantic = &mut documents[i].semantic;
        semantic.producer_id = Some(producer);
        semantic.complete = complete;
        semantic.anchors = anchors;
        semantic.relationships = found;
    }
    Ok(())
}

#[cfg(test)]
mod origin_filter_tests {
    use super::*;

    #[test]
    fn excludes_dependent_facts_without_rewriting_retained_origin_evidence() {
        let document = DocumentKey {
            source_set_id: "source".into(),
            language: "rust".into(),
            path: "src/lib.rs".into(),
        };
        let mut snapshot = sem::SemanticSnapshot {
            producers: vec![],
            coverage: vec![],
            provenance: vec![],
            symbols: vec![],
            declaration_bindings: vec![],
            references: vec![],
            call_bindings: vec![],
            type_relationships: vec![],
        };
        for id in ["dependent", "independent"] {
            let symbol = sem::SymbolKey {
                scheme: sem::SymbolScheme::Scip,
                symbol: id.into(),
                scope: sem::SymbolScope::Global,
                document: None,
            };
            let target = sem::Target::External {
                symbol: symbol.clone(),
            };
            let join = sem::Join {
                anchor: sem::MeasuredAnchor {
                    document: document.clone(),
                    revision_id: "origin".into(),
                    content_hash: "captured".into(),
                    range: crate::native_evidence::Range { start: 1, end: 2 },
                    kind: sem::JoinKind::Callee,
                },
                status: sem::JoinStatus::Exact,
                candidate_ids: vec![],
                diagnostic: None,
            };
            snapshot.provenance.push(sem::Provenance {
                id: id.into(),
                producer_id: "producer".into(),
                document: document.clone(),
                revision_id: "origin".into(),
                content_hash: "captured".into(),
                evidence_kind: sem::EvidenceKind::MeasuredSyntax,
                basis: None,
                freshness: sem::Freshness::PossiblyStale,
                derived_from: None,
            });
            snapshot.symbols.push(sem::Symbol {
                key: symbol.clone(),
                display_name: None,
                declarations: vec![target.clone()],
                provenance_id: id.into(),
            });
            snapshot.declaration_bindings.push(sem::DeclarationBinding {
                syntax_id: Some(id.into()),
                symbols: vec![symbol],
                join: join.clone(),
                provenance_id: id.into(),
            });
            snapshot.references.push(sem::Reference {
                id: id.into(),
                owner_syntax_id: id.into(),
                ordinal: 0,
                document: document.clone(),
                revision_id: "origin".into(),
                range: crate::native_evidence::Range { start: 1, end: 2 },
                spelling: id.into(),
                lookup_key: id.into(),
                site: sem::ReferenceSite::Use,
                roles: vec![],
                resolution: sem::Resolution::External,
                declared_target: Some(target.clone()),
                candidates: vec![],
                provenance_id: id.into(),
            });
            snapshot.call_bindings.push(sem::CallBinding {
                call_id: Some(id.into()),
                join,
                resolution: sem::Resolution::External,
                declared_target: Some(target.clone()),
                candidates: vec![],
                dispatch: sem::Dispatch::Direct,
                possible_dispatch: vec![],
                possible_dispatch_complete: false,
                stale_target: None,
                provenance_id: id.into(),
            });
            snapshot.type_relationships.push(sem::TypeRelationship {
                kind: sem::RelationshipKind::Extends,
                source: target.clone(),
                target,
                provenance_id: id.into(),
            });
        }
        let retained_proof = snapshot.provenance[1].clone();
        let retained_call = snapshot.call_bindings[1].clone();
        filter_semantic_facts(&mut snapshot, &BTreeSet::from(["dependent".to_owned()]));
        assert_eq!(snapshot.provenance, vec![retained_proof]);
        assert_eq!(snapshot.call_bindings, vec![retained_call]);
        assert_eq!(snapshot.symbols.len(), 1);
        assert_eq!(snapshot.declaration_bindings.len(), 1);
        assert_eq!(snapshot.references.len(), 1);
        assert_eq!(snapshot.type_relationships.len(), 1);
        assert_eq!(snapshot.symbols[0].provenance_id, "independent");
        assert_eq!(
            snapshot.declaration_bindings[0].provenance_id,
            "independent"
        );
        assert_eq!(snapshot.references[0].provenance_id, "independent");
        assert_eq!(snapshot.type_relationships[0].provenance_id, "independent");
    }
}
