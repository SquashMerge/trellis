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
        self as engine, DefaultProducers, DocumentFacts, GraphIntegrity, GraphSnapshot,
        GraphSource, HistoryFacts, SemanticDocument,
    },
    model::{BoundaryReason, SemanticState},
    native_records::DocumentKey,
    semantic_records as sem,
};
use serde_json::json;

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
                (!languages.contains(language)).then(|| engine::LANGUAGE_NOTE.to_string())
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
    pub projection: Option<crate::model::SemanticProjection>,
    pub historical_freshness: sem::Freshness,
    pub effective_freshness: sem::Freshness,
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

impl CallSemantic {
    /// #724: a fresh binding the graph would traverse directly (`boundaryReason:"none"`)
    /// whose located target is not `selected`. A dispatch boundary, a stale or possibly
    /// stale binding and a binding without a located target never qualify.
    pub fn bound_elsewhere(&self, selected: &str) -> bool {
        self.boundary_reason == BoundaryReason::None
            && self.effective_freshness == sem::Freshness::Fresh
            && self
                .target_id
                .as_deref()
                .is_some_and(|target| target != selected)
    }
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
    fn projection_endpoint(
        &mut self,
        projection: &crate::model::SemanticProjection,
        producer: &str,
        document: &DocumentKey,
    ) -> Result<Option<crate::model::BoundaryReason>> {
        native_first::endpoint_boundary(
            self.db,
            self.selected.pin,
            producer,
            &document.language,
            &document.path,
            projection.semantic_basis_pin,
        )
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
        let projection = native_first::semantic_projection_for(
            self.db,
            self.selected.pin,
            producer,
            language,
            path,
        )?;
        if projection
            .as_ref()
            .is_some_and(|projection| projection.semantic_status.is_pending())
        {
            self.tuples.insert(key, None);
            return Ok(None);
        }
        let basis_key = projection.map_or_else(
            || self.selected.key.clone(),
            |projection| semantic_pin_key(projection.semantic_basis_pin),
        );
        let tuple: Option<i64> = self.db.query_row(
            "SELECT tuple_id FROM semantic_coverage WHERE revision_id=?1 AND producer_id=?2 AND language=?3 AND path=?4",
            params![basis_key, producer, language, path], |row| row.get(0),
        ).optional()?;
        let tuple = tuple.context(SelectedIntegrity("missing producer coverage tuple".into()))?;
        self.tuples.insert(key, Some(tuple));
        Ok(Some(tuple))
    }

