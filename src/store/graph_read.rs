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
pub(super) fn sequence_calls(
    store: &Store,
    db: &Connection,
    selected: &ReadRevision,
    view: &mut crate::behavior::SequenceView,
) -> Result<()> {
    use crate::graph_query::{self as engine, BoundaryReason, GraphEdge, GraphNode, Visit};
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
        return Ok(());
    };
    let languages = Some(defaults.languages());
    let document = source.document(&key)?;
    let declaration = document
        .declarations
        .get(&view.seed.id)
        .ok_or_else(|| SelectedIntegrity("selected sequence declaration missing".into()))?
        .clone();
    let Some(semantic) = engine::semantic(languages.as_ref(), &document).map_err(engine_error)?
    else {
        return Ok(());
    };
    // Syntax receiver groups are display hints, not resolved participant lanes.
    view.participants
        .retain(|participant| participant.id == view.seed.id);
    let calls: BTreeMap<_, _> = document
        .calls
        .get(&view.seed.id)
        .into_iter()
        .flatten()
        .map(|call| (call.id.as_str(), call))
        .collect();
    if let Some(participant) = view.participants.iter_mut().find(|p| p.id == view.seed.id) {
        participant.declaration = Some(view.seed.clone());
    }
    struct Enrichment<'a, 'b> {
        store: &'a Store,
        db: &'a Connection,
        selected: &'a ReadRevision,
        source: &'b mut dyn GraphSource,
        semantic: Option<&'a SemanticDocument>,
        calls: &'a BTreeMap<&'a str, &'a crate::native_evidence::Call>,
        participants: &'b mut Vec<crate::behavior::Participant>,
        edges: Vec<GraphEdge>,
        limited: bool,
    }
    impl Enrichment<'_, '_> {
        fn steps(&mut self, steps: &mut [crate::behavior::SequenceStep]) -> Result<()> {
            for step in steps {
                self.source.check_work()?;
                if let Some(call) = step.call_id.as_deref().and_then(|id| self.calls.get(id)) {
                    let disposition = engine::call_disposition(self.semantic, call, self.source)
                        .map_err(engine_error)?;
                    let mut reason = disposition.boundary_reason;
                    if reason == BoundaryReason::None {
                        let (id, key) = disposition.exact_target.expect("eligible target is exact");
                        if !self.participants.iter().any(|p| &p.id == id) {
                            if self.participants.len() == 20 {
                                reason = BoundaryReason::NodeLimit;
                                self.limited = true;
                            } else {
                                let target = self
                                    .store
                                    .selected_graph_document_for(self.db, &key.path, self.selected)?
                                    .nodes
                                    .into_iter()
                                    .find(|node| &node.id == id)
                                    .ok_or_else(|| {
                                        SelectedIntegrity("selected sequence target missing".into())
                                    })?;
                                self.participants.push(crate::behavior::Participant {
                                    id: id.clone(),
                                    label: target.name.clone(),
                                    kind: serde_json::to_value(target.kind)?
                                        .as_str()
                                        .ok_or_else(|| {
                                            SelectedIntegrity("invalid sequence target kind".into())
                                        })?
                                        .into(),
                                    identification:
                                        "Fresh exact semantic call target in the selected snapshot."
                                            .into(),
                                    declaration: Some(target),
                                });
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
                    });
                    self.edges.push(GraphEdge {
                        call_site_count: 1,
                        call: (*call).clone(),
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
                }
                self.steps(&mut step.children)?;
                self.steps(&mut step.alternate)?;
            }
            Ok(())
        }
    }
    let mut enrichment = Enrichment {
        store,
        db,
        selected,
        source: &mut source,
        semantic: Some(semantic),
        calls: &calls,
        participants: &mut view.participants,
        edges: vec![],
        limited: false,
    };
    enrichment.steps(&mut view.steps)?;
    let limited = enrichment.limited;
    let edges = enrichment.edges;
    if limited {
        view.truncated = true;
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
    let mut nodes = vec![GraphNode {
        declaration,
        depth: 0,
    }];
    for participant in &view.participants {
        if participant.declaration.is_none() || participant.id == view.seed.id {
            continue;
        }
        let key = source
            .locate(&participant.id)?
            .ok_or_else(|| SelectedIntegrity("selected sequence target missing".into()))?;
        let target = source
            .document(&key)?
            .declarations
            .get(&participant.id)
            .cloned()
            .ok_or_else(|| SelectedIntegrity("selected sequence target missing".into()))?;
        nodes.push(GraphNode {
            declaration: target,
            depth: 1,
        });
    }
    view.semantic_warnings =
        engine::occurrence_warnings(&request, &mut source, languages.as_ref(), &nodes, &edges)
            .map_err(engine_error)?;
    view.warnings.extend(
        view.semantic_warnings
            .iter()
            .map(|warning| warning.message.clone()),
    );
    Ok(())
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
