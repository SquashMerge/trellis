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
                _ => {
                    return Err(
                        self.corrupt_evidence("store_unavailable: invalid selected language")
                    );
                }
            };
            let category = if chosen {
                match state.as_str() {
                    "complete" => 1,
                    "partial" => 2,
                    "failed" => 3,
                    _ => {
                        return Err(
                            self.corrupt_evidence("store_unavailable: invalid selected coverage")
                        );
                    }
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
        // A length disagreeing with the BLOB can only arrive as on-disk
        // corruption; the schema's CHECK refuses to write it.
        if fault == "bad_length" {
            db.pragma_update(None, "ignore_check_constraints", "ON")?;
        }
        let tx = storage_result(db.transaction_with_behavior(TransactionBehavior::Immediate))?;
        let count = match fault {
            "missing_scope" => tx.execute("UPDATE document_versions SET path='missing.js' WHERE path='a.js'", [])?,
            "missing_header" => tx.execute("DELETE FROM native_version_headers WHERE syntax_id=(SELECT syntax_id FROM native_version_declarations WHERE name='Alpha' LIMIT 1)", [])?,
            "bad_digest" => tx.execute("UPDATE document_versions SET content_hash=?1 WHERE path='a.js'", ["0".repeat(64)])?,
            "bad_length" => tx.execute("UPDATE document_versions SET byte_length=byte_length+1 WHERE path='a.js'", [])?,
            _ => anyhow::bail!("unknown find corruption fixture"),
        };
        ensure!(count == 1, "fixture corruption target absent");
        storage_result(tx.commit())?;
        if fault == "missing_scope" {
            db.pragma_update(None, "foreign_keys", "ON")?;
        }
        if fault == "bad_length" {
            db.pragma_update(None, "ignore_check_constraints", "OFF")?;
        }
        Ok(())
    }

    /// Fixture-only invalid keyed caller role, using Store's cached writer.
    pub fn mcp_corrupt_caller_role_for_tests(
        &self,
        writer: &topology::WriterGuard,
        name: &str,
    ) -> Result<()> {
        writer.belongs_to(&self.identity)?;
        self.identity.verify()?;
        writer.verify()?;
        let mut db = self.cache_write()?;
        db.pragma_update(None, "ignore_check_constraints", "ON")?;
        let result = (|| -> Result<()> {
            let tx = storage_result(db.transaction_with_behavior(TransactionBehavior::Immediate))?;
            let changed = storage_result(tx.execute(
                "UPDATE native_version_declarations SET role='invalid' WHERE name=?1 AND vid IN (SELECT vid FROM revision_documents WHERE revision_id=(SELECT 'pin:v1:'||index_generation||':'||index_revision FROM index_metadata WHERE singleton=1))",
                [name],
            ))?;
            ensure!(changed == 1, "fixture caller absent");
            storage_result(tx.commit())?;
            Ok(())
        })();
        db.pragma_update(None, "ignore_check_constraints", "OFF")?;
        result
    }

    /// Fixture-only foreign indexed call key, through Store's cached writer.
    pub fn mcp_rekey_call_language_for_tests(
        &self,
        writer: &topology::WriterGuard,
        path: &str,
        language: &str,
    ) -> Result<()> {
        writer.belongs_to(&self.identity)?;
        self.identity.verify()?;
        writer.verify()?;
        let mut db = self.cache_write()?;
        let tx = storage_result(db.transaction_with_behavior(TransactionBehavior::Immediate))?;
        let changed = storage_result(tx.execute(
            "UPDATE native_version_calls SET language=?1 WHERE vid=(SELECT vid FROM revision_documents WHERE revision_id=(SELECT 'pin:v1:'||index_generation||':'||index_revision FROM index_metadata WHERE singleton=1) AND path=?2) AND callee_lookup_key IS NOT NULL",
            params![language, path],
        ))?;
        ensure!(changed == 1, "fixture call absent");
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

fn mcp_lf_lines(bytes: &[u8]) -> impl Iterator<Item = (usize, usize)> + '_ {
    bytes
        .split_inclusive(|byte| *byte == b'\n')
        .scan(0, |start, line| {
            let bounds = (*start, *start + line.len());
            *start = bounds.1;
            Some(bounds)
        })
}

struct McpSelectedLines {
    bytes: Vec<u8>,
    // Sparse by both line and byte count, so neither long lines nor LF-only
    // source cause repeated full scans or unbounded per-line index allocation.
    checkpoints: Vec<(usize, u64)>,
}

impl McpSelectedLines {
    fn new(bytes: Vec<u8>) -> Self {
        let mut checkpoints = vec![(0, 1)];
        let mut lines = 1_u64;
        for (at, byte) in bytes.iter().enumerate() {
            if at > 0 && at.is_multiple_of(65536) {
                checkpoints.push((at, lines));
            }
            if *byte == b'\n' {
                lines += 1;
                if (lines - 1).is_multiple_of(256) {
                    checkpoints.push((at + 1, lines));
                }
            }
        }
        Self { bytes, checkpoints }
    }

    fn range(&self, range: &crate::native_evidence::Range) -> Result<Option<McpLines>> {
        if !(range.start <= range.end && range.end <= self.bytes.len()) {
            return Err(SelectedIntegrity(
                "store_unavailable: selected byte range outside source".into(),
            )
            .into());
        }
        if self.bytes.is_empty() {
            return Ok(None);
        }
        let line_at = |offset: usize| -> u64 {
            let offset = offset.min(self.bytes.len() - 1);
            let index = self
                .checkpoints
                .partition_point(|&(start, _)| start <= offset)
                - 1;
            let (start, line) = self.checkpoints[index];
            line + self.bytes[start..offset]
                .iter()
                .filter(|&&byte| byte == b'\n')
                .count() as u64
        };
        Ok(Some(McpLines {
            start: line_at(range.start),
            end: line_at(if range.end > range.start {
                range.end - 1
            } else {
                range.start
            }),
        }))
    }
}

#[cfg(test)]
fn mcp_lines_for_range(
    bytes: &[u8],
    range: &crate::native_evidence::Range,
) -> Result<Option<McpLines>> {
    McpSelectedLines::new(bytes.to_vec()).range(range)
}

fn mcp_lf_range(bytes: &[u8], start: u64, end: u64) -> Result<(usize, usize)> {
    ensure!(
        start > 0 && end >= start,
        "invalid_request: reversed source range"
    );
    ensure!(end - start < 200, "range_too_large: source line cap");
    let first = usize::try_from(start - 1)
        .ok()
        .and_then(|index| mcp_lf_lines(bytes).nth(index));
    let last = usize::try_from(end - 1)
        .ok()
        .and_then(|index| mcp_lf_lines(bytes).nth(index));
    let (from, to) = first
        .map(|(from, _)| from)
        .zip(last.map(|(_, to)| to))
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

/// Full selected document-version identity for compact MCP document references.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpDocumentVersion {
    pub document: crate::native_evidence::DocumentKey,
    pub revision_id: String,
    pub content_hash: String,
    pub provenance_id: String,
}

/// Inclusive, 1-based LF-delimited source lines in a selected cached BLOB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct McpLines {
    pub start: u64,
    pub end: u64,
}

pub struct McpItemSource {
    pub document: McpDocumentVersion,
    pub lines: Option<McpLines>,
}

struct McpCheckedSource {
    document: McpDocumentVersion,
    lines: McpSelectedLines,
}

impl McpCheckedSource {
    fn item(&self, range: &crate::native_evidence::Range) -> Result<McpItemSource> {
        Ok(McpItemSource {
            document: self.document.clone(),
            lines: self.lines.range(range)?,
        })
    }
}

impl EvidenceResponse {
    fn mcp_checked_source<'a>(
        &self,
        cache: &'a mut BTreeMap<crate::native_evidence::DocumentKey, McpCheckedSource>,
        selected: &ReadRevision,
        declaration: &crate::native_evidence::Declaration,
        hash: &str,
        provenance: &crate::native_evidence::Provenance,
    ) -> Result<&'a McpCheckedSource> {
        if !cache.contains_key(&declaration.document) {
            let source = self.mcp_document_version(selected, declaration, hash, provenance)?;
            cache.insert(declaration.document.clone(), source);
        }
        let source = &cache[&declaration.document];
        if !(source.document.revision_id == declaration.revision_id
            && source.document.provenance_id == declaration.provenance_id
            && source.document.content_hash == hash
            && provenance.document == source.document.document
            && provenance.revision_id == source.document.revision_id
            && provenance.id == source.document.provenance_id
            && provenance.content_hash == source.document.content_hash)
        {
            return Err(
                self.corrupt_evidence("store_unavailable: selected document proof mismatch")
            );
        }
        Ok(source)
    }

    fn mcp_document_version(
        &self,
        selected: &ReadRevision,
        declaration: &crate::native_evidence::Declaration,
        hash: &str,
        provenance: &crate::native_evidence::Provenance,
    ) -> Result<McpCheckedSource> {
        #[cfg(test)]
        super::count_selected_read(|counts| counts.source_reads += 1);
        use sha2::{Digest, Sha256};
        let scope =
            v8_native_scope_for(&self.db, &declaration.document, selected)?.ok_or_else(|| {
                self.corrupt_evidence("store_unavailable: selected document scope absent")
            })?;
        if !(declaration.revision_id == scope.native_revision_id
            && declaration.provenance_id == scope.proof_id()
            && provenance.document == declaration.document
            && provenance.revision_id == declaration.revision_id
            && provenance.id == declaration.provenance_id
            && provenance.content_hash == hash)
        {
            return Err(
                self.corrupt_evidence("store_unavailable: selected document proof mismatch")
            );
        }
        let row: Option<(String, String, String, String, i64, Vec<u8>)> = self
            .db
            .query_row(
                "SELECT s.id,d.language,d.path,d.content_hash,d.byte_length,d.source_bytes
             FROM revision_documents m JOIN document_versions d ON d.vid=m.vid
             JOIN native_source_sets s ON 1=1
             WHERE m.revision_id=?1 AND d.id=?2
               AND s.id=?3 AND m.language=?4 AND m.path=?5
               AND d.language=m.language AND d.path=m.path",
                params![
                    selected.key,
                    scope.version_id,
                    scope.key.source_set_id,
                    scope.key.language,
                    scope.key.path
                ],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?;
        let (source_set, language, path, stored_hash, length, bytes) = row.ok_or_else(|| {
            self.corrupt_evidence("store_unavailable: selected raw source version absent")
        })?;
        if !(source_set == declaration.document.source_set_id
            && language == declaration.document.language
            && path == declaration.document.path
            && stored_hash == hash)
        {
            return Err(
                self.corrupt_evidence("store_unavailable: selected raw source identity mismatch")
            );
        }
        if usize::try_from(length).ok() != Some(bytes.len())
            || stored_hash != hex::encode(Sha256::digest(&bytes))
        {
            return Err(self.corrupt_evidence(
                "store_unavailable: selected raw source length or digest mismatch",
            ));
        }
        Ok(McpCheckedSource {
            document: McpDocumentVersion {
                document: declaration.document.clone(),
                revision_id: declaration.revision_id.clone(),
                content_hash: stored_hash,
                provenance_id: declaration.provenance_id.clone(),
            },
            lines: McpSelectedLines::new(bytes),
        })
    }
}