    /// The selected binding of one measured call, or `None` for syntax-only evidence.
    pub(super) fn call(
        &mut self,
        call: &crate::native_records::Call,
    ) -> Result<Option<CallSemantic>> {
        let Some(tuple) = self.tuple(&call.document.language, &call.document.path)? else {
            return Ok(None);
        };
        let encoded = id_codec::encode_occ(&call.id)
            .context(SelectedIntegrity("invalid selected call ID".into()))?;
        let producer = self
            .selection
            .producer(&call.document.language)
            .expect("selected tuple producer");
        let projection = native_first::semantic_projection_for(
            self.db,
            self.selected.pin,
            producer,
            &call.document.language,
            &call.document.path,
        )?;
        let basis_key = projection.as_ref().map_or_else(
            || self.selected.key.clone(),
            |projection| semantic_pin_key(projection.semantic_basis_pin),
        );
        let facts = read_semantic_tuple_for_call(
            self.db,
            &basis_key,
            Some(tuple),
            None,
            true,
            Some(&encoded),
        )?;
        let [coverage] = <[_; 1]>::try_from(facts.coverage)
            .map_err(|_| SelectedIntegrity("corrupt selected semantic coverage".into()))?;
        let semantic = SemanticDocument {
            projection: projection.clone(),
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
            historical_freshness: proof.freshness,
            effective_freshness: projection.as_ref().map_or(proof.freshness, |projection| {
                projection.effective_freshness(proof.freshness)
            }),
            projection,
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
                    EXISTS(SELECT 1 FROM semantic_coverage_freshness f WHERE f.revision_id=c.revision_id AND f.tuple_id=c.tuple_id AND f.freshness<>'fresh')
             FROM semantic_coverage c WHERE c.tuple_id=?1",
            [tuple],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let producer = self
            .selection
            .producer(language)
            .expect("selected tuple producer");
        let projected = native_first::semantic_projection_for(
            self.db,
            self.selected.pin,
            producer,
            language,
            path,
        )?
        .is_some();
        Ok(match (usable, stale || projected) {
            (false, _) => SemanticState::Unavailable,
            (true, true) => SemanticState::Stale,
            (true, false) => SemanticState::Fresh,
        })
    }
}

pub(super) fn document_projections(
    db: &Connection,
    pin: IndexPin,
    selection: &ProducerSelection,
) -> Result<Vec<crate::model::DocumentSemanticStatus>> {
    let key = semantic_pin_key(pin);
    let mut statement = db.prepare_cached(
        "SELECT language,path FROM revision_documents WHERE revision_id=?1 ORDER BY language,path",
    )?;
    let mut rows = statement.query([key])?;
    let mut documents = Vec::new();
    while let Some(row) = rows.next()? {
        let language: String = row.get(0)?;
        let path: String = row.get(1)?;
        let Some(producer) = selection.producer(&language) else {
            continue;
        };
        if let Some(projection) =
            native_first::semantic_projection_for(db, pin, producer, &language, &path)?
        {
            documents.push(crate::model::DocumentSemanticStatus {
                producer_id: producer.to_owned(),
                language,
                path,
                projection,
            });
        }
    }
    Ok(documents)
}

#[cfg(feature = "test-support")]
thread_local! {
    static THREAD_STATUS_SEMANTIC_PROJECTIONS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

/// How many status semantic projections the calling thread has run. A caller brackets
/// its own read with this, so background work on other threads, such as the dependency
/// catalog build, is never attributed to it (#700).
#[cfg(feature = "test-support")]
pub(crate) fn status_semantic_projections_on_thread_for_tests() -> usize {
    THREAD_STATUS_SEMANTIC_PROJECTIONS.with(std::cell::Cell::get)
}

/// Replace status's publication-time semantic counts with each language's selected producer
/// bindings (#483): internal, external and ambiguous calls, with the rest unresolved.
/// `semanticLanguages` reports every indexed language; `stats` is their combined total.
pub(super) fn status_semantics(
    db: &Connection,
    status: &mut crate::model::IndexStatus,
) -> Result<()> {
    #[cfg(feature = "test-support")]
    THREAD_STATUS_SEMANTIC_PROJECTIONS.with(|count| count.set(count.get() + 1));
    let key = semantic_pin_key(status.revision);
    let defaults = default_producers(db, &key)?;
    let reimports = super::semantic_retention::semantic_reimport_at(db, &key)?;
    status.semantic_languages.clear();
    if !defaults.published && reimports.is_empty() {
        status.semantic_producer_id = None;
        status.semantic_note = Some(engine::NO_PRODUCER_NOTE.into());
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
        .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
    let mut languages = languages;
    for language in reimports.keys() {
        languages.entry(language.clone()).or_insert(0);
    }
    let mut selected = BTreeSet::new();
    let mut notes: Vec<String> = Vec::new();
    for (language, calls) in languages {
        let producer = defaults.producer(&language).map(str::to_owned);
        let reimport = reimports.get(&language).cloned();
        let mut row = crate::model::LanguageSemantic {
            documents: vec![],
            semantic_note: reimport
                .as_ref()
                .map(reimport_note)
                .or_else(|| defaults.note(&language)),
            reimport,
            language,
            semantic_producer_id: producer.clone(),
            semantic_state: SemanticState::Unavailable,
            calls,
            internal: 0,
            external: 0,
            ambiguous: 0,
            unresolved: calls,
            coverage: None,
        };
        if let Some(producer) = producer {
            language_counts(db, &key, &producer, &mut row)?;
            selected.insert(producer);
        }
        if let Some(note) = &row.semantic_note
            && !notes.contains(note)
        {
            notes.push(note.clone());
        }
        status.semantic_languages.push(row);
    }
    let selection = select_producers(db, &key, None)?;
    for document in document_projections(db, status.revision, &selection)? {
        if let Some(language) = status
            .semantic_languages
            .iter_mut()
            .find(|row| row.language == document.language)
        {
            if document.projection.semantic_status == crate::model::SemanticStatus::OlderBasis {
                let basis = semantic_pin_key(document.projection.semantic_basis_pin);
                let usable: bool = db.query_row("SELECT selected=1 AND state IN ('complete','partial') FROM semantic_coverage WHERE revision_id=?1 AND producer_id=?2 AND language=?3 AND path=?4", params![basis,document.producer_id,document.language,document.path], |row| row.get(0))?;
                if usable {
                    language.semantic_state = SemanticState::Stale;
                }
                let counts: (i64,i64,i64) = db.query_row(
                    "SELECT count(DISTINCT CASE WHEN b.resolution='resolved' THEN b.call_id END),count(DISTINCT CASE WHEN b.resolution='external' THEN b.call_id END),count(DISTINCT CASE WHEN b.resolution='ambiguous' THEN b.call_id END) FROM semantic_coverage c JOIN semantic_call_bindings b ON b.payload_id=c.payload_id WHERE c.revision_id=?1 AND c.producer_id=?2 AND c.language=?3 AND c.path=?4 AND c.selected=1 AND c.state IN ('complete','partial')",
                    params![basis, document.producer_id, document.language, document.path],
                    |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
                )?;
                let count = |value| {
                    usize::try_from(value)
                        .map_err(|_| SelectedIntegrity("invalid projected semantic count".into()))
                };
                language.internal += count(counts.0)?;
                language.external += count(counts.1)?;
                language.ambiguous += count(counts.2)?;
                language.unresolved = language
                    .calls
                    .saturating_sub(language.internal + language.external + language.ambiguous);
            }
            language.documents.push(document);
        }
    }
    status.catching_up |= db.query_row(
        "SELECT EXISTS(SELECT 1 FROM semantic_pending_target WHERE revision_id=?1)",
        [&key],
        |row| row.get::<_, bool>(0),
    )?;
    let rows = &status.semantic_languages;
    let stats = &mut status.stats;
    if selected.is_empty() {
        status.semantic_producer_id = None;
        status.semantic_note = Some(if notes.is_empty() {
            engine::LANGUAGE_NOTE.into()
        } else {
            notes.join("; ")
        });
        stats.semantic_state = SemanticState::Unavailable;
        return Ok(());
    }
    // A language no published producer covers is ordinary; only conflicts are noted.
    notes.retain(|note| note != engine::LANGUAGE_NOTE);
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

fn reimport_note(reimport: &crate::model::SemanticReimport) -> String {
    match reimport {
        crate::model::SemanticReimport::NeedsReimport { reason } => {
            format!("semantic evidence needs re-import: {reason}")
        }
    }
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
         FROM semantic_coverage c JOIN semantic_call_bindings b ON b.payload_id=c.payload_id
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
                EXISTS(SELECT 1 FROM semantic_coverage c JOIN semantic_coverage_freshness f ON f.revision_id=c.revision_id AND f.tuple_id=c.tuple_id
                       WHERE c.revision_id=?1 AND c.producer_id=?2 AND c.language=?3 AND c.selected=1
                         AND c.state IN ('complete','partial') AND f.freshness<>'fresh')",
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
    row.coverage = Some(coverage_summary(db, key, producer, &row.language)?);
    Ok(())
}

/// One language's coverage tuples under `producer` at revision `key`, by state, with the
/// import-time reasons of its partial tuples (#520).
pub(super) fn coverage_summary(
    db: &Connection,
    key: &str,
    producer: &str,
    language: &str,
) -> Result<crate::model::SemanticCoverageSummary> {
    use crate::model::{GapOccurrences, PartialReasons, SemanticCoverageSummary};
    let mut summary = SemanticCoverageSummary::default();
    let mut statement = db.prepare_cached(
        "SELECT c.state, count(*), count(COALESCE(rp.rowid,rt.rowid)),
                coalesce(sum(COALESCE(rp.macro_invocation,rt.macro_invocation)+COALESCE(rp.attribute_item,rt.attribute_item)+COALESCE(rp.macro_definition,rt.macro_definition)+COALESCE(rp.other,rt.other)>0),0),
                coalesce(sum(COALESCE(rp.macro_invocation,rt.macro_invocation)+COALESCE(rp.attribute_item,rt.attribute_item)+COALESCE(rp.macro_definition,rt.macro_definition)+COALESCE(rp.other,rt.other)>0
                             AND COALESCE(rp.capped,rt.capped)=0 AND COALESCE(rp.omitted,rt.omitted)=0 AND COALESCE(rp.captured_diagnostics,rt.captured_diagnostics)=0),0),
                coalesce(sum(COALESCE(rp.capped,rt.capped)),0), coalesce(sum(COALESCE(rp.omitted,rt.omitted)),0),
                coalesce(sum(COALESCE(rp.captured_diagnostics,rt.captured_diagnostics)),0),
                coalesce(sum(COALESCE(rp.macro_invocation,rt.macro_invocation)),0), coalesce(sum(COALESCE(rp.attribute_item,rt.attribute_item)),0),
                coalesce(sum(COALESCE(rp.macro_definition,rt.macro_definition)),0), coalesce(sum(COALESCE(rp.other,rt.other)),0),
                coalesce(sum(c.diagnostic='excluded_failed_by_producer'),0),
                coalesce(sum(c.diagnostic='unmeasurable_source_anchor'),0),
                coalesce(sum(c.diagnostic LIKE 'selected raw capture unavailable:%'),0)
         FROM semantic_coverage c LEFT JOIN semantic_coverage_reasons rp ON rp.payload_id=c.payload_id
         LEFT JOIN semantic_coverage_reasons rt ON rt.tuple_id=c.tuple_id
         WHERE c.revision_id=?1 AND c.producer_id=?2 AND c.language=?3
         GROUP BY c.state",
    )?;
    let mut rows = statement.query(params![key, producer, language])?;
    while let Some(row) = rows.next()? {
        let state: String = row.get(0)?;
        let count = row.get::<_, i64>(1)? as usize;
        match state.as_str() {
            "complete" => summary.complete += count,
            "failed" => {
                summary.failed += count;
                let documents =
                    |index| -> rusqlite::Result<usize> { Ok(row.get::<_, i64>(index)? as usize) };
                summary.failed_reasons.not_in_output += documents(12)?;
                summary.failed_reasons.unmeasurable_anchor += documents(13)?;
                summary.failed_reasons.capture_unavailable += documents(14)?;
            }
            "partial" => {
                let documents =
                    |index| -> rusqlite::Result<usize> { Ok(row.get::<_, i64>(index)? as usize) };
                let occurrences =
                    |index| -> rusqlite::Result<u64> { Ok(row.get::<_, i64>(index)? as u64) };
                summary.partial += count;
                summary.partial_reasons = PartialReasons {
                    native_gaps: documents(3)?,
                    native_gaps_only: documents(4)?,
                    capped: documents(5)?,
                    omitted: documents(6)?,
                    captured_diagnostics: documents(7)?,
                    unrecorded: count - documents(2)?,
                    unsupported_occurrences: GapOccurrences {
                        macro_invocation: occurrences(8)?,
                        attribute_item: occurrences(9)?,
                        macro_definition: occurrences(10)?,
                        other: occurrences(11)?,
                    },
                };
            }
            _ => summary.unselected += count,
        }
    }
    summary.summary = summary.describe();
    Ok(summary)
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
        documents: vec![],
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
            ordered.sort_by(crate::model::compare_reader_calls);
            let native: BTreeMap<String, crate::native_records::Call> = if bound {
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
                        call.provenance.projection = binding.projection.clone();
                        call.provenance.semantic = match binding.effective_freshness {
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
                            projection: binding.projection,
                            historical_freshness: Some(binding.historical_freshness),
                            call_id: call.id.clone(),
                            boundary_reason: binding.boundary_reason,
                            target_id: binding.target_id,
                            freshness: binding.effective_freshness,
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
            let producer = reader
                .selection
                .producer(language)
                .expect("selected producer");
            node.provenance.projection = native_first::semantic_projection_for(
                db,
                selected.pin,
                producer,
                language,
                &node.path,
            )?;
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
    for (path, document) in &documents {
        let language = &document.file.language;
        let Some(producer) = reader.selection.producer(language) else {
            continue;
        };
        if let Some(projection) =
            native_first::semantic_projection_for(db, selected.pin, producer, language, path)?
        {
            semantic
                .documents
                .push(crate::model::DocumentSemanticStatus {
                    producer_id: producer.to_owned(),
                    language: language.clone(),
                    path: path.clone(),
                    projection,
                });
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
    let projection = reader
        .selection
        .producer(language)
        .map(|producer| {
            native_first::semantic_projection_for(db, selected.pin, producer, language, path)
        })
        .transpose()?
        .flatten();
    for symbol in symbols {
        symbol.provenance.semantic = state;
        symbol.provenance.projection = projection.clone();
    }
    Ok(())
}

/// The selected bindings of MCP call items, aligned with the calls (#467).
pub struct McpSemantic {
    pub documents: Vec<crate::model::DocumentSemanticStatus>,
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
            documents: vec![],
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

/// Why a language's answers wait on semantic evidence (#660): the status an agent should
/// show and the reason behind it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanguagePending {
    pub status: crate::model::SemanticStatus,
    pub reason: String,
}

/// Runtime labels for languages whose producer has an automatic re-run in flight or failed
/// (#660), keyed by language. Plain data: only the label comes from runtime state, never
/// evidence, coverage or freshness.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AutoRunLabels {
    /// Languages of every producer whose automatic re-run is scheduled or running.
    pub catching_up: std::collections::BTreeSet<String>,
    /// Languages of every producer whose last automatic re-run failed, with the reason.
    pub failed: BTreeMap<String, String>,
}

/// The reason shown for a language whose producer re-run is scheduled or running, and the
/// `autoRun.reason` of a scheduled producer (#660): one string for both.
pub(crate) const RERUN_SCHEDULED: &str = "producer re-run scheduled";

/// Rules 1 and 2 of the pending label: a catching-up language relabels its `NeedsReimport`
/// documents `CatchingUp`; a failed one carries the failure as its re-import reason. Counts,
/// freshness and producer never change. Returns the label when a rule matched.
fn apply_auto_run_labels(
    language: &str,
    documents: &mut [crate::model::DocumentSemanticStatus],
    reimport: &mut Option<crate::model::SemanticReimport>,
    labels: &AutoRunLabels,
) -> Option<LanguagePending> {
    use crate::model::{SemanticReimport, SemanticStatus};
    if labels.catching_up.contains(language) {
        for document in documents {
            if document.projection.semantic_status == SemanticStatus::NeedsReimport {
                document.projection.semantic_status = SemanticStatus::CatchingUp;
            }
        }
        return Some(LanguagePending {
            status: SemanticStatus::CatchingUp,
            reason: RERUN_SCHEDULED.to_owned(),
        });
    }
    let reason = labels.failed.get(language)?;
    *reimport = Some(SemanticReimport::NeedsReimport {
        reason: reason.clone(),
    });
    Some(LanguagePending {
        status: SemanticStatus::NeedsReimport,
        reason: reason.clone(),
    })
}

/// Applies the automatic re-run labels to a CLI status's committed projection (#660), after
/// the cached status returns. A language without a row is not added.
pub fn relabel_status(status: &mut crate::model::IndexStatus, labels: &AutoRunLabels) {
    for row in &mut status.semantic_languages {
        apply_auto_run_labels(&row.language, &mut row.documents, &mut row.reimport, labels);
    }
}

/// The pending state of one language: its re-import reason, else `re-import required`
/// when a document needs re-import, else `re-import pending` when one is catching up.
fn language_pending(tier: &LanguageTier) -> Option<LanguagePending> {
    use crate::model::{SemanticReimport, SemanticStatus};
    let (status, reason) = if let Some(SemanticReimport::NeedsReimport { reason }) = &tier.reimport
    {
        (SemanticStatus::NeedsReimport, reason.clone())
    } else if tier
        .documents
        .iter()
        .any(|d| d.projection.semantic_status == SemanticStatus::NeedsReimport)
    {
        (
            SemanticStatus::NeedsReimport,
            "re-import required".to_owned(),
        )
    } else if tier
        .documents
        .iter()
        .any(|d| d.projection.semantic_status == SemanticStatus::CatchingUp)
    {
        (SemanticStatus::CatchingUp, "re-import pending".to_owned())
    } else {
        return None;
    };
    Some(LanguagePending { status, reason })
}

/// One language's selected imported evidence at the admitted head (#528): its default
/// producer (#483), that producer's coverage tuple counts and its worst proof freshness.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LanguageTier {
    pub documents: Vec<crate::model::DocumentSemanticStatus>,
    pub producer_id: Option<String>,
    /// Retained input still awaiting validated publication, independent of proof freshness.
    pub reimport: Option<crate::model::SemanticReimport>,
    /// Why the language shows syntax only, when no producer is selected.
    pub note: Option<String>,
    /// Tuple counts `[selected, complete, partial, failed, unselected]`, as native ones.
    pub counts: [u64; 5],
    /// `fresh`, `possiblyStale` or `stale` over the usable tuples' proofs, else `unavailable`.
    pub freshness: &'static str,
    /// Why the producer's partial tuples are partial (#520), when any is.
    pub partial_reasons: Option<crate::model::SemanticCoverageSummary>,
    /// Set when the language has pending semantic evidence, from committed data only.
    pub pending: Option<LanguagePending>,
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

/// One failed or partial semantic document of a language's selected producer (#780).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageDocument {
    pub language: &'static str,
    pub path: String,
    /// Failed, else partial.
    pub failed: bool,
    /// Typed reason codes: one for a failed tuple, one or more for a partial one.
    pub reasons: Vec<&'static str>,
}

/// The reason code of a failed tuple, from its stored diagnostic (the buckets of the #520
/// summary).
pub(crate) fn failed_reason(diagnostic: Option<&str>) -> &'static str {
    match diagnostic {
        Some("excluded_failed_by_producer") => "notInProducerOutput",
        Some("unmeasurable_source_anchor") => "unmeasurableSourceAnchor",
        Some(text) if text.starts_with(native_first::UNAVAILABLE_DIAGNOSTIC) => {
            "rawCaptureUnavailable"
        }
        _ => "other",
    }
}

/// The reason codes of a partial tuple, named as the #520 `partialReasons` counts, from its
/// recorded reasons `[nativeGaps, capped, omitted, capturedDiagnostics]`; `unrecorded` when
/// the import recorded none.
pub(crate) fn partial_reasons(reasons: [Option<bool>; 4]) -> Vec<&'static str> {
    let named: Vec<&'static str> = ["nativeGaps", "capped", "omitted", "capturedDiagnostics"]
        .into_iter()
        .zip(reasons)
        .filter_map(|(name, set)| set.unwrap_or(false).then_some(name))
        .collect();
    if named.is_empty() {
        vec!["unrecorded"]
    } else {
        named
    }
}

/// The five MCP languages in #22 order.
pub const MCP_LANGUAGES: [&str; 5] = crate::semantic_records::LANGUAGE_ORDER;

impl EvidenceResponse {
    /// Advisory work status from the admitted pin, not a publication eligibility proof.
    pub(crate) fn semantic_catching_up(&self) -> Result<bool> {
        let selected = self.selected(None)?;
        Ok(self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM semantic_pending_target WHERE revision_id=?1)",
            [&selected.key],
            |row| row.get(0),
        )?)
    }

    /// Each MCP language's selected semantic evidence at the admitted head, in #22 order
    /// (#528). Uses the per-language default producer, as status and the call tools do.
    pub fn mcp_language_tiers(&self) -> Result<[LanguageTier; 5]> {
        let selected = self.selected(None)?;
        let defaults = default_producers(&self.db, &selected.key)?;
        let reimports = super::semantic_retention::semantic_reimport_at(&self.db, &selected.key)?;
        let mut tiers: [LanguageTier; 5] = Default::default();
        for (tier, language) in tiers.iter_mut().zip(MCP_LANGUAGES) {
            tier.freshness = "unavailable";
            tier.reimport = reimports.get(language).cloned();
            tier.note = tier
                .reimport
                .as_ref()
                .map(reimport_note)
                .or_else(|| defaults.note(language));
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
            if tier.counts[2] > 0 {
                tier.partial_reasons = Some(coverage_summary(
                    &self.db,
                    &selected.key,
                    producer,
                    language,
                )?);
            }
            if tier.counts[1] + tier.counts[2] == 0 {
                continue;
            }
            // Publication derives a proof's freshness from its document and that document's
            // captured basis only, so every proof of one tuple shares it: one proof per
            // tuple is exact for what Trellis published (#261), and avoids scanning the
            // hundreds of thousands of proofs a language can hold on every read. A tuple
            // without proofs (an empty fact set) is valid and adds nothing; a proof without
            // its required freshness row is corruption.
            let mut statement = self.db.prepare_cached(
                "SELECT (SELECT COALESCE(f.freshness,'missing') FROM semantic_provenance p
                         LEFT JOIN semantic_coverage_freshness f
                           ON f.revision_id=c.revision_id AND f.tuple_id=c.tuple_id AND f.basis_id=p.basis_id
                         WHERE p.payload_id=c.payload_id
                         ORDER BY p.ordinal LIMIT 1)
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
        let selection = select_producers(&self.db, &selected.key, None)?;
        for document in document_projections(&self.db, selected.pin, &selection)? {
            if let Some(at) = crate::semantic_records::language_order(&document.language) {
                if document.projection.semantic_status == crate::model::SemanticStatus::OlderBasis {
                    let basis = semantic_pin_key(document.projection.semantic_basis_pin);
                    let tuple: Option<(bool,String)> = self.db.query_row(
                        "SELECT selected,state FROM semantic_coverage WHERE revision_id=?1 AND producer_id=?2 AND language=?3 AND path=?4",
                        params![basis,document.producer_id,document.language,document.path], |row| Ok((row.get(0)?,row.get(1)?)),
                    ).optional()?;
                    let (selected, state) = tuple
                        .context(SelectedIntegrity("missing projected coverage tuple".into()))?;
                    let bucket = match (selected, state.as_str()) {
                        (true, "complete") => Some(1),
                        (true, "partial") => Some(2),
                        (true, "failed") => Some(3),
                        _ => None,
                    };
                    if let Some(bucket) = bucket {
                        tiers[at].counts[0] += 1;
                        tiers[at].counts[bucket] += 1;
                    } else {
                        tiers[at].counts[4] += 1;
                    }
                    if tiers[at].freshness != "stale" {
                        tiers[at].freshness = "possiblyStale";
                    }
                }
                tiers[at].documents.push(document);
            }
        }
        for (tier, language) in tiers.iter_mut().zip(MCP_LANGUAGES) {
            tier.pending = apply_auto_run_labels(
                language,
                &mut tier.documents,
                &mut tier.reimport,
                &self.auto_run,
            )
            .or_else(|| language_pending(tier));
        }
        Ok(tiers)
    }

    /// The documents behind each language's `semantic.coverage.failed` and `partial` counts
    /// (#780): the selected failed or partial tuples of the language's default producer at
    /// the admitted head, plus older-basis projected documents (#620) whose basis tuple is
    /// failed or partial, as `tiers` counts them. Sorted by #22 language order, failed
    /// before partial, then path bytes. `language` keeps one language.
    pub fn mcp_coverage_documents(
        &self,
        tiers: &[LanguageTier; 5],
        language: Option<&str>,
    ) -> Result<Vec<CoverageDocument>> {
        let selected = self.selected(None)?;
        let mut documents = Vec::new();
        for (tier, name) in tiers.iter().zip(MCP_LANGUAGES) {
            if language.is_some_and(|wanted| wanted != name) {
                continue;
            }
            let Some(producer) = tier.producer_id.as_deref() else {
                continue;
            };
            let mut statement = self.db.prepare_cached(
                "SELECT c.path,c.state,c.diagnostic,
                        COALESCE(rp.macro_invocation,rt.macro_invocation)+COALESCE(rp.attribute_item,rt.attribute_item)+COALESCE(rp.macro_definition,rt.macro_definition)+COALESCE(rp.other,rt.other)>0,
                        COALESCE(rp.capped,rt.capped),COALESCE(rp.omitted,rt.omitted),COALESCE(rp.captured_diagnostics,rt.captured_diagnostics)
                 FROM semantic_coverage c LEFT JOIN semantic_coverage_reasons rp ON rp.payload_id=c.payload_id
                 LEFT JOIN semantic_coverage_reasons rt ON rt.tuple_id=c.tuple_id
                 WHERE c.revision_id=?1 AND c.producer_id=?2 AND c.language=?3 AND c.selected=1
                   AND c.state IN ('failed','partial') AND (?4 IS NULL OR c.path=?4)",
            )?;
            let mut read = |key: &str, path: Option<&str>| -> Result<()> {
                for row in statement.query_map(params![key, producer, name, path], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        [
                            row.get::<_, Option<bool>>(3)?,
                            row.get::<_, Option<bool>>(4)?,
                            row.get::<_, Option<bool>>(5)?,
                            row.get::<_, Option<bool>>(6)?,
                        ],
                    ))
                })? {
                    let (path, state, diagnostic, reasons) = row?;
                    let failed = state == "failed";
                    documents.push(CoverageDocument {
                        language: name,
                        failed,
                        reasons: if failed {
                            vec![failed_reason(diagnostic.as_deref())]
                        } else {
                            partial_reasons(reasons)
                        },
                        path,
                    });
                }
                Ok(())
            };
            read(&selected.key, None)?;
            for document in &tier.documents {
                if document.projection.semantic_status == crate::model::SemanticStatus::OlderBasis {
                    let basis = semantic_pin_key(document.projection.semantic_basis_pin);
                    read(&basis, Some(&document.path))?;
                }
            }
        }
        documents.sort_by(|a, b| {
            (
                crate::semantic_records::language_order(a.language),
                !a.failed,
                &a.path,
            )
                .cmp(&(
                    crate::semantic_records::language_order(b.language),
                    !b.failed,
                    &b.path,
                ))
        });
        Ok(documents)
    }

