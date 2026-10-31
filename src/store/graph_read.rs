//! Rooted call-graph reads over the one admitted selected SQLite snapshot.
//!
//! The pure engine in `graph_query` asks a [`SnapshotGraphSource`] for facts; every fact it
//! reads is recorded so a byte-reduced answer can be re-evaluated without SQL (#40).
use super::*;
use crate::{
    graph_query::{
        DefaultProducers, DocumentFacts, GraphAnswer, GraphFacts, GraphIntegrity, GraphQuery,
        GraphRequest, GraphSnapshot, GraphSource, HistoryFacts, SemanticDocument, evaluate,
    },
    native_evidence::DocumentKey,
    semantic_evidence as sem,
};

/// Corruption found by the engine fails like any other selected-read corruption (#261).
fn engine_error(error: anyhow::Error) -> anyhow::Error {
    match error.downcast::<GraphIntegrity>() {
        Ok(integrity) => SelectedIntegrity(integrity.0).into(),
        Err(error) => error,
    }
}

fn native_to_semantic<T: serde::Serialize, U: serde::de::DeserializeOwned>(value: &T) -> Result<U> {
    serde_json::to_value(value)
        .and_then(serde_json::from_value)
        .map_err(|error| SelectedIntegrity(format!("store_unavailable: {error}")).into())
}

struct SnapshotGraphSource<'a> {
    db: &'a Connection,
    selected: &'a ReadRevision,
    /// An explicit producer; `None` selects each language's default producer (#483).
    producer: Option<String>,
    snapshot: GraphSnapshot,
    scopes: BTreeMap<DocumentKey, V8NativeScope>,
    declaration_roles: BTreeMap<(DocumentKey, String), crate::native_evidence::DeclarationRole>,
    facts: GraphFacts,
    check: &'a mut dyn FnMut() -> Result<()>,
}

impl SnapshotGraphSource<'_> {
    fn scope(&mut self, key: &DocumentKey) -> Result<V8NativeScope> {
        if let Some(scope) = self.scopes.get(key) {
            return Ok(scope.clone());
        }
        let scope = v8_native_scope_for(self.db, key, self.selected)?.ok_or_else(|| {
            SelectedIntegrity("store_unavailable: call graph document missing".into())
        })?;
        self.scopes.insert(key.clone(), scope.clone());
        Ok(scope)
    }

    /// The per-language default selection, read once and recorded in the facts (#483).
    fn defaults(&mut self) -> Result<&DefaultProducers> {
        if self.facts.defaults.is_none() {
            self.facts.defaults = Some(semantic_display::default_producers(
                self.db,
                &self.selected.key,
            )?);
        }
        Ok(self.facts.defaults.as_ref().expect("defaults read"))
    }

    /// The producer of a document in `language`: the explicit one when it covers the
    /// language, else the language's default.
    fn producer_for(&mut self, language: &str) -> Result<Option<String>> {
        match &self.producer {
            Some(producer) => Ok(self
                .facts
                .producers
                .get(producer)
                .is_some_and(|languages| languages.contains(language))
                .then(|| producer.clone())),
            None => Ok(self.defaults()?.producer(language).map(str::to_owned)),
        }
    }

    fn read_document(&mut self, key: &DocumentKey) -> Result<DocumentFacts> {
        let scope = self.scope(key)?;
        let declarations = read_native_declarations_v8(self.db, &scope, None, true)?
            .into_iter()
            .map(|declaration| (declaration.syntax_id.clone(), declaration))
            .collect();
        let mut calls: BTreeMap<String, Vec<crate::native_evidence::Call>> = BTreeMap::new();
        for call in read_native_calls_v8(self.db, &scope, None)? {
            calls
                .entry(call.owner_syntax_id.clone())
                .or_default()
                .push(call);
        }
        let native_coverage = native_to_semantic(&read_native_coverage_v8(self.db, &scope)?)?;
        let hash: String = self.db.query_row(
            "SELECT content_hash FROM document_versions d JOIN native_source_sets s ON s.id=?2 WHERE d.id=?1 AND d.language=?3 AND d.path=?4",
            params![scope.version_id, scope.key.source_set_id, scope.key.language, scope.key.path],
            |r| r.get(0),
        )?;
        let native_provenance = native_to_semantic(&crate::native_evidence::Provenance {
            id: scope.proof_id(),
            producer_id: scope.producer_id.clone(),
            document: scope.key.clone(),
            revision_id: scope.native_revision_id.clone(),
            content_hash: hash,
            evidence_kind: "measuredSyntax".into(),
            basis: None,
            freshness: "fresh".into(),
            derived_from: None,
        })?;
        let semantic = match self.producer_for(&key.language)? {
            Some(producer) => self.read_semantic(&producer, key)?,
            None => None,
        };
        Ok(DocumentFacts {
            key: key.clone(),
            declarations,
            calls,
            native_coverage,
            native_provenance,
            semantic,
        })
    }

    /// `None` when the producer has no coverage row for the document; the engine reports it.
    fn read_semantic(&self, producer: &str, key: &DocumentKey) -> Result<Option<SemanticDocument>> {
        let tuple: Option<i64> = self
            .db
            .query_row(
                "SELECT tuple_id FROM semantic_coverage WHERE revision_id=?1 AND producer_id=?2 AND language=?3 AND path=?4",
                params![self.selected.key, producer, key.language, key.path],
                |r| r.get(0),
            )
            .optional()?;
        let Some(tuple) = tuple else {
            return Ok(None);
        };
        let snapshot = read_semantic_tuple(self.db, &self.selected.key, Some(tuple), None, true)?;
        let [coverage] = <[_; 1]>::try_from(snapshot.coverage).map_err(|_| {
            SelectedIntegrity("incompatible_index: corrupt selected semantic coverage".into())
        })?;
        Ok(Some(SemanticDocument {
            coverage,
            provenance: snapshot
                .provenance
                .into_iter()
                .map(|proof| (proof.id.clone(), proof))
                .collect(),
            call_bindings: snapshot.call_bindings,
            declaration_bindings: snapshot.declaration_bindings,
            symbols: snapshot.symbols,
            type_relationships: snapshot.type_relationships,
        }))
    }
}

