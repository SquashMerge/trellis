//! Narrow MCP projections over the already admitted selected SQLite snapshot.
use super::*;

impl EvidenceResponse {
    pub fn mcp_language_counts(&self) -> Result<[[u64; 5]; 4]> {
        #[cfg(feature = "test-support")]
        if self
            .store
            .mcp_read_busy_attempts
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count > 0).then(|| count - 1)
            })
            .is_ok()
        {
            return Err(SqliteContention(()).into());
        }
        let selected = self.store.read_revision_for(&self.db, None, true)?;
        let mut counts = [[0_u64; 5]; 4];
        let mut rows = self.db.prepare(
            "SELECT language,coverage_selected,coverage_state FROM revision_documents WHERE revision_id=?1"
        )?;
        let records = rows.query_map([&selected.key], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, bool>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for record in records {
            let (language, chosen, state) = record?;
            let bucket = match language.as_str() {
                "java" => 0,
                "rust" => 1,
                "python" => 2,
                "javascript" => 3,
                _ => anyhow::bail!("store_unavailable: invalid selected language"),
            };
            let category = if chosen {
                match state.as_str() {
                    "complete" => 1,
                    "partial" => 2,
                    "failed" => 3,
                    _ => anyhow::bail!("store_unavailable: invalid selected coverage"),
                }
            } else {
                4
            };
            counts[bucket][category] += 1;
            if chosen {
                counts[bucket][0] += 1;
            }
        }
        Ok(counts)
    }
}

#[cfg(feature = "test-support")]
impl Store {
    /// Inject direct typed SQLite contention after a W1 selected admission.
    pub fn mcp_busy_reads_for_tests(&self, count: usize) {
        self.mcp_read_busy_attempts.store(count, Ordering::Release);
    }

    /// Use the normal virgin v1 queue staging path before fixture publication.
    pub fn mcp_prepare_virgin_queue_for_tests(&self) -> Result<()> {
        self.prepare_virgin_queue_before_activation()
    }

    /// Introduce one required-row fault through Store's cached connection.
    pub fn mcp_corrupt_find_for_tests(
        &self,
        writer: &topology::WriterGuard,
        fault: &str,
    ) -> Result<()> {
        writer.belongs_to(&self.identity)?;
        self.identity.verify()?;
        writer.verify()?;
        let mut db = self.cache_write()?;
        if fault == "missing_scope" {
            db.pragma_update(None, "foreign_keys", "OFF")?;
        }
        let tx = storage_result(db.transaction_with_behavior(TransactionBehavior::Immediate))?;
        let count = match fault {
            "missing_scope" => tx.execute("UPDATE document_versions SET path='missing.js' WHERE path='a.js'", [])?,
            "missing_header" => tx.execute("DELETE FROM native_version_headers WHERE syntax_id=(SELECT syntax_id FROM native_version_declarations WHERE name='Alpha' LIMIT 1)", [])?,
            "bad_digest" => tx.execute("UPDATE document_versions SET content_hash=?1 WHERE path='a.js'", ["0".repeat(64)])?,
            _ => anyhow::bail!("unknown find corruption fixture"),
        };
        ensure!(count == 1, "fixture corruption target absent");
        storage_result(tx.commit())?;
        Ok(())
    }

    /// Fixture-only revision row setup for W1 projection tests. Uses Store's own
    /// cached SQLite connection; never closes another FD on a live index inode.
    pub fn mcp_set_coverage_for_tests(
        &self,
        writer: &topology::WriterGuard,
        path: &str,
        selected: bool,
        state: &str,
    ) -> Result<()> {
        anyhow::ensure!(
            matches!(
                state,
                "complete" | "partial" | "failed" | "omitted" | "unsupported"
            ),
            "invalid fixture state"
        );
        writer.belongs_to(&self.identity)?;
        self.identity.verify()?;
        writer.verify()?;
        let mut db = self.cache_write()?;
        let tx = storage_result(db.transaction_with_behavior(TransactionBehavior::Immediate))?;
        let count = storage_result(tx.execute(
            "UPDATE revision_documents SET coverage_selected=?1,coverage_state=?2,coverage_diagnostic=?3 WHERE revision_id=(SELECT 'pin:v1:'||index_generation||':'||index_revision FROM index_metadata WHERE singleton=1) AND path=?4",
            params![selected, state, (state != "complete").then_some("fixture incomplete coverage"), path],
        ))?;
        anyhow::ensure!(count == 1, "fixture document absent");
        storage_result(tx.commit())?;
        Ok(())
    }
}

/// Verified raw source range from the one selected MCP read snapshot.
pub struct McpSource {
    pub path: String,
    pub content_hash: String,
    pub start_line: u64,
    pub end_line: u64,
    pub byte_start: usize,
    pub byte_end: usize,
    pub text: String,
    pub pin: IndexPin,
}