    /// Bindings for measured calls of the admitted head, each under its document language's
    /// producer (#483). `language` is the selected declaration's, which names the producer
    /// and, without calls, the note.
    pub(crate) fn mcp_call_semantics<'c>(
        &self,
        explicit: Option<&str>,
        owner_document: &crate::native_records::DocumentKey,
        calls: impl IntoIterator<Item = &'c crate::native_records::Call>,
    ) -> Result<McpSemantic> {
        let selected = self.selected(None)?;
        let mut reader = SemanticCalls::new(&self.db, &selected, explicit)?;
        let calls: Vec<_> = calls.into_iter().collect();
        let language = owner_document.language.as_str();
        let mut call_documents: BTreeSet<_> =
            calls.iter().map(|call| call.document.clone()).collect();
        call_documents.insert(owner_document.clone());
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
        let mut documents = Vec::new();
        for document in call_documents {
            let Some(producer) = reader.selection.producer(&document.language) else {
                continue;
            };
            if let Some(projection) = native_first::semantic_projection_for(
                &self.db,
                selected.pin,
                producer,
                &document.language,
                &document.path,
            )? {
                documents.push(crate::model::DocumentSemanticStatus {
                    producer_id: producer.to_owned(),
                    language: document.language,
                    path: document.path,
                    projection,
                });
            }
        }
        Ok(McpSemantic {
            documents,
            producer_id: reader.selection.named(language).map(str::to_owned),
            note: (!notes.is_empty()).then(|| notes.join("; ")),
            calls: semantic,
        })
    }
}

