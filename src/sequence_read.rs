//! The Sequence read: one seed's document from the selected snapshot, the syntax view built
//! by `behavior`, then enrichment by `graph_read`.
//!
//! Moved up from `store.rs` (S6, #1048). The methods are still `impl Store` and
//! `impl EvidenceResponse` blocks, so they run in the same read transaction and behind the
//! same fence as before. This file contains SQL until the read API is narrowed (S14).
use crate::graph_read;
use crate::model::*;
use crate::store::{EvidenceResponse, SelectedIntegrity, Store, id_codec};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};

impl EvidenceResponse {
    pub fn sequence_at(
        &self,
        seed: &str,
        expected: IndexPin,
        show_all: bool,
    ) -> Result<Option<crate::behavior::SequenceView>> {
        self.store
            .sequence_in(&self.db, seed, expected, show_all, false, true)
    }
    /// The candidates of the browser's Interactions view (#663): see [`Store::sequence_interactions_at`].
    pub fn sequence_interactions_at(
        &self,
        seed: &str,
        expected: IndexPin,
    ) -> Result<Option<crate::behavior::SequenceView>> {
        self.store
            .sequence_in(&self.db, seed, expected, false, true, true)
    }
}

impl Store {
    /// Only cached source and measured calls from the same snapshot are used.
    pub fn sequence_at(
        &self,
        seed: &str,
        expected: IndexPin,
        show_all: bool,
    ) -> Result<Option<crate::behavior::SequenceView>> {
        self.with_evidence(|tx| self.sequence_in(tx, seed, expected, show_all, false, false))
    }
    /// The candidates of the browser's Interactions view (#663): an enriched view with a
    /// shown call is kept within the work, depth and participant bounds but not yet the
    /// visible step bound, which counts internal steps Interactions never draw. The browser
    /// bounds the arrows it draws after filtering. Any other view is [`Self::sequence_at`].
    pub fn sequence_interactions_at(
        &self,
        seed: &str,
        expected: IndexPin,
    ) -> Result<Option<crate::behavior::SequenceView>> {
        self.with_evidence(|tx| self.sequence_in(tx, seed, expected, false, true, false))
    }
    fn sequence_in(
        &self,
        tx: &Connection,
        seed: &str,
        expected: IndexPin,
        show_all: bool,
        interactions: bool,
        pre_startup: bool,
    ) -> Result<Option<crate::behavior::SequenceView>> {
        let selected_revision = self.read_revision_for(tx, Some(expected), pre_startup)?;
        let revision = selected_revision.pin;
        let path: Option<String> = tx
            .query_row(
                "SELECT m.path FROM graph_nodes n JOIN revision_documents m
             ON m.revision_id=?2 AND m.gid=n.gid WHERE n.id=?1",
                params![id_codec::encode_sid(seed), selected_revision.key],
                |r| r.get(0),
            )
            .optional()?;
        let Some(path) = path else { return Ok(None) };
        let document = self.selected_graph_document_for(tx, &path, &selected_revision)?;
        let symbol = document
            .nodes
            .iter()
            .find(|node| node.id == seed)
            .ok_or_else(|| {
                SelectedIntegrity("incompatible_index: selected sequence seed missing".into())
            })?;
        ensure!(
            matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method),
            "invalid sequence symbol kind"
        );
        let mut calls = document.graph_calls;
        calls.sort_by(|a, b| (a.range.start_byte, &a.id).cmp(&(b.range.start_byte, &b.id)));
        let file = document.file;
        // Enrichment applies the visible step bound once incidental calls are hidden (#536).
        let mut view =
            crate::behavior::build_sequence_work(revision, symbol, &file, &calls, show_all)?;
        graph_read::sequence_calls(
            self,
            tx,
            &selected_revision,
            &mut view,
            show_all,
            interactions,
        )?;
        // #888: a syntax-only view's callbacks can hold only a hop, decided by the read.
        if view.semantic.tier == crate::behavior::EvidenceTier::Syntax {
            graph_read::syntax_callbacks(self, tx, &selected_revision, &mut view, show_all)?;
        }
        Ok(Some(view))
    }
}