fn mcp_lf_range(bytes: &[u8], start: u64, end: u64) -> Result<(usize, usize)> {
    ensure!(
        start > 0 && end >= start,
        "invalid_request: reversed source range"
    );
    ensure!(end - start < 200, "range_too_large: source line cap");
    let mut line = 1_u64;
    let mut from = (start == 1).then_some(0);
    let mut to = None;
    for (at, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            if line == end {
                to = Some(at + 1);
                break;
            }
            line += 1;
            if line == start {
                from = Some(at + 1);
            }
        }
    }
    if !bytes.is_empty() && bytes.last() != Some(&b'\n') && line == end {
        to = Some(bytes.len());
    }
    let (from, to) = from
        .zip(to)
        .context("range_too_large: source line absent")?;
    ensure!(to - from <= 16384, "range_too_large: source byte cap");
    Ok((from, to))
}

impl EvidenceResponse {
    pub fn mcp_source_hash(&self, path: &str) -> Result<Option<String>> {
        let selected = self.store.read_revision_for(&self.db, None, true)?;
        Ok(self
            .store
            .mcp_selected_raw_source_for(&self.db, path, &selected)?
            .map(|(hash, _)| hash))
    }

    pub fn mcp_source(
        &self,
        path: &str,
        start_line: u64,
        end_line: u64,
        expected: Option<IndexPin>,
        expected_hash: Option<&str>,
    ) -> Result<Option<McpSource>> {
        let selected = self.store.read_revision_for(&self.db, None, true)?;
        ensure!(
            expected.is_none_or(|pin| pin == selected.pin),
            "revision conflict: expected basis differs from admitted head"
        );
        let Some((content_hash, bytes)) = self
            .store
            .mcp_selected_raw_source_for(&self.db, path, &selected)?
        else {
            return Ok(None);
        };
        ensure!(
            expected_hash.is_none_or(|hash| hash == content_hash),
            "revision conflict: expected content hash differs from selected source"
        );
        let (byte_start, byte_end) = mcp_lf_range(&bytes, start_line, end_line)?;
        let text = std::str::from_utf8(&bytes[byte_start..byte_end])
            .map_err(|_| anyhow::anyhow!("unsupported_encoding: selected source slice"))?
            .to_owned();
        Ok(Some(McpSource {
            path: path.to_owned(),
            content_hash,
            start_line,
            end_line,
            byte_start,
            byte_end,
            text,
            pin: selected.pin,
        }))
    }
}

/// A find result keeps every required coverage tuple, independent of the hit prefix.
pub struct McpFind {
    pub symbols: Vec<McpFindSymbol>,
    pub coverage: Vec<crate::native_evidence::Coverage>,
    pub provenance: Vec<crate::native_evidence::Provenance>,
    pub has_more: bool,
    pub pin: IndexPin,
}

pub struct McpFindSymbol {
    pub declaration: crate::native_evidence::Declaration,
    pub content_hash: String,
}

