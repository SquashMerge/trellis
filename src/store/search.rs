//! Version-owned capture cards and the lexical corpus of the selected head.
use super::*;

#[cfg(test)]
#[path = "search_matrix_tests.rs"]
mod matrix_tests;

/// The fill owner releases its index snapshot between these bounded pages.
pub(crate) const SEARCH_CARD_BATCH_SIZE: usize = 16;

#[derive(serde::Deserialize, Serialize)]
struct StoredFacts {
    facts: crate::search_card::PreparedFacts,
    ancestors: Vec<crate::search_card::Ancestor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadSearchCard {
    pub syntax_id: String,
    pub card_text: String,
    pub card_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadSearchCards {
    pub pin: IndexPin,
    pub cards: Vec<HeadSearchCard>,
}

#[derive(Debug)]
pub struct SearchHeadChanged;
impl std::fmt::Display for SearchHeadChanged {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("revision conflict: search card head changed")
    }
}
impl std::error::Error for SearchHeadChanged {}

impl Store {
    /// Read a short, current-head-only card page. A continuation never reads a
    /// superseded revision, even if that revision is still retained.
    #[doc(hidden)]
    pub fn head_search_cards(
        &self,
        expected: Option<IndexPin>,
        after: Option<&str>,
        limit: usize,
    ) -> Result<HeadSearchCards> {
        ensure!(
            (1..=SEARCH_CARD_BATCH_SIZE).contains(&limit),
            "invalid search card batch size"
        );
        let after = after.map(stored_sid).transpose()?;
        self.with_evidence(|db| {
            let selected = self.read_revision(db, None)?;
            if expected.is_some_and(|pin| pin != selected.pin) {
                return Err(SearchHeadChanged.into());
            }
            let cursor = if after.is_some() {
                "AND c.syntax_id>?2"
            } else {
                "AND ?2 IS NULL"
            };
            let mut statement = db.prepare(&format!(
                "SELECT c.syntax_id,c.card_text,c.card_hash
                 FROM native_version_search_cards c
                 JOIN revision_documents m ON m.vid=c.vid
                 WHERE m.revision_id=?1 {cursor}
                 ORDER BY c.syntax_id LIMIT ?3"
            ))?;
            let rows = statement.query_map(params![selected.key, after, limit as i64], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            let cards = rows
                .map(|row| {
                    let (id, card_text, card_hash) = row?;
                    Ok(HeadSearchCard {
                        syntax_id: id_codec::decode_sid(&id)?,
                        card_text,
                        card_hash,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(HeadSearchCards {
                pin: selected.pin,
                cards,
            })
        })
    }
}

pub(super) fn write_cards(
    db: &Connection,
    artifact: &crate::native_evidence::Artifact,
    storage_ids: &BTreeMap<String, (i64, i64, i64)>,
    reuse: &PreflightReuse,
    immutable: &mut ImmutableAppend,
    cancel: &CancelFlag,
) -> Result<()> {
    let declarations: BTreeMap<_, _> = artifact
        .declarations
        .iter()
        .filter(|d| crate::search_card::is_eligible(&d.kind, d.name.as_deref()))
        .map(|d| ((d.document.path.as_str(), d.syntax_id.as_str()), d))
        .collect();
    let mut seen = BTreeSet::new();
    for captured in &artifact.search_cards {
        check_cancel(cancel)?;
        let key = (captured.path.as_str(), captured.syntax_id.as_str());
        ensure!(
            seen.insert(key),
            "native_evidence_required: duplicate search card"
        );
        let declaration = declarations
            .get(&key)
            .context("native_evidence_required: search card lacks eligible declaration")?;
        if reuse.for_path(&captured.path).native {
            continue;
        }
        let vid = storage_ids
            .get(&captured.path)
            .context("native_evidence_required: search card document absent")?
            .0;
        let card = crate::search_capture::render_card(declaration, captured);
        let terms = crate::search_lexical::card_terms(db, &card.text)?;
        immutable.insert(
            db,
            "INSERT INTO native_version_search_cards VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                vid,
                stored_sid(&captured.syntax_id)?,
                card.text,
                card.hash,
                json(&StoredFacts {
                    facts: captured.facts.clone(),
                    ancestors: captured.ancestors.clone()
                })?,
                terms
            ],
        )?;
    }
    ensure!(
        seen.len() == declarations.len(),
        "native_evidence_required: eligible declaration missing captured search card"
    );
    Ok(())
}

pub(super) fn read_cards(
    db: &Connection,
    vid: i64,
    path: &str,
) -> Result<Vec<crate::search_capture::CapturedCard>> {
    let mut statement = db.prepare(
        "SELECT syntax_id,card_text,card_hash,facts_json FROM native_version_search_cards WHERE vid=?1 ORDER BY syntax_id",
    )?;
    statement
        .query_map([vid], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .map(|row| {
            let (id, text, hash, facts) = row?;
            let facts: StoredFacts = serde_json::from_str(&facts).context(SelectedIntegrity(
                "incompatible_index: corrupt captured search facts".into(),
            ))?;
            Ok(crate::search_capture::CapturedCard {
                path: path.to_owned(),
                syntax_id: id_codec::decode_sid(&id)?,
                card: crate::search_card::SearchCard { text, hash },
                facts: facts.facts,
                ancestors: facts.ancestors,
            })
        })
        .collect()
}

/// Called in every head-changing transaction, before its metadata update.
/// History retention never writes this corpus. Immutable document versions keep
/// their existing rows; only removed and added versions change BM25 statistics.
pub(super) fn publish_head(db: &Connection, revision: &str) -> Result<()> {
    #[cfg(feature = "test-support")]
    let _audit = fts_write_audit::Guard::install(db);
    db.execute(
        "DELETE FROM head_search_fts WHERE vid NOT IN
         (SELECT vid FROM revision_documents WHERE revision_id=?1)",
        [revision],
    )?;
    db.execute(
        "WITH added(vid) AS MATERIALIZED
         (SELECT vid FROM revision_documents WHERE revision_id=?1
          EXCEPT SELECT vid FROM head_search_fts)
         INSERT INTO head_search_fts(terms,vid,syntax_id)
         SELECT terms,vid,syntax_id FROM native_version_search_cards
         WHERE vid IN (SELECT vid FROM added)",
        [revision],
    )?;
    Ok(())
}

/// A retained-revision membership change, not a fresh enumeration of the head.
#[derive(Debug)]
pub struct SearchHeadDelta {
    pub pin: IndexPin,
    pub removed_version_ids: Vec<i64>,
    pub added: std::sync::Arc<[crate::search_rank::SearchCandidate]>,
    /// Required head corpus count; callers validate their patched membership
    /// against it before publishing a new in-memory matrix.
    pub head_card_count: usize,
}

/// The previous matrix basis belongs to another generation or is no longer
/// retained. This is a cold rebuild request, never a corruption fallback.
#[derive(Debug)]
pub struct SearchDeltaColdRebuild;
impl std::fmt::Display for SearchDeltaColdRebuild {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("search_delta_cold_rebuild: previous head is no longer retained")
    }
}
impl std::error::Error for SearchDeltaColdRebuild {}

/// Candidate identities and lexical order from one admitted head.
#[derive(Debug)]
pub struct McpSearchCandidates {
    pub pin: IndexPin,
    pub cards: Vec<crate::search_rank::SearchCandidate>,
    pub lexical: Vec<crate::search_rank::SearchCandidateKey>,
}

fn search_candidate(row: &rusqlite::Row<'_>) -> Result<crate::search_rank::SearchCandidate> {
    use crate::search_rank::{SearchCandidate, SearchCandidateKey};
    let card_hash: String = row.get(2)?;
    ensure!(
        card_hash.len() == 64
            && card_hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        SelectedIntegrity("store_unavailable: invalid search card hash".into())
    );
    Ok(SearchCandidate {
        key: SearchCandidateKey {
            version_id: row.get(0)?,
            syntax_id: stored_id_row(row, 1, false)?,
        },
        card_hash,
    })
}

fn search_role_filter(role: crate::search_rank::SearchRole) -> Option<&'static str> {
    use crate::search_rank::SearchRole;
    match role {
        SearchRole::Any => None,
        SearchRole::Production => Some("production"),
        SearchRole::Test => Some("test"),
    }
}

fn search_head_count(db: &Connection) -> Result<usize> {
    let count: i64 = db.query_row("SELECT count(*) FROM head_search_fts", [], |row| row.get(0))?;
    usize::try_from(count).context(SelectedIntegrity(
        "store_unavailable: invalid head search count".into(),
    ))
}

// The callback and guard are scoped to a synchronous admitted read. Preserve the
// caller's typed cancellation/deadline error rather than SQLite's INTERRUPT.
struct SearchReadProgress<'a> {
    check: std::cell::RefCell<&'a mut dyn FnMut() -> Result<()>>,
    failure: std::cell::RefCell<Option<anyhow::Error>>,
}
impl SearchReadProgress<'_> {
    fn check(&self) -> Result<()> {
        if self.failure.borrow().is_some() {
            anyhow::bail!("cancelled");
        }
        match (self.check.borrow_mut())() {
            Ok(()) => Ok(()),
            Err(error) => {
                *self.failure.borrow_mut() = Some(error);
                anyhow::bail!("cancelled")
            }
        }
    }
}
unsafe extern "C" fn search_progress_callback(arg: *mut std::ffi::c_void) -> libc::c_int {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: the registration is removed before this scope is dropped.
        let progress = unsafe { &*arg.cast::<SearchReadProgress<'_>>() };
        i32::from(progress.check().is_err())
    }))
    .unwrap_or(1)
}
struct SearchProgressGuard(*mut rusqlite::ffi::sqlite3);
impl Drop for SearchProgressGuard {
    fn drop(&mut self) {
        // SAFETY: the admitted connection outlives this guard.
        unsafe { rusqlite::ffi::sqlite3_progress_handler(self.0, 0, None, std::ptr::null_mut()) };
    }
}
impl EvidenceResponse {
    fn search_read<T>(
        &self,
        check: &mut dyn FnMut() -> Result<()>,
        read: impl FnOnce(&mut dyn FnMut() -> Result<()>) -> Result<T>,
    ) -> Result<T> {
        let progress = SearchReadProgress {
            check: std::cell::RefCell::new(check),
            failure: std::cell::RefCell::new(None),
        };
        let handle = unsafe { self.db.handle() };
        // SAFETY: handle and progress remain live for the registration's scope.
        unsafe {
            rusqlite::ffi::sqlite3_progress_handler(
                handle,
                1000,
                Some(search_progress_callback),
                (&progress as *const SearchReadProgress<'_>)
                    .cast_mut()
                    .cast(),
            );
        }
        let guard = SearchProgressGuard(handle);
        let result = (|| {
            progress.check()?;
            let value = read(&mut || progress.check())?;
            progress.check()?;
            Ok(value)
        })();
        drop(guard);
        if let Some(error) = progress.failure.into_inner() {
            Err(error)
        } else {
            result
        }
    }

    /// Read all candidate identities from this admitted head for a cold matrix
    /// build. No source or card text is materialized. Warm callers use the delta
    /// and lexical reads instead of enumerating these identities again.
    pub fn mcp_search_head(
        &self,
        expected: Option<IndexPin>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<(
        IndexPin,
        std::sync::Arc<[crate::search_rank::SearchCandidate]>,
    )> {
        self.mcp_search_head_admitted(expected, check, &mut |_| Ok(()))
    }

    pub(crate) fn mcp_search_head_admitted(
        &self,
        expected: Option<IndexPin>,
        check: &mut impl FnMut() -> Result<()>,
        admit: &mut dyn FnMut(usize) -> Result<()>,
    ) -> Result<(
        IndexPin,
        std::sync::Arc<[crate::search_rank::SearchCandidate]>,
    )> {
        self.search_read(check, |check| {
            let selected = self.store.read_revision_for(&self.db, None, true)?;
            ensure!(
                expected.is_none_or(|pin| pin == selected.pin),
                "revision conflict: expected basis differs from admitted head"
            );
            let count: i64 = self.db.query_row(
                "SELECT count(*) FROM native_version_search_cards c
                 JOIN revision_documents m ON m.vid=c.vid AND m.revision_id=?1
                 JOIN document_versions v ON v.vid=c.vid
                 JOIN native_version_declarations d ON d.vid=c.vid AND d.syntax_id=c.syntax_id",
                [&selected.key],
                |row| row.get(0),
            )?;
            let count = usize::try_from(count).context(SelectedIntegrity(
                "store_unavailable: invalid head search count".into(),
            ))?;
            admit(count)?;
            let _phase = crate::search_model::SearchPhaseTimer::start("head_candidates");
            let mut statement = self.db.prepare(
                "SELECT c.vid,c.syntax_id,c.card_hash
                 FROM native_version_search_cards c
                 JOIN revision_documents m ON m.vid=c.vid AND m.revision_id=?1
                 JOIN document_versions v ON v.vid=c.vid
                 JOIN native_version_declarations d ON d.vid=c.vid AND d.syntax_id=c.syntax_id
                 ORDER BY c.syntax_id",
            )?;
            let mut rows = statement.query([&selected.key])?;
            let mut cards = Vec::new();
            cards.try_reserve_exact(count)?;
            while let Some(row) = rows.next()? {
                check()?;
                ensure!(
                    cards.len() < count,
                    SelectedIntegrity("store_unavailable: head search count mismatch".into())
                );
                cards.push(search_candidate(row)?);
            }
            ensure!(
                cards.len() == count,
                SelectedIntegrity("store_unavailable: head search count mismatch".into())
            );
            ensure!(
                search_head_count(&self.db)? == cards.len(),
                SelectedIntegrity("store_unavailable: head search corpus mismatch".into())
            );
            Ok((selected.pin, cards.into()))
        })
    }

    /// Read only candidates in added document versions and the removed version
    /// IDs between two pins, using this response's admitted SQLite snapshot.
    /// A missing/released old pin or generation change requests a cold rebuild;
    /// retained revision corruption and read faults remain errors.
    pub fn mcp_search_head_delta(
        &self,
        previous: IndexPin,
        expected_new: IndexPin,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<SearchHeadDelta> {
        self.mcp_search_head_delta_admitted(previous, expected_new, check, &mut |_, _| Ok(()))
    }

    pub(crate) fn mcp_search_head_delta_admitted(
        &self,
        previous: IndexPin,
        expected_new: IndexPin,
        check: &mut impl FnMut() -> Result<()>,
        admit: &mut dyn FnMut(usize, usize) -> Result<()>,
    ) -> Result<SearchHeadDelta> {
        self.search_read(check, |check| {
            let selected = self.store.read_revision_for(&self.db, None, true)?;
            ensure!(
                selected.pin == expected_new,
                "revision conflict: expected basis differs from admitted head"
            );
            if previous.index_generation != selected.pin.index_generation {
                return Err(SearchDeltaColdRebuild.into());
            }
            ensure!(
                previous.index_revision > 0
                    && previous.index_revision <= selected.pin.index_revision,
                "revision conflict: invalid previous search basis"
            );
            let old_key = format!(
                "pin:v1:{}:{}",
                previous.index_generation, previous.index_revision
            );
            let retained: Option<(i64, bool)> = self
                .db
                .query_row(
                    "SELECT r.published_index_revision,
                     EXISTS(SELECT 1 FROM revision_capture_inputs i
                        WHERE i.revision_id=r.id
                        AND i.input_key IN ('__released:v1','__pending_release:v1'))
                     FROM native_revisions r WHERE r.id=?1",
                    [&old_key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((revision, false)) = retained else {
                return Err(SearchDeltaColdRebuild.into());
            };
            ensure!(
                u64::try_from(revision).ok() == Some(previous.index_revision),
                SelectedIntegrity("store_unavailable: invalid retained search basis".into())
            );
            // Retention is checked above; corruption in a retained revision is
            // still an integrity failure, never permission for a cold fallback.
            self.store
                .read_revision_for(&self.db, Some(previous), true)?;
            let _phase = crate::search_model::SearchPhaseTimer::start("head_delta");
            let added_count: i64 = self.db.query_row(
                "WITH added(vid) AS (SELECT vid FROM revision_documents WHERE revision_id=?1
                 EXCEPT SELECT vid FROM revision_documents WHERE revision_id=?2)
                 SELECT count(*) FROM added JOIN native_version_search_cards c ON c.vid=added.vid",
                params![selected.key, old_key],
                |row| row.get(0),
            )?;
            let removed_count: i64 = self.db.query_row(
                "SELECT count(*) FROM (SELECT vid FROM revision_documents WHERE revision_id=?1
                 EXCEPT SELECT vid FROM revision_documents WHERE revision_id=?2)",
                params![old_key, selected.key],
                |row| row.get(0),
            )?;
            let added_count = usize::try_from(added_count)?;
            let removed_count = usize::try_from(removed_count)?;
            admit(added_count, removed_count)?;
            let mut removed_statement = self.db.prepare(
                "SELECT vid FROM revision_documents WHERE revision_id=?1
                 EXCEPT SELECT vid FROM revision_documents WHERE revision_id=?2",
            )?;
            let mut removed_rows = removed_statement.query(params![old_key, selected.key])?;
            let mut removed_version_ids = Vec::new();
            removed_version_ids.try_reserve_exact(removed_count)?;
            while let Some(row) = removed_rows.next()? {
                check()?;
                ensure!(
                    removed_version_ids.len() < removed_count,
                    SelectedIntegrity("store_unavailable: delta removed count mismatch".into())
                );
                removed_version_ids.push(row.get(0)?);
            }
            let mut added_statement = self.db.prepare(
                "WITH added(vid) AS MATERIALIZED
                 (SELECT vid FROM revision_documents WHERE revision_id=?1
                  EXCEPT SELECT vid FROM revision_documents WHERE revision_id=?2)
                 SELECT c.vid,c.syntax_id,c.card_hash,v.vid,d.vid
                 FROM added
                 CROSS JOIN native_version_search_cards c ON c.vid=added.vid
                 LEFT JOIN document_versions v ON v.vid=c.vid
                 LEFT JOIN native_version_declarations d ON d.vid=c.vid AND d.syntax_id=c.syntax_id
                 ORDER BY c.syntax_id",
            )?;
            let mut added_rows = added_statement.query(params![selected.key, old_key])?;
            let mut added = Vec::new();
            added.try_reserve_exact(added_count)?;
            while let Some(row) = added_rows.next()? {
                check()?;
                ensure!(
                    added.len() < added_count,
                    SelectedIntegrity("store_unavailable: delta added count mismatch".into())
                );
                let candidate = search_candidate(row)?;
                ensure!(
                    row.get::<_, Option<i64>>(3)?.is_some()
                        && row.get::<_, Option<i64>>(4)?.is_some(),
                    SelectedIntegrity("store_unavailable: search candidate owner absent".into())
                );
                added.push(candidate);
            }
            ensure!(
                added.len() == added_count && removed_version_ids.len() == removed_count,
                SelectedIntegrity("store_unavailable: delta search count mismatch".into())
            );
            let head_card_count = search_head_count(&self.db)?;
            Ok(SearchHeadDelta {
                pin: selected.pin,
                removed_version_ids,
                added: added.into(),
                head_card_count,
            })
        })
    }

    /// Count canonical head cards without enumerating candidates or trusting
    /// the derived FTS corpus that the lexical read must independently check.
    pub fn mcp_search_head_count(
        &self,
        expected: IndexPin,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<usize> {
        self.search_read(check, |_| {
            let selected = self.store.read_revision_for(&self.db, None, true)?;
            ensure!(
                selected.pin == expected,
                "revision conflict: expected basis differs from admitted head"
            );
            let count: i64 = self.db.query_row(
                "SELECT count(*)
                 FROM native_version_search_cards c
                 JOIN revision_documents m ON m.vid=c.vid AND m.revision_id=?1
                 JOIN document_versions v ON v.vid=c.vid
                 JOIN native_version_declarations d ON d.vid=c.vid AND d.syntax_id=c.syntax_id",
                [&selected.key],
                |row| row.get(0),
            )?;
            usize::try_from(count).context(SelectedIntegrity(
                "store_unavailable: invalid head search count".into(),
            ))
        })
    }

    /// Query-specific metadata from this admitted head, without loading source
    /// or card text. The vector arm uses allowed keys before its depth cutoff.
    pub fn mcp_search_policy(
        &self,
        query: &str,
        expected: IndexPin,
        role: crate::search_rank::SearchRole,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<crate::search_rank::SearchPolicy> {
        self.search_read(check, |check| {
            let selected = self.store.read_revision_for(&self.db, None, true)?;
            ensure!(
                selected.pin == expected,
                "revision conflict: expected basis differs from admitted head"
            );
            let role = search_role_filter(role);
            let mut policy = crate::search_rank::SearchPolicy {
                exact: BTreeMap::new(),
                allowed: role.map(|_| BTreeSet::new()),
            };
            let mut statement = self.db.prepare(
                "SELECT c.vid,c.syntax_id,d.role,d.name=?2 COLLATE BINARY
                 FROM native_version_search_cards c
                 JOIN revision_documents m ON m.vid=c.vid AND m.revision_id=?1
                 JOIN native_version_declarations d ON d.vid=c.vid AND d.syntax_id=c.syntax_id
                 WHERE (?3 IS NULL OR d.role=?3)
                   AND (?3 IS NOT NULL OR d.name=?2 COLLATE BINARY)",
            )?;
            let mut rows = statement.query(params![selected.key, query, role])?;
            while let Some(row) = rows.next()? {
                check()?;
                let key = crate::search_rank::SearchCandidateKey {
                    version_id: row.get(0)?,
                    syntax_id: stored_id_row(row, 1, false)?,
                };
                let stored_role: String = row.get(2)?;
                let stored_role =
                    crate::native_evidence::DeclarationRole::from_stored(&stored_role)
                        .map_err(|error| SelectedIntegrity(error.to_string()))?;
                if row.get::<_, Option<bool>>(3)?.unwrap_or(false) {
                    policy.exact.insert(key.clone(), stored_role);
                }
                if let Some(allowed) = &mut policy.allowed {
                    allowed.insert(key);
                }
            }
            Ok(policy)
        })
    }

    /// Rank lexical and additive exact-name candidates at the admitted pin.
    /// The role filter and exact/production tiers precede the depth cutoff.
    /// Even termless queries validate the complete required head corpus.
    pub fn mcp_search_lexical(
        &self,
        query: &str,
        expected: IndexPin,
        expected_cards: usize,
        role: crate::search_rank::SearchRole,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Vec<crate::search_rank::SearchCandidateKey>> {
        use crate::search_rank::{CANDIDATE_DEPTH, SearchCandidateKey};
        self.search_read(check, |check| {
            let selected = self.store.read_revision_for(&self.db, None, true)?;
            ensure!(
                selected.pin == expected,
                "revision conflict: expected basis differs from admitted head"
            );
            ensure!(
                search_head_count(&self.db)? == expected_cards,
                SelectedIntegrity("store_unavailable: head search corpus mismatch".into())
            );
            let _phase = crate::search_model::SearchPhaseTimer::start("lexical_search");
            let expression = crate::search_lexical::query_expression(&self.db, query)?;
            let matched = if expression.is_some() {
                "SELECT vid,syntax_id,bm25(head_search_fts) AS score
                 FROM head_search_fts WHERE head_search_fts MATCH ?4"
            } else {
                "SELECT vid,syntax_id,CAST(NULL AS REAL) AS score
                 FROM head_search_fts WHERE 0 AND ?4 IS NULL"
            };
            let sql = format!(
                "WITH matched AS MATERIALIZED ({matched}),
                 candidates AS (
                   SELECT vid,syntax_id,score FROM matched
                   UNION ALL
                   SELECT c.vid,c.syntax_id,NULL FROM native_version_search_cards c
                   JOIN revision_documents m ON m.vid=c.vid AND m.revision_id=?1
                   JOIN native_version_declarations d ON d.vid=c.vid AND d.syntax_id=c.syntax_id
                   WHERE d.name=?2 COLLATE BINARY AND NOT EXISTS
                     (SELECT 1 FROM matched f WHERE f.vid=c.vid AND f.syntax_id=c.syntax_id)
                 )
                 SELECT f.vid,f.syntax_id,f.score,d.role
                 FROM candidates f
                 JOIN native_version_search_cards c ON c.vid=f.vid AND c.syntax_id=f.syntax_id
                 JOIN revision_documents m ON m.vid=c.vid AND m.revision_id=?1
                 JOIN native_version_declarations d ON d.vid=f.vid AND d.syntax_id=f.syntax_id
                 WHERE ?3 IS NULL OR d.role=?3
                 ORDER BY (d.name=?2 COLLATE BINARY) DESC,
                          CASE d.role WHEN 'production' THEN 0 ELSE 1 END,
                          f.score IS NULL,f.score,f.syntax_id
                 LIMIT ?5"
            );
            let mut statement = self.db.prepare(&sql)?;
            let mut rows = statement.query(params![
                selected.key,
                query,
                search_role_filter(role),
                expression,
                CANDIDATE_DEPTH as i64
            ])?;
            let mut lexical = Vec::new();
            while let Some(row) = rows.next()? {
                check()?;
                if let Some(score) = row.get::<_, Option<f64>>(2)? {
                    ensure!(
                        score.is_finite(),
                        SelectedIntegrity("store_unavailable: invalid lexical score".into())
                    );
                }
                let stored_role: String = row.get(3)?;
                crate::native_evidence::DeclarationRole::from_stored(&stored_role)
                    .map_err(|error| SelectedIntegrity(error.to_string()))?;
                lexical.push(SearchCandidateKey {
                    version_id: row.get(0)?,
                    syntax_id: stored_id_row(row, 1, false)?,
                });
            }
            Ok(lexical)
        })
    }

    /// Read complete head candidates and the lexical arm for existing callers.
    pub fn mcp_search_candidates(
        &self,
        query: &str,
        expected: Option<IndexPin>,
        role: crate::search_rank::SearchRole,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<McpSearchCandidates> {
        let (pin, cards) = self.mcp_search_head(expected, check)?;
        let lexical = self.mcp_search_lexical(query, pin, cards.len(), role, check)?;
        Ok(McpSearchCandidates {
            pin,
            cards: cards.to_vec(),
            lexical,
        })
    }

    /// Hydrate only the response-limited ranked prefix, not the whole corpus.
    /// The caller sets has_more from the complete fused arm union before clipping.
    pub fn mcp_search_hydrate(
        &self,
        keys: &[crate::search_rank::SearchCandidateKey],
        expected: IndexPin,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<super::mcp_read::McpFindWithLines> {
        let _phase = crate::search_model::SearchPhaseTimer::start("search_hydration");
        ensure!(keys.len() <= 50, "invalid_request: search hydration limit");
        self.search_read(check, |check| {
            let selected = self.store.read_revision_for(&self.db, None, true)?;
            ensure!(
                selected.pin == expected,
                "revision conflict: expected basis differs from admitted head"
            );
            let scope = self.mcp_find_scope(&selected, check)?;
            let mut hits = Vec::with_capacity(keys.len());
            let mut seen = BTreeSet::new();
            for key in keys {
                check()?;
                ensure!(
                    seen.insert(key),
                    SelectedIntegrity("store_unavailable: duplicate ranked candidate".into())
                );
                let version = self
                    .db
                    .query_row(
                        "SELECT v.id FROM native_version_search_cards c
                     JOIN document_versions v ON v.vid=c.vid
                     JOIN revision_documents m ON m.vid=c.vid AND m.revision_id=?1
                     WHERE c.vid=?2 AND c.syntax_id=?3",
                        params![selected.key, key.version_id, stored_sid(&key.syntax_id)?],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                    .ok_or_else(|| {
                        SelectedIntegrity(
                            "store_unavailable: ranked search candidate absent".into(),
                        )
                    })?;
                hits.push((version, key.syntax_id.clone()));
            }
            let found = self.mcp_hydrate_find(&selected, &scope, hits, false, check)?;
            self.mcp_find_lines(found, check)
        })
    }
}

#[cfg(feature = "test-support")]
mod fts_write_audit {
    use super::*;

    thread_local! {
        static WRITES: std::cell::RefCell<Vec<(i32, i64)>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    unsafe extern "C" fn record(
        _context: *mut std::ffi::c_void,
        operation: i32,
        _database: *const std::ffi::c_char,
        table: *const std::ffi::c_char,
        rowid: i64,
    ) {
        let _ = std::panic::catch_unwind(|| {
            // SAFETY: SQLite supplies a valid, nul-terminated table name.
            if unsafe { std::ffi::CStr::from_ptr(table) }.to_bytes() == b"head_search_fts_content" {
                WRITES.with_borrow_mut(|writes| writes.push((operation, rowid)));
            }
        });
    }

    pub(super) struct Guard(*mut rusqlite::ffi::sqlite3);
    impl Guard {
        pub(super) fn install(db: &Connection) -> Self {
            WRITES.with_borrow_mut(Vec::clear);
            // SAFETY: the connection remains live until this guard is dropped;
            // the callback uses only same-thread state, with no borrowed context.
            let handle = unsafe { db.handle() };
            unsafe {
                rusqlite::ffi::sqlite3_update_hook(handle, Some(record), std::ptr::null_mut());
            }
            Self(handle)
        }
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            // SAFETY: publish_head drops the guard before releasing its connection.
            unsafe {
                rusqlite::ffi::sqlite3_update_hook(self.0, None, std::ptr::null_mut());
            }
        }
    }
    pub(super) fn take() -> Vec<(i32, i64)> {
        WRITES.with_borrow_mut(std::mem::take)
    }
}

#[cfg(feature = "test-support")]
impl Store {
    /// Drain logical FTS content writes attempted by the last head publication
    /// on this thread, including writes later rolled back by its transaction.
    pub fn take_search_fts_writes_for_tests(&self) -> Vec<(i32, i64)> {
        fts_write_audit::take()
    }

    /// Fault the required head ranking table through the retained writer handle.
    pub fn corrupt_search_fts_for_tests(&self, writer: &topology::WriterGuard) -> Result<()> {
        writer.belongs_to(&self.identity)?;
        self.identity.verify()?;
        writer.verify()?;
        let db = self.cache_write()?;
        storage_result(db.execute("DROP TABLE head_search_fts", []))?;
        Ok(())
    }
}
