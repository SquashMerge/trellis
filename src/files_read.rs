//! The Files reads: the outline of one cached file and the syntax matches of one selected
//! declaration, each read from the selected snapshot and projected by `files_view`.
//!
//! Moved up from `store.rs` (S16). The methods are still `impl Store` and
//! `impl EvidenceResponse` blocks, so they run in the same read transaction and behind the
//! same fence as before. This file contains SQL until the read API is narrowed (S14).
use crate::model::*;
use crate::store::{
    EvidenceResponse, SelectedIntegrity, Store, read_native_declarations_v8, selected_ensure,
    v8_owner_scope_for,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeMap;
use std::time::Instant;

impl EvidenceResponse {
    pub fn file_symbols_at(
        &self,
        path: &str,
        expected: IndexPin,
        offset: usize,
        limit: usize,
    ) -> Result<Option<crate::files_view::Outline>> {
        self.store
            .file_symbols_in(&self.db, path, expected, offset, limit, true)
    }
    pub fn syntax_matches_at(
        &self,
        request: &crate::files_view::SyntaxMatchesRequest,
    ) -> Result<Option<crate::files_view::SyntaxMatches>> {
        self.store.syntax_matches_in(&self.db, request, true)
    }
}

impl Store {
    /// All measured declarations in one cached file, retaining their exact native IDs.
    pub fn file_symbols_at(
        &self,
        path: &str,
        expected: IndexPin,
        offset: usize,
        limit: usize,
    ) -> Result<Option<crate::files_view::Outline>> {
        self.with_evidence(|tx| self.file_symbols_in(tx, path, expected, offset, limit, false))
    }
    fn file_symbols_in(
        &self,
        tx: &Connection,
        path: &str,
        expected: IndexPin,
        offset: usize,
        limit: usize,
        pre_startup: bool,
    ) -> Result<Option<crate::files_view::Outline>> {
        use crate::files_view::{InvalidRequest, Outline, OutlineItem, RESPONSE_LIMIT};
        ensure!(
            crate::files_view::valid_path(path)
                && (1..=200).contains(&limit)
                && offset <= i64::MAX as usize,
            InvalidRequest
        );
        let selected = self.read_revision_for(tx, Some(expected), pre_startup)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM revision_documents WHERE revision_id=?1 AND path=?2)",
            params![selected.key, path],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(None);
        }
        let header = self.fetch_selected_header(tx, path, &selected, None)?;
        let fetched = self.fetch_selected_rows(tx, header, &selected, None)?;
        let mut metadata: BTreeMap<_, _> = fetched
            .declarations
            .iter()
            .map(|d| (d.syntax_id.clone(), (d.kind.clone(), d.header.clone())))
            .collect();
        let mut nodes = Self::project_fetched(fetched)?.nodes;
        nodes.sort_by(|a, b| (a.range.start_byte, &a.id).cmp(&(b.range.start_byte, &b.id)));
        let count = nodes.len();
        let mut result = Outline {
            revision: selected.pin,
            path: path.into(),
            items: vec![],
            next_offset: None,
            truncated: false,
            warnings: vec![],
        };
        // Bindings and impl blocks are read from the parsed cached file (#526), only when the
        // page has one.
        let page: Vec<_> = nodes.into_iter().skip(offset).take(limit).collect();
        let syntax = if page.iter().any(|symbol| {
            metadata
                .get(&symbol.id)
                .is_some_and(|(kind, _)| kind == "variable" || kind == "implementation")
        }) {
            let impl_blocks: Vec<_> = page
                .iter()
                .filter(|symbol| {
                    metadata
                        .get(&symbol.id)
                        .is_some_and(|(kind, _)| kind == "implementation")
                })
                .map(|symbol| (symbol.range.start_byte, symbol.range.end_byte))
                .collect();
            match self.selected_source_row_for(tx, path, &selected)? {
                Some(file) => crate::files_view::outline_syntax(&file, &impl_blocks)?,
                None => None,
            }
        } else {
            None
        };
        // A file beyond the outline syntax bounds says so, never a silent partial (#1119).
        result.warnings.extend(
            syntax
                .as_ref()
                .and_then(|syntax| syntax.warning)
                .map(String::from),
        );
        for symbol in page {
            let (native_kind, header) = metadata.remove(&symbol.id).context(SelectedIntegrity(
                "incompatible_index: outline native declaration missing".into(),
            ))?;
            let range = (symbol.range.start_byte, symbol.range.end_byte);
            let callable_initializer = native_kind == "variable"
                && syntax
                    .as_ref()
                    .is_some_and(|syntax| syntax.callable_initializers.contains(&range));
            let impl_target = (native_kind == "implementation")
                .then(|| syntax.as_ref()?.impl_targets.get(&range).cloned())
                .flatten();
            result.items.push(OutlineItem {
                symbol,
                native_kind,
                header,
                callable_initializer,
                impl_target,
            });
            let next = offset + result.items.len();
            result.next_offset = (next < count).then_some(next);
            result.truncated = result.next_offset.is_some();
            if serde_json::to_vec(&result)?.len() > RESPONSE_LIMIT {
                result.items.pop();
                ensure!(
                    !result.items.is_empty(),
                    "response_too_large: Files outline item"
                );
                result.next_offset = Some(offset + result.items.len());
                result.truncated = true;
                break;
            }
        }
        ensure!(
            serde_json::to_vec(&result)?.len() <= RESPONSE_LIMIT,
            "response_too_large: Files outline"
        );
        Ok(Some(result))
    }
    pub fn syntax_matches_at(
        &self,
        request: &crate::files_view::SyntaxMatchesRequest,
    ) -> Result<Option<crate::files_view::SyntaxMatches>> {
        request.validate()?;
        self.with_evidence(|tx| self.syntax_matches_in(tx, request, false))
    }
    fn syntax_matches_in(
        &self,
        tx: &Connection,
        request: &crate::files_view::SyntaxMatchesRequest,
        pre_startup: bool,
    ) -> Result<Option<crate::files_view::SyntaxMatches>> {
        use crate::files_view::{self, MatchScope, SyntaxMatches};
        request.validate()?;
        let selected = self.read_revision_for(tx, Some(request.expected_revision), pre_startup)?;
        let Some(scope) = v8_owner_scope_for(tx, &request.symbol_id, &selected)? else {
            return Ok(None);
        };
        // Decode the selected document's native rows strictly, including noncallables.
        let declaration = read_native_declarations_v8(tx, &scope, None, true)?
            .into_iter()
            .find(|d| d.syntax_id == request.symbol_id)
            .context(SelectedIntegrity(
                "incompatible_index: match declaration missing".into(),
            ))?;
        let mut result = SyntaxMatches::new(
            request,
            MatchScope {
                source_set_id: scope.key.source_set_id.clone(),
                language: scope.key.language.clone(),
                spelling: declaration.name.clone(),
            },
        );
        if declaration.name_range.is_none()
            || declaration.name.as_ref().is_none_or(|n| n.is_empty())
        {
            result.scope.spelling = None;
            result.warn("Selected declaration has no captured identifier name; syntax matching unavailable.");
            result.bounded()?;
            return Ok(Some(result));
        }
        if let Some(cursor) = &request.continuation {
            let present: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM revision_documents m
                 JOIN native_revisions r ON r.id=m.revision_id WHERE m.revision_id=?1
                 AND m.path=?2 AND m.language=?3 AND r.source_set_id=?4)",
                params![
                    selected.key,
                    cursor.path,
                    scope.key.language,
                    scope.key.source_set_id
                ],
                |row| row.get(0),
            )?;
            ensure!(present, files_view::InvalidRequest);
        }
        let started = Instant::now();
        let mut bytes = 0usize;
        let mut after: Option<String> = None;
        // Read manifest entries one at a time. Never materialize a checkout graph/catalog.
        let mut entries = tx.prepare(
            "SELECT m.path,length(d.source_bytes),d.byte_length,m.coverage_diagnostic FROM revision_documents m
             JOIN native_revisions r ON r.id=m.revision_id
             LEFT JOIN document_versions d ON d.vid=m.vid AND d.language=m.language AND d.path=m.path
             WHERE m.revision_id=?1 AND m.language=?2 AND (?3 IS NULL OR m.path>=?3)
             AND (?4 IS NULL OR m.path>?4) AND r.source_set_id=?5 ORDER BY m.path LIMIT 1")?;
        loop {
            let entry: Option<(String, Option<i64>, Option<i64>, Option<String>)> = entries
                .query_row(
                    params![
                        selected.key,
                        scope.key.language,
                        request.continuation.as_ref().map(|c| c.path.as_str()),
                        after,
                        scope.key.source_set_id
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?;
            let Some((path, size, recorded, diagnostic)) = entry else {
                break;
            };
            let (Some(size), Some(recorded)) = (size, recorded) else {
                return Err(
                    SelectedIntegrity("incompatible_index: match source absent".into()).into(),
                );
            };
            selected_ensure!(
                size >= 0 && size == recorded,
                "incompatible_index: match source length invalid"
            );
            let start = request
                .continuation
                .as_ref()
                .filter(|c| c.path == path)
                .map_or(0, |c| c.offset);
            ensure!(start <= size as usize, files_view::InvalidRequest);
            if started.elapsed() >= files_view::TOTAL_TIME_LIMIT {
                result.warnings.push(
                    "Request syntax scan budget reached; continue for remaining cached files."
                        .into(),
                );
                result.resume(&path, start);
                break;
            }
            // Indexing found this Rust source past measurement's parse-work budget: it is never
            // read or parsed again, and is skipped as a parse over its budget is (#1300).
            if crate::syntax_bounds::past_parse_work(diagnostic.as_deref()) {
                ensure!(start == 0, files_view::InvalidRequest);
                result.warn(files_view::PARSE_BUDGET_SKIPPED);
                after = Some(path);
                continue;
            }
            if size as usize > files_view::FILE_INPUT_LIMIT {
                ensure!(start == 0, files_view::InvalidRequest);
                result.warn("Cached file exceeds 2 MiB syntax scan input; file skipped and coverage incomplete.");
                after = Some(path);
                continue;
            }
            if bytes.saturating_add(size as usize) > files_view::TOTAL_INPUT_LIMIT {
                result.warnings.push(
                    "Request syntax scan budget reached; continue for remaining cached files."
                        .into(),
                );
                result.resume(&path, start);
                break;
            }
            bytes += size as usize;
            let file = self
                .selected_source_row_for(tx, &path, &selected)?
                .context(SelectedIntegrity(
                    "incompatible_index: match source missing".into(),
                ))?;
            ensure!(
                file.text.is_char_boundary(start),
                files_view::InvalidRequest
            );
            let excluded = (path == scope.key.path)
                .then_some(declaration.name_range.as_ref())
                .flatten()
                .map(|r| (r.start, r.end));
            if files_view::scan_file(&mut result, request, &file, start, excluded, started)? {
                break;
            }
            after = Some(path);
        }
        result.bounded()?;
        Ok(Some(result))
    }
}
