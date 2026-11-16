//! The navigation read: the selected document a navigation request names, read and checked
//! in one snapshot, then projected by `navigation`.
//!
//! Moved up from `store.rs` (S6, #1048). The methods are still `impl Store` and
//! `impl EvidenceResponse` blocks, so they run in the same read transaction and behind the
//! same fence as before. This file contains SQL until the read API is narrowed (S14).
use crate::graph_read;
use crate::store::{
    EvidenceResponse, SelectedIntegrity, Store, id_codec, selected_ensure, semantic_display,
};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

impl EvidenceResponse {
    pub fn navigation_at(
        &self,
        request: &crate::navigation::NavigationRequest,
    ) -> Result<crate::navigation::NavigationResult> {
        self.store.navigation_in(&self.db, request, true)
    }
}

impl Store {
    /// Search only the persisted projection. Wildcards are literal user text.
    /// One read snapshot and revision guard; no source reads or catalog rebuilds.
    pub fn navigation_at(
        &self,
        request: &crate::navigation::NavigationRequest,
    ) -> Result<crate::navigation::NavigationResult> {
        self.with_evidence(|tx| self.navigation_in(tx, request, false))
    }
    fn navigation_in(
        &self,
        tx: &Connection,
        request: &crate::navigation::NavigationRequest,
        pre_startup: bool,
    ) -> Result<crate::navigation::NavigationResult> {
        request.validate()?;
        let selected =
            self.read_revision_for(tx, Some(request.expected_revision()), pre_startup)?;
        let revision = selected.pin;
        // Policy selection is local to this exact selected snapshot, never the head.
        // The policy selects the source document language's sole covering producer (#483).
        let mut producer_bytes = Vec::new();
        let mut multiple_producers = None;
        let effective_request = match request {
            crate::navigation::NavigationRequest::Source(selector)
                if selector.semantic_producer_policy.is_some() =>
            {
                let language: Option<String> = tx
                    .query_row(
                        "SELECT language FROM revision_documents WHERE revision_id=?1 AND path=?2 ORDER BY language LIMIT 1",
                        params![selected.key, selector.path],
                        |row| row.get(0),
                    )
                    .optional()?;
                let mut ids = Vec::new();
                if let Some(language) = &language {
                    let selection = semantic_display::select_producers(tx, &selected.key, None)?;
                    for producer in selection.covering(language).iter().take(2) {
                        let bytes = serde_json::to_vec(producer)?.len();
                        selected_ensure!(
                            bytes <= crate::navigation::RECORD_BYTES as usize,
                            "selected semantic producer exceeds navigation budget"
                        );
                        producer_bytes.push(bytes);
                        ids.push(producer.clone());
                    }
                }
                if ids.len() > 1 {
                    multiple_producers = language
                        .as_deref()
                        .map(crate::graph_query::multiple_producers_note);
                }
                let mut selector = selector.clone();
                selector.semantic_producer_policy = None;
                selector.semantic_producer_id = if ids.len() == 1 { ids.pop() } else { None };
                Some(crate::navigation::NavigationRequest::Source(selector))
            }
            _ => None,
        };
        let request = effective_request.as_ref().unwrap_or(request);
        // Navigation's source selector counts lines from the stored source BLOB,
        // and its member selector reads versioned class/node rows. Before either
        // consumes a selected document, authenticate it against that source BLOB
        // in this same read transaction. Never scan the entire workspace here.
        let selected_path: Option<String> = match request {
            crate::navigation::NavigationRequest::Source(s) => Some(s.path.clone()),
            crate::navigation::NavigationRequest::Member(s) => {
                // The member's measured declaration lives in the member's own
                // document, which a Rust impl merge (#452) can make another file
                // than the class's. Read it from the one recorded member row.
                let paths: Vec<String> = tx
                    .prepare(
                        "SELECT DISTINCT json_extract(m.value,'$.path') FROM classes c
                         JOIN revision_documents d ON d.revision_id=?5 AND d.cid=c.cid,
                         json_each(c.payload) a, json_each(a.value) m
                         WHERE c.id=?1 AND a.key IN ('fields','methods')
                         AND json_extract(m.value,'$.name')=?2
                         AND json_extract(m.value,'$.range.startByte')=?3 AND json_extract(m.value,'$.range.endByte')=?4
                         AND (?6 IS NULL OR json_extract(m.value,'$.path')=?6) LIMIT 2",
                    )?
                    .query_map(
                        params![
                            id_codec::encode_sid(&s.class_id),
                            s.member_name,
                            s.start_byte as i64,
                            s.end_byte as i64,
                            selected.key,
                            s.member_path
                        ],
                        |row| row.get::<_, Option<String>>(0),
                    )?
                    .filter_map(|row| row.transpose())
                    .collect::<rusqlite::Result<_>>()?;
                if let [path] = paths.as_slice() {
                    Some(path.clone())
                } else {
                    // Locate the class document by identity, not the
                    // materialized kind; `member` then refuses an absent or
                    // ambiguous selector.
                    tx.query_row(
                        "SELECT m.path FROM graph_nodes n JOIN revision_documents m ON m.revision_id=?2 AND m.gid=n.gid WHERE n.id=?1 LIMIT 1",
                        params![id_codec::encode_sid(&s.class_id), selected.key],
                        |row| row.get(0),
                    )
                    .optional()?
                }
            }
        };
        let mut selected_document = None;
        if let Some(path) = selected_path {
            // Gate allocation of the selected JSON/BLOB before decoding either.
            // This reads only SQLite byte lengths, not every workspace document.
            let sizes: Option<(i64, i64)> = tx
                .query_row(
                    "SELECT length(d.source_bytes),length(d.source_bytes) FROM revision_documents m
                 JOIN document_versions d ON d.vid=m.vid
                 WHERE m.revision_id=?2 AND m.path=?1",
                    params![path, selected.key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((graph_len, native_len)) = sizes {
                if graph_len > 2 * 1024 * 1024 || native_len > 2 * 1024 * 1024 {
                    return Err(self.report_index_failure(
                        SelectedIntegrity(
                            "incompatible_index: selected navigation source exceeds budget".into(),
                        )
                        .into(),
                    ));
                }
                selected_document = Some(self.selected_graph_document_for(tx, &path, &selected)?);
            } else {
                let graph_file: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM revision_documents WHERE revision_id=?2 AND path=?1)",
                    params![path, selected.key],
                    |row| row.get(0),
                )?;
                if graph_file {
                    return Err(self.report_index_failure(
                        SelectedIntegrity(
                            "incompatible_index: selected native document missing".into(),
                        )
                        .into(),
                    ));
                }
            }
        }
        crate::navigation::navigate(
            tx,
            request,
            revision,
            &selected.key,
            selected_document
                .as_ref()
                .map(|document| document.nodes.as_slice()),
            &mut |reader, selector| {
                if let Some(note) = &multiple_producers {
                    reader.warn(note);
                }
                for bytes in &producer_bytes {
                    if !reader.charge(*bytes) {
                        return Ok(());
                    }
                }
                graph_read::navigation_calls(
                    self,
                    tx,
                    &selected,
                    selected_document.as_ref(),
                    selector,
                    reader,
                )
            },
        )
    }
}
