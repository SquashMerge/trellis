//! The call-graph view read: one seed's document from the selected snapshot, then the
//! bounded walk and its warnings.
//!
//! Moved up from `store.rs` (S6, #1048). The methods are still `impl Store` and
//! `impl EvidenceResponse` blocks, so they run in the same read transaction and behind the
//! same fence as before. This file contains SQL until the read API is narrowed (S14).
use crate::model::*;
use crate::store::{EvidenceResponse, SelectedIntegrity, Store, id_codec, semantic_display};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

impl EvidenceResponse {
    pub fn query_view(&self, query: &ViewQuery) -> Result<Option<ViewResult>> {
        self.query_view_at(query, None)
    }
    pub fn query_view_at(
        &self,
        query: &ViewQuery,
        expected: Option<&IndexPin>,
    ) -> Result<Option<ViewResult>> {
        query.validate()?;
        self.store
            .query_view_in_for(&self.db, query, expected, true)
    }
}

impl Store {
    pub fn query_view(&self, query: &ViewQuery) -> Result<Option<ViewResult>> {
        self.query_view_at(query, None)
    }

    pub fn query_view_at(
        &self,
        query: &ViewQuery,
        expected_pin: Option<&IndexPin>,
    ) -> Result<Option<ViewResult>> {
        query.validate()?;
        self.with_evidence(|tx| self.query_view_in(tx, query, expected_pin))
    }
    fn query_view_in(
        &self,
        tx: &Connection,
        query: &ViewQuery,
        expected_pin: Option<&IndexPin>,
    ) -> Result<Option<ViewResult>> {
        self.query_view_in_for(tx, query, expected_pin, false)
    }
    fn query_view_in_for(
        &self,
        tx: &Connection,
        query: &ViewQuery,
        expected_pin: Option<&IndexPin>,
        pre_startup: bool,
    ) -> Result<Option<ViewResult>> {
        let selected = self.read_revision_for(tx, expected_pin.copied(), pre_startup)?;
        let revision = selected.pin;
        let selected_path: Option<String> = tx
            .query_row("SELECT m.path FROM graph_nodes n JOIN revision_documents m ON m.revision_id=?2 AND m.gid=n.gid WHERE n.id=?1", params![id_codec::encode_sid(&query.seed), selected.key], |r| {
                r.get(0)
            })
            .optional()?;
        let Some(path) = selected_path else {
            return Ok(None);
        };
        let seed_document = self.selected_graph_document_for(tx, &path, &selected)?;
        let seed = seed_document
            .nodes
            .iter()
            .find(|symbol| symbol.id == query.seed)
            .cloned()
            .ok_or_else(|| {
                SelectedIntegrity("incompatible_index: selected view seed missing".into())
            })?;
        let walk = semantic_display::view_walk(self, tx, &selected, query, seed_document, seed)?;
        let mut warnings = Vec::new();
        if walk.truncated {
            warnings.push("Measured calls truncated at the request limit.".into());
        }
        if walk.omitted_nodes > 0 {
            warnings.push("Node limit reached; additional targets are not shown.".into());
        }
        Ok(Some(ViewResult {
            revision,
            query: query.clone(),
            nodes: walk.nodes,
            calls: walk.calls,
            regions: walk.regions,
            truncated: walk.truncated || walk.omitted_nodes > 0,
            omitted_nodes: walk.omitted_nodes,
            warnings,
            semantic: walk.semantic,
        }))
    }
}
