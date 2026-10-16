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