impl EvidenceResponse {
    pub fn mcp_find(
        &self,
        query: &str,
        limit: usize,
        expected: Option<IndexPin>,
    ) -> Result<McpFind> {
        use crate::native_evidence::{DocumentKey, Provenance};
        #[cfg(feature = "test-support")]
        if self
            .store
            .mcp_read_busy_attempts
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count > 0).then(|| count - 1)
            })
            .is_ok()
        {
            return Err(SqliteContention(()).into());
        }
        let selected = self.store.read_revision_for(&self.db, None, true)?;
        ensure!(
            expected.is_none_or(|pin| pin == selected.pin),
            "revision conflict: expected basis differs from admitted head"
        );
        // Include manifest rows without native declarations, including failed and unselected rows.
        let keys = self
            .db
            .prepare(
                "SELECT source_set_id,language,path FROM revision_documents
            WHERE revision_id=?1 ORDER BY source_set_id,language,path",
            )?
            .query_map([&selected.key], |row| {
                Ok(DocumentKey {
                    source_set_id: row.get(0)?,
                    language: row.get(1)?,
                    path: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut coverage = Vec::with_capacity(keys.len());
        let mut scopes = BTreeMap::new();
        for key in keys {
            let scope = v8_native_scope_for(&self.db, &key, &selected)?
                .context("store_unavailable: missing native coverage scope")?;
            let row = read_native_coverage_v8(&self.db, &scope)?;
            ensure!(
                row.revision_id == scope.native_revision_id
                    && row.producer_id == scope.producer_id
                    && row.language == key.language
                    && row.source_set_id == key.source_set_id
                    && row.document_path == key.path,
                "store_unavailable: native coverage identity mismatch"
            );
            let valid_state = match row.state.as_str() {
                "notRequested" => !row.requested && !row.selected && row.diagnostic.is_none(),
                "omitted" | "unsupported" => {
                    row.requested && !row.selected && row.diagnostic.is_some()
                }
                "failed" | "partial" => row.requested && row.selected && row.diagnostic.is_some(),
                "complete" => row.requested && row.selected && row.diagnostic.is_none(),
                _ => false,
            };
            let ordered_roles = |roles: &[String]| {
                roles.windows(2).all(|pair| {
                    matches!((pair[0].as_str(), pair[1].as_str()), ("definition", "call"))
                })
            };
            ensure!(
                valid_state
                    && ordered_roles(&row.supported_roles)
                    && ordered_roles(&row.observed_roles)
                    && row
                        .observed_roles
                        .iter()
                        .all(|role| row.supported_roles.contains(role))
                    && row
                        .supported_roles
                        .iter()
                        .all(|role| matches!(role.as_str(), "definition" | "call"))
                    && row
                        .observed_roles
                        .iter()
                        .all(|role| matches!(role.as_str(), "definition" | "call")),
                "store_unavailable: inconsistent native coverage tuple"
            );
            coverage.push(row);
            scopes.insert(scope.version_id.clone(), scope);
        }
        let language_order = |language: &str| -> Result<u8> {
            match language {
                "java" => Ok(0),
                "rust" => Ok(1),
                "python" => Ok(2),
                "javascript" => Ok(3),
                _ => anyhow::bail!("store_unavailable: invalid native coverage language"),
            }
        };
        for row in &coverage {
            language_order(&row.language)?;
        }
        coverage.sort_by(|a, b| {
            (
                &a.producer_id,
                language_order(&a.language).unwrap(),
                &a.source_set_id,
                &a.document_path,
                &a.revision_id,
            )
                .cmp(&(
                    &b.producer_id,
                    language_order(&b.language).unwrap(),
                    &b.source_set_id,
                    &b.document_path,
                    &b.revision_id,
                ))
        });
        // SQLite lower folds ASCII only. instr treats the query as literal bytes, not a pattern.
        let mut statement = self.db.prepare(
            "SELECT d.version_id,d.syntax_id FROM native_version_declarations d
            JOIN revision_documents m ON m.document_version_id=d.version_id AND m.revision_id=?1
            WHERE instr(lower(d.name),lower(?2))>0 OR instr(lower(d.syntax_id),lower(?2))>0
            ORDER BY CASE WHEN lower(d.name)=lower(?2) THEN 0
                WHEN instr(lower(d.name),lower(?2))=1 THEN 1 ELSE 2 END,
                d.name COLLATE BINARY ASC, d.syntax_id COLLATE BINARY ASC LIMIT ?3",
        )?;
        let hits = statement
            .query_map(
                params![selected.key, query, i64::try_from(limit + 1)?],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let has_more = hits.len() > limit;
        let mut symbols = Vec::new();
        let mut provenance = Vec::new();
        let mut loaded: BTreeMap<
            String,
            (
                BTreeMap<String, crate::native_evidence::Declaration>,
                String,
            ),
        > = BTreeMap::new();
        for (version, id) in hits.into_iter().take(limit) {
            let scope = scopes
                .get(&version)
                .context("store_unavailable: find scope absent")?;
            if !loaded.contains_key(&version) {
                self.store
                    .attest_selected_document_for(&self.db, &scope.key.path, &selected)?;
                let declarations = read_native_declarations_v8(&self.db, scope, None, true)?
                    .into_iter()
                    .map(|d| (d.syntax_id.clone(), d))
                    .collect();
                let hash: String = self.db.query_row(
                    "SELECT content_hash FROM document_versions WHERE id=?1 AND source_set_id=?2 AND language=?3 AND path=?4",
                    params![scope.version_id, scope.key.source_set_id, scope.key.language, scope.key.path],
                    |r| r.get(0))?;
                loaded.insert(version.clone(), (declarations, hash.clone()));
                provenance.push(Provenance {
                    id: scope.proof_id(),
                    producer_id: scope.producer_id.clone(),
                    document: scope.key.clone(),
                    revision_id: scope.native_revision_id.clone(),
                    content_hash: hash,
                    evidence_kind: "measuredSyntax".into(),
                    basis: None,
                    freshness: "fresh".into(),
                    derived_from: None,
                });
            }
            let (declarations, hash) = &loaded[&version];
            let declaration = declarations
                .get(&id)
                .context("store_unavailable: selected declaration absent")?
                .clone();
            ensure!(
                declaration.provenance_id == scope.proof_id()
                    && declaration.revision_id == scope.native_revision_id,
                "store_unavailable: native proof mismatch"
            );
            symbols.push(McpFindSymbol {
                declaration,
                content_hash: hash.clone(),
            });
        }
        provenance.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(McpFind {
            symbols,
            coverage,
            provenance,
            has_more,
            pin: selected.pin,
        })
    }
}

/// One selected native declaration with its complete required evidence closure.
pub struct McpDeclaration {
    pub declaration: crate::native_evidence::Declaration,
    pub content_hash: String,
    pub coverage: Vec<crate::native_evidence::Coverage>,
    pub provenance: Vec<crate::native_evidence::Provenance>,
    pub pin: IndexPin,
}

impl EvidenceResponse {
    pub fn mcp_declaration(
        &self,
        symbol_id: &str,
        expected: Option<IndexPin>,
    ) -> Result<Option<McpDeclaration>> {
        use crate::native_evidence::Provenance;
        #[cfg(feature = "test-support")]
        if self
            .store
            .mcp_read_busy_attempts
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count > 0).then(|| count - 1)
            })
            .is_ok()
        {
            return Err(SqliteContention(()).into());
        }
        let selected = self.store.read_revision_for(&self.db, None, true)?;
        ensure!(
            expected.is_none_or(|pin| pin == selected.pin),
            "revision conflict: expected basis differs from admitted head"
        );
        let Some(scope) = v8_owner_scope_for(&self.db, symbol_id, &selected)? else {
            return Ok(None);
        };
        self.store
            .attest_selected_document_for(&self.db, &scope.key.path, &selected)?;
        let declaration = read_native_declarations_v8(&self.db, &scope, None, true)?
            .into_iter()
            .find(|row| row.syntax_id == symbol_id)
            .context("store_unavailable: selected declaration absent")?;
        ensure!(
            declaration.provenance_id == scope.proof_id()
                && declaration.revision_id == scope.native_revision_id
                && declaration.document == scope.key,
            "store_unavailable: selected declaration proof mismatch"
        );
        let coverage = read_native_coverage_v8(&self.db, &scope)?;
        ensure!(
            coverage.producer_id == scope.producer_id
                && coverage.language == scope.key.language
                && coverage.source_set_id == scope.key.source_set_id
                && coverage.document_path == scope.key.path
                && coverage.revision_id == scope.native_revision_id
                && coverage.requested
                && coverage.selected
                && matches!(coverage.state.as_str(), "complete" | "partial" | "failed"),
            "store_unavailable: selected declaration coverage mismatch"
        );
        let hash: String = self.db.query_row(
            "SELECT content_hash FROM document_versions WHERE id=?1 AND source_set_id=?2 AND language=?3 AND path=?4",
            params![scope.version_id, scope.key.source_set_id, scope.key.language, scope.key.path],
            |r| r.get(0),
        )?;
        let provenance = Provenance {
            id: scope.proof_id(),
            producer_id: scope.producer_id.clone(),
            document: scope.key,
            revision_id: scope.native_revision_id,
            content_hash: hash.clone(),
            evidence_kind: "measuredSyntax".into(),
            basis: None,
            freshness: "fresh".into(),
            derived_from: None,
        };
        Ok(Some(McpDeclaration {
            declaration,
            content_hash: hash,
            coverage: vec![coverage],
            provenance: vec![provenance],
            pin: selected.pin,
        }))
    }
}