/// A find result: the emitted hits, each with its document's coverage tuple,
/// and per-language counts of every native coverage tuple in the searched
/// scope (#156). The counts are independent of query, limit and clipping.
pub struct McpFind {
    pub symbols: Vec<McpFindSymbol>,
    /// Per language (java, rust, python, javascript):
    /// [selected, complete, partial, failed, unselected].
    pub scope: [[u64; 5]; 4],
    pub provenance: Vec<crate::native_evidence::Provenance>,
    pub has_more: bool,
    pub pin: IndexPin,
}

pub struct McpFindSymbol {
    pub declaration: crate::native_evidence::Declaration,
    pub content_hash: String,
    /// The native coverage tuple of the symbol's document.
    pub coverage: crate::native_evidence::Coverage,
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
                "SELECT s.id,m.language,m.path FROM revision_documents m
            JOIN native_source_sets s ON 1=1
            WHERE m.revision_id=?1 ORDER BY s.id,m.language,m.path",
            )?
            .query_map([&selected.key], |row| {
                Ok(DocumentKey {
                    source_set_id: row.get(0)?,
                    language: row.get(1)?,
                    path: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // Every searched tuple is still validated: corrupt required scope
        // evidence fails closed, even though only counts leave this layer.
        let mut coverage = BTreeMap::new();
        let mut scopes = BTreeMap::new();
        for key in keys {
            let scope = v8_native_scope_for(&self.db, &key, &selected)?.ok_or_else(|| {
                self.corrupt_evidence("store_unavailable: missing native coverage scope")
            })?;
            let row = read_native_coverage_v8(&self.db, &scope)?;
            if !(row.revision_id == scope.native_revision_id
                && row.producer_id == scope.producer_id
                && row.language == key.language
                && row.source_set_id == key.source_set_id
                && row.document_path == key.path)
            {
                return Err(
                    self.corrupt_evidence("store_unavailable: native coverage identity mismatch")
                );
            }
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
            if !(valid_state
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
                    .all(|role| matches!(role.as_str(), "definition" | "call")))
            {
                return Err(
                    self.corrupt_evidence("store_unavailable: inconsistent native coverage tuple")
                );
            }
            coverage.insert(scope.version_id.clone(), row);
            scopes.insert(scope.version_id.clone(), scope);
        }
        let mut scope_counts = [[0_u64; 5]; 4];
        for row in coverage.values() {
            let language = match row.language.as_str() {
                "java" => 0,
                "rust" => 1,
                "python" => 2,
                "javascript" => 3,
                _ => {
                    return Err(self
                        .corrupt_evidence("store_unavailable: invalid native coverage language"));
                }
            };
            let counts = &mut scope_counts[language];
            if row.selected {
                counts[0] += 1;
                counts[match row.state.as_str() {
                    "complete" => 1,
                    "partial" => 2,
                    _ => 3,
                }] += 1;
            } else {
                counts[4] += 1;
            }
        }
        let hits = super::symbol_trigrams::w2_hits(
            &self.db,
            &selected.key,
            query,
            i64::try_from(limit + 1)?,
        )?;
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
                .ok_or_else(|| self.corrupt_evidence("store_unavailable: find scope absent"))?;
            if !loaded.contains_key(&version) {
                let declarations = read_native_declarations_v8(&self.db, scope, None, true)?
                    .into_iter()
                    .map(|d| (d.syntax_id.clone(), d))
                    .collect();
                let hash: String = self.db.query_row(
                    "SELECT content_hash FROM document_versions d JOIN native_source_sets s ON s.id=?2 WHERE d.id=?1 AND d.language=?3 AND d.path=?4",
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
                .ok_or_else(|| {
                    self.corrupt_evidence("store_unavailable: selected declaration absent")
                })?
                .clone();
            if !(declaration.provenance_id == scope.proof_id()
                && declaration.revision_id == scope.native_revision_id)
            {
                return Err(self.corrupt_evidence("store_unavailable: native proof mismatch"));
            }
            symbols.push(McpFindSymbol {
                declaration,
                content_hash: hash.clone(),
                coverage: coverage
                    .get(&version)
                    .ok_or_else(|| {
                        self.corrupt_evidence("store_unavailable: find coverage absent")
                    })?
                    .clone(),
            });
        }
        provenance.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(McpFind {
            symbols,
            scope: scope_counts,
            provenance,
            has_more,
            pin: selected.pin,
        })
    }
}

pub struct McpFindSymbolWithLines {
    pub symbol: McpFindSymbol,
    pub source: McpItemSource,
}

pub struct McpFindWithLines {
    pub symbols: Vec<McpFindSymbolWithLines>,
    pub scope: [[u64; 5]; 4],
    pub provenance: Vec<crate::native_evidence::Provenance>,
    pub has_more: bool,
    pub pin: IndexPin,
}

impl EvidenceResponse {
    pub fn mcp_find_with_lines(
        &self,
        query: &str,
        limit: usize,
        expected: Option<IndexPin>,
    ) -> Result<McpFindWithLines> {
        let found = self.mcp_find(query, limit, expected)?;
        let selected = self
            .store
            .read_revision_for(&self.db, Some(found.pin), true)?;
        // Hits from one document may be interleaved with others in rank order.
        // Visit them document by document so at most one raw BLOB is held (#165).
        let mut by_document: BTreeMap<&crate::native_evidence::DocumentKey, Vec<usize>> =
            BTreeMap::new();
        for (index, symbol) in found.symbols.iter().enumerate() {
            by_document
                .entry(&symbol.declaration.document)
                .or_default()
                .push(index);
        }
        let mut sources: Vec<Option<McpItemSource>> = std::iter::repeat_with(|| None)
            .take(found.symbols.len())
            .collect();
        for indexes in by_document.into_values() {
            let mut source_cache = BTreeMap::new();
            for index in indexes {
                let symbol = &found.symbols[index];
                let proof = found
                    .provenance
                    .iter()
                    .find(|proof| proof.id == symbol.declaration.provenance_id)
                    .ok_or_else(|| {
                        self.corrupt_evidence("store_unavailable: find provenance absent")
                    })?;
                sources[index] = Some(
                    self.mcp_checked_source(
                        &mut source_cache,
                        &selected,
                        &symbol.declaration,
                        &symbol.content_hash,
                        proof,
                    )?
                    .item(&symbol.declaration.range)?,
                );
            }
        }
        let symbols = found
            .symbols
            .into_iter()
            .zip(sources)
            .map(|(symbol, source)| McpFindSymbolWithLines {
                symbol,
                source: source.expect("every hit belongs to one document group"),
            })
            .collect();
        Ok(McpFindWithLines {
            symbols,
            scope: found.scope,
            provenance: found.provenance,
            has_more: found.has_more,
            pin: found.pin,
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
        self.mcp_declaration_selected(symbol_id, &selected)
    }

    /// Stored evidence on a required path that disagrees with its selected
    /// scope is corruption: it fails typed and schedules a re-index (#261).
    fn corrupt_evidence(&self, message: &'static str) -> anyhow::Error {
        SelectedIntegrity(message.into()).into()
    }

    fn mcp_declaration_selected(
        &self,
        symbol_id: &str,
        selected: &ReadRevision,
    ) -> Result<Option<McpDeclaration>> {
        use crate::native_evidence::Provenance;
        let Some(scope) = v8_owner_scope_for(&self.db, symbol_id, selected)? else {
            return Ok(None);
        };
        let declaration = read_native_declarations_v8(&self.db, &scope, None, true)?
            .into_iter()
            .find(|row| row.syntax_id == symbol_id)
            .ok_or_else(|| {
                self.corrupt_evidence("store_unavailable: selected declaration absent")
            })?;
        if !(declaration.provenance_id == scope.proof_id()
            && declaration.revision_id == scope.native_revision_id
            && declaration.document == scope.key)
        {
            return Err(
                self.corrupt_evidence("store_unavailable: selected declaration proof mismatch")
            );
        }
        let coverage = read_native_coverage_v8(&self.db, &scope)?;
        if !(coverage.producer_id == scope.producer_id
            && coverage.language == scope.key.language
            && coverage.source_set_id == scope.key.source_set_id
            && coverage.document_path == scope.key.path
            && coverage.revision_id == scope.native_revision_id
            && coverage.requested
            && coverage.selected
            && matches!(coverage.state.as_str(), "complete" | "partial" | "failed"))
        {
            return Err(
                self.corrupt_evidence("store_unavailable: selected declaration coverage mismatch")
            );
        }
        let hash: String = self.db.query_row(
            "SELECT content_hash FROM document_versions d JOIN native_source_sets s ON s.id=?2 WHERE d.id=?1 AND d.language=?3 AND d.path=?4",
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

pub struct McpDeclarationWithLines {
    pub selected: McpDeclaration,
    pub source: McpItemSource,
}

impl EvidenceResponse {
    pub fn mcp_declaration_with_lines(
        &self,
        symbol_id: &str,
        expected: Option<IndexPin>,
    ) -> Result<Option<McpDeclarationWithLines>> {
        let Some(declaration) = self.mcp_declaration(symbol_id, expected)? else {
            return Ok(None);
        };
        let selected = self
            .store
            .read_revision_for(&self.db, Some(declaration.pin), true)?;
        let proof = declaration
            .provenance
            .iter()
            .find(|proof| proof.id == declaration.declaration.provenance_id)
            .ok_or_else(|| {
                self.corrupt_evidence("store_unavailable: declaration provenance absent")
            })?;
        let source = self
            .mcp_document_version(
                &selected,
                &declaration.declaration,
                &declaration.content_hash,
                proof,
            )?
            .item(&declaration.declaration.range)?;
        Ok(Some(McpDeclarationWithLines {
            selected: declaration,
            source,
        }))
    }
}

/// An outgoing call and its optional, independently verified lexical suggestions.
pub struct McpOutgoingCall {
    pub call: crate::native_evidence::Call,
    pub content_hash: String,
    pub callee_text: Option<String>,
    pub callee_text_truncated: bool,
    /// At most eight verified hints, in rank order.
    pub candidates: Vec<McpOutgoingCandidate>,
    /// Distinct eligible matches left unexamined after eight hints passed, or
    /// `None` when the hint budget ran out.
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

pub struct McpOutgoingCallWithLines {
    pub selected: McpOutgoingCall,
    pub source: McpItemSource,
    /// Aligned with `selected.candidates`; candidates have no line ranges.
    pub candidate_documents: Vec<McpDocumentVersion>,
}

pub struct McpOutgoingWithLines {
    pub owner: McpDeclarationWithLines,
    pub calls: Vec<McpOutgoingCallWithLines>,
    pub has_more: bool,
}

fn mcp_callee_text(
    source: &str,
    range: Option<&crate::native_evidence::Range>,
) -> Result<(Option<String>, bool)> {
    let Some(range) = range else {
        return Ok((None, false));
    };
    let text = source.get(range.start..range.end).ok_or_else(|| {
        SelectedIntegrity("store_unavailable: invalid selected callee range".into())
    })?;
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
    fn optional_hint_failure(&self, error: anyhow::Error) -> Result<()> {
        if recovery_class(&error) == RecoveryClass::Hard {
            return Err(error);
        }
        self.report_optional_corruption(error);
        Ok(())
    }

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
        let scope = v8_owner_scope_for(&self.db, symbol_id, &selected)?.ok_or_else(|| {
            self.corrupt_evidence("store_unavailable: selected outgoing owner absent")
        })?;
        let source = self
            .store
            .selected_source_row_for(&self.db, &scope.key.path, &selected)?
            .ok_or_else(|| {
                self.corrupt_evidence("store_unavailable: selected outgoing source absent")
            })?;
        if !(source.hash == owner.content_hash) {
            return Err(
                self.corrupt_evidence("store_unavailable: selected outgoing source hash mismatch")
            );
        }
        let mut measured = read_native_calls_v8(&self.db, &scope, Some(symbol_id))?;
        for call in &measured {
            if !(call.owner_syntax_id == symbol_id
                && call.document == scope.key
                && call.revision_id == scope.native_revision_id
                && call.provenance_id == scope.proof_id())
            {
                return Err(
                    self.corrupt_evidence("store_unavailable: outgoing call proof mismatch")
                );
            }
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
        crate::mcp::phase_hook("outgoing_hint_start")?;
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
        crate::mcp::phase_hook("outgoing_hint")?;
        // SQLite checks inside long optional statements as well as between rows.
        // The read-only selected snapshot already holds its shared lock; no new
        // connection or lock transition is needed for these candidate SELECTs.
        let _progress_guard =
            (!calls.is_empty()).then(|| install_optional_hint_progress(&self.db, &progress));
        let owner_is_test = owner.declaration.role == crate::native_evidence::DeclarationRole::Test;
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
                crate::mcp::phase_hook("outgoing_hint_item_1")?;
            }
            #[cfg(not(feature = "test-support"))]
            let _ = item_index;
            if progress.expired() {
                item.omitted = None;
                continue;
            }
            let Some(lookup) = crate::native_evidence::verified_callee_lookup(
                &scope.key.language,
                &item.call,
                &source.text,
            ) else {
                continue;
            };
            let mut statement = match self.db.prepare(
                "SELECT v.id,d.syntax_id,s.id,m.language,m.path,
                        d.start_byte,d.end_byte,d.role
                 FROM native_version_declarations d JOIN revision_documents m
                   ON m.vid=d.vid AND m.revision_id=?1
                 JOIN document_versions v ON v.vid=d.vid
                 JOIN native_source_sets s ON s.id=?2
                 WHERE m.language=?3 AND d.lookup_key=?4",
            ) {
                Ok(statement) => statement,
                Err(error) if optional_hint_interrupt(&error, &progress) => {
                    item.omitted = None;
                    continue;
                }
                Err(error) => {
                    self.optional_hint_failure(error.into())?;
                    item.omitted = None;
                    continue;
                }
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
                Err(error) => {
                    self.optional_hint_failure(error.into())?;
                    item.omitted = None;
                    continue;
                }
            };
            // Rank the cheap lexical rows first, then verify in rank order only
            // until eight hints pass (#170); later matches are left unexamined.
            let mut ranked = Vec::new();
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
                    Err(error) => {
                        self.optional_hint_failure(error.into())?;
                        timed_out = true;
                        break;
                    }
                };
                let Some(row) = row else { break };
                #[cfg(feature = "test-support")]
                crate::mcp::phase_hook("outgoing_hint_row")?;
                if progress.expired() {
                    timed_out = true;
                    break;
                }
                let decoded = (|| -> Result<_> {
                    let version: String = row.get(0)?;
                    let id = id_codec::decode_sid(&row.get::<_, Vec<u8>>(1)?)?;
                    let key = DocumentKey {
                        source_set_id: row.get(2)?,
                        language: row.get(3)?,
                        path: row.get(4)?,
                    };
                    let (start, end): (i64, i64) = (row.get(5)?, row.get(6)?);
                    let role = crate::native_evidence::DeclarationRole::from_stored(
                        &row.get::<_, String>(7)?,
                    )
                    .map_err(|error| SelectedIntegrity(error.to_string()))?;
                    Ok((version, id, key, start, end, role))
                })();
                let (version, id, key, start, end, role) = match decoded {
                    Ok(row) => row,
                    Err(error) => {
                        self.optional_hint_failure(error)?;
                        continue;
                    }
                };
                // Test code is ineligible for non-test callers.
                if !owner_is_test && role == crate::native_evidence::DeclarationRole::Test {
                    continue;
                }
                ranked.push((
                    mcp_hint_proximity(&scope.key.path, &key.path),
                    key.path.clone(),
                    start,
                    end,
                    id,
                    version,
                    key,
                ));
            }
            if timed_out || progress.expired() {
                item.omitted = None;
                continue;
            }
            ranked.sort_by(|a, b| (a.0, &a.1, a.2, a.3, &a.4).cmp(&(b.0, &b.1, b.2, b.3, &b.4)));
            let mut candidates = BTreeMap::<String, McpOutgoingCandidate>::new();
            let mut unexamined = std::collections::BTreeSet::new();
            for (_, _, _, _, id, version, key) in ranked {
                if candidates.len() == 8 {
                    if !candidates.contains_key(&id) {
                        unexamined.insert(id);
                    }
                    continue;
                }
                if progress.expired() {
                    timed_out = true;
                    break;
                }
                let candidate = (|| -> Result<Option<McpOutgoingCandidate>> {
                    optional_hint_check(Some(&|| progress.expired()))?;
                    let candidate_scope = v8_native_scope_for(&self.db, &key, &selected)?
                        .ok_or_else(|| {
                            SelectedIntegrity("optional candidate scope absent".into())
                        })?;
                    if !(candidate_scope.version_id == version) {
                        return Err(SelectedIntegrity(
                            "optional candidate version mismatch".into(),
                        )
                        .into());
                    }
                    if !verified_versions.contains_key(&version) {
                        if progress.expired() {
                            return Ok(None);
                        }
                        #[cfg(feature = "test-support")]
                        crate::mcp::phase_hook("outgoing_hint_verify_version")?;
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
                        // Ineligible candidates are skipped. Corrupt candidates also
                        // schedule recovery after the required answer passes its fence.
                        let declaration = declarations
                            .iter()
                            .find(|row| row.syntax_id == id)
                            .ok_or_else(|| {
                                SelectedIntegrity("optional candidate declaration absent".into())
                            })?;
                        if !(coverage.revision_id == candidate_scope.native_revision_id
                            && coverage.producer_id == candidate_scope.producer_id
                            && coverage.language == key.language
                            && coverage.source_set_id == key.source_set_id
                            && coverage.document_path == key.path)
                        {
                            return Err(SelectedIntegrity(
                                "optional candidate coverage identity mismatch".into(),
                            )
                            .into());
                        }
                        if !coverage.requested
                            || !coverage.selected
                            || !matches!(coverage.state.as_str(), "complete" | "partial")
                        {
                            return Ok(None);
                        }
                        if declaration.name.is_none()
                            || declaration.lookup_key.as_deref() != Some(lookup.as_str())
                            || declaration.provenance_id != candidate_scope.proof_id()
                            || declaration.revision_id != candidate_scope.native_revision_id
                            || declaration.document != candidate_scope.key
                            || !mcp_canonical_syntax_id(&id)
                            || coverage.revision_id != candidate_scope.native_revision_id
                            || coverage.producer_id != candidate_scope.producer_id
                            || coverage.language != key.language
                            || coverage.source_set_id != key.source_set_id
                            || coverage.document_path != key.path
                        {
                            return Err(SelectedIntegrity(
                                "optional candidate evidence mismatch".into(),
                            )
                            .into());
                        }
                        let hash: String = self.db.query_row(
                            "SELECT content_hash FROM document_versions d JOIN native_source_sets s ON s.id=?2 WHERE d.id=?1 AND d.language=?3 AND d.path=?4",
                            params![version,key.source_set_id,key.language,key.path],|r|r.get(0))?;
                        if progress.expired() {
                            return Ok(None);
                        }
                        verified_versions.insert(version.clone(), (declarations, coverage, hash));
                    }
                    let (declarations, coverage, hash) = &verified_versions[&version];
                    let declaration = declarations
                        .iter()
                        .find(|row| row.syntax_id == id)
                        .ok_or_else(|| {
                            SelectedIntegrity("optional candidate declaration absent".into())
                        })?;
                    if declaration.name.is_none()
                        || declaration.lookup_key.as_deref() != Some(lookup.as_str())
                        || declaration.provenance_id != candidate_scope.proof_id()
                        || declaration.revision_id != candidate_scope.native_revision_id
                        || declaration.document != candidate_scope.key
                        || !mcp_canonical_syntax_id(&id)
                    {
                        return Err(SelectedIntegrity(
                            "optional candidate declaration mismatch".into(),
                        )
                        .into());
                    }
                    if coverage.revision_id != candidate_scope.native_revision_id
                        || coverage.producer_id != candidate_scope.producer_id
                        || coverage.language != key.language
                        || coverage.source_set_id != key.source_set_id
                        || coverage.document_path != key.path
                    {
                        return Err(SelectedIntegrity(
                            "optional candidate coverage mismatch".into(),
                        )
                        .into());
                    }
                    let proof_id = candidate_scope.proof_id();
                    if !(declaration.provenance_id == proof_id) {
                        return Err(
                            SelectedIntegrity("optional candidate proof mismatch".into()).into(),
                        );
                    }
                    let provenance = Provenance {
                        id: proof_id,
                        producer_id: candidate_scope.producer_id,
                        document: key,
                        revision_id: candidate_scope.native_revision_id,
                        content_hash: hash.clone(),
                        evidence_kind: "measuredSyntax".into(),
                        basis: None,
                        freshness: "fresh".into(),
                        derived_from: None,
                    };
                    if !(mcp_candidate_proof_matches(declaration, &provenance)) {
                        return Err(
                            SelectedIntegrity("optional candidate proof mismatch".into()).into(),
                        );
                    }
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
                        self.optional_hint_failure(error)?;
                        continue;
                    }
                };
                if let Err(error) = insert_mcp_candidate(&mut candidates, id, candidate) {
                    self.optional_hint_failure(error)?;
                }
            }
            if timed_out || progress.expired() {
                item.omitted = None;
                continue;
            }
            item.candidates = candidates.into_values().collect();
            item.candidates.sort_by(|a, b| {
                let key = |c: &McpOutgoingCandidate| {
                    let d = &c.declaration;
                    (
                        mcp_hint_proximity(&scope.key.path, &d.document.path),
                        d.document.path.clone(),
                        d.range.start,
                        d.range.end,
                        d.syntax_id.clone(),
                    )
                };
                key(a).cmp(&key(b))
            });
            // Distinct eligible matches left unexamined once eight hints passed.
            item.omitted = Some(unexamined.len());
        }
        Ok(Some(McpOutgoing {
            owner,
            calls,
            has_more,
        }))
    }
}

impl EvidenceResponse {
    pub fn mcp_outgoing_with_lines(
        &self,
        symbol_id: &str,
        limit: usize,
        expected: Option<IndexPin>,
        hint_now: &dyn Fn() -> std::time::Instant,
        required_deadline: std::time::Instant,
    ) -> Result<Option<McpOutgoingWithLines>> {
        let Some(outgoing) =
            self.mcp_outgoing(symbol_id, limit, expected, hint_now, required_deadline)?
        else {
            return Ok(None);
        };
        let selected = self
            .store
            .read_revision_for(&self.db, Some(outgoing.owner.pin), true)?;
        let proof = outgoing
            .owner
            .provenance
            .iter()
            .find(|proof| proof.id == outgoing.owner.declaration.provenance_id)
            .ok_or_else(|| {
                self.corrupt_evidence("store_unavailable: outgoing owner provenance absent")
            })?;
        let mut source_cache = BTreeMap::new();
        let owner_source = self
            .mcp_checked_source(
                &mut source_cache,
                &selected,
                &outgoing.owner.declaration,
                &outgoing.owner.content_hash,
                proof,
            )?
            .item(&outgoing.owner.declaration.range)?;
        let calls = outgoing
            .calls
            .into_iter()
            .map(|call| {
                if !(call.call.document == outgoing.owner.declaration.document
                    && call.call.revision_id == outgoing.owner.declaration.revision_id
                    && call.call.provenance_id == outgoing.owner.declaration.provenance_id)
                {
                    return Err(
                        self.corrupt_evidence("store_unavailable: outgoing call proof mismatch")
                    );
                }
                let source = self
                    .mcp_checked_source(
                        &mut source_cache,
                        &selected,
                        &outgoing.owner.declaration,
                        &call.content_hash,
                        proof,
                    )?
                    .item(&call.call.range)?;
                // The optional phase already validated each candidate's selected
                // scope, provenance and source hash inside its shared hint budget.
                let candidate_documents = call
                    .candidates
                    .iter()
                    .map(|candidate| McpDocumentVersion {
                        document: candidate.provenance.document.clone(),
                        revision_id: candidate.provenance.revision_id.clone(),
                        content_hash: candidate.provenance.content_hash.clone(),
                        provenance_id: candidate.provenance.id.clone(),
                    })
                    .collect();
                Ok(McpOutgoingCallWithLines {
                    selected: call,
                    source,
                    candidate_documents,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Some(McpOutgoingWithLines {
            owner: McpDeclarationWithLines {
                selected: outgoing.owner,
                source: owner_source,
            },
            calls,
            has_more: outgoing.has_more,
        }))
    }
}

/// Required syntax-tier reverse hits and their selected document evidence.
pub struct McpIncomingCallWithLines {
    pub call: crate::native_evidence::Call,
    pub caller: crate::native_evidence::Declaration,
    pub content_hash: String,
    pub source: McpItemSource,
    pub callee_text: String,
    pub coverage: crate::native_evidence::Coverage,
    pub provenance: crate::native_evidence::Provenance,
}

pub struct McpIncomingWithLines {
    pub selected: McpDeclarationWithLines,
    pub calls: Vec<McpIncomingCallWithLines>,
    pub same_name_declarations: u64,
    /// Per language (java, rust, python, javascript): selected, complete,
    /// partial, failed, unselected. Includes zero-hit scope documents.
    pub scope: [[u64; 5]; 4],
    pub has_more: bool,
}

impl EvidenceResponse {
    pub fn mcp_incoming_with_lines(
        &self,
        symbol_id: &str,
        limit: usize,
        expected: Option<IndexPin>,
    ) -> Result<Option<McpIncomingWithLines>> {
        use crate::native_evidence::{DeclarationRole, DocumentKey, Provenance};
        let selected = self.store.read_revision_for(&self.db, None, true)?;
        ensure!(
            expected.is_none_or(|pin| pin == selected.pin),
            "revision conflict: expected basis differs from admitted head"
        );
        let Some(owner) = self.mcp_declaration_selected(symbol_id, &selected)? else {
            return Ok(None);
        };
        let selected_key = &owner.declaration.document;
        let mut scope_counts = [[0_u64; 5]; 4];
        let language_index = match selected_key.language.as_str() {
            "java" => 0,
            "rust" => 1,
            "python" => 2,
            "javascript" => 3,
            _ => return Err(self.corrupt_evidence("store_unavailable: invalid selected language")),
        };
        let keys = self.db.prepare(
            "SELECT path FROM revision_documents m JOIN native_source_sets s ON s.id=?2 WHERE m.revision_id=?1 AND m.language=?3 ORDER BY path"
        )?.query_map(params![selected.key, selected_key.source_set_id, selected_key.language], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut scopes = BTreeMap::new();
        let mut coverages = BTreeMap::new();
        for path in keys {
            let key = DocumentKey {
                source_set_id: selected_key.source_set_id.clone(),
                language: selected_key.language.clone(),
                path,
            };
            let scope = v8_native_scope_for(&self.db, &key, &selected)?
                .ok_or_else(|| self.corrupt_evidence("store_unavailable: incoming scope absent"))?;
            let coverage = read_native_coverage_v8(&self.db, &scope)?;
            if !(coverage.revision_id == scope.native_revision_id
                && coverage.producer_id == scope.producer_id
                && coverage.language == key.language
                && coverage.source_set_id == key.source_set_id
                && coverage.document_path == key.path)
            {
                return Err(
                    self.corrupt_evidence("store_unavailable: incoming coverage identity mismatch")
                );
            }
            let valid = match coverage.state.as_str() {
                "notRequested" => {
                    !coverage.requested && !coverage.selected && coverage.diagnostic.is_none()
                }
                "omitted" | "unsupported" => {
                    coverage.requested && !coverage.selected && coverage.diagnostic.is_some()
                }
                "failed" | "partial" => {
                    coverage.requested && coverage.selected && coverage.diagnostic.is_some()
                }
                "complete" => {
                    coverage.requested && coverage.selected && coverage.diagnostic.is_none()
                }
                _ => false,
            };
            let manifest: (bool, String) = self.db.query_row(
                "SELECT m.coverage_selected,m.coverage_state FROM revision_documents m JOIN document_versions d ON d.vid=m.vid JOIN native_source_sets s ON s.id=?3 WHERE m.revision_id=?1 AND d.id=?2 AND m.language=?4 AND m.path=?5",
                params![selected.key, scope.version_id, key.source_set_id, key.language, key.path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if !(valid && manifest.0 == coverage.selected && manifest.1 == coverage.state) {
                return Err(self
                    .corrupt_evidence("store_unavailable: inconsistent incoming coverage tuple"));
            }
            let counts = &mut scope_counts[language_index];
            if coverage.selected {
                counts[0] += 1;
                counts[match coverage.state.as_str() {
                    "complete" => 1,
                    "partial" => 2,
                    _ => 3,
                }] += 1;
            } else {
                counts[4] += 1;
            }
            coverages.insert(scope.version_id.clone(), coverage);
            scopes.insert(scope.version_id.clone(), scope);
        }
        let proof = owner
            .provenance
            .iter()
            .find(|p| p.id == owner.declaration.provenance_id)
            .ok_or_else(|| {
                self.corrupt_evidence("store_unavailable: incoming selected provenance absent")
            })?;
        let mut source_cache = BTreeMap::new();
        let selected_source = self.mcp_checked_source(
            &mut source_cache,
            &selected,
            &owner.declaration,
            &owner.content_hash,
            proof,
        )?;
        let owner_lines = selected_source.item(&owner.declaration.range)?;
        let mut calls = Vec::new();
        let mut same_name_declarations = 0;
        let mut has_more = false;
        if let Some(lookup) = owner.declaration.lookup_key.as_deref() {
            // Count declarations across every eligible selected version, not calls
            // or only the limited response prefix.
            for (version, scope) in &scopes {
                if !matches!(coverages[version].state.as_str(), "complete" | "partial") {
                    continue;
                }
                let count: i64 = self.db.query_row(
                    "SELECT count(*) FROM native_version_declarations n JOIN document_versions d ON d.vid=n.vid WHERE d.id=?1 AND n.lookup_key=?2 AND n.name IS NOT NULL",
                    params![version, lookup], |r| r.get(0)
                )?;
                same_name_declarations += u64::try_from(count)?;
                if !(scope.key.source_set_id == selected_key.source_set_id) {
                    return Err(self
                        .corrupt_evidence("store_unavailable: incoming selected scope mismatch"));
                }
            }
            // A selected indexed call with a denormalized key that disagrees
            // with its selected version is corrupt, not a negative name match.
            let mismatched: bool = self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM revision_documents m
                 JOIN native_version_calls c ON c.vid=m.vid
                 JOIN native_source_sets s ON s.id=?2
                 WHERE m.revision_id=?1 AND m.language=?3
                   AND c.callee_lookup_key=?4
                   AND c.language<>m.language)",
                params![
                    selected.key,
                    selected_key.source_set_id,
                    selected_key.language,
                    lookup
                ],
                |r| r.get(0),
            )?;
            if !(!mismatched) {
                return Err(self
                    .corrupt_evidence("store_unavailable: incoming indexed call scope mismatch"));
            }
            // This uses the exact three-key native-call index. The manifest
            // selects one immutable version; no historical call is a fallback.
            let directory = selected_key
                .path
                .rsplit_once('/')
                .map_or("", |(dir, _)| dir);
            // SQLite orders the indexed matches before returning only the
            // required prefix. A directory match must not include descendants.
            let ranked = self
                .db
                .prepare(
                    "SELECT v.id,c.id,c.owner_syntax_id,m.path,d.role
                 FROM native_version_calls c
                 JOIN revision_documents m ON m.vid=c.vid AND m.revision_id=?1
                 JOIN document_versions v ON v.vid=c.vid
                 JOIN native_source_sets s ON s.id=?2
                 JOIN native_version_declarations d ON d.vid=c.vid AND d.syntax_id=c.owner_syntax_id
                 WHERE c.language=?3 AND c.callee_lookup_key=?4
                   AND m.language=c.language
                   AND m.coverage_selected=1 AND m.coverage_state IN ('complete','partial')
                 ORDER BY CASE d.role WHEN 'production' THEN 1 WHEN 'test' THEN 2 ELSE 0 END,
                   CASE WHEN m.path=?5 THEN 0
                     WHEN (?6='' AND instr(m.path,'/')=0)
                       OR (?6<>'' AND substr(m.path,1,length(?6)+1)=?6||'/'
                           AND instr(substr(m.path,length(?6)+2),'/')=0)
                     THEN 1 ELSE 2 END,
                   m.path COLLATE BINARY, c.start_byte, c.end_byte, c.id COLLATE BINARY
                 LIMIT ?7",
                )?
                .query_map(
                    params![
                        selected.key,
                        selected_key.source_set_id,
                        selected_key.language,
                        lookup,
                        selected_key.path,
                        directory,
                        i64::try_from(limit + 1)?
                    ],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, Vec<u8>>(1)?,
                            r.get::<_, Vec<u8>>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, String>(4)?,
                        ))
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            has_more = ranked.len() > limit;
            let mut loaded = BTreeMap::new();
            for (version, id_blob, caller_blob, path, stored_role) in ranked {
                let id = id_codec::decode_occ(&id_blob)?;
                let caller_id = id_codec::decode_sid(&caller_blob)?;
                let scope = scopes.get(&version).ok_or_else(|| {
                    self.corrupt_evidence("store_unavailable: incoming call scope absent")
                })?;
                if !(scope.key.path == path
                    && matches!(coverages[&version].state.as_str(), "complete" | "partial"))
                {
                    return Err(
                        self.corrupt_evidence("store_unavailable: incoming call scope mismatch")
                    );
                }
                let role = DeclarationRole::from_stored(&stored_role)
                    .map_err(|error| SelectedIntegrity(error.to_string()))?;
                if !loaded.contains_key(&version) {
                    let declarations = read_native_declarations_v8(&self.db, scope, None, true)?
                        .into_iter()
                        .map(|d| (d.syntax_id.clone(), d))
                        .collect::<BTreeMap<_, _>>();
                    let measured = read_native_calls_v8(&self.db, scope, None)?
                        .into_iter()
                        .map(|c| (c.id.clone(), c))
                        .collect::<BTreeMap<_, _>>();
                    let hash: String = self.db.query_row(
                        "SELECT content_hash FROM document_versions d JOIN native_source_sets s ON s.id=?2 WHERE d.id=?1 AND d.language=?3 AND d.path=?4",
                        params![version, scope.key.source_set_id, scope.key.language, scope.key.path], |r| r.get(0)
                    )?;
                    loaded.insert(version.clone(), (declarations, measured, hash));
                }
                let (declarations, measured, hash) = &loaded[&version];
                let caller = declarations.get(&caller_id).ok_or_else(|| {
                    self.corrupt_evidence("store_unavailable: incoming caller absent")
                })?;
                let call = measured.get(&id).ok_or_else(|| {
                    self.corrupt_evidence("store_unavailable: incoming call absent")
                })?;
                if !(caller.role == role
                    && caller.document == scope.key
                    && caller.revision_id == scope.native_revision_id
                    && caller.provenance_id == scope.proof_id()
                    && call.owner_syntax_id == caller.syntax_id
                    && call.document == scope.key
                    && call.revision_id == scope.native_revision_id
                    && call.provenance_id == scope.proof_id())
                {
                    return Err(self.corrupt_evidence(
                        "store_unavailable: incoming call or caller proof mismatch",
                    ));
                }
                let provenance = Provenance {
                    id: scope.proof_id(),
                    producer_id: scope.producer_id.clone(),
                    document: scope.key.clone(),
                    revision_id: scope.native_revision_id.clone(),
                    content_hash: hash.clone(),
                    evidence_kind: "measuredSyntax".into(),
                    basis: None,
                    freshness: "fresh".into(),
                    derived_from: None,
                };
                let checked = self.mcp_checked_source(
                    &mut source_cache,
                    &selected,
                    caller,
                    hash,
                    &provenance,
                )?;
                let bytes = &checked.lines.bytes;
                let text = std::str::from_utf8(bytes)
                    .map_err(|_| anyhow::anyhow!("store_unavailable: incoming source encoding"))?;
                let range = call.callee_range.as_ref().ok_or_else(|| {
                    self.corrupt_evidence("store_unavailable: incoming callee range absent")
                })?;
                let callee_text = text
                    .get(range.start..range.end)
                    .ok_or_else(|| {
                        self.corrupt_evidence("store_unavailable: incoming callee outside source")
                    })?
                    .to_owned();
                if calls.len() < limit {
                    calls.push(McpIncomingCallWithLines {
                        call: call.clone(),
                        caller: caller.clone(),
                        content_hash: hash.clone(),
                        source: checked.item(&call.range)?,
                        callee_text,
                        coverage: coverages[&version].clone(),
                        provenance,
                    });
                }
            }
        }
        Ok(Some(McpIncomingWithLines {
            selected: McpDeclarationWithLines {
                selected: owner,
                source: owner_lines,
            },
            calls,
            same_name_declarations,
            scope: scope_counts,
            has_more,
        }))
    }
}

/// Hint rank by location relative to the calling document: the same document,
/// then the same directory, then elsewhere (#167).
fn mcp_hint_proximity(owner: &str, candidate: &str) -> u8 {
    fn dir(path: &str) -> &str {
        path.rsplit_once('/').map_or("", |(dir, _)| dir)
    }
    if owner == candidate {
        0
    } else if dir(owner) == dir(candidate) {
        1
    } else {
        2
    }
}

fn mcp_candidate_proof_matches(
    declaration: &crate::native_evidence::Declaration,
    provenance: &crate::native_evidence::Provenance,
) -> bool {
    declaration.document == provenance.document
        && declaration.revision_id == provenance.revision_id
        && declaration.provenance_id == provenance.id
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
            if !(entry.get().declaration == candidate.declaration
                && entry.get().coverage == candidate.coverage
                && entry.get().provenance == candidate.provenance)
            {
                return Err(SelectedIntegrity(
                    "store_unavailable: conflicting duplicate native hint".into(),
                )
                .into());
            }
        }
    }
    Ok(())
}

fn mcp_canonical_syntax_id(id: &str) -> bool {
    id_codec::encode_sid(id).is_some()
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
            "revisionId":"revision:v2:test","kind":"function","role":"production","name":"Target",
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
        assert!(mcp_candidate_proof_matches(
            &candidate.declaration,
            &candidate.provenance
        ));
        let mut wrong_proof = candidate.provenance.clone();
        wrong_proof.id.push_str(":other");
        assert!(!mcp_candidate_proof_matches(
            &candidate.declaration,
            &wrong_proof
        ));
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
mod enriched_read_tests {
    use super::*;
    use crate::indexer::IndexOptions;
    use std::sync::{Arc, atomic::AtomicBool};

    #[test]
    fn corrupt_ranked_ids_mark_rebuild_in_outgoing_hints_and_incoming_calls() {
        for fault in ["outgoing_hint_sid", "incoming_call_occ"] {
            // Each arm starts with a fresh selected index; a prior corrupt read
            // must not affect admission of the next fixture.
            let state = tempfile::tempdir().unwrap();
            let workspace = tempfile::tempdir().unwrap();
            std::fs::write(
                workspace.path().join("owner.js"),
                "function Owner() { Target(); }\n",
            )
            .unwrap();
            std::fs::write(workspace.path().join("target.js"), "function Target() {}\n").unwrap();
            let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
            let (pin, _) = crate::index_coordinator::reconcile_workspace(
                &store,
                &IndexOptions::new(workspace.path().to_owned()),
                &Arc::new(AtomicBool::new(false)),
                |_| {},
            )
            .unwrap();
            let read = store.evidence_response().unwrap();
            let id = |name| {
                read.mcp_find(name, 20, Some(pin))
                    .unwrap()
                    .symbols
                    .into_iter()
                    .find(|hit| hit.declaration.name.as_deref() == Some(name))
                    .unwrap()
                    .declaration
                    .syntax_id
            };
            let owner = id("Owner");
            let target = id("Target");
            let now = std::time::Instant::now();
            assert_eq!(
                read.mcp_outgoing(
                    &owner,
                    20,
                    Some(pin),
                    &|| now,
                    now + std::time::Duration::from_secs(5)
                )
                .unwrap()
                .unwrap()
                .calls[0]
                    .candidates
                    .len(),
                1,
                "{fault}: fixture needs an optional candidate"
            );
            assert_eq!(
                read.mcp_incoming_with_lines(&target, 20, Some(pin))
                    .unwrap()
                    .unwrap()
                    .calls
                    .len(),
                1,
                "{fault}: fixture needs a ranked incoming call"
            );
            read.finish(()).unwrap();
            drop(read);

            // Simulate one corrupt stored ID using Store's connection. Turning
            // off foreign keys allows a malformed key in otherwise valid rows.
            {
                let db = store.cache_write().unwrap();
                db.pragma_update(None, "foreign_keys", "OFF").unwrap();
                let (sql, expected) = match fault {
                    "outgoing_hint_sid" => (
                        "UPDATE native_version_declarations SET syntax_id=x'00' WHERE name='Target'",
                        1,
                    ),
                    "incoming_call_occ" => (
                        "UPDATE native_version_calls SET id=x'00' WHERE callee_lookup_key='Target'",
                        1,
                    ),
                    _ => unreachable!(),
                };
                assert_eq!(db.execute(sql, []).unwrap(), expected, "{fault}");
                db.pragma_update(None, "foreign_keys", "ON").unwrap();
            }
            let read = store.evidence_response().unwrap();
            if fault == "outgoing_hint_sid" {
                let outgoing = read
                    .mcp_outgoing_with_lines(
                        &owner,
                        20,
                        Some(pin),
                        &|| now,
                        now + std::time::Duration::from_secs(5),
                    )
                    .unwrap()
                    .unwrap();
                assert_eq!(outgoing.calls.len(), 1);
                assert!(outgoing.calls[0].selected.candidates.is_empty());
                assert_eq!(store.disposition(), RecoveryDisposition::Ready);
                read.finish(()).unwrap();
            } else {
                let error = read
                    .mcp_incoming_with_lines(&target, 20, Some(pin))
                    .map_err(|error| read.store.report_index_failure(error))
                    .err()
                    .expect("corrupt incoming call must fail");
                assert!(Store::is_rebuild_failure(&error), "{fault}: {error:#}");
            }
            assert_eq!(store.disposition(), RecoveryDisposition::Rebuild, "{fault}");
        }
    }

    #[test]
    fn required_mcp_faults_rebuild_and_reread_cleanly() {
        for (fault, sql) in [
            (
                "find_scope",
                "UPDATE document_versions SET path='missing.js' WHERE path='owner.js'",
            ),
            (
                "find_row_type",
                "UPDATE native_version_declarations SET start_byte=x'00' WHERE name='Owner'",
            ),
            (
                "language_counts",
                "UPDATE revision_documents SET coverage_state='invalid' WHERE path='owner.js'",
            ),
            (
                "outgoing_range",
                "UPDATE native_version_calls SET callee_end=9999 WHERE callee_lookup_key='Target'",
            ),
            (
                "incoming_callee",
                "UPDATE native_version_calls SET callee_start=NULL,callee_end=NULL WHERE callee_lookup_key='Target'",
            ),
            (
                "source_digest",
                "UPDATE document_versions SET content_hash=printf('%064d',0) WHERE path='owner.js'",
            ),
        ] {
            let state = tempfile::tempdir().unwrap();
            let workspace = tempfile::tempdir().unwrap();
            std::fs::write(
                workspace.path().join("owner.js"),
                "function Owner() { Target(); }\n",
            )
            .unwrap();
            std::fs::write(workspace.path().join("target.js"), "function Target() {}\n").unwrap();
            let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
            let options = IndexOptions::new(workspace.path().to_owned());
            let (pin, _) = crate::index_coordinator::reconcile_workspace(
                &store,
                &options,
                &Arc::new(AtomicBool::new(false)),
                |_| {},
            )
            .unwrap();
            let read = store.evidence_response().unwrap();
            let ids = read
                .mcp_find("", 20, Some(pin))
                .unwrap()
                .symbols
                .into_iter()
                .filter_map(|hit| {
                    hit.declaration
                        .name
                        .map(|name| (name, hit.declaration.syntax_id))
                })
                .collect::<BTreeMap<_, _>>();
            read.finish(()).unwrap();
            drop(read);
            {
                let db = store.cache_write().unwrap();
                db.pragma_update(None, "foreign_keys", "OFF").unwrap();
                db.pragma_update(None, "ignore_check_constraints", "ON")
                    .unwrap();
                assert_eq!(db.execute(sql, []).unwrap(), 1, "{fault}");
                db.pragma_update(None, "ignore_check_constraints", "OFF")
                    .unwrap();
                db.pragma_update(None, "foreign_keys", "ON").unwrap();
            }
            let read = store.evidence_response().unwrap();
            let now = std::time::Instant::now();
            let result = match fault {
                "find_scope" | "find_row_type" => {
                    read.mcp_find_with_lines("Owner", 20, Some(pin)).map(|_| ())
                }
                "language_counts" => read.mcp_language_counts().map(|_| ()),
                "outgoing_range" => read
                    .mcp_outgoing_with_lines(
                        &ids["Owner"],
                        20,
                        Some(pin),
                        &|| now,
                        now + std::time::Duration::from_secs(5),
                    )
                    .map(|_| ()),
                "incoming_callee" => read
                    .mcp_incoming_with_lines(&ids["Target"], 20, Some(pin))
                    .map(|_| ()),
                "source_digest" => read
                    .mcp_source("owner.js", 1, 1, Some(pin), None)
                    .map(|_| ()),
                _ => unreachable!(),
            }
            .map_err(|error| read.store.report_index_failure(error));
            let error = result.expect_err(fault);
            assert!(Store::is_rebuild_failure(&error), "{fault}: {error:#}");
            assert_eq!(store.disposition(), RecoveryDisposition::Rebuild, "{fault}");
            drop(read);
            let (rebuilt, _) = crate::index_coordinator::reconcile_workspace(
                &store,
                &options,
                &Arc::new(AtomicBool::new(false)),
                |_| {},
            )
            .unwrap_or_else(|error| panic!("{fault}: {error:#}"));
            let read = store.evidence_response().unwrap();
            assert_eq!(
                read.mcp_find_with_lines("Owner", 20, Some(rebuilt))
                    .unwrap()
                    .symbols
                    .len(),
                1,
                "{fault}"
            );
            let owner = &ids["Owner"];
            assert!(
                read.mcp_declaration_with_lines(owner, Some(rebuilt))
                    .unwrap()
                    .is_some(),
                "{fault}"
            );
            match fault {
                "incoming_callee" => assert_eq!(
                    read.mcp_incoming_with_lines(&ids["Target"], 20, Some(rebuilt))
                        .unwrap()
                        .unwrap()
                        .calls
                        .len(),
                    1
                ),
                "source_digest" => assert_eq!(
                    read.mcp_source("owner.js", 1, 1, Some(rebuilt), None)
                        .unwrap()
                        .unwrap()
                        .text,
                    "function Owner() { Target(); }\n"
                ),
                "outgoing_range" => assert_eq!(
                    read.mcp_outgoing_with_lines(
                        &ids["Owner"],
                        20,
                        Some(rebuilt),
                        &|| now,
                        now + std::time::Duration::from_secs(5)
                    )
                    .unwrap()
                    .unwrap()
                    .calls
                    .len(),
                    1
                ),
                "language_counts" => assert_eq!(read.mcp_language_counts().unwrap()[3][0], 2),
                _ => {}
            }
            read.finish(()).unwrap();
            assert_eq!(store.disposition(), RecoveryDisposition::Ready, "{fault}");
        }
    }

    #[test]
    fn optional_sql_failures_only_defer_typed_corruption() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("a.js"), "function A() {}\n").unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        crate::index_coordinator::reconcile_workspace(
            &store,
            &IndexOptions::new(workspace.path().to_owned()),
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        for code in [
            rusqlite::ffi::SQLITE_BUSY,
            rusqlite::ffi::SQLITE_LOCKED,
            rusqlite::ffi::SQLITE_IOERR,
            rusqlite::ffi::SQLITE_FULL,
            rusqlite::ffi::SQLITE_INTERRUPT,
        ] {
            let read = store.evidence_response().unwrap();
            let error = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None);
            let returned = read.optional_hint_failure(error.into()).unwrap_err();
            assert!(matches!(returned.downcast_ref::<rusqlite::Error>(),
                Some(rusqlite::Error::SqliteFailure(info, _)) if info.extended_code == code));
            read.finish(()).unwrap();
            assert_eq!(store.disposition(), RecoveryDisposition::Ready);
        }
        let read = store.evidence_response().unwrap();
        let corrupt = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            None,
        );
        read.optional_hint_failure(corrupt.into()).unwrap();
        assert_eq!(store.disposition(), RecoveryDisposition::Ready);
        read.finish(()).unwrap();
        assert_eq!(store.disposition(), RecoveryDisposition::RecreatePending);
    }

    #[test]
    fn request_range_and_selected_slice_encoding_do_not_mark_recovery() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("bad.js"), b"a\nb\n").unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let (pin, _) = crate::index_coordinator::reconcile_workspace(
            &store,
            &IndexOptions::new(workspace.path().to_owned()),
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        // Simulate a byte edit after validated publication, retaining a matching
        // whole-BLOB digest so only the requested slice's encoding is invalid.
        let bytes = b"a\n\xff\n";
        {
            use sha2::{Digest, Sha256};
            let db = store.cache_write().unwrap();
            assert_eq!(db.execute(
                "UPDATE document_versions SET source_bytes=?1,byte_length=?2,content_hash=?3 WHERE path='bad.js'",
                params![bytes.as_slice(), bytes.len() as i64, hex::encode(Sha256::digest(bytes))],
            ).unwrap(), 1);
        }
        let read = store.evidence_response().unwrap();
        assert_eq!(
            read.mcp_source("bad.js", 1, 1, Some(pin), None)
                .unwrap()
                .unwrap()
                .text,
            "a\n"
        );
        let error = read
            .mcp_source("bad.js", 2, 2, Some(pin), None)
            .err()
            .unwrap();
        assert!(
            error.to_string().starts_with("unsupported_encoding:"),
            "{error:#}"
        );
        let error = read
            .mcp_source("bad.js", 0, 1, Some(pin), None)
            .err()
            .unwrap();
        assert!(
            error.to_string().starts_with("invalid_request:"),
            "{error:#}"
        );
        assert_eq!(store.disposition(), RecoveryDisposition::Ready);
        read.finish(()).unwrap();
    }

    #[test]
    fn inconsistent_declaration_coverage_marks_rebuild() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("owner.js"), "function Owner() {}\n").unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let (pin, _) = crate::index_coordinator::reconcile_workspace(
            &store,
            &IndexOptions::new(workspace.path().to_owned()),
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let read = store.evidence_response().unwrap();
        let owner = read
            .mcp_find("Owner", 20, Some(pin))
            .unwrap()
            .symbols
            .into_iter()
            .find(|hit| hit.declaration.name.as_deref() == Some("Owner"))
            .unwrap()
            .declaration
            .syntax_id;
        read.finish(()).unwrap();
        drop(read);
        // SQL constraints admit an unselected 'complete' row with no
        // diagnostic, so it decodes; the required declaration read rejects it.
        assert_eq!(
            store
                .cache_write()
                .unwrap()
                .execute(
                    "UPDATE revision_documents SET coverage_selected=0
                     WHERE path='owner.js' AND coverage_state='complete'
                       AND coverage_diagnostic IS NULL",
                    [],
                )
                .unwrap(),
            1
        );
        let read = store.evidence_response().unwrap();
        let error = read
            .mcp_declaration(&owner, Some(pin))
            .map_err(|error| read.store.report_index_failure(error))
            .err()
            .expect("inconsistent coverage must fail");
        assert_eq!(
            format!("{error:#}"),
            "store_unavailable: selected declaration coverage mismatch"
        );
        assert_eq!(store.disposition(), RecoveryDisposition::Rebuild);
    }

    #[test]
    fn stored_roles_hide_inline_rust_test_helpers_from_production_calls() {
        use crate::native_evidence::DeclarationRole;
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("lib.rs"),
            "fn ProdCaller() { Target(); }\n#[cfg(test)] mod tests { fn Target() {} fn TestCaller() { Target(); } }\n"
        ).unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let (pin, _) = crate::index_coordinator::reconcile_workspace(
            &store,
            &IndexOptions::new(workspace.path().to_owned()),
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let read = store.evidence_response().unwrap();
        let find = |name: &str| {
            read.mcp_find_with_lines(name, 20, Some(pin))
                .unwrap()
                .symbols
                .into_iter()
                .find(|row| row.symbol.declaration.name.as_deref() == Some(name))
                .unwrap()
                .symbol
                .declaration
        };
        assert_eq!(find("Target").role, DeclarationRole::Test);
        let base = std::time::Instant::now();
        for (name, role, hints) in [
            ("ProdCaller", DeclarationRole::Production, 0),
            ("TestCaller", DeclarationRole::Test, 1),
        ] {
            let owner = find(name);
            assert_eq!(owner.role, role);
            let outgoing = read
                .mcp_outgoing_with_lines(
                    &owner.syntax_id,
                    20,
                    Some(pin),
                    &|| base,
                    base + std::time::Duration::from_secs(5),
                )
                .unwrap()
                .unwrap();
            assert_eq!(outgoing.calls.len(), 1);
            assert_eq!(outgoing.calls[0].selected.candidates.len(), hints, "{name}");
            assert_eq!(outgoing.calls[0].selected.omitted, Some(0));
        }
        read.finish(()).unwrap();
    }

    #[test]
    fn distinct_document_candidate_is_verified_and_budgeted_in_enriched_outgoing() {
        use sha2::{Digest, Sha256};
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join("owner.js"),
            "function Owner() { Target(); }\n",
        )
        .unwrap();
        let target_bytes = b"function Target() {}\n";
        std::fs::write(workspace.path().join("target.js"), target_bytes).unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let (pin, _owner) = crate::index_coordinator::reconcile_workspace(
            &store,
            &IndexOptions::new(workspace.path().to_owned()),
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let read = store.evidence_response().unwrap();
        let owner = read
            .mcp_find_with_lines("Owner", 20, Some(pin))
            .unwrap()
            .symbols
            .into_iter()
            .find(|symbol| symbol.symbol.declaration.name.as_deref() == Some("Owner"))
            .unwrap();
        let id = owner.symbol.declaration.syntax_id;
        let base = std::time::Instant::now();
        let result = read
            .mcp_outgoing_with_lines(
                &id,
                20,
                Some(pin),
                &|| base,
                base + std::time::Duration::from_secs(5),
            )
            .unwrap()
            .unwrap();
        assert_eq!(result.calls.len(), 1);
        let call = &result.calls[0];
        assert_eq!(call.selected.omitted, Some(0));
        assert_eq!(
            call.selected.candidates.len(),
            1,
            "fixture must exercise optional candidate metadata"
        );
        assert_eq!(call.candidate_documents.len(), 1);
        let candidate = &call.selected.candidates[0];
        let document = &call.candidate_documents[0];
        assert_eq!(document.document.path, "target.js");
        assert_ne!(document.document, result.owner.source.document.document);
        assert_eq!(document.document, candidate.declaration.document);
        assert_eq!(
            document.document.source_set_id,
            result.owner.source.document.document.source_set_id
        );
        assert_eq!(document.document.language, "javascript");
        assert_eq!(document.revision_id, candidate.declaration.revision_id);
        assert_eq!(document.provenance_id, candidate.declaration.provenance_id);
        assert_eq!(document.provenance_id, candidate.provenance.id);
        assert_eq!(
            document.content_hash,
            hex::encode(Sha256::digest(target_bytes))
        );
        assert_eq!(document.content_hash, candidate.provenance.content_hash);
        let target = read
            .mcp_declaration_with_lines(&candidate.declaration.syntax_id, Some(pin))
            .unwrap()
            .unwrap();
        assert_eq!(*document, target.source.document);
        read.finish(()).unwrap();
        drop(read);

        // Expiry before the first optional lookup cannot remove a mandatory call.
        let read = store.evidence_response().unwrap();
        let ticks = std::cell::Cell::new(0);
        let clock = || {
            let tick = ticks.get();
            ticks.set(tick + 1);
            if tick == 0 {
                base
            } else {
                base + std::time::Duration::from_secs(60)
            }
        };
        let expired = read
            .mcp_outgoing_with_lines(
                &id,
                20,
                Some(pin),
                &clock,
                base + std::time::Duration::from_secs(5),
            )
            .unwrap()
            .unwrap();
        assert_eq!(expired.calls.len(), 1);
        assert_eq!(expired.calls[0].selected.omitted, None);
        assert!(expired.calls[0].selected.candidates.is_empty());
        assert!(expired.calls[0].candidate_documents.is_empty());
        read.finish(()).unwrap();
        drop(read);

        // Malformed optional declaration evidence is omitted, not a partial
        // required answer or an unverified candidate document.
        let db = store.cache_write().unwrap();
        db.pragma_update(None, "foreign_keys", "OFF").unwrap();
        assert_eq!(
            db.execute(
                "DELETE FROM native_version_headers WHERE syntax_id IN
                (SELECT syntax_id FROM native_version_declarations WHERE name='Target')",
                [],
            )
            .unwrap(),
            1
        );
        db.pragma_update(None, "foreign_keys", "ON").unwrap();
        drop(db);
        let read = store.evidence_response().unwrap();
        let after_fault = read
            .mcp_outgoing_with_lines(
                &id,
                20,
                Some(pin),
                &|| base,
                base + std::time::Duration::from_secs(5),
            )
            .unwrap()
            .unwrap();
        assert_eq!(after_fault.calls.len(), 1);
        assert_eq!(after_fault.calls[0].selected.omitted, Some(0));
        assert!(after_fault.calls[0].selected.candidates.is_empty());
        assert!(after_fault.calls[0].candidate_documents.is_empty());
        assert_eq!(store.disposition(), RecoveryDisposition::Ready);
        read.finish(()).unwrap();
        assert_eq!(store.disposition(), RecoveryDisposition::Rebuild);
        drop(read);
        drop(_owner);
        let (rebuilt, _) = crate::index_coordinator::reconcile_workspace(
            &store,
            &IndexOptions::new(workspace.path().to_owned()),
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let read = store.evidence_response().unwrap();
        let repaired_owner = read
            .mcp_find("Owner", 20, Some(rebuilt))
            .unwrap()
            .symbols
            .into_iter()
            .find(|hit| hit.declaration.name.as_deref() == Some("Owner"))
            .unwrap()
            .declaration
            .syntax_id;
        let repaired = read
            .mcp_outgoing_with_lines(
                &repaired_owner,
                20,
                Some(rebuilt),
                &|| base,
                base + std::time::Duration::from_secs(5),
            )
            .unwrap()
            .unwrap();
        assert_eq!(repaired.calls[0].selected.candidates.len(), 1);
        read.finish(()).unwrap();
    }

    #[test]
    fn selected_find_declaration_and_outgoing_return_checked_source_lines() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join("a.js"),
            "function Alpha() { Beta(); }\nfunction Beta() {}\n",
        )
        .unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let (pin, _owner) = crate::index_coordinator::reconcile_workspace(
            &store,
            &IndexOptions::new(workspace.path().to_owned()),
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let read = store.evidence_response().unwrap();
        let repeated = read.mcp_find_with_lines("a", 20, Some(pin)).unwrap();
        let alpha = repeated
            .symbols
            .iter()
            .find(|symbol| symbol.symbol.declaration.name.as_deref() == Some("Alpha"))
            .unwrap();
        let beta = repeated
            .symbols
            .iter()
            .find(|symbol| symbol.symbol.declaration.name.as_deref() == Some("Beta"))
            .unwrap();
        assert_eq!(alpha.source.document, beta.source.document);
        let find = read.mcp_find_with_lines("Alpha", 20, Some(pin)).unwrap();
        let symbol = find
            .symbols
            .iter()
            .find(|symbol| symbol.symbol.declaration.name.as_deref() == Some("Alpha"))
            .unwrap();
        let id = symbol.symbol.declaration.syntax_id.clone();
        let lines = symbol.source.lines.unwrap();
        assert_eq!(lines, McpLines { start: 1, end: 1 });
        assert_eq!(
            symbol.source.document.document,
            symbol.symbol.declaration.document
        );
        assert_eq!(
            symbol.source.document.revision_id,
            symbol.symbol.declaration.revision_id
        );
        assert_eq!(
            symbol.source.document.content_hash,
            symbol.symbol.content_hash
        );
        assert_eq!(
            symbol.source.document.provenance_id,
            symbol.symbol.declaration.provenance_id
        );
        let fetched = read
            .mcp_source(
                &symbol.source.document.document.path,
                lines.start,
                lines.end,
                Some(pin),
                Some(&symbol.source.document.content_hash),
            )
            .unwrap()
            .unwrap();
        assert!(
            fetched.byte_start <= symbol.symbol.declaration.range.start
                && fetched.byte_end >= symbol.symbol.declaration.range.end
        );
        let declaration = read
            .mcp_declaration_with_lines(&id, Some(pin))
            .unwrap()
            .unwrap();
        assert_eq!(declaration.source.document, symbol.source.document);
        assert_eq!(declaration.source.lines, symbol.source.lines);
        let now = || std::time::Instant::now();
        let outgoing = read
            .mcp_outgoing_with_lines(
                &id,
                20,
                Some(pin),
                &now,
                now() + std::time::Duration::from_secs(5),
            )
            .unwrap()
            .unwrap();
        assert_eq!(outgoing.owner.source.document, symbol.source.document);
        assert!(!outgoing.calls.is_empty());
        for call in &outgoing.calls {
            let lines = call.source.lines.unwrap();
            let fetched = read
                .mcp_source(
                    &call.source.document.document.path,
                    lines.start,
                    lines.end,
                    Some(pin),
                    Some(&call.source.document.content_hash),
                )
                .unwrap()
                .unwrap();
            assert!(
                fetched.byte_start <= call.selected.call.range.start
                    && fetched.byte_end >= call.selected.call.range.end
            );
            assert_eq!(
                call.source.document.provenance_id,
                call.selected.call.provenance_id
            );
            for (candidate, document) in call
                .selected
                .candidates
                .iter()
                .zip(&call.candidate_documents)
            {
                assert_eq!(candidate.declaration.provenance_id, candidate.provenance.id);
                assert_eq!(document.provenance_id, candidate.declaration.provenance_id);
                assert_eq!(document.document, candidate.declaration.document);
            }
            assert_eq!(
                call.candidate_documents.len(),
                call.selected.candidates.len()
            );
        }
        read.finish(()).unwrap();
        let mut conflicting_proof = find
            .provenance
            .iter()
            .find(|proof| proof.id == symbol.source.document.provenance_id)
            .unwrap()
            .clone();
        conflicting_proof.id.push_str(":conflict");
        let selected = store.read_revision_for(&read.db, Some(pin), true).unwrap();
        let error = read
            .mcp_document_version(
                &selected,
                &symbol.symbol.declaration,
                &symbol.symbol.content_hash,
                &conflicting_proof,
            )
            .map_err(|error| read.store.report_index_failure(error))
            .err()
            .unwrap();
        assert!(
            format!("{error:#}").contains("selected document proof mismatch"),
            "{error:#}"
        );
        assert_eq!(store.disposition(), RecoveryDisposition::Rebuild);
    }

    #[test]
    fn interleaved_find_hits_keep_rank_lines_and_document_versions() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        // Name order ranks the prefix hits xa (a.js), xb (b.js), xc (a.js).
        std::fs::write(
            workspace.path().join("a.js"),
            "function xa() {}\n\nfunction xc() {}\n",
        )
        .unwrap();
        std::fs::write(workspace.path().join("b.js"), "\nfunction xb() {}\n").unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let (pin, _owner) = crate::index_coordinator::reconcile_workspace(
            &store,
            &IndexOptions::new(workspace.path().to_owned()),
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let read = store.evidence_response().unwrap();
        let found = read.mcp_find_with_lines("x", 20, Some(pin)).unwrap();
        let hits: Vec<_> = found
            .symbols
            .iter()
            .map(|symbol| {
                (
                    symbol.symbol.declaration.name.as_deref().unwrap(),
                    symbol.source.document.document.path.as_str(),
                    symbol.source.lines.unwrap(),
                )
            })
            .collect();
        assert_eq!(
            hits,
            [
                ("xa", "a.js", McpLines { start: 1, end: 1 }),
                ("xb", "b.js", McpLines { start: 2, end: 2 }),
                ("xc", "a.js", McpLines { start: 3, end: 3 }),
            ]
        );
        assert_eq!(
            found.symbols[0].source.document,
            found.symbols[2].source.document
        );
        // Each hit's grouped result equals its independently read declaration.
        for symbol in &found.symbols {
            let declaration = read
                .mcp_declaration_with_lines(&symbol.symbol.declaration.syntax_id, Some(pin))
                .unwrap()
                .unwrap();
            assert_eq!(declaration.source.document, symbol.source.document);
            assert_eq!(declaration.source.lines, symbol.source.lines);
        }
        read.finish(()).unwrap();
    }

    #[test]
    fn hints_skip_test_code_for_production_callers_and_rank_by_proximity() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        for (path, text) in [
            (
                "src/owner.js",
                "function Owner() { Target(); }\nfunction Target() {}\n",
            ),
            ("src/sibling.js", "function Target() {}\n"),
            ("lib/far.js", "function Target() {}\n"),
            ("src/x.test.js", "function Target() {}\n"),
            ("src/c.integration.test.js", "function Target() {}\n"),
            ("tests/t.js", "function Target() {}\n"),
            ("tests/owner.js", "function TestOwner() { Target(); }\n"),
        ] {
            let path = workspace.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let (pin, _owner) = crate::index_coordinator::reconcile_workspace(
            &store,
            &IndexOptions::new(workspace.path().to_owned()),
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let read = store.evidence_response().unwrap();
        let base = std::time::Instant::now();
        let hints = |name: &str| {
            let owner = read
                .mcp_find_with_lines(name, 20, Some(pin))
                .unwrap()
                .symbols
                .into_iter()
                .find(|symbol| symbol.symbol.declaration.name.as_deref() == Some(name))
                .unwrap();
            let outgoing = read
                .mcp_outgoing(
                    &owner.symbol.declaration.syntax_id,
                    20,
                    Some(pin),
                    &|| base,
                    base + std::time::Duration::from_secs(5),
                )
                .unwrap()
                .unwrap();
            let call = &outgoing.calls[0];
            (
                call.candidates
                    .iter()
                    .map(|c| c.declaration.document.path.clone())
                    .collect::<Vec<_>>(),
                call.omitted,
            )
        };
        // Production caller: the same document, the same directory, then elsewhere;
        // test-code matches are not eligible and not counted as omitted.
        assert_eq!(
            hints("Owner"),
            (
                vec![
                    "src/owner.js".into(),
                    "src/sibling.js".into(),
                    "lib/far.js".into()
                ],
                Some(0)
            )
        );
        // Test caller: test code is eligible, nearest first.
        assert_eq!(
            hints("TestOwner"),
            (
                vec![
                    "tests/t.js".into(),
                    "lib/far.js".into(),
                    "src/c.integration.test.js".into(),
                    "src/owner.js".into(),
                    "src/sibling.js".into(),
                    "src/x.test.js".into(),
                ],
                Some(0)
            )
        );
        read.finish(()).unwrap();
    }

    #[test]
    fn malformed_top_ranked_hint_is_skipped_and_the_next_valid_one_shown() {
        // Proximity ranks src/near.js (same directory) above lib/far.js. After
        // the near match is made malformed, rank-first verification (#170)
        // skips it, shows the far match, and does not count the failed one.
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        for (path, text) in [
            ("src/owner.js", "function Owner() { Target(); }\n"),
            ("src/near.js", "function Target() {}\n"),
            ("lib/far.js", "function Target() {}\n"),
        ] {
            let path = workspace.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let (pin, _owner) = crate::index_coordinator::reconcile_workspace(
            &store,
            &IndexOptions::new(workspace.path().to_owned()),
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let base = std::time::Instant::now();
        let hints = || {
            let read = store.evidence_response().unwrap();
            let owner = read
                .mcp_find_with_lines("Owner", 20, Some(pin))
                .unwrap()
                .symbols
                .into_iter()
                .find(|symbol| symbol.symbol.declaration.name.as_deref() == Some("Owner"))
                .unwrap();
            let outgoing = read
                .mcp_outgoing(
                    &owner.symbol.declaration.syntax_id,
                    20,
                    Some(pin),
                    &|| base,
                    base + std::time::Duration::from_secs(5),
                )
                .unwrap()
                .unwrap();
            read.finish(()).unwrap();
            let call = &outgoing.calls[0];
            (
                call.candidates
                    .iter()
                    .map(|c| c.declaration.document.path.clone())
                    .collect::<Vec<_>>(),
                call.omitted,
            )
        };
        assert_eq!(
            hints(),
            (vec!["src/near.js".into(), "lib/far.js".into()], Some(0))
        );
        let db = store.cache_write().unwrap();
        db.pragma_update(None, "foreign_keys", "OFF").unwrap();
        assert_eq!(
            db.execute(
                "DELETE FROM native_version_headers WHERE syntax_id IN
                (SELECT d.syntax_id FROM native_version_declarations d
                 JOIN document_versions v ON v.vid=d.vid
                 WHERE d.name='Target' AND v.path='src/near.js')",
                [],
            )
            .unwrap(),
            1
        );
        db.pragma_update(None, "foreign_keys", "ON").unwrap();
        drop(db);
        assert_eq!(hints(), (vec!["lib/far.js".into()], Some(0)));
    }

    #[test]
    fn test_paths_follow_the_documented_conventions() {
        for path in [
            "tests/a.rs",
            "crate/tests/deep/a.rs",
            "src/test/java/a/B.java",
            "web/__tests__/a.js",
            "spec/a.rb",
            "src/http/tests.rs",
            "src/store/serving_holder_tests.rs",
            "src/a_test.rs",
            "pkg/test_a.py",
            "pkg/a_test.py",
            "pkg/conftest.py",
            "src/a/BTest.java",
            "src/a/BTests.java",
            "web/a.test.js",
            "web/a.spec.ts",
            "web/component.integration.test.ts",
            "web/a.b.spec.js",
        ] {
            assert!(crate::native_evidence::test_path(path), "{path}");
        }
        for path in [
            "src/store.rs",
            "src/testing.rs",
            "src/attest.rs",
            "pkg/testing.py",
            "pkg/contest.py",
            "src/a/Testable.java",
            "web/latest.js",
            "web/a.js",
            "testdata.js",
            "web/test.js",
            "web/a.testing.js",
            "src/a.b.rs",
        ] {
            assert!(!crate::native_evidence::test_path(path), "{path}");
        }
    }
}

#[cfg(test)]
mod source_range_tests {
    use super::{McpLines, mcp_lf_range, mcp_lines_for_range};
    use crate::native_evidence::Range;

    #[test]
    fn selected_item_lines_reciprocate_lf_source_reads() {
        for bytes in [b"a\r\nb\n".as_slice(), b"a\nb", b"a\n", b"ab", b"\n"] {
            for start in 0..=bytes.len() {
                for end in start..=bytes.len() {
                    let range = Range { start, end };
                    let lines = mcp_lines_for_range(bytes, &range).unwrap().unwrap();
                    let (from, to) = mcp_lf_range(bytes, lines.start, lines.end).unwrap();
                    assert!(from <= start && end <= to, "{bytes:?} {range:?} {lines:?}");
                }
            }
        }
        let lines = |bytes: &[u8], start, end| mcp_lines_for_range(bytes, &Range { start, end });
        assert_eq!(
            lines(b"a\r\nb\n", 3, 4).unwrap(),
            Some(McpLines { start: 2, end: 2 })
        );
        assert_eq!(
            lines(b"a\r\nb\n", 1, 3).unwrap(),
            Some(McpLines { start: 1, end: 1 })
        );
        assert_eq!(
            lines(b"a\n", 2, 2).unwrap(),
            Some(McpLines { start: 1, end: 1 })
        );
        assert_eq!(
            lines(b"ab", 2, 2).unwrap(),
            Some(McpLines { start: 1, end: 1 })
        );
        assert_eq!(lines(b"", 0, 0).unwrap(), None);
        assert!(lines(b"a", 0, 2).is_err());
        assert!(lines(b"a", 1, 0).is_err());
        let many = "a\n".repeat(201);
        assert_eq!(
            lines(many.as_bytes(), 0, many.len()).unwrap(),
            Some(McpLines { start: 1, end: 201 })
        );
        assert!(mcp_lf_range(many.as_bytes(), 1, 201).is_err());
    }

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
