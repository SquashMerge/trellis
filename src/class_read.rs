//! The class reads: the class catalog page and the class diagram, each read from the selected
//! snapshot and projected by `class_diagram`, with the presentation helpers that resolve a
//! seed to its class and clip one class to the presentation byte limit.
//!
//! Moved up from `store.rs` (S7, #1052). The methods are still `impl Store` and
//! `impl EvidenceResponse` blocks, so they run in the same read transaction and behind the
//! same fence as before. `class_metadata` stays in storage. This file contains SQL until the
//! read API is narrowed (S14).
use crate::model::*;
use crate::store::{
    EvidenceResponse, ReadRevision, SelectedDocument, SelectedIntegrity, Store, class_metadata,
    id_codec, stored_id_row,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

impl EvidenceResponse {
    pub fn classes_at(
        &self,
        path: Option<&str>,
        query: &str,
        expected: Option<IndexPin>,
        offset: usize,
        limit: usize,
    ) -> Result<crate::class_diagram::ClassPage> {
        self.store
            .classes_in(&self.db, path, query, expected, (offset, limit), true)
    }
    pub fn class_diagram_at(
        &self,
        request: &crate::class_diagram::ClassDiagramRequest,
    ) -> Result<crate::class_diagram::ClassDiagram> {
        self.store.class_diagram_in(&self.db, request, true)
    }
}

impl Store {
    pub fn classes_at(
        &self,
        path: Option<&str>,
        query: &str,
        expected: Option<IndexPin>,
        offset: usize,
        limit: usize,
    ) -> Result<crate::class_diagram::ClassPage> {
        self.with_evidence(|tx| self.classes_in(tx, path, query, expected, (offset, limit), false))
    }
    fn classes_in(
        &self,
        tx: &Connection,
        path: Option<&str>,
        query: &str,
        expected: Option<IndexPin>,
        pagination: (usize, usize),
        pre_startup: bool,
    ) -> Result<crate::class_diagram::ClassPage> {
        use crate::class_diagram::ClassPage;
        let (offset, limit) = pagination;
        use crate::class_diagram::InvalidRequest;
        let path = path.filter(|path| !path.is_empty());
        ensure!(
            (1..=100).contains(&limit)
                && offset <= 1_000_000
                && query.len() <= 512
                && !query.contains('\0'),
            InvalidRequest("Class search allows 1–100 results and a query of at most 512 bytes.")
        );
        if let Some(path) = path {
            ensure!(
                !path.is_empty()
                    && path.len() <= 8192
                    && !path.contains(['\0', '\\', ':'])
                    && !path
                        .split('/')
                        .any(|part| part.is_empty() || part == "." || part == ".."),
                InvalidRequest("Choose a workspace-relative class source path.")
            );
        }
        let selected_revision = self.read_revision_for(tx, expected, pre_startup)?;
        let revision = selected_revision.pin;
        let (mut warnings, truncated) = class_metadata(tx, &selected_revision)?;
        let pattern = format!(
            "%{}%",
            query
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let mut stmt = tx.prepare("SELECT c.id FROM classes c JOIN revision_documents m ON m.revision_id=?6 AND m.cid=c.cid WHERE (?1 IS NULL OR c.path=?1)
            AND (c.name LIKE ?2 ESCAPE '\\' OR c.qualified_name LIKE ?2 ESCAPE '\\' OR ('sid:v1:'||lower(hex(c.id)))=?3)
            ORDER BY CASE WHEN lower(c.name)=lower(?3) THEN 0 ELSE 1 END,c.qualified_name,c.path,c.id LIMIT ?4 OFFSET ?5")?;
        let ids = stmt
            .query_map(
                params![
                    path,
                    pattern,
                    query,
                    (limit + 1) as i64,
                    offset as i64,
                    selected_revision.key
                ],
                |r| stored_id_row(r, 0, false),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut items = Vec::new();
        let mut consumed = 0;
        let mut bytes = 0;
        let mut byte_limited = false;
        for id in ids.iter().take(limit) {
            let Some((class, size, clipped)) =
                presentation_class(self, tx, id, &selected_revision)?
            else {
                // Consume an individually oversized row so pagination always progresses.
                consumed += 1;
                byte_limited = true;
                continue;
            };
            if bytes + size > crate::class_diagram::PAGE_BYTES {
                byte_limited = true;
                break;
            }
            bytes += size;
            consumed += 1;
            byte_limited |= clipped;
            items.push(class);
        }
        let next_offset = (consumed < ids.len()).then_some(offset + consumed);
        if byte_limited {
            warnings.push(crate::class_diagram::BYTE_NOTICE.into());
        }
        let truncated = truncated || byte_limited;
        if let Some(path) = path
            && !crate::capture_records::language_of_path(Path::new(path))
                .is_some_and(crate::classes::supports_language)
        {
            warnings.push(
                "Class diagrams currently support Java, Python, JavaScript, TypeScript and Rust declarations only."
                    .into(),
            );
        }
        Ok(ClassPage {
            revision,
            items,
            next_offset,
            truncated,
            warnings,
            require_index: false,
            supported_languages: crate::classes::SUPPORTED_LANGUAGES
                .iter()
                .map(|language| (*language).to_owned())
                .collect(),
        })
    }

    /// Bounded one-hop relation reads and seed resolution share a revision-pinned
    /// read transaction. No filesystem access or graph/provider augmentation.
    pub fn class_diagram_at(
        &self,
        request: &crate::class_diagram::ClassDiagramRequest,
    ) -> Result<crate::class_diagram::ClassDiagram> {
        request.validate()?;
        self.with_evidence(|tx| self.class_diagram_in(tx, request, false))
    }
    fn class_diagram_in(
        &self,
        tx: &Connection,
        request: &crate::class_diagram::ClassDiagramRequest,
        pre_startup: bool,
    ) -> Result<crate::class_diagram::ClassDiagram> {
        use crate::class_diagram::{self, InvalidRequest};
        let selected = self.read_revision_for(tx, Some(request.expected_revision), pre_startup)?;
        let revision = selected.pin;
        let (warnings, truncated) = class_metadata(tx, &selected)?;
        let (seed, clipped) = resolve_class(self, tx, &request.seed, &selected)?;
        // Explicitly selected measured declarations are independent roots, never
        // connected by lexical type-name matches or candidate relationships.
        let mut seeds = vec![seed.symbol.id.clone()];
        let mut classes = BTreeMap::from([(seed.symbol.id.clone(), seed)]);
        for expanded in &request.expanded {
            let (class, _) = resolve_class(self, tx, expanded, &selected)?;
            if !classes.contains_key(&class.symbol.id) {
                ensure!(
                    classes.len() < class_diagram::MAX_NODES,
                    InvalidRequest("Too many selected classes.")
                );
                seeds.push(class.symbol.id.clone());
                classes.insert(class.symbol.id.clone(), class);
            }
        }
        class_diagram::project(
            revision,
            &seeds,
            &classes,
            vec![],
            vec![],
            warnings,
            truncated || clipped,
        )
    }
}

/// Read at most 64 KiB of class JSON into Rust. Large member arrays are
/// clipped in SQLite, without materializing their full strings in the API.
/// IDs, class metadata, and source ranges remain exact; only member arrays shrink.
fn presentation_class(
    store: &Store,
    db: &Connection,
    id: &str,
    selected: &ReadRevision,
) -> Result<Option<(crate::model::ClassDefinition, usize, bool)>> {
    use crate::class_diagram::CLASS_BYTES;
    let size: Option<i64> = db
        .query_row(
            "SELECT length(CAST(payload AS BLOB)) FROM classes JOIN revision_documents m ON m.revision_id=?2 AND m.cid=classes.cid WHERE classes.id=?1",
            params![id_codec::encode_sid(id), selected.key],
            |r| r.get(0),
        )
        .optional()
        .map_err(|error| store.report_index_failure(error.into()))?;
    let Some(size) = size else {
        return Ok(None);
    };
    let valid: bool = db
        .query_row(
            "SELECT json_valid(payload) FROM classes JOIN revision_documents m ON m.revision_id=?2 AND m.cid=classes.cid WHERE classes.id=?1",
            params![id_codec::encode_sid(id), selected.key],
            |r| r.get(0),
        )
        .map_err(|error| store.report_index_failure(error.into()))?;
    if !valid {
        return Err(store.report_index_failure(
            SelectedIntegrity("incompatible_index: selected class JSON invalid".into()).into(),
        ));
    }
    if size <= CLASS_BYTES as i64 {
        let payload: String = db
            .query_row("SELECT payload FROM classes JOIN revision_documents m ON m.revision_id=?2 AND m.cid=classes.cid WHERE classes.id=?1", params![id_codec::encode_sid(id), selected.key], |r| {
                r.get(0)
            })
            .map_err(|error| store.report_index_failure(error.into()))?;
        let class = serde_json::from_str(&payload)
            .context(SelectedIntegrity(
                "incompatible_index: stored JSON decode failed".into(),
            ))
            .map_err(|error| store.report_index_failure(error))?;
        return Ok(Some((class, payload.len(), false)));
    }
    let clip_shape: bool = db
        .query_row(
            "SELECT json_type(payload,'$.fields')='array'
                AND json_type(payload,'$.methods')='array'
                AND NOT EXISTS(SELECT 1 FROM json_each(payload,'$.fields') WHERE type!='object')
                AND NOT EXISTS(SELECT 1 FROM json_each(payload,'$.methods') WHERE type!='object')
             FROM classes JOIN revision_documents m ON m.revision_id=?2 AND m.cid=classes.cid WHERE classes.id=?1",
            params![id_codec::encode_sid(id), selected.key],
            |r| r.get(0),
        )
        .map_err(|error| store.report_index_failure(error.into()))?;
    if !clip_shape {
        return Err(store.report_index_failure(
            SelectedIntegrity("incompatible_index: selected class member JSON invalid".into())
                .into(),
        ));
    }
    for count in [32, 16, 8, 4, 2, 1, 0] {
        let payload: Option<String> = db.query_row("WITH clipped AS (
            SELECT json_set(payload,
                '$.fields',(SELECT json_group_array(json(value)) FROM json_each(classes.payload,'$.fields') WHERE key < ?2),
                '$.methods',(SELECT json_group_array(json(value)) FROM json_each(classes.payload,'$.methods') WHERE key < ?2),
                '$.truncated',json('true')) AS payload FROM classes JOIN revision_documents m ON m.revision_id=?4 AND m.cid=classes.cid WHERE classes.id=?1)
            SELECT CASE WHEN length(CAST(payload AS BLOB)) <= ?3 THEN payload ELSE NULL END FROM clipped",
            params![id_codec::encode_sid(id), count, CLASS_BYTES as i64, selected.key], |r| r.get(0))
            .map_err(|error| store.report_index_failure(error.into()))?;
        if let Some(payload) = payload {
            let class = serde_json::from_str(&payload)
                .context(SelectedIntegrity(
                    "incompatible_index: stored JSON decode failed".into(),
                ))
                .map_err(|error| store.report_index_failure(error))?;
            return Ok(Some((class, payload.len(), true)));
        }
    }
    // Pathological non-member metadata cannot fit without changing identity.
    Ok(None)
}
fn resolve_class_id(
    store: &Store,
    db: &Connection,
    id: &str,
    selected: &ReadRevision,
) -> Result<String> {
    let lexical = lexical_class_id(store, db, id, selected);
    match &lexical {
        Ok(_) => return lexical,
        // Only a missing class ancestor falls back; storage and integrity
        // failures stay what they are.
        Err(error)
            if error
                .downcast_ref::<crate::class_diagram::InvalidRequest>()
                .is_none() =>
        {
            return lexical;
        }
        Err(_) => {}
    }
    // A Rust method's measured parent is its `impl` block, which is not a class.
    // Composition recorded the method on exactly one class (#452): use it.
    let owners: Vec<Vec<u8>> = db
        .prepare(
            "SELECT DISTINCT c.id FROM classes c JOIN revision_documents m ON m.revision_id=?2 AND m.cid=c.cid,
             json_each(c.payload,'$.methods') j WHERE m.language='rust' AND json_extract(j.value,'$.symbolId')=?1 LIMIT 2",
        )
        .map_err(|error| store.report_index_failure(error.into()))?
        .query_map(params![id, selected.key], |r| r.get(0))
        .map_err(|error| store.report_index_failure(error.into()))?
        .collect::<rusqlite::Result<_>>()
        .map_err(|error| store.report_index_failure(error.into()))?;
    match owners.as_slice() {
        [owner] => id_codec::decode_sid(owner).context(SelectedIntegrity(
            "incompatible_index: stored class id decode failed".into(),
        )),
        _ => lexical,
    }
}
fn lexical_class_id(
    store: &Store,
    db: &Connection,
    id: &str,
    selected: &ReadRevision,
) -> Result<String> {
    use crate::class_diagram::InvalidRequest;
    let mut current = id.to_owned();
    let mut seen = BTreeSet::new();
    let mut document: Option<SelectedDocument> = None;
    for depth in 0..64 {
        ensure!(
            seen.insert(current.clone()),
            InvalidRequest("The selected symbol has a cyclic class ancestry.")
        );
        if depth > 0 {
            ensure!(
                document
                    .as_ref()
                    .is_some_and(|doc| doc.nodes.iter().any(|node| node.id == current)),
                InvalidRequest("Choose an indexed class or method.")
            );
        }
        if db.query_row(
            "SELECT EXISTS(SELECT 1 FROM classes c JOIN revision_documents m ON m.revision_id=?2 AND m.cid=c.cid WHERE c.id=?1)",
            params![id_codec::encode_sid(&current), selected.key],
            |r| r.get::<_, bool>(0),
        )? {
            return Ok(current);
        }
        if document.is_none() {
            let path: Option<String> = db.query_row(
                "SELECT m.path FROM graph_nodes n JOIN revision_documents m ON m.revision_id=?2 AND m.gid=n.gid WHERE n.id=?1",
                params![id_codec::encode_sid(&current), selected.key], |r| r.get(0),
            ).optional().map_err(|error| store.report_index_failure(error.into()))?;
            document = path
                .map(|path| store.selected_graph_document_for(db, &path, selected))
                .transpose()?;
        }
        let symbol = document
            .as_ref()
            .and_then(|doc| doc.nodes.iter().find(|node| node.id == current))
            .ok_or(InvalidRequest("Choose an indexed class or method."))?;
        ensure!(
            depth > 0 || matches!(symbol.kind, SymbolKind::Method | SymbolKind::Function),
            InvalidRequest("Choose an indexed class or method.")
        );
        current = symbol.parent.clone().ok_or(InvalidRequest(
            "The selected method has no supported enclosing class.",
        ))?;
    }
    Err(InvalidRequest("The selected symbol exceeds the class ancestry limit.").into())
}

fn resolve_class(
    store: &Store,
    db: &Connection,
    id: &str,
    selected: &ReadRevision,
) -> Result<(crate::model::ClassDefinition, bool)> {
    let id = resolve_class_id(store, db, id, selected)?;
    let (class, _, clipped) = presentation_class(store, db, &id, selected)?.ok_or(
        crate::class_diagram::InvalidRequest("Class metadata exceeds the presentation byte limit."),
    )?;
    Ok((class, clipped))
}