/// At most this many semantic rows are admitted into one architecture snapshot (#523).
const ARCHITECTURE_SEMANTIC_ROWS: usize = 2_000_000;

#[cfg(feature = "test-support")]
static TEST_SEMANTIC_ROWS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(ARCHITECTURE_SEMANTIC_ROWS);

/// Lower the semantic evidence row bound, so a test's small revision can reach it.
#[cfg(feature = "test-support")]
pub fn set_architecture_semantic_rows_for_tests(limit: usize) {
    TEST_SEMANTIC_ROWS.store(limit, std::sync::atomic::Ordering::Release);
}

fn semantic_rows_limit() -> usize {
    #[cfg(feature = "test-support")]
    return TEST_SEMANTIC_ROWS.load(std::sync::atomic::Ordering::Acquire);
    #[cfg(not(feature = "test-support"))]
    ARCHITECTURE_SEMANTIC_ROWS
}

fn architecture_derived_from(
    capture_id: Option<i64>,
    provenance_id: Option<String>,
    record_kind: Option<String>,
    fact_digest: Option<String>,
) -> Result<Option<sem::DerivedFrom>> {
    match (capture_id, provenance_id, record_kind, fact_digest) {
        (None, None, None, None) => Ok(None),
        (Some(_), Some(provenance_id), Some(record_kind), Some(fact_digest)) => {
            Ok(Some(sem::DerivedFrom {
                provenance_id,
                record_kind: semantic_enum(record_kind)?,
                fact_digest,
            }))
        }
        _ => Err(SelectedIntegrity("incompatible_index: corrupt raw proof link".into()).into()),
    }
}