/// An outgoing call and its optional, independently verified lexical suggestions.
pub struct McpOutgoingCall {
    pub call: crate::native_evidence::Call,
    pub content_hash: String,
    pub callee_text: Option<String>,
    pub callee_text_truncated: bool,
    pub candidates: Vec<McpOutgoingCandidate>,
    pub omitted: Option<usize>,
}

#[derive(Clone)]
pub struct McpOutgoingCandidate {
    pub declaration: crate::native_evidence::Declaration,
    pub coverage: crate::native_evidence::Coverage,
    pub provenance: crate::native_evidence::Provenance,
}

pub struct McpOutgoing {
    pub owner: McpDeclaration,
    pub calls: Vec<McpOutgoingCall>,
    pub has_more: bool,
}

fn mcp_callee_text(
    source: &str,
    range: Option<&crate::native_evidence::Range>,
) -> Result<(Option<String>, bool)> {
    let Some(range) = range else {
        return Ok((None, false));
    };
    let text = source
        .get(range.start..range.end)
        .context("store_unavailable: invalid selected callee range")?;
    let end = text
        .char_indices()
        .map(|(at, ch)| at + ch.len_utf8())
        .take_while(|&end| end <= 1024)
        .last()
        .unwrap_or(0);
    Ok((Some(text[..end].to_owned()), end < text.len()))
}

// Only optional candidate work installs this handler on the admitted snapshot.
// Remove it before the connection finishes or can serve another read.
struct OptionalHintProgress<'a> {
    now: &'a dyn Fn() -> std::time::Instant,
    end: std::time::Instant,
}

impl OptionalHintProgress<'_> {
    fn expired(&self) -> bool {
        (self.now)() >= self.end
    }
}

unsafe extern "C" fn optional_hint_progress_callback(arg: *mut std::ffi::c_void) -> libc::c_int {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: the scope below outlives its SQLite progress registration.
        let progress = unsafe { &*(arg as *const OptionalHintProgress<'_>) };
        i32::from(progress.expired())
    }))
    .unwrap_or(1)
}

