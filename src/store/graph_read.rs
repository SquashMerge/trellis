//! Rooted call-graph reads over the one admitted selected SQLite snapshot.
//!
//! The pure engine in `graph_query` asks a [`SnapshotGraphSource`] for facts; every fact it
//! reads is recorded so a byte-reduced answer can be re-evaluated without SQL (#40).
use super::*;
use crate::{
    graph_query::{
        DocumentFacts, GraphAnswer, GraphFacts, GraphIntegrity, GraphQuery, GraphRequest,
        GraphSnapshot, GraphSource, HistoryFacts, SemanticDocument, evaluate,
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
    producer: Option<String>,
    snapshot: GraphSnapshot,
    scopes: BTreeMap<DocumentKey, V8NativeScope>,
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
        let in_scope = match &self.producer {
            Some(producer) => self
                .facts
                .producers
                .get(producer)
                .is_some_and(|languages| languages.contains(&key.language)),
            None => false,
        };
        let semantic = match (&self.producer, in_scope) {
            (Some(producer), true) => self.read_semantic(producer, key)?,
            _ => None,
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
            .producer
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("call graph history requires a producer"))?;
        let history = read_semantic_history(
            self.db,
            &self.selected.key,
            self.selected.pin.index_revision,
            producer,
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
}

impl EvidenceResponse {
    /// Answers one rooted call-graph query over the admitted head. `check` runs once per
    /// dequeued node and aborts the query, with no answer, when it fails.
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
    fn graph_integrity_maps_to_selected_integrity() {
        let mapped = engine_error(GraphIntegrity("corrupt".into()).into());
        let integrity = mapped.downcast::<SelectedIntegrity>().unwrap();
        assert_eq!(integrity.0, "corrupt");
        let other = engine_error(anyhow::anyhow!("busy"));
        assert!(other.downcast_ref::<SelectedIntegrity>().is_none());
        assert_eq!(other.to_string(), "busy");
    }
}