/// Bindings of each document under its language's default producer (#483), for
/// architecture references and rules (#523). Only rows of a selected, complete or partial
/// coverage tuple whose proof is not `stale` are read; anything else stays syntax-only.
/// Possibly stale rows, such as the evidence S carries forward after an edit, are read until
/// a producer re-run replaces them, each labelled with its own freshness (#936, owner ruling
/// 2026-11-12). An internal call target must also be confirmed current (`stale_target = 0`),
/// and an internal target outside the snapshot's documents is left out.
pub(super) fn architecture_semantic(
    db: &Connection,
    revision_key: &str,
    documents: &mut [crate::references::DocumentInput],
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    use crate::references::{SemanticAnchor, SemanticOutcome, SemanticRelationship};
    let defaults = default_producers(db, revision_key)?;
    let pin_text = revision_key
        .strip_prefix("pin:v1:")
        .context(SelectedIntegrity("invalid architecture pin".into()))?;
    let (generation, revision) = pin_text
        .rsplit_once(':')
        .context(SelectedIntegrity("invalid architecture pin".into()))?;
    let pin = IndexPin {
        index_generation: generation
            .parse()
            .map_err(|_| SelectedIntegrity("invalid architecture generation".into()))?,
        index_revision: revision
            .parse()
            .map_err(|_| SelectedIntegrity("invalid architecture revision".into()))?,
    };
    ensure!(
        pin.index_revision > 0 && semantic_pin_key(pin) == revision_key,
        SelectedIntegrity("invalid architecture pin".into())
    );
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
            rows <= semantic_rows_limit(),
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
    let range = |start: i64, end: i64| -> Result<crate::native_records::Range> {
        match (usize::try_from(start), usize::try_from(end)) {
            (Ok(start), Ok(end)) if start <= end => Ok(crate::native_records::Range { start, end }),
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
    for (i, doc) in documents.iter_mut().enumerate() {
        let Some(producer) = defaults.producer(&doc.key.language) else {
            continue;
        };
        check()?;
        let projection = super::native_first::semantic_projection_for(
            db,
            pin,
            producer,
            &doc.key.language,
            &doc.key.path,
        )?;
        doc.semantic.projection = projection.clone();
        // Pending still selects the producer, but publishes no semantic rows.
        doc.semantic.producer_id = Some(producer.to_owned());
        if projection
            .as_ref()
            .is_some_and(|p| p.semantic_status.is_pending())
        {
            doc.semantic.boundary = true;
            continue;
        }
        let tuple = if projection.is_some() {
            super::native_first::projection_tuple(
                db,
                pin,
                producer,
                &doc.key.language,
                &doc.key.path,
            )?
            .map(|(_, tuple, _)| tuple)
        } else {
            db.query_row(
                "SELECT tuple_id FROM semantic_coverage WHERE revision_id=?1 AND producer_id=?2 AND language=?3 AND path=?4",
                params![revision_key, producer, doc.key.language, doc.key.path], |row| row.get(0),
            ).optional()?
        };
        let tuple = tuple.context(SelectedIntegrity(
            "missing architecture semantic tuple".into(),
        ))?;
        let (selected, state): (bool, String) = db.query_row(
            "SELECT selected,state FROM semantic_coverage WHERE tuple_id=?1",
            [tuple],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        doc.semantic.coverage_state = Some(state.clone());
        if selected && matches!(state.as_str(), "complete" | "partial") {
            tuples.push((
                tuple,
                i,
                producer.to_owned(),
                state == "complete",
                projection,
            ));
        }
    }
    // Only selected rows are loaded. Proof identities and basis metadata are keyed by
    // tuple/provenance; raw proof bytes and full semantic snapshots are never scanned.
    let mut proof = db.prepare_cached(
        "SELECT p.ordinal,p.content_hash,p.evidence_kind,p.capture_id,
                p.derived_from_provenance_id,p.derived_from_record_kind,p.derived_from_fact_digest,
                b.producer_id,b.producer_version,b.producer_hash,b.artifact_hash,b.language,
                b.source_set_id,b.captured_native_revision_id,b.source_manifest_hash,
                b.toolchain_hash,b.config_hash,b.dependency_hash,b.basis_id
         FROM semantic_provenance p JOIN semantic_basis b ON b.payload_id=p.payload_id AND b.basis_id=p.basis_id
         WHERE p.payload_id=?1 AND p.ordinal=?2",
    )?;
    let mut lookup_dependencies = db.prepare_cached(
        "SELECT lookup_key FROM semantic_basis_lookup_dependencies WHERE basis_id=?1 ORDER BY ordinal",
    )?;
    let mut metadata_rows = 0usize;
    let mut metadata = |pp: &super::semantic_payload::PayloadProjection<'_>,
                        ordinal: i64,
                        projection: Option<&crate::model::SemanticProjection>|
     -> Result<BTreeMap<String, serde_json::Value>> {
        metadata_rows += 1;
        ensure!(
            metadata_rows <= ARCHITECTURE_SEMANTIC_ROWS,
            "bounded_work: architecture proof metadata"
        );
        let payload = pp.payload_id.context(SelectedIntegrity(
            "incompatible_index: architecture tuple names no payload".into(),
        ))?;
        let mut values = proof.query_row(params![payload, ordinal], |row| {
            let mut value = BTreeMap::new();
            value.insert("historicalContentHash".into(), json!(row.get::<_, String>(1)?));
            value.insert("evidenceKind".into(), json!(row.get::<_, String>(2)?));
            let derivation = (
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
            );
            value.insert("basis".into(), json!({"producerId":row.get::<_, String>(7)?,"producerVersion":row.get::<_, String>(8)?,"producerHash":row.get::<_, String>(9)?,"artifactHash":row.get::<_, String>(10)?,"language":row.get::<_, String>(11)?,"sourceSetId":row.get::<_, String>(12)?,"revisionId":row.get::<_, String>(13)?,"sourceManifestHash":row.get::<_, String>(14)?,"toolchainHash":row.get::<_, String>(15)?,"configHash":row.get::<_, String>(16)?,"dependencyHash":row.get::<_, String>(17)?}));
            Ok((value, row.get::<_, i64>(18)?, derivation))
        })?;
        let (capture, proof, kind, digest) = values.2;
        let historical = semantic_variant(&pp.freshness(values.1)?)?;
        values
            .0
            .insert("historicalFreshness".into(), json!(historical));
        values.0.insert(
            "freshness".into(),
            json!(if projection.is_some() && historical != "stale" {
                "possiblyStale"
            } else {
                &historical
            }),
        );
        values.0.insert(
            "derivedFrom".into(),
            serde_json::to_value(architecture_derived_from(capture, proof, kind, digest)?)?,
        );
        let historical = values.0["historicalFreshness"]
            .as_str()
            .expect("stored string");
        ensure!(
            matches!(historical, "fresh" | "possiblyStale" | "stale"),
            SelectedIntegrity("invalid architecture freshness".into())
        );
        let mut dependencies = Vec::new();
        let mut dependency_rows = lookup_dependencies.query([values.1])?;
        while let Some(row) = dependency_rows.next()? {
            metadata_rows += 1;
            ensure!(
                metadata_rows <= ARCHITECTURE_SEMANTIC_ROWS,
                "bounded_work: architecture lookup dependencies"
            );
            dependencies.push(row.get::<_, String>(0)?);
        }
        values.0.get_mut("basis").expect("constructed basis")["lookupDependencies"] =
            json!(dependencies);
        if let Some(projection) = projection {
            values
                .0
                .insert("projection".into(), serde_json::to_value(projection)?);
        }
        Ok(values.0)
    };
    let mut calls = db.prepare_cached(
        "SELECT b.call_id,b.join_start,b.join_end,b.resolution,b.declared_kind,
                b.declared_internal_syntax_id,b.declared_internal_language,b.declared_internal_path,
                b.dispatch,b.stale_target,b.provenance_ordinal,b.binding_id,b.join_status,b.join_kind,b.join_document_language,b.join_document_path,COALESCE(b.join_native_revision_id,?6),p.basis_id
         FROM semantic_call_bindings b
         JOIN semantic_provenance p ON p.payload_id=b.payload_id AND p.ordinal=b.provenance_ordinal
         LEFT JOIN semantic_coverage_freshness f ON f.revision_id=?5 AND f.tuple_id=?7 AND f.basis_id=p.basis_id
         WHERE b.payload_id=?1 AND b.call_id IS NOT NULL AND (?4 OR f.basis_id IS NULL OR f.freshness<>'stale')
           AND b.join_document_language=?2 AND b.join_document_path=?3",
    )?;
    let mut references = db.prepare_cached(
        "SELECT r.start_byte,r.end_byte,r.resolution,r.declared_internal_syntax_id,
                r.declared_internal_language,r.declared_internal_path,r.provenance_ordinal,
                EXISTS(SELECT 1 FROM semantic_reference_roles o
                       WHERE o.payload_id=r.payload_id AND o.reference_id=r.reference_id AND o.role='type'),r.reference_id,p.basis_id
         FROM semantic_references r
         JOIN semantic_provenance p ON p.payload_id=r.payload_id AND p.ordinal=r.provenance_ordinal
         LEFT JOIN semantic_coverage_freshness f ON f.revision_id=?3 AND f.tuple_id=?4 AND f.basis_id=p.basis_id
         WHERE r.payload_id=?1 AND r.site='use' AND (?2 OR f.basis_id IS NULL OR f.freshness<>'stale')
           AND (?2 OR (r.resolution='resolved' AND r.declared_kind='internal') OR r.resolution='external')",
    )?;
    let mut relationships = db.prepare_cached(
        "SELECT t.kind,t.source_internal_syntax_id,t.target_kind,t.target_internal_syntax_id,
                t.target_internal_language,t.target_internal_path,t.provenance_ordinal,
                t.source_internal_language,t.source_internal_path,COALESCE(t.source_internal_native_revision_id,?5),
                CASE WHEN t.target_kind='internal' THEN COALESCE(t.target_internal_native_revision_id,?5) END,t.target_external_scheme,t.target_external_symbol,
                t.target_external_scope,t.target_external_language,t.target_external_path,p.basis_id
         FROM semantic_type_relationships t
         JOIN semantic_provenance p ON p.payload_id=t.payload_id AND p.ordinal=t.provenance_ordinal
         LEFT JOIN semantic_coverage_freshness f ON f.revision_id=?3 AND f.tuple_id=?4 AND f.basis_id=p.basis_id
         WHERE t.payload_id=?1 AND (?2 OR f.basis_id IS NULL OR f.freshness<>'stale') AND t.source_kind='internal'
           AND t.kind IN ('extends','implements')
         ORDER BY t.ordinal",
    )?;
    let mut call_endpoints = db.prepare_cached(
        "SELECT declared_kind,declared_internal_language,declared_internal_path,declared_internal_syntax_id,CASE WHEN declared_kind='internal' THEN COALESCE(declared_internal_native_revision_id,?2) END,declared_external_scheme,declared_external_symbol,declared_external_scope,declared_external_language,declared_external_path,'declared' FROM semantic_call_bindings WHERE binding_id=?1
         UNION ALL SELECT kind,internal_language,internal_path,internal_syntax_id,CASE WHEN kind='internal' THEN COALESCE(internal_native_revision_id,?2) END,external_scheme,external_symbol,external_scope,external_language,external_path,'candidate' FROM semantic_call_candidates WHERE binding_id=?1
         UNION ALL SELECT kind,internal_language,internal_path,internal_syntax_id,CASE WHEN kind='internal' THEN COALESCE(internal_native_revision_id,?2) END,external_scheme,external_symbol,external_scope,external_language,external_path,'dispatch' FROM semantic_call_possible_dispatch WHERE binding_id=?1",
    )?;
    let mut reference_endpoints = db.prepare_cached(
        "SELECT declared_kind,declared_internal_language,declared_internal_path,declared_internal_syntax_id,CASE WHEN declared_kind='internal' THEN COALESCE(declared_internal_native_revision_id,?3) END,declared_external_scheme,declared_external_symbol,declared_external_scope,declared_external_language,declared_external_path,'declared' FROM semantic_references WHERE payload_id=?1 AND reference_id=?2
         UNION ALL SELECT kind,internal_language,internal_path,internal_syntax_id,CASE WHEN kind='internal' THEN COALESCE(internal_native_revision_id,?3) END,external_scheme,external_symbol,external_scope,external_language,external_path,'candidate' FROM semantic_reference_candidates WHERE payload_id=?1 AND reference_id=?2",
    )?;
    let eligible: std::cell::RefCell<HashMap<(String, usize, IndexPin), bool>> =
        std::cell::RefCell::default();
    for (tuple, i, producer, complete, projection) in tuples {
        let historical = projection.is_some();
        let endpoint = |language: Option<&str>, path: Option<&str>| -> Result<Option<usize>> {
            let Some(target) = locate(language, path) else {
                return Ok(None);
            };
            if let Some(projection) = &projection {
                // One eligibility read per target document and basis (#934): every
                // endpoint of every projected row asks, and the answer is the document's.
                let basis = projection.semantic_basis_pin;
                let known = eligible
                    .borrow()
                    .get(&(producer.clone(), target, basis))
                    .copied();
                let current = match known {
                    Some(current) => current,
                    None => {
                        let doc = &documents[target];
                        let current = super::native_first::endpoint_boundary(
                            db,
                            pin,
                            &producer,
                            &doc.key.language,
                            &doc.key.path,
                            basis,
                        )?
                        .is_none();
                        eligible
                            .borrow_mut()
                            .insert((producer.clone(), target, basis), current);
                        current
                    }
                };
                if !current {
                    return Ok(None);
                }
            }
            Ok(Some(target))
        };
        check()?;
        let key = projection.as_ref().map_or_else(
            || revision_key.to_owned(),
            |projection| semantic_pin_key(projection.semantic_basis_pin),
        );
        let pp = super::semantic_payload::project_payload(
            db,
            &key,
            tuple,
            super::semantic_payload::FreshnessSource::Stored,
        )?;
        let payload = pp.payload_id.context(SelectedIntegrity(
            "incompatible_index: architecture tuple names no payload".into(),
        ))?;
        let native = pp.native().to_owned();
        let (language, path) = (
            documents[i].key.language.clone(),
            documents[i].key.path.clone(),
        );
        // One binding per measured call: the lowest provenance, as the call graph selects.
        // The lowest provenance represents a call, as in the call graph; an unusable one
        // (stale, or outside this snapshot) leaves the call without a binding.
        let mut by_call: BTreeMap<String, (String, Option<SemanticAnchor>)> = BTreeMap::new();
        let mut cursor = calls.query(params![
            payload, language, path, historical, key, native, tuple
        ])?;
        while let Some(row) = cursor.next()? {
            admit()?;
            // A missing required freshness row is corruption, never a filtered fact.
            let basis_freshness = pp.freshness(row.get(17)?)?;
            let call_id = id_codec::decode_occ(row.get_ref(0)?.as_blob()?)
                .map_err(|_| SelectedIntegrity("invalid semantic call ID".into()))?;
            let ordinal: i64 = row.get(10)?;
            let provenance_id = pp.provenance_id(ordinal)?;
            // Every row's offsets and dispatch decode strictly, used or not.
            let anchor_range = range(row.get(1)?, row.get(2)?)?;
            let is_direct = direct(row.get_ref(8)?.as_str()?)?;
            if by_call
                .get(&call_id)
                .is_some_and(|(kept, _)| *kept <= provenance_id)
            {
                continue;
            }
            let mut binding_metadata = metadata(&pp, ordinal, projection.as_ref())?;
            if historical {
                binding_metadata.insert(
                    "historicalResolution".into(),
                    json!(row.get::<_, String>(3)?),
                );
                binding_metadata
                    .insert("historicalDispatch".into(), json!(row.get::<_, String>(8)?));
                binding_metadata.insert("historicalJoin".into(), json!({"status":row.get::<_, String>(12)?,"kind":row.get::<_, String>(13)?,"language":row.get::<_, String>(14)?,"path":row.get::<_, String>(15)?,"revisionId":row.get::<_, String>(16)?,"range":anchor_range}));
            }
            let mut eligible_endpoints = true;
            if historical {
                let mut targets = Vec::new();
                let mut external_targets = Vec::new();
                let mut alternatives =
                    call_endpoints.query(params![row.get::<_, i64>(11)?, native])?;
                while let Some(target) = alternatives.next()? {
                    // The declared endpoint is this binding's own row, admitted above (#934).
                    if target.get_ref(10)?.as_str()? != "declared" {
                        admit()?;
                    }
                    if target.get_ref(0)?.as_str_or_null()? == Some("internal") {
                        eligible_endpoints &= endpoint(
                            target.get_ref(1)?.as_str_or_null()?,
                            target.get_ref(2)?.as_str_or_null()?,
                        )?
                        .is_some();
                        targets.push(json!({"role":target.get::<_, String>(10)?,"kind":"internal","language":target.get::<_, String>(1)?,"path":target.get::<_, String>(2)?,"syntaxId":sid(target.get_ref(3)?.as_blob_or_null()?)?,"revisionId":target.get::<_, String>(4)?}));
                    } else if target.get_ref(0)?.as_str_or_null()? == Some("external") {
                        external_targets.push(json!({"role":target.get::<_, String>(10)?,"scheme":target.get::<_, String>(5)?,"symbol":target.get::<_, String>(6)?,"scope":target.get::<_, String>(7)?,"language":target.get::<_, Option<String>>(8)?,"path":target.get::<_, Option<String>>(9)?}));
                    }
                }
                binding_metadata.insert("historicalTargets".into(), json!(targets));
                binding_metadata
                    .insert("historicalExternalTargets".into(), json!(external_targets));
            }
            // What a read at the basis binds: a resolved internal target, confirmed current
            // there and in this snapshot (#934), from a proof that read admits: any not
            // `stale`, possibly stale included (#936).
            let basis_site = basis_freshness != sem::Freshness::Stale
                && row.get_ref(3)?.as_str()? == "resolved"
                && row.get_ref(4)?.as_str_or_null()? == Some("internal")
                && row.get_ref(5)?.as_blob_or_null()?.is_some()
                && row.get::<_, Option<i64>>(9)? == Some(0)
                && locate(
                    row.get_ref(6)?.as_str_or_null()?,
                    row.get_ref(7)?.as_str_or_null()?,
                )
                .is_some();
            let outcome = match (
                row.get_ref(3)?.as_str()?,
                row.get_ref(4)?.as_str_or_null()?,
                sid(row.get_ref(5)?.as_blob_or_null()?)?,
            ) {
                ("resolved", Some("internal"), Some(target)) => {
                    let target_doc = endpoint(
                        row.get_ref(6)?.as_str_or_null()?,
                        row.get_ref(7)?.as_str_or_null()?,
                    )?;
                    // A target not confirmed current, or outside this snapshot, is stale.
                    match (row.get::<_, Option<i64>>(9)?, target_doc) {
                        (Some(0), Some(target_doc)) => {
                            SemanticOutcome::Internal(target_doc, target)
                        }
                        _ if historical => SemanticOutcome::Boundary,
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
                outcome: if eligible_endpoints {
                    outcome
                } else {
                    SemanticOutcome::Boundary
                },
                basis_site,
                provenance_id: provenance_id.clone(),
                metadata: binding_metadata,
            };
            by_call.insert(call_id, (provenance_id, Some(anchor)));
        }
        drop(cursor);
        let mut anchors: Vec<SemanticAnchor> = by_call
            .into_values()
            .filter_map(|(_, anchor)| anchor)
            .collect();
        // Resolved and external use-site references: the names a site spells.
        let mut cursor = references.query(params![payload, historical, key, tuple])?;
        while let Some(row) = cursor.next()? {
            admit()?;
            let basis_freshness = pp.freshness(row.get(9)?)?;
            // What a read at the basis binds: a resolved internal target in this snapshot
            // (#934), from a proof that read admits: any not `stale` (#936).
            let basis_site = basis_freshness != sem::Freshness::Stale
                && row.get_ref(2)?.as_str()? == "resolved"
                && row.get_ref(3)?.as_blob_or_null()?.is_some()
                && locate(
                    row.get_ref(4)?.as_str_or_null()?,
                    row.get_ref(5)?.as_str_or_null()?,
                )
                .is_some();
            let outcome = match (
                row.get_ref(2)?.as_str()?,
                sid(row.get_ref(3)?.as_blob_or_null()?)?,
            ) {
                ("resolved", Some(target)) => match endpoint(
                    row.get_ref(4)?.as_str_or_null()?,
                    row.get_ref(5)?.as_str_or_null()?,
                )? {
                    Some(target_doc) => SemanticOutcome::Internal(target_doc, target),
                    None if historical => SemanticOutcome::Boundary,
                    None => continue,
                },
                ("ambiguous", _) if historical => SemanticOutcome::Ambiguous,
                ("unresolved", _) if historical => SemanticOutcome::Unresolved,
                ("external", _) => SemanticOutcome::External,
                _ => return Err(SelectedIntegrity("corrupt semantic reference".into()).into()),
            };
            let reference_ordinal: i64 = row.get(6)?;
            let mut reference_metadata = metadata(&pp, reference_ordinal, projection.as_ref())?;
            if historical {
                reference_metadata.insert(
                    "historicalResolution".into(),
                    json!(row.get::<_, String>(2)?),
                );
            }
            let mut eligible_endpoints = true;
            if historical {
                let mut targets = Vec::new();
                let mut external_targets = Vec::new();
                let mut alternatives = reference_endpoints.query(params![
                    payload,
                    row.get_ref(8)?.as_blob()?,
                    native
                ])?;
                while let Some(target) = alternatives.next()? {
                    // The declared endpoint is this reference's own row, admitted above (#934).
                    if target.get_ref(10)?.as_str()? != "declared" {
                        admit()?;
                    }
                    if target.get_ref(0)?.as_str_or_null()? == Some("internal") {
                        eligible_endpoints &= endpoint(
                            target.get_ref(1)?.as_str_or_null()?,
                            target.get_ref(2)?.as_str_or_null()?,
                        )?
                        .is_some();
                        targets.push(json!({"role":target.get::<_, String>(10)?,"kind":"internal","language":target.get::<_, String>(1)?,"path":target.get::<_, String>(2)?,"syntaxId":sid(target.get_ref(3)?.as_blob_or_null()?)?,"revisionId":target.get::<_, String>(4)?}));
                    } else if target.get_ref(0)?.as_str_or_null()? == Some("external") {
                        external_targets.push(json!({"role":target.get::<_, String>(10)?,"scheme":target.get::<_, String>(5)?,"symbol":target.get::<_, String>(6)?,"scope":target.get::<_, String>(7)?,"language":target.get::<_, Option<String>>(8)?,"path":target.get::<_, Option<String>>(9)?}));
                    }
                }
                reference_metadata.insert("historicalTargets".into(), json!(targets));
                reference_metadata
                    .insert("historicalExternalTargets".into(), json!(external_targets));
            }
            anchors.push(SemanticAnchor {
                range: range(row.get(0)?, row.get(1)?)?,
                call_id: None,
                type_role: row.get(7)?,
                direct: false,
                outcome: if eligible_endpoints {
                    outcome
                } else {
                    SemanticOutcome::Boundary
                },
                basis_site,
                provenance_id: pp.provenance_id(reference_ordinal)?,
                metadata: reference_metadata,
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
        let mut cursor = relationships.query(params![payload, historical, key, tuple, native])?;
        while let Some(row) = cursor.next()? {
            admit()?;
            pp.freshness(row.get(16)?)?;
            let source = sid(row.get_ref(1)?.as_blob_or_null()?)?.context(SelectedIntegrity(
                "corrupt semantic type relationship".into(),
            ))?;
            let target = match (
                row.get_ref(2)?.as_str()?,
                sid(row.get_ref(3)?.as_blob_or_null()?)?,
            ) {
                ("internal", Some(target)) => endpoint(
                    row.get_ref(4)?.as_str_or_null()?,
                    row.get_ref(5)?.as_str_or_null()?,
                )?
                .map(|target_doc| (target_doc, target)),
                _ => None,
            };
            let relationship_ordinal: i64 = row.get(6)?;
            let mut relationship_metadata =
                metadata(&pp, relationship_ordinal, projection.as_ref())?;
            let source_eligible = !historical
                || endpoint(
                    row.get_ref(7)?.as_str_or_null()?,
                    row.get_ref(8)?.as_str_or_null()?,
                )?
                .is_some();
            if historical {
                relationship_metadata.insert("historicalTargets".into(), json!([
                    {"kind":"internal","language":row.get::<_, String>(7)?,"path":row.get::<_, String>(8)?,"syntaxId":source,"revisionId":row.get::<_, String>(9)?},
                    {"kind":row.get::<_, String>(2)?,"language":row.get::<_, Option<String>>(4)?,"path":row.get::<_, Option<String>>(5)?,"syntaxId":sid(row.get_ref(3)?.as_blob_or_null()?)?,"revisionId":row.get::<_, Option<String>>(10)?}
                ]));
                if row.get_ref(2)?.as_str()? == "external" {
                    relationship_metadata.insert("historicalExternalTargets".into(), json!([{"scheme":row.get::<_, String>(11)?,"symbol":row.get::<_, String>(12)?,"scope":row.get::<_, String>(13)?,"language":row.get::<_, Option<String>>(14)?,"path":row.get::<_, Option<String>>(15)?}]));
                }
                relationship_metadata.insert(
                    "semanticBoundary".into(),
                    json!(
                        !source_eligible
                            || (row.get_ref(2)?.as_str()? == "internal" && target.is_none())
                    ),
                );
            }
            found.push(SemanticRelationship {
                kind: row.get(0)?,
                source_syntax_id: source,
                target: if source_eligible { target } else { None },
                provenance_id: pp.provenance_id(relationship_ordinal)?,
                metadata: relationship_metadata,
            });
        }
        drop(cursor);
        let semantic = &mut documents[i].semantic;
        semantic.producer_id = Some(producer);
        semantic.complete = complete && !historical;
        semantic.boundary |= anchors
            .iter()
            .any(|a| a.outcome == SemanticOutcome::Boundary)
            || found
                .iter()
                .any(|r| r.metadata.get("semanticBoundary") == Some(&json!(true)));
        semantic.anchors = anchors;
        semantic.relationships = found;
    }
    Ok(())
}

#[cfg(test)]
mod reimport_tests {
    use super::*;

    #[test]
    fn architecture_derivation_preserves_complete_null_and_corrupt_shapes() {
        let proof = "a".repeat(64);
        let digest = "b".repeat(64);
        let derived = architecture_derived_from(
            Some(1),
            Some(proof.clone()),
            Some("occurrence".into()),
            Some(digest.clone()),
        )
        .unwrap();
        assert_eq!(
            derived,
            Some(sem::DerivedFrom {
                provenance_id: proof.clone(),
                record_kind: sem::RecordKind::Occurrence,
                fact_digest: digest.clone(),
            })
        );
        assert_eq!(
            serde_json::to_value(architecture_derived_from(None, None, None, None).unwrap())
                .unwrap(),
            serde_json::Value::Null
        );
        let error =
            architecture_derived_from(None, Some(proof), Some("occurrence".into()), Some(digest))
                .unwrap_err();
        assert!(error.is::<SelectedIntegrity>(), "{error:#}");
    }

    #[test]
    fn no_history_keeps_plain_unavailable_status_without_reimport() {
        let (_state, _root, store) = crate::store::selected_read_once_tests::published_store(1);
        let response = store.evidence_response().unwrap();
        let status = response.status().unwrap();
        assert!(status.semantic_languages.is_empty());
        assert_eq!(status.semantic_producer_id, None);
        assert_eq!(
            status.semantic_note.as_deref(),
            Some(engine::NO_PRODUCER_NOTE)
        );
        assert_eq!(status.stats.semantic_state, SemanticState::Unavailable);
        for tier in response.mcp_language_tiers().unwrap() {
            assert!(tier.reimport.is_none());
            assert!(tier.pending.is_none());
            assert!(!tier.semantic());
            assert_eq!(tier.freshness, "unavailable");
        }
    }

    #[test]
    fn retained_language_absent_from_native_is_reported_without_a_published_producer() {
        let (_state, _root, store) = crate::store::selected_read_once_tests::published_store(1);
        let pin = store.index_baseline().unwrap();
        let key = semantic_pin_key(pin);
        {
            let db = store.cache_write().unwrap();
            for (producer, reason) in [
                (
                    "z-producer",
                    "retained artifact no longer matches captured inputs",
                ),
                ("a-producer", "producer profile pin changed"),
            ] {
                db.execute(
                    "INSERT INTO semantic_reimport VALUES(?1,?2,'rust',?3)",
                    params![key, producer, reason],
                )
                .unwrap();
            }
        }
        let response = store.evidence_response().unwrap();
        let status = response.status().unwrap();
        assert_eq!(status.revision, pin);
        assert_eq!(status.semantic_producer_id, None);
        assert_eq!(status.stats.semantic_state, SemanticState::Unavailable);
        assert_eq!(
            status.stats.internal + status.stats.external + status.stats.ambiguous,
            0
        );
        assert_eq!(
            status
                .semantic_languages
                .iter()
                .map(|row| row.language.as_str())
                .collect::<Vec<_>>(),
            ["javascript", "rust"]
        );
        let javascript = &status.semantic_languages[0];
        assert!(javascript.reimport.is_none());
        assert_eq!(javascript.calls, 1);
        let rust = &status.semantic_languages[1];
        let expected = Some(crate::model::SemanticReimport::NeedsReimport {
            reason:
                "producer profile pin changed; retained artifact no longer matches captured inputs"
                    .into(),
        });
        assert_eq!(rust.reimport, expected);
        assert_eq!(rust.calls, 0);
        assert_eq!(rust.semantic_producer_id, None);
        assert_eq!(rust.semantic_state, SemanticState::Unavailable);
        assert!(rust.coverage.is_none());
        let tiers = response.mcp_language_tiers().unwrap();
        assert_eq!(tiers[1].reimport, expected);
        assert!(!tiers[1].semantic());
        assert_eq!(tiers[1].producer_id, None);
        assert_eq!(tiers[1].freshness, "unavailable");
        assert_eq!(tiers[1].counts, [0; 5]);
        assert!(tiers[3].reimport.is_none());
        let Some(crate::model::SemanticReimport::NeedsReimport { reason }) = expected else {
            unreachable!()
        };
        assert_eq!(
            tiers[1].pending,
            Some(LanguagePending {
                status: crate::model::SemanticStatus::NeedsReimport,
                reason
            })
        );
        assert!(tiers[3].pending.is_none());
    }

    fn document(status: crate::model::SemanticStatus) -> crate::model::DocumentSemanticStatus {
        let pin = crate::model::IndexPin {
            index_generation: uuid::Uuid::nil(),
            index_revision: 1,
        };
        crate::model::DocumentSemanticStatus {
            producer_id: "p".into(),
            language: "rust".into(),
            path: "src/lib.rs".into(),
            projection: crate::model::SemanticProjection {
                current_pin: pin,
                semantic_basis_pin: pin,
                basis_native_revision_id: "r".into(),
                semantic_status: status,
            },
        }
    }

    /// #660: a re-import reason wins, then a document that needs re-import, then one that
    /// is catching up; an older basis alone is not pending.
    #[test]
    fn language_pending_follows_reimport_then_required_then_pending() {
        use crate::model::SemanticStatus::{CatchingUp, NeedsReimport, OlderBasis};
        let mut tier = LanguageTier::default();
        assert_eq!(language_pending(&tier), None);
        tier.documents = vec![document(OlderBasis)];
        assert_eq!(language_pending(&tier), None);
        tier.documents.push(document(CatchingUp));
        let pending = language_pending(&tier).unwrap();
        assert_eq!(
            (pending.status, pending.reason.as_str()),
            (CatchingUp, "re-import pending")
        );
        tier.documents.push(document(NeedsReimport));
        let required = language_pending(&tier).unwrap();
        assert_eq!(
            (required.status, required.reason.as_str()),
            (NeedsReimport, "re-import required")
        );
        tier.reimport = Some(crate::model::SemanticReimport::NeedsReimport {
            reason: "producer profile pin changed".into(),
        });
        let reimport = language_pending(&tier).unwrap();
        assert_eq!(
            (reimport.status, reimport.reason.as_str()),
            (NeedsReimport, "producer profile pin changed")
        );
    }

    /// #660 rule 1 relabels only `NeedsReimport` documents; rule 2 sets the reason and
    /// leaves the documents; rule 1 wins when both name the language.
    #[test]
    fn auto_run_labels_relabel_only_needs_reimport_documents() {
        use crate::model::SemanticStatus::{CatchingUp, NeedsReimport, OlderBasis};
        let mut documents = vec![
            document(NeedsReimport),
            document(OlderBasis),
            document(CatchingUp),
            document(NeedsReimport),
        ];
        let mut reimport = None;
        let mut labels = AutoRunLabels::default();
        assert_eq!(
            apply_auto_run_labels("rust", &mut documents, &mut reimport, &labels),
            None
        );
        labels.catching_up.insert("rust".into());
        labels
            .failed
            .insert("rust".into(), "automatic re-run failed: x".into());
        let pending = apply_auto_run_labels("rust", &mut documents, &mut reimport, &labels);
        assert_eq!(
            pending,
            Some(LanguagePending {
                status: CatchingUp,
                reason: "producer re-run scheduled".into()
            })
        );
        let statuses: Vec<_> = documents
            .iter()
            .map(|d| d.projection.semantic_status)
            .collect();
        assert_eq!(statuses, [CatchingUp, OlderBasis, CatchingUp, CatchingUp]);
        assert_eq!(reimport, None);
        assert_eq!(
            apply_auto_run_labels("java", &mut documents, &mut reimport, &labels),
            None
        );

        labels.catching_up.clear();
        let mut documents = vec![document(NeedsReimport), document(CatchingUp)];
        let pending = apply_auto_run_labels("rust", &mut documents, &mut reimport, &labels);
        assert_eq!(
            pending,
            Some(LanguagePending {
                status: NeedsReimport,
                reason: "automatic re-run failed: x".into()
            })
        );
        let statuses: Vec<_> = documents
            .iter()
            .map(|d| d.projection.semantic_status)
            .collect();
        assert_eq!(statuses, [NeedsReimport, CatchingUp]);
        assert_eq!(
            reimport,
            Some(crate::model::SemanticReimport::NeedsReimport {
                reason: "automatic re-run failed: x".into()
            })
        );
    }
}

#[cfg(test)]
mod mcp_languages_tests {
    #[test]
    fn mcp_languages_are_the_five_language_rows_in_contract_order() {
        assert_eq!(
            super::MCP_LANGUAGES,
            ["java", "rust", "python", "javascript", "typescript"]
        );
        assert_eq!(
            super::MCP_LANGUAGES,
            crate::semantic_records::LANGUAGE_ORDER
        );
    }

    /// #780: each stored failure diagnostic and recorded partial reason has its typed code,
    /// in the #520 summary's buckets; anything else is `other` or `unrecorded`.
    #[test]
    fn coverage_document_reason_codes() {
        use super::{failed_reason, partial_reasons};
        for (diagnostic, code) in [
            (Some("excluded_failed_by_producer"), "notInProducerOutput"),
            (
                Some("unmeasurable_source_anchor"),
                "unmeasurableSourceAnchor",
            ),
            (
                Some("selected raw capture unavailable: batch 3 missing"),
                "rawCaptureUnavailable",
            ),
            (Some("Failed fixture"), "other"),
            (None, "other"),
        ] {
            assert_eq!(failed_reason(diagnostic), code, "{diagnostic:?}");
        }
        assert_eq!(partial_reasons([None; 4]), ["unrecorded"]);
        assert_eq!(partial_reasons([Some(false); 4]), ["unrecorded"]);
        assert_eq!(
            partial_reasons([Some(true), Some(false), Some(false), Some(false)]),
            ["nativeGaps"]
        );
        assert_eq!(
            partial_reasons([Some(false), Some(true), Some(true), Some(true)]),
            ["capped", "omitted", "capturedDiagnostics"]
        );
    }
}