struct OptionalHintProgressGuard(*mut rusqlite::ffi::sqlite3);
impl Drop for OptionalHintProgressGuard {
    fn drop(&mut self) {
        // SAFETY: the admitted connection remains alive until after this guard.
        unsafe { rusqlite::ffi::sqlite3_progress_handler(self.0, 0, None, std::ptr::null_mut()) };
    }
}

fn optional_hint_interrupt(error: &rusqlite::Error, progress: &OptionalHintProgress<'_>) -> bool {
    progress.expired()
        && matches!(error, rusqlite::Error::SqliteFailure(info, _)
            if info.code == rusqlite::ErrorCode::OperationInterrupted)
}

fn install_optional_hint_progress(
    db: &rusqlite::Connection,
    progress: &OptionalHintProgress<'_>,
) -> OptionalHintProgressGuard {
    // SAFETY: only this thread uses the admitted connection. The guard removes
    // the handler before `progress` or the connection can be dropped.
    let handle = unsafe { db.handle() };
    unsafe {
        rusqlite::ffi::sqlite3_progress_handler(
            handle,
            256,
            Some(optional_hint_progress_callback),
            (progress as *const OptionalHintProgress<'_>)
                .cast_mut()
                .cast(),
        );
    }
    OptionalHintProgressGuard(handle)
}

impl EvidenceResponse {
    pub fn mcp_outgoing(
        &self,
        symbol_id: &str,
        limit: usize,
        expected: Option<IndexPin>,
        hint_now: &dyn Fn() -> std::time::Instant,
        required_deadline: std::time::Instant,
    ) -> Result<Option<McpOutgoing>> {
        use crate::native_evidence::{DocumentKey, Provenance};
        let selected = self.store.read_revision_for(&self.db, None, true)?;
        ensure!(
            expected.is_none_or(|pin| pin == selected.pin),
            "revision conflict: expected basis differs from admitted head"
        );
        let Some(owner) = self.mcp_declaration(symbol_id, expected)? else {
            return Ok(None);
        };
        let scope = v8_owner_scope_for(&self.db, symbol_id, &selected)?
            .context("store_unavailable: selected outgoing owner absent")?;
        let source = self
            .store
            .selected_source_row_for(&self.db, &scope.key.path, &selected)?
            .context("store_unavailable: selected outgoing source absent")?;
        ensure!(
            source.hash == owner.content_hash,
            "store_unavailable: selected outgoing source hash mismatch"
        );
        let mut measured = read_native_calls_v8(&self.db, &scope, Some(symbol_id))?;
        for call in &measured {
            ensure!(
                call.owner_syntax_id == symbol_id
                    && call.document == scope.key
                    && call.revision_id == scope.native_revision_id
                    && call.provenance_id == scope.proof_id(),
                "store_unavailable: outgoing call proof mismatch"
            );
        }
        measured.sort_by(|a, b| {
            (&a.document.path, a.range.start, a.range.end, &a.id).cmp(&(
                &b.document.path,
                b.range.start,
                b.range.end,
                &b.id,
            ))
        });
        let has_more = measured.len() > limit;
        let mut calls = Vec::new();
        for call in measured.into_iter().take(limit) {
            let (callee_text, callee_text_truncated) =
                mcp_callee_text(&source.text, call.callee_range.as_ref())?;
            calls.push(McpOutgoingCall {
                call,
                content_hash: owner.content_hash.clone(),
                callee_text,
                callee_text_truncated,
                candidates: vec![],
                omitted: Some(0),
            });
        }
        #[cfg(feature = "test-support")]
        crate::mcp::phase_hook("outgoing_hint_start").map_err(|e| anyhow::anyhow!(e))?;
        // Leave time to finish the selected read and serialize/send its mandatory
        // answer. Optional hints never extend the accepted call's deadline.
        let required_hint_end = required_deadline
            .checked_sub(std::time::Duration::from_millis(75))
            .unwrap_or(required_deadline);
        let progress = OptionalHintProgress {
            now: hint_now,
            end: (hint_now() + std::time::Duration::from_millis(1000)).min(required_hint_end),
        };
        #[cfg(feature = "test-support")]
        crate::mcp::phase_hook("outgoing_hint").map_err(|e| anyhow::anyhow!(e))?;
        // SQLite checks inside long optional statements as well as between rows.
        // The read-only selected snapshot already holds its shared lock; no new
        // connection or lock transition is needed for these candidate SELECTs.
        let _progress_guard =
            (!calls.is_empty()).then(|| install_optional_hint_progress(&self.db, &progress));
        // One admitted snapshot: verify and decode each selected version only once.
        // A failed optional verification is never cached as trusted evidence.
        let mut verified_versions = BTreeMap::<
            String,
            (
                Vec<crate::native_evidence::Declaration>,
                crate::native_evidence::Coverage,
                String,
            ),
        >::new();
        for (item_index, item) in calls.iter_mut().enumerate() {
            #[cfg(feature = "test-support")]
            if item_index == 1 {
                crate::mcp::phase_hook("outgoing_hint_item_1").map_err(|e| anyhow::anyhow!(e))?;
            }
            #[cfg(not(feature = "test-support"))]
            let _ = item_index;
            if progress.expired() {
                item.omitted = None;
                continue;
            }
            let Some(range) = &item.call.callee_range else {
                continue;
            };
            let Some(spelling) = item.call.spelling.as_deref() else {
                continue;
            };
            let Some(token) = source.text.get(range.start..range.end) else {
                continue;
            };
            // The full measured spelling is the token, not a member expression suffix.
            if token != spelling {
                continue;
            }
            let Ok(lookup) = crate::native_evidence::lookup(&scope.key.language, token) else {
                continue;
            };
            let mut statement = match self.db.prepare(
                "SELECT d.version_id,d.syntax_id,m.source_set_id,m.language,m.path
                 FROM native_version_declarations d JOIN revision_documents m
                   ON m.document_version_id=d.version_id AND m.revision_id=?1
                 WHERE m.source_set_id=?2 AND m.language=?3 AND d.lookup_key=?4",
            ) {
                Ok(statement) => statement,
                Err(error) if optional_hint_interrupt(&error, &progress) => {
                    item.omitted = None;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let mut rows = match statement.query(params![
                selected.key,
                scope.key.source_set_id,
                scope.key.language,
                lookup
            ]) {
                Ok(rows) => rows,
                Err(error) if optional_hint_interrupt(&error, &progress) => {
                    item.omitted = None;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let mut candidates = BTreeMap::<String, McpOutgoingCandidate>::new();
            let mut timed_out = false;
            loop {
                if progress.expired() {
                    timed_out = true;
                    break;
                }
                let row = match rows.next() {
                    Ok(row) => row,
                    Err(error) if optional_hint_interrupt(&error, &progress) => {
                        timed_out = true;
                        break;
                    }
                    Err(error) => return Err(error.into()),
                };
                let Some(row) = row else { break };
                #[cfg(feature = "test-support")]
                crate::mcp::phase_hook("outgoing_hint_row").map_err(|e| anyhow::anyhow!(e))?;
                if progress.expired() {
                    timed_out = true;
                    break;
                }
                let version: String = row.get(0)?;
                let id: String = row.get(1)?;
                let key = DocumentKey {
                    source_set_id: row.get(2)?,
                    language: row.get(3)?,
                    path: row.get(4)?,
                };
                let candidate = (|| -> Result<Option<McpOutgoingCandidate>> {
                    optional_hint_check(Some(&|| progress.expired()))?;
                    let Some(candidate_scope) = v8_native_scope_for(&self.db, &key, &selected)?
                    else {
                        return Ok(None);
                    };
                    if candidate_scope.version_id != version {
                        return Ok(None);
                    }
                    if !verified_versions.contains_key(&version) {
                        if progress.expired() {
                            return Ok(None);
                        }
                        #[cfg(feature = "test-support")]
                        crate::mcp::phase_hook("outgoing_hint_verify_version")
                            .map_err(|error| anyhow::anyhow!(error))?;
                        if progress.expired() {
                            return Ok(None);
                        }
                        let declarations = read_native_declarations_v8_optional(
                            &self.db,
                            &candidate_scope,
                            None,
                            true,
                            &|| progress.expired(),
                        )?;
                        let coverage = read_native_coverage_v8(&self.db, &candidate_scope)?;
                        // Reject an ineligible optional row before the expensive full
                        // attestation: an omitted candidate must not poison required calls.
                        let Some(declaration) = declarations.iter().find(|row| row.syntax_id == id)
                        else {
                            return Ok(None);
                        };
                        if declaration.name.is_none()
                            || declaration.lookup_key.as_deref() != Some(lookup.as_str())
                            || declaration.provenance_id != candidate_scope.proof_id()
                            || declaration.revision_id != candidate_scope.native_revision_id
                            || declaration.document != candidate_scope.key
                            || !mcp_canonical_syntax_id(&id)
                            || !coverage.requested
                            || !coverage.selected
                            || !matches!(coverage.state.as_str(), "complete" | "partial")
                            || coverage.revision_id != candidate_scope.native_revision_id
                            || coverage.producer_id != candidate_scope.producer_id
                            || coverage.language != key.language
                            || coverage.source_set_id != key.source_set_id
                            || coverage.document_path != key.path
                        {
                            return Ok(None);
                        }
                        self.store.attest_selected_document_optional_for(
                            &self.db,
                            &key.path,
                            &selected,
                            &|| progress.expired(),
                        )?;
                        let hash: String = self.db.query_row(
                            "SELECT content_hash FROM document_versions WHERE id=?1 AND source_set_id=?2 AND language=?3 AND path=?4",
                            params![version,key.source_set_id,key.language,key.path],|r|r.get(0))?;
                        if progress.expired() {
                            return Ok(None);
                        }
                        verified_versions.insert(version.clone(), (declarations, coverage, hash));
                    }
                    let (declarations, coverage, hash) = &verified_versions[&version];
                    let Some(declaration) = declarations.iter().find(|row| row.syntax_id == id)
                    else {
                        return Ok(None);
                    };
                    if declaration.name.is_none()
                        || declaration.lookup_key.as_deref() != Some(lookup.as_str())
                        || declaration.provenance_id != candidate_scope.proof_id()
                        || declaration.revision_id != candidate_scope.native_revision_id
                        || declaration.document != candidate_scope.key
                        || !mcp_canonical_syntax_id(&id)
                    {
                        return Ok(None);
                    }
                    if !coverage.requested
                        || !coverage.selected
                        || !matches!(coverage.state.as_str(), "complete" | "partial")
                        || coverage.revision_id != candidate_scope.native_revision_id
                        || coverage.producer_id != candidate_scope.producer_id
                        || coverage.language != key.language
                        || coverage.source_set_id != key.source_set_id
                        || coverage.document_path != key.path
                    {
                        return Ok(None);
                    }
                    let provenance = Provenance {
                        id: candidate_scope.proof_id(),
                        producer_id: candidate_scope.producer_id,
                        document: key,
                        revision_id: candidate_scope.native_revision_id,
                        content_hash: hash.clone(),
                        evidence_kind: "measuredSyntax".into(),
                        basis: None,
                        freshness: "fresh".into(),
                        derived_from: None,
                    };
                    Ok(Some(McpOutgoingCandidate {
                        declaration: declaration.clone(),
                        coverage: coverage.clone(),
                        provenance,
                    }))
                })();
                let candidate = match candidate {
                    Ok(Some(candidate)) => candidate,
                    Ok(None) => continue,
                    Err(error) => {
                        if error.downcast_ref::<OptionalHintExpired>().is_some() {
                            timed_out = true;
                            break;
                        }
                        if error
                            .downcast_ref::<rusqlite::Error>()
                            .is_some_and(|sqlite| optional_hint_interrupt(sqlite, &progress))
                        {
                            timed_out = true;
                            break;
                        }
                        continue;
                    }
                };
                insert_mcp_candidate(&mut candidates, id, candidate)?;
            }
            if timed_out || progress.expired() {
                item.omitted = None;
                continue;
            }
            item.candidates = candidates.into_values().collect();
            item.candidates.sort_by(|a, b| {
                (
                    &a.declaration.document.path,
                    a.declaration.range.start,
                    a.declaration.range.end,
                    &a.declaration.syntax_id,
                )
                    .cmp(&(
                        &b.declaration.document.path,
                        b.declaration.range.start,
                        b.declaration.range.end,
                        &b.declaration.syntax_id,
                    ))
            });
            item.omitted = Some(item.candidates.len().saturating_sub(8));
        }
        Ok(Some(McpOutgoing {
            owner,
            calls,
            has_more,
        }))
    }
}

fn insert_mcp_candidate(
    candidates: &mut BTreeMap<String, McpOutgoingCandidate>,
    id: String,
    candidate: McpOutgoingCandidate,
) -> Result<()> {
    match candidates.entry(id) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(candidate);
        }
        std::collections::btree_map::Entry::Occupied(entry) => {
            ensure!(
                entry.get().declaration == candidate.declaration
                    && entry.get().coverage == candidate.coverage
                    && entry.get().provenance == candidate.provenance,
                "store_unavailable: conflicting duplicate native hint"
            );
        }
    }
    Ok(())
}

fn mcp_canonical_syntax_id(id: &str) -> bool {
    id.len() == 39
        && id.starts_with("sid:v1:")
        && id[7..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

#[cfg(test)]
mod outgoing_tests {
    use super::*;
    #[test]
    fn duplicate_verified_hint_dedupes_but_conflicting_record_fails() {
        use crate::native_evidence::{Coverage, Declaration, DocumentKey, Provenance};
        use serde_json::json;
        let document = DocumentKey {
            source_set_id: "source-set:v1:test".into(),
            language: "javascript".into(),
            path: "a.js".into(),
        };
        let declaration: Declaration = serde_json::from_value(json!({
            "syntaxId":format!("sid:v1:{}","a".repeat(32)),"document":document,
            "revisionId":"revision:v2:test","kind":"function","name":"Target",
            "lookupKey":"Target","ancestors":[],
            "key":{"kind":"function","name":"Target","signature":null,"ordinal":0},
            "range":{"start":0,"end":20},"nameRange":null,
            "header":{"kind":"function","name":"Target","modifiers":[],
                "typeParameters":[],"parameters":[],"resultType":null,"bases":[]},
            "provenanceId":"native-proof:v1:test"
        }))
        .unwrap();
        let candidate = McpOutgoingCandidate {
            declaration: declaration.clone(),
            coverage: Coverage {
                producer_id: "native:v1".into(),
                language: "javascript".into(),
                source_set_id: document.source_set_id.clone(),
                document_path: document.path.clone(),
                revision_id: "revision:v2:test".into(),
                requested: true,
                selected: true,
                state: "complete".into(),
                supported_roles: vec!["definition".into()],
                observed_roles: vec!["definition".into()],
                diagnostic: None,
            },
            provenance: Provenance {
                id: "native-proof:v1:test".into(),
                producer_id: "native:v1".into(),
                document,
                revision_id: "revision:v2:test".into(),
                content_hash: "a".repeat(64),
                evidence_kind: "measuredSyntax".into(),
                basis: None,
                freshness: "fresh".into(),
                derived_from: None,
            },
        };
        let mut found = BTreeMap::new();
        insert_mcp_candidate(&mut found, declaration.syntax_id.clone(), candidate.clone()).unwrap();
        insert_mcp_candidate(&mut found, declaration.syntax_id.clone(), candidate.clone()).unwrap();
        assert_eq!(found.len(), 1);
        let mut conflict = candidate;
        conflict.provenance.content_hash = "b".repeat(64);
        assert!(
            insert_mcp_candidate(&mut found, declaration.syntax_id, conflict)
                .unwrap_err()
                .to_string()
                .contains("conflicting duplicate native hint")
        );
    }

    #[test]
    fn expired_optional_sqlite_work_interrupts_and_restores_selected_connection() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        let ticks = std::cell::Cell::new(0);
        let start = std::time::Instant::now();
        let clock = || {
            let tick = ticks.get();
            ticks.set(tick + 1);
            if tick < 2 {
                start
            } else {
                start + std::time::Duration::from_secs(61)
            }
        };
        let progress = OptionalHintProgress {
            now: &clock,
            end: start + std::time::Duration::from_secs(60),
        };
        let guard = install_optional_hint_progress(&db, &progress);
        let error = db
            .query_row(
                "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<100000) SELECT sum(x) FROM n",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_err();
        assert!(optional_hint_interrupt(&error, &progress), "{error}");
        drop(guard);
        assert_eq!(
            db.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn unicode_callee_clips_on_utf8_scalar_boundary() {
        let text = format!("{}éz", "a".repeat(1023));
        let range = crate::native_evidence::Range {
            start: 0,
            end: text.len(),
        };
        let (shown, clipped) = mcp_callee_text(&text, Some(&range)).unwrap();
        assert_eq!(shown.unwrap().len(), 1023);
        assert!(clipped);
        let range = crate::native_evidence::Range {
            start: 1023,
            end: 1025,
        };
        assert_eq!(
            mcp_callee_text(&text, Some(&range)).unwrap(),
            (Some("é".into()), false)
        );
        assert_eq!(mcp_callee_text(&text, None).unwrap(), (None, false));
        assert!(
            mcp_callee_text(
                &text,
                Some(&crate::native_evidence::Range {
                    start: 1024,
                    end: 1025
                })
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod source_range_tests {
    use super::mcp_lf_range;

    #[test]
    fn lf_inclusive_final_terminator_crlf_empty_and_exact_caps() {
        let bytes = b"a\r\nb\n";
        assert_eq!(mcp_lf_range(bytes, 1, 1).unwrap(), (0, 3));
        assert_eq!(mcp_lf_range(bytes, 2, 2).unwrap(), (3, 5));
        assert_eq!(mcp_lf_range(bytes, 1, 2).unwrap(), (0, 5));
        assert!(mcp_lf_range(bytes, 3, 3).is_err(), "no phantom final line");
        assert!(mcp_lf_range(b"", 1, 1).is_err());
        assert_eq!(mcp_lf_range(b"a", 1, 1).unwrap(), (0, 1));
        assert_eq!(mcp_lf_range(b"\n", 1, 1).unwrap(), (0, 1));
        assert!(mcp_lf_range(b"\n", 2, 2).is_err());
        assert_eq!(mcp_lf_range(&vec![b'a'; 16384], 1, 1).unwrap(), (0, 16384));
        assert!(mcp_lf_range(&vec![b'a'; 16385], 1, 1).is_err());
        let two_hundred = "x\n".repeat(200);
        assert_eq!(
            mcp_lf_range(two_hundred.as_bytes(), 1, 200).unwrap(),
            (0, 400)
        );
        assert!(mcp_lf_range(two_hundred.as_bytes(), 1, 201).is_err());
        assert!(mcp_lf_range(b"x", 2, 1).is_err());
        assert!(mcp_lf_range(b"x", 0, 1).is_err());
    }
}