impl GraphSource for SnapshotGraphSource<'_> {
    fn snapshot(&self) -> &GraphSnapshot {
        &self.snapshot
    }

    fn producer_languages(&mut self, producer_id: &str) -> Result<Option<BTreeSet<String>>> {
        if let Some(languages) = self.facts.producers.get(producer_id) {
            return Ok(Some(languages.clone()));
        }
        let languages = self
            .db
            .prepare(
                "SELECT l.language FROM semantic_revision_producers p JOIN semantic_producer_languages l ON l.id=p.id AND l.version=p.version WHERE p.revision_id=?1 AND p.id=?2",
            )?
            .query_map(params![self.selected.key, producer_id], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<BTreeSet<_>>>()?;
        if languages.is_empty() {
            return Ok(None);
        }
        self.facts
            .producers
            .insert(producer_id.to_string(), languages.clone());
        Ok(Some(languages))
    }

    fn default_producers(&mut self) -> Result<Option<DefaultProducers>> {
        Ok(Some(self.defaults()?.clone()))
    }

    fn locate(&mut self, syntax_id: &str) -> Result<Option<DocumentKey>> {
        if let Some(location) = self.facts.locations.get(syntax_id) {
            return Ok(location.clone());
        }
        let key = v8_owner_scope_for(self.db, syntax_id, self.selected)?.map(|scope| {
            let key = scope.key.clone();
            self.scopes.insert(key.clone(), scope);
            key
        });
        self.facts
            .locations
            .insert(syntax_id.to_string(), key.clone());
        Ok(key)
    }

    fn declaration_role(
        &mut self,
        key: &DocumentKey,
        syntax_id: &str,
    ) -> Result<crate::native_evidence::DeclarationRole> {
        let cache_key = (key.clone(), syntax_id.to_string());
        if let Some(role) = self.declaration_roles.get(&cache_key) {
            return Ok(*role);
        }
        let scope = self.scope(key)?;
        let id = id_codec::encode_sid(syntax_id).ok_or_else(|| {
            SelectedIntegrity("store_unavailable: invalid graph declaration identity".into())
        })?;
        let stored: String = self
            .db
            .query_row(
                "SELECT role FROM native_version_declarations WHERE vid=?1 AND syntax_id=?2",
                params![scope.vid, id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| {
                SelectedIntegrity("store_unavailable: graph declaration role missing".into())
            })?;
        let role = crate::native_evidence::DeclarationRole::from_stored(&stored)
            .map_err(|error| SelectedIntegrity(error.to_string()))?;
        self.declaration_roles.insert(cache_key, role);
        Ok(role)
    }

    fn document(&mut self, key: &DocumentKey) -> Result<Arc<DocumentFacts>> {
        if let Some(document) = self.facts.documents.get(key) {
            return Ok(document.clone());
        }
        let document = Arc::new(self.read_document(key)?);
        self.facts.documents.insert(key.clone(), document.clone());
        Ok(document)
    }

    fn history(&mut self, key: &DocumentKey, syntax_id: &str) -> Result<Option<Arc<HistoryFacts>>> {
        let cache_key = (key.clone(), syntax_id.to_string());
        if let Some(history) = self.facts.history.get(&cache_key) {
            return Ok(history.clone());
        }
        let producer = self
            .producer_for(&key.language)?
            .ok_or_else(|| anyhow::anyhow!("call graph history requires a producer"))?;
        let history = read_semantic_history(
            self.db,
            &self.selected.key,
            self.selected.pin.index_revision,
            &producer,
            key,
            syntax_id,
        )?
        .map(|history| {
            let freshness = history.presentation_freshness;
            Arc::new(HistoryFacts {
                coverage: history.coverage,
                provenance: history
                    .provenance
                    .into_iter()
                    .map(|proof| sem::Provenance { freshness, ..proof })
                    .collect(),
            })
        });
        self.facts.history.insert(cache_key, history.clone());
        Ok(history)
    }

    fn check(&mut self) -> Result<()> {
        #[cfg(feature = "test-support")]
        crate::mcp::phase_hook("call_graph_node")?;
        (self.check)()
    }

    fn check_work(&mut self) -> Result<()> {
        (self.check)()
    }
}

/// Bound the exact facts the shared decoder will read, without loading unrelated tuple facts.
fn navigation_call_budget(
    db: &Connection,
    tuple: i64,
    call: &[u8],
    reader: &mut crate::navigation::Reader<'_>,
) -> Result<bool> {
    let sql = "SELECT length(CAST(json_object('tuple_id',tuple_id,'revision_id',revision_id,'producer_id',producer_id,'producer_version',producer_version,'language',language,'path',path,'requested',requested,'selected',selected,'state',state,'diagnostic',diagnostic) AS BLOB)) FROM semantic_coverage WHERE tuple_id=?1
UNION ALL
SELECT length(CAST(json_object('tuple_id',tuple_id,'kind',kind,'ordinal',ordinal,'role',role) AS BLOB)) FROM semantic_coverage_roles WHERE tuple_id=?1
UNION ALL
SELECT length(CAST(json_object('id',id,'version',version,'executable_hash',executable_hash,'kind',kind,'position_encoding',position_encoding) AS BLOB)) FROM semantic_producers WHERE (id,version) IN (SELECT producer_id,producer_version FROM semantic_coverage WHERE tuple_id=?1)
UNION ALL
SELECT length(CAST(json_object('id',id,'version',version,'language',language,'ordinal',ordinal) AS BLOB)) FROM semantic_producer_languages WHERE (id,version) IN (SELECT producer_id,producer_version FROM semantic_coverage WHERE tuple_id=?1)
UNION ALL
SELECT length(CAST(json_object('binding_id',binding_id,'tuple_id',tuple_id,'ordinal',ordinal,'call_id',hex(call_id),'join_document_language',join_document_language,'join_document_path',join_document_path,'join_native_revision_id',join_native_revision_id,'join_content_hash',join_content_hash,'join_start',join_start,'join_end',join_end,'join_kind',join_kind,'join_status',join_status,'join_diagnostic',join_diagnostic,'resolution',resolution,'declared_kind',declared_kind,'declared_internal_syntax_id',hex(declared_internal_syntax_id),'declared_internal_language',declared_internal_language,'declared_internal_path',declared_internal_path,'declared_internal_native_revision_id',declared_internal_native_revision_id,'declared_external_scheme',declared_external_scheme,'declared_external_symbol',declared_external_symbol,'declared_external_scope',declared_external_scope,'declared_external_language',declared_external_language,'declared_external_path',declared_external_path,'dispatch',dispatch,'possible_dispatch_complete',possible_dispatch_complete,'stale_target',stale_target,'provenance_id',provenance_id) AS BLOB)) FROM semantic_call_bindings WHERE tuple_id=?1 AND call_id=?2
UNION ALL
SELECT length(CAST(json_object('binding_id',binding_id,'ordinal',ordinal,'kind',kind,'internal_syntax_id',hex(internal_syntax_id),'internal_language',internal_language,'internal_path',internal_path,'internal_native_revision_id',internal_native_revision_id,'external_scheme',external_scheme,'external_symbol',external_symbol,'external_scope',external_scope,'external_language',external_language,'external_path',external_path) AS BLOB)) FROM semantic_call_candidates WHERE binding_id IN (SELECT binding_id FROM semantic_call_bindings WHERE tuple_id=?1 AND call_id=?2)
UNION ALL
SELECT length(CAST(json_object('binding_id',binding_id,'ordinal',ordinal,'kind',kind,'internal_syntax_id',hex(internal_syntax_id),'internal_language',internal_language,'internal_path',internal_path,'internal_native_revision_id',internal_native_revision_id,'external_scheme',external_scheme,'external_symbol',external_symbol,'external_scope',external_scope,'external_language',external_language,'external_path',external_path) AS BLOB)) FROM semantic_call_possible_dispatch WHERE binding_id IN (SELECT binding_id FROM semantic_call_bindings WHERE tuple_id=?1 AND call_id=?2)
UNION ALL
SELECT length(CAST(json_object('binding_id',binding_id,'ordinal',ordinal,'candidate_id',hex(candidate_id)) AS BLOB)) FROM semantic_call_join_candidates WHERE binding_id IN (SELECT binding_id FROM semantic_call_bindings WHERE tuple_id=?1 AND call_id=?2)
UNION ALL
SELECT length(CAST(json_object('tuple_id',tuple_id,'provenance_id',provenance_id,'content_hash',content_hash,'evidence_kind',evidence_kind,'freshness',freshness,'basis_id',basis_id,'capture_id',capture_id,'derived_from_provenance_id',derived_from_provenance_id,'derived_from_record_kind',derived_from_record_kind,'derived_from_fact_digest',derived_from_fact_digest) AS BLOB)) FROM semantic_provenance WHERE tuple_id=?1 AND provenance_id IN (SELECT provenance_id FROM semantic_call_bindings WHERE tuple_id=?1 AND call_id=?2)
UNION ALL
SELECT length(CAST(json_object('basis_id',basis_id,'tuple_id',tuple_id,'producer_id',producer_id,'producer_version',producer_version,'producer_hash',producer_hash,'artifact_hash',artifact_hash,'language',language,'source_set_id',source_set_id,'captured_native_revision_id',captured_native_revision_id,'source_manifest_hash',source_manifest_hash,'toolchain_hash',toolchain_hash,'config_hash',config_hash,'dependency_hash',dependency_hash) AS BLOB)) FROM semantic_basis WHERE basis_id IN (SELECT basis_id FROM semantic_provenance WHERE tuple_id=?1 AND provenance_id IN (SELECT provenance_id FROM semantic_call_bindings WHERE tuple_id=?1 AND call_id=?2))
UNION ALL
SELECT length(CAST(json_object('basis_id',basis_id,'ordinal',ordinal,'lookup_key',lookup_key) AS BLOB)) FROM semantic_basis_lookup_dependencies WHERE basis_id IN (SELECT basis_id FROM semantic_provenance WHERE tuple_id=?1 AND provenance_id IN (SELECT provenance_id FROM semantic_call_bindings WHERE tuple_id=?1 AND call_id=?2))
UNION ALL
SELECT length(CAST(json_object('capture_id',capture_id,'proof_id',proof_id,'record_kind',record_kind,'fact_digest',fact_digest,'language',language,'path',path) AS BLOB)) FROM semantic_raw_proofs WHERE (capture_id,proof_id) IN (SELECT capture_id,derived_from_provenance_id FROM semantic_provenance WHERE tuple_id=?1 AND provenance_id IN (SELECT provenance_id FROM semantic_call_bindings WHERE tuple_id=?1 AND call_id=?2))
UNION ALL
SELECT length(CAST(json_object('capture_id',capture_id,'producer_id',producer_id,'artifact_hash',artifact_hash,'source_set_id',source_set_id,'captured_revision',captured_revision,'decoder_version',decoder_version) AS BLOB)) FROM semantic_raw_captures WHERE capture_id IN (SELECT capture_id FROM semantic_provenance WHERE tuple_id=?1 AND provenance_id IN (SELECT provenance_id FROM semantic_call_bindings WHERE tuple_id=?1 AND call_id=?2))
LIMIT ?3";
    let mut statement = db.prepare(sql)?;
    let mut rows = statement.query(params![
        tuple,
        call,
        (crate::navigation::QUERY_ROWS + 1) as i64
    ])?;
    let mut count = 0;
    while let Some(row) = rows.next()? {
        count += 1;
        if count > crate::navigation::QUERY_ROWS || !reader.charge(row.get::<_, i64>(0)? as usize) {
            reader.clipped();
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn navigation_calls(
    store: &Store,
    db: &Connection,
    selected: &ReadRevision,
    document: Option<&SelectedDocument>,
    selector: &crate::navigation::SourceSelector,
    reader: &mut crate::navigation::Reader<'_>,
) -> Result<()> {
    use crate::native_evidence::{Call, Range};
    let Some(producer) = selector.semantic_producer_id.as_deref() else {
        return Ok(());
    };
    let Some(document) = document else {
        return Ok(());
    };
    let (source_set_id, revision_id): (String, String) = db.query_row(
        "SELECT source_set_id,native_revision_id FROM native_revisions WHERE id=?1",
        [&selected.key],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let snapshot = GraphSnapshot {
        source_set_id: source_set_id.clone(),
        revision_id: revision_id.clone(),
    };
    let mut check = || Ok(());
    let mut source = SnapshotGraphSource {
        db,
        selected,
        producer: Some(producer.to_string()),
        snapshot: snapshot.clone(),
        scopes: BTreeMap::new(),
        declaration_roles: BTreeMap::new(),
        facts: GraphFacts::memo(snapshot),
        check: &mut check,
    };
    let Some(languages) = source.producer_languages(producer)? else {
        return Err(crate::navigation::InvalidRequest(
            "Semantic producer is not published for the selected revision.",
        )
        .into());
    };
    if !languages.contains(&document.file.language) {
        return Ok(());
    }
    let key = DocumentKey {
        source_set_id,
        language: document.file.language.clone(),
        path: selector.path.clone(),
    };
    let tuple: Option<i64> = db.query_row(
        "SELECT tuple_id FROM semantic_coverage WHERE revision_id=?1 AND producer_id=?2 AND language=?3 AND path=?4",
        params![selected.key, producer, key.language, key.path], |row| row.get(0),
    ).optional()?;
    let tuple = tuple.context(SelectedIntegrity("missing producer coverage tuple".into()))?;
    let proof_id = source.scope(&key)?.proof_id();
    let mut target_documents = BTreeMap::new();
    let local = document.graph_calls.iter().filter(|call| {
        call.path == selector.path
            && call.range.start_line <= selector.line
            && (call.range.end_line > selector.line
                || call.range.end_line == selector.line && call.range.end_column > 1)
    });
    for (index, measured) in local.enumerate() {
        if index >= crate::navigation::QUERY_ROWS
            || !reader.charge(serde_json::to_vec(measured)?.len())
        {
            reader.clipped();
            break;
        }
        let call = Call {
            id: measured.id.clone(),
            owner_syntax_id: measured.caller.clone(),
            ordinal: measured.ordinal,
            document: key.clone(),
            revision_id: revision_id.clone(),
            range: Range {
                start: measured.range.start_byte,
                end: measured.range.end_byte,
            },
            callee_range: measured.callee_range.as_ref().map(|range| Range {
                start: range.start_byte,
                end: range.end_byte,
            }),
            spelling: measured.callee_text.clone(),
            region_ids: measured.regions.clone(),
            provenance_id: proof_id.clone(),
        };
        let encoded = id_codec::encode_occ(&call.id).context(SelectedIntegrity(
            "invalid selected navigation call ID".into(),
        ))?;
        if !navigation_call_budget(db, tuple, &encoded, reader)? {
            break;
        }
        let facts = read_semantic_tuple_for_call(
            db,
            &selected.key,
            Some(tuple),
            None,
            true,
            Some(&encoded),
        )?;
        let [coverage] = <[_; 1]>::try_from(facts.coverage)
            .map_err(|_| SelectedIntegrity("corrupt selected semantic coverage".into()))?;
        let semantic = SemanticDocument {
            coverage,
            provenance: facts
                .provenance
                .into_iter()
                .map(|proof| (proof.id.clone(), proof))
                .collect(),
            call_bindings: facts.call_bindings,
            declaration_bindings: vec![],
            symbols: vec![],
            type_relationships: vec![],
        };
        let Some((id, target_key)) =
            crate::graph_query::resolved_call_target(Some(&semantic), &call)
                .map_err(engine_error)?
        else {
            continue;
        };
        if source.locate(id)?.as_ref() != Some(target_key) {
            continue;
        }
        if !target_documents.contains_key(target_key) {
            let target = if target_key == &key {
                document.nodes.clone()
            } else {
                store
                    .selected_graph_document_for(db, &target_key.path, selected)?
                    .nodes
            };
            target_documents.insert(target_key.clone(), target);
        }
        if let Some(symbol) = target_documents[target_key]
            .iter()
            .find(|symbol| symbol.id == id && symbol.path == target_key.path)
        {
            for symbol in reader.symbol_records([symbol.clone()])? {
                reader.add_call(symbol, &call)?;
            }
        }
    }
    Ok(())
}

/// Enrich the existing syntax tree at its already selected transaction/revision.
///
/// With fresh bindings from the sole producer covering the seed's language (#483), each
/// measured call carries the engine's disposition. Exact direct/constructor targets become
/// participants, and the first call to each one has the target's own measured body expanded
/// beneath it, within the view's step, depth and participant bounds (#522). Calls bound to
/// external declarations (the standard library included) are incidental: counted, and
/// hidden unless `show_all`. Without fresh evidence for the seed the syntax view is kept,
/// and `semantic.reason` says why.
pub(super) fn sequence_calls(
    store: &Store,
    db: &Connection,
    selected: &ReadRevision,
    view: &mut crate::behavior::SequenceView,
    show_all: bool,
) -> Result<()> {
    use crate::behavior::{EvidenceTier, SequenceLimit, SequenceSemantic, cap_visible};
    use crate::graph_query::{self as engine, GraphNode};
    let (source_set_id, revision_id): (String, String) = db.query_row(
        "SELECT source_set_id,native_revision_id FROM native_revisions WHERE id=?1",
        [&selected.key],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let snapshot = GraphSnapshot {
        source_set_id: source_set_id.clone(),
        revision_id: revision_id.clone(),
    };
    let mut check = || Ok(());
    // Each document uses its language's default producer (#483).
    let mut source = SnapshotGraphSource {
        db,
        selected,
        producer: None,
        snapshot: snapshot.clone(),
        scopes: BTreeMap::new(),
        declaration_roles: BTreeMap::new(),
        facts: GraphFacts::memo(snapshot),
        check: &mut check,
    };
    let key = source
        .locate(&view.seed.id)?
        .ok_or_else(|| SelectedIntegrity("selected sequence declaration missing".into()))?;
    let defaults = source.defaults()?.clone();
    let Some(producer) = defaults.producer(&key.language).map(str::to_owned) else {
        if defaults
            .by_language
            .get(&key.language)
            .is_some_and(|producers| producers.len() > 1)
        {
            view.warnings
                .push(engine::multiple_producers_note(&key.language));
        }
        view.semantic = SequenceSemantic::syntax(defaults.note(&key.language));
        cap_visible(view, crate::behavior::MAX_STEPS);
        return Ok(());
    };
    let languages = defaults.languages();
    let document = source.document(&key)?;
    let declaration = document
        .declarations
        .get(&view.seed.id)
        .ok_or_else(|| SelectedIntegrity("selected sequence declaration missing".into()))?
        .clone();
    let Some(semantic) = engine::semantic(Some(&languages), &document).map_err(engine_error)?
    else {
        cap_visible(view, crate::behavior::MAX_STEPS);
        return Ok(());
    };
    let seed_calls = document
        .calls
        .get(&view.seed.id)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if let Some(reason) = syntax_fallback(semantic, seed_calls, &key.path, &mut source)? {
        view.semantic = SequenceSemantic::syntax(Some(reason));
        cap_visible(view, crate::behavior::MAX_STEPS);
        return Ok(());
    }
    view.semantic = SequenceSemantic {
        tier: EvidenceTier::Semantic,
        producer_id: Some(producer.clone()),
        reason: None,
    };
    // Syntax receiver groups are display hints, not resolved participant lanes.
    view.participants
        .retain(|participant| participant.id == view.seed.id);
    if let Some(participant) = view.participants.iter_mut().find(|p| p.id == view.seed.id) {
        participant.declaration = Some(view.seed.clone());
    }
    // Hidden incidental calls do not count toward the visible bound (#536): hide them
    // first, then bound what is shown. The work bound already holds every step built. A
    // seed with a resolved target keeps room for the callee bodies it expands (#542).
    let targets = hide_incidental(
        &mut source,
        &languages,
        show_all,
        &mut view.steps,
        &document,
        &view.seed.id,
    )?;
    cap_visible(
        view,
        if targets {
            crate::behavior::SEED_STEPS
        } else {
            crate::behavior::MAX_STEPS
        },
    );
    let mut enrichment = Enrichment {
        store,
        db,
        selected,
        source: &mut source,
        languages: &languages,
        revision: view.revision,
        show_all,
        participants: &mut view.participants,
        warnings: &mut view.warnings,
        nodes: vec![GraphNode {
            declaration,
            depth: 0,
        }],
        edges: vec![],
        expanded: BTreeSet::new(),
        stack: vec![view.seed.id.clone()],
        used: crate::behavior::visible_count(&view.steps),
        work: crate::behavior::step_count(&view.steps),
        truncated: false,
        limits: BTreeSet::new(),
        incidental: 0,
    };
    let seed_id = view.seed.id.clone();
    enrichment.steps(&mut view.steps, &document, &seed_id, 0)?;
    let Enrichment {
        nodes,
        edges,
        truncated,
        limits,
        incidental,
        ..
    } = enrichment;
    view.hidden_steps = crate::behavior::hidden_count(&view.steps);
    view.incidental_steps = incidental;
    view.truncated |= truncated;
    for limit in limits {
        view.limit(limit);
    }
    if view.truncated_by.contains(&SequenceLimit::Participants) {
        view.warnings
            .push("Sequence participant limit reached; additional targets are not shown.".into());
    }
    let request = GraphRequest {
        source_set_id,
        revision_id,
        root_syntax_id: view.seed.id.clone(),
        semantic_producer_id: Some(producer),
        depth: 1,
        max_nodes: 20,
        max_calls: 200,
        include_external: true,
        include_tests: true,
    };
    view.semantic_warnings =
        engine::occurrence_warnings(&request, &mut source, Some(&languages), &nodes, &edges)
            .map_err(engine_error)?;
    view.warnings.extend(
        view.semantic_warnings
            .iter()
            .map(|warning| warning.message.clone()),
    );
    Ok(())
}

/// Why a seed with a selected producer still shows syntax (#522): its coverage is not
/// usable, or none of its measured calls has a fresh selected binding. One fresh binding
/// is enough to enrich; the others then keep their own stale or missing boundaries.
fn syntax_fallback(
    semantic: &SemanticDocument,
    calls: &[crate::native_evidence::Call],
    path: &str,
    source: &mut dyn GraphSource,
) -> Result<Option<String>> {
    use sem::CoverageState;
    let coverage = &semantic.coverage;
    if !coverage.selected {
        return Ok(Some(format!(
            "Semantic coverage of {path} is not selected at this index revision; showing syntax."
        )));
    }
    if !matches!(
        coverage.state,
        CoverageState::Complete | CoverageState::Partial
    ) {
        let state = serde_json::to_value(coverage.state)?;
        return Ok(Some(format!(
            "Semantic coverage of {path} is {}; showing syntax.",
            state.as_str().unwrap_or("unusable")
        )));
    }
    if calls.is_empty() {
        return Ok(None);
    }
    let mut stale = false;
    for call in calls {
        match crate::graph_query::call_disposition(Some(semantic), call, source)
            .map_err(engine_error)?
            .selected
        {
            Some((_, proof)) if proof.freshness == sem::Freshness::Fresh => return Ok(None),
            Some(_) => stale = true,
            None => {}
        }
    }
    Ok(Some(if stale {
        format!(
            "Semantic evidence for {path} is stale: it was imported for an earlier version of the file. Showing syntax."
        )
    } else {
        "No semantic binding covers this method's calls; showing syntax.".to_string()
    }))
}

/// One enrichment pass over a view's steps and the callee bodies it expands.
struct Enrichment<'a, 'b> {
    store: &'a Store,
    db: &'a Connection,
    selected: &'a ReadRevision,
    source: &'b mut SnapshotGraphSource<'a>,
    languages: &'b BTreeSet<String>,
    revision: IndexPin,
    show_all: bool,
    participants: &'b mut Vec<crate::behavior::Participant>,
    warnings: &'b mut Vec<String>,
    nodes: Vec<crate::graph_query::GraphNode>,
    edges: Vec<crate::graph_query::GraphEdge>,
    /// Targets whose body has been expanded once, or was refused by a bound.
    expanded: BTreeSet<String>,
    /// The owners on the current expansion path; a target on it is not re-entered.
    stack: Vec<String>,
    /// Shown steps in the view so far, expanded bodies included: the visible bound (#536).
    used: usize,
    /// Every step built so far, hidden ones included: the work bound (#536).
    work: usize,
    /// Truncated by an adapter bound with no typed limit.
    truncated: bool,
    limits: BTreeSet<crate::behavior::SequenceLimit>,
    incidental: usize,
}

/// Sets which of `steps`, measured in `owner`'s body in `document`, are shown, before the
/// visible bound is applied (#536). A standalone call bound to an external declaration is
/// incidental and hidden unless `show_all`; a call with a fresh exact internal target is
/// never hidden by a spelling heuristic (JavaScript `console.log`), nor is its body; a
/// chain whose calls are all hidden is hidden. A call that may return early (#542) is
/// control flow, so it is never incidental. Returns whether any call has a resolved target.
fn hide_incidental(
    source: &mut SnapshotGraphSource<'_>,
    languages: &BTreeSet<String>,
    show_all: bool,
    steps: &mut [crate::behavior::SequenceStep],
    document: &DocumentFacts,
    owner: &str,
) -> Result<bool> {
    use crate::graph_query::{self as engine, BoundaryReason};
    let semantic = engine::semantic(Some(languages), document).map_err(engine_error)?;
    let mut targets = false;
    for step in steps {
        source.check_work()?;
        targets |= hide_incidental(
            source,
            languages,
            show_all,
            &mut step.children,
            document,
            owner,
        )?;
        targets |= hide_incidental(
            source,
            languages,
            show_all,
            &mut step.alternate,
            document,
            owner,
        )?;
        if step.kind == "group"
            && !step.children.is_empty()
            && step.children.iter().all(|child| child.hidden)
        {
            step.hidden = true;
        }
        let Some(call) = step
            .call_id
            .as_deref()
            .and_then(|id| document.calls.get(owner)?.iter().find(|call| call.id == id))
        else {
            continue;
        };
        let reason = engine::call_disposition(semantic, call, source)
            .map_err(engine_error)?
            .boundary_reason;
        if reason == BoundaryReason::None {
            step.hidden = false;
            targets = true;
        } else if !show_all && is_incidental(step, reason) {
            step.hidden = true;
        }
    }
    Ok(targets)
}

/// Only a standalone call is incidental: one that already nests steps (an expanded body or
/// a chain) keeps them visible. A measured call the syntax adapter drew as a boundary (its
/// source-hint lanes were full) is still a call here: those hint lanes are not drawn.
fn is_incidental(
    step: &crate::behavior::SequenceStep,
    reason: crate::graph_query::BoundaryReason,
) -> bool {
    reason == crate::graph_query::BoundaryReason::External
        && matches!(step.kind.as_str(), "call" | "boundary")
        && step.children.is_empty()
        && step.alternate.is_empty()
        && step.folded.is_empty()
}

impl Enrichment<'_, '_> {
    fn warn(&mut self, message: String) {
        if !self.warnings.contains(&message) {
            self.warnings.push(message);
        }
    }

    /// Enriches `steps`, measured in `owner`'s body in `document`, at tree `depth`.
    fn steps(
        &mut self,
        steps: &mut [crate::behavior::SequenceStep],
        document: &DocumentFacts,
        owner: &str,
        depth: usize,
    ) -> Result<()> {
        use crate::graph_query::{self as engine, BoundaryReason, GraphEdge, Visit};
        let semantic = engine::semantic(Some(self.languages), document).map_err(engine_error)?;
        for step in steps {
            self.source.check_work()?;
            self.steps(&mut step.children, document, owner, depth + 1)?;
            self.steps(&mut step.alternate, document, owner, depth + 1)?;
            let Some(call) = step
                .call_id
                .as_deref()
                .and_then(|id| document.calls.get(owner)?.iter().find(|call| call.id == id))
            else {
                continue;
            };
            let disposition =
                engine::call_disposition(semantic, call, self.source).map_err(engine_error)?;
            let mut reason = disposition.boundary_reason;
            if reason == BoundaryReason::None {
                let (id, key) = disposition.exact_target.expect("eligible target is exact");
                if !self.participants.iter().any(|p| &p.id == id) {
                    if self.participants.len() == 20 {
                        reason = BoundaryReason::NodeLimit;
                        self.limits
                            .insert(crate::behavior::SequenceLimit::Participants);
                    } else {
                        self.participant(id, key)?;
                    }
                }
                if reason == BoundaryReason::None {
                    step.target = Some(id.clone());
                }
            }
            let selected = disposition.selected;
            step.call_evidence = Some(crate::behavior::CallEvidence {
                boundary_reason: reason,
                resolution: selected.map(|(binding, _)| binding.resolution),
                dispatch: selected.map(|(binding, _)| binding.dispatch),
                freshness: selected.map(|(_, proof)| proof.freshness),
                provenance_id: selected.map(|(binding, _)| binding.provenance_id.clone()),
                external_package: selected
                    .filter(|_| reason == BoundaryReason::External)
                    .and_then(|(binding, _)| match &binding.declared_target {
                        Some(sem::Target::External { symbol }) => {
                            crate::behavior::ExternalPackage::from_scip(&symbol.symbol)
                        }
                        _ => None,
                    }),
                semantic_basis: selected.and_then(|(_, proof)| proof.basis.clone()),
            });
            self.edges.push(GraphEdge {
                call_site_count: 1,
                call: call.clone(),
                from: call.owner_syntax_id.clone(),
                to: step.target.clone(),
                binding: selected.map(|(binding, _)| binding.clone()),
                visit: if step.target.is_some() {
                    Visit::New
                } else {
                    Visit::Boundary
                },
                boundary_reason: reason,
            });
            // `hide_incidental` already hid it unless `show_all`.
            if is_incidental(step, reason) {
                self.incidental += 1;
            }
            if let Some(target) = step.target.clone() {
                self.expand(step, &target, depth)?;
            }
        }
        Ok(())
    }

    /// Adds the exact target `id` in `key` as a participant lane.
    fn participant(&mut self, id: &str, key: &crate::native_evidence::DocumentKey) -> Result<()> {
        let target = self
            .store
            .selected_graph_document_for(self.db, &key.path, self.selected)?
            .nodes
            .into_iter()
            .find(|node| node.id == id)
            .ok_or_else(|| SelectedIntegrity("selected sequence target missing".into()))?;
        let declaration = self
            .source
            .document(key)?
            .declarations
            .get(id)
            .cloned()
            .ok_or_else(|| SelectedIntegrity("selected sequence target missing".into()))?;
        self.nodes.push(crate::graph_query::GraphNode {
            declaration,
            // The expansion level of the call that first reached it.
            depth: self.stack.len() as u64,
        });
        self.participants.push(crate::behavior::Participant {
            id: id.to_owned(),
            label: target.name.clone(),
            kind: serde_json::to_value(target.kind)?
                .as_str()
                .ok_or_else(|| SelectedIntegrity("invalid sequence target kind".into()))?
                .into(),
            identification: "Fresh exact semantic call target in the selected snapshot.".into(),
            declaration: Some(target),
        });
        Ok(())
    }

    /// Expands the measured body of `target` beneath `step`, once per target, when it fits
    /// the visible step, work and depth bounds and is not already on the expansion path.
    /// The body's incidental calls are hidden first, so they use no visible room (#536).
    fn expand(
        &mut self,
        step: &mut crate::behavior::SequenceStep,
        target: &str,
        depth: usize,
    ) -> Result<()> {
        use crate::behavior::{
            MAX_STEP_DEPTH, MAX_STEPS, MAX_WORK_STEPS, SequenceLimit, step_count, tree_depth,
            visible_count,
        };
        if self.stack.iter().any(|owner| owner == target) || !self.expanded.insert(target.into()) {
            return Ok(());
        }
        let Some(symbol) = self
            .participants
            .iter()
            .find(|p| p.id == target)
            .and_then(|p| p.declaration.clone())
            .filter(|symbol| matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method))
        else {
            return Ok(());
        };
        let document =
            self.store
                .selected_graph_document_for(self.db, &symbol.path, self.selected)?;
        let mut calls = document.graph_calls;
        calls.sort_by(|a, b| (a.range.start_byte, &a.id).cmp(&(b.range.start_byte, &b.id)));
        let mut body = crate::behavior::build_sequence_work(
            self.revision,
            &symbol,
            &document.file,
            &calls,
            self.show_all,
        )?;
        if body.steps.is_empty() {
            return Ok(());
        }
        let work = step_count(&body.steps);
        if self.work + work > MAX_WORK_STEPS {
            self.limits.insert(SequenceLimit::WorkSteps);
            self.warn(format!(
                "Sequence work limit reached; the body of {} is not expanded.",
                symbol.name
            ));
            return Ok(());
        }
        let key = self
            .source
            .locate(target)?
            .ok_or_else(|| SelectedIntegrity("selected sequence target missing".into()))?;
        let callee = self.source.document(&key)?;
        hide_incidental(
            self.source,
            self.languages,
            self.show_all,
            &mut body.steps,
            &callee,
            target,
        )?;
        let shown = visible_count(&body.steps);
        if self.used + shown > MAX_STEPS {
            self.limits.insert(SequenceLimit::VisibleSteps);
            self.warn(format!(
                "Sequence step limit reached; the body of {} is not expanded.",
                symbol.name
            ));
            return Ok(());
        }
        if depth + 1 + tree_depth(&body.steps) >= MAX_STEP_DEPTH {
            self.limits.insert(SequenceLimit::Depth);
            self.warn(format!(
                "Sequence depth limit reached; the body of {} is not expanded.",
                symbol.name
            ));
            return Ok(());
        }
        self.used += shown;
        self.work += work;
        fn relabel(steps: &mut [crate::behavior::SequenceStep], prefix: &str, lane: &str) {
            for step in steps {
                step.id = format!("{prefix}>{}", step.id);
                step.lane = Some(lane.to_owned());
                relabel(&mut step.children, prefix, lane);
                relabel(&mut step.alternate, prefix, lane);
            }
        }
        relabel(&mut body.steps, &step.id, target);
        for warning in std::mem::take(&mut body.warnings) {
            self.warn(warning);
        }
        self.truncated |= body.truncated;
        self.limits.extend(body.truncated_by);
        self.stack.push(target.to_owned());
        self.steps(&mut body.steps, &callee, target, depth + 1)?;
        self.stack.pop();
        step.children = body.steps;
        Ok(())
    }
}

impl EvidenceResponse {
    /// Answers one rooted call-graph query over the admitted head. `check` runs during
    /// node and local-call inspection and aborts the query, with no answer, when it fails.
    pub fn graph_query(
        &self,
        query: &GraphQuery,
        expected: Option<IndexPin>,
        check: &mut dyn FnMut() -> Result<()>,
    ) -> Result<GraphAnswer> {
        let selected = self.selected(None)?;
        ensure!(
            expected.is_none_or(|pin| pin == selected.pin),
            "revision conflict: expected basis differs from admitted head"
        );
        let (source_set_id, revision_id): (String, String) = self
            .db
            .query_row(
                "SELECT source_set_id,native_revision_id FROM native_revisions WHERE id=?1",
                [&selected.key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| SelectedIntegrity("call graph native revision row missing".into()))?;
        let requested = GraphRequest {
            source_set_id: source_set_id.clone(),
            revision_id: revision_id.clone(),
            root_syntax_id: query.root_syntax_id.clone(),
            semantic_producer_id: query.semantic_producer_id.clone(),
            depth: query.depth,
            max_nodes: query.max_nodes,
            max_calls: query.max_calls,
            include_external: query.include_external,
            include_tests: query.include_tests,
        };
        let snapshot = GraphSnapshot {
            source_set_id,
            revision_id,
        };
        let mut source = SnapshotGraphSource {
            db: &self.db,
            selected: &selected,
            producer: query.semantic_producer_id.clone(),
            snapshot: snapshot.clone(),
            scopes: BTreeMap::new(),
            declaration_roles: BTreeMap::new(),
            facts: GraphFacts::memo(snapshot),
            check,
        };
        let outcome = evaluate(&requested, &mut source).map_err(engine_error)?;
        Ok(GraphAnswer {
            basis: selected.pin,
            requested,
            outcome,
            facts: source.facts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declaration_role_reads_only_the_pinned_row_and_rejects_corruption() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE native_version_declarations(vid INTEGER,syntax_id BLOB,role TEXT);",
        )
        .unwrap();
        let id = format!("sid:v1:{}", "1".repeat(32));
        let encoded = id_codec::encode_sid(&id).unwrap();
        db.execute(
            "INSERT INTO native_version_declarations VALUES(1,?1,'production'),(2,?1,'test')",
            [&encoded],
        )
        .unwrap();
        let key = DocumentKey {
            source_set_id: "source-set:v1:test".into(),
            language: "rust".into(),
            path: "target.rs".into(),
        };
        let selected = ReadRevision::new(
            IndexPin {
                index_generation: uuid::Uuid::nil(),
                index_revision: 1,
            },
            "selected".into(),
        );
        let snapshot = GraphSnapshot {
            source_set_id: key.source_set_id.clone(),
            revision_id: "revision".into(),
        };
        let scope = V8NativeScope {
            revision_key: selected.key.clone(),
            version_id: "version".into(),
            vid: 2,
            key: key.clone(),
            native_revision_id: snapshot.revision_id.clone(),
            producer_id: "native".into(),
        };
        let mut check = || Ok(());
        let mut source = SnapshotGraphSource {
            db: &db,
            selected: &selected,
            producer: None,
            snapshot: snapshot.clone(),
            scopes: BTreeMap::from([(key.clone(), scope)]),
            declaration_roles: BTreeMap::new(),
            facts: GraphFacts::memo(snapshot),
            check: &mut check,
        };
        assert_eq!(
            source.declaration_role(&key, &id).unwrap(),
            crate::native_evidence::DeclarationRole::Test
        );
        assert!(source.facts.documents.is_empty());
        source.declaration_roles.clear();
        db.execute(
            "UPDATE native_version_declarations SET role='invalid' WHERE vid=2",
            [],
        )
        .unwrap();
        let invalid = source.declaration_role(&key, &id).unwrap_err();
        assert!(invalid.downcast_ref::<SelectedIntegrity>().is_some());
        db.execute("DELETE FROM native_version_declarations WHERE vid=2", [])
            .unwrap();
        let missing = source.declaration_role(&key, &id).unwrap_err();
        assert!(missing.downcast_ref::<SelectedIntegrity>().is_some());
    }

    #[test]
    fn graph_integrity_maps_to_selected_integrity() {
        let mapped = engine_error(GraphIntegrity("corrupt".into()).into());
        let integrity = mapped.downcast::<SelectedIntegrity>().unwrap();
        assert_eq!(integrity.0, "corrupt");
        let other = engine_error(anyhow::anyhow!("busy"));
        assert!(other.downcast_ref::<SelectedIntegrity>().is_none());
        assert_eq!(other.to_string(), "busy");
    }
}
