//! Version-owned capture cards and the lexical corpus of the selected head.
use super::*;

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

/// Candidate identities and lexical order from one admitted head.
#[derive(Debug)]
pub struct McpSearchCandidates {
    pub pin: IndexPin,
    pub cards: Vec<crate::search_rank::SearchCandidate>,
    pub lexical: Vec<crate::search_rank::SearchCandidateKey>,
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

    pub fn mcp_search_candidates(
        &self,
        query: &str,
        expected: Option<IndexPin>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<McpSearchCandidates> {
        use crate::search_rank::{CANDIDATE_DEPTH, SearchCandidate, SearchCandidateKey};
        self.search_read(check, |check| {
            let selected = self.store.read_revision_for(&self.db, None, true)?;
            ensure!(expected.is_none_or(|pin| pin == selected.pin),
                "revision conflict: expected basis differs from admitted head");
            let candidates_phase = crate::search_model::SearchPhaseTimer::start("head_candidates");
            let mut statement = self.db.prepare(
                "SELECT c.vid,c.syntax_id,c.card_hash
                 FROM native_version_search_cards c
                 JOIN revision_documents m ON m.vid=c.vid AND m.revision_id=?1
                 JOIN document_versions v ON v.vid=c.vid
                 JOIN native_version_declarations d ON d.vid=c.vid AND d.syntax_id=c.syntax_id
                 ORDER BY c.syntax_id"
            )?;
            let mut rows = statement.query([&selected.key])?;
            let mut cards = Vec::new();
            while let Some(row) = rows.next()? {
                check()?;
                let card_hash: String = row.get(2)?;
                ensure!(card_hash.len() == 64 && card_hash.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                    SelectedIntegrity("store_unavailable: invalid search card hash".into()));
                cards.push(SearchCandidate {
                    key: SearchCandidateKey { version_id: row.get(0)?, syntax_id: stored_id_row(row,1,false)? },
                    card_hash,
                });
            }
            // Even a termless query must read the required head corpus. Schema,
            // stepping and I/O errors never become lexical-only best effort.
            let corpus: i64 = self.db.query_row("SELECT count(*) FROM head_search_fts", [], |row| row.get(0))?;
            ensure!(usize::try_from(corpus).ok() == Some(cards.len()),
                SelectedIntegrity("store_unavailable: head search corpus mismatch".into()));
            drop(candidates_phase);
            let _phase = crate::search_model::SearchPhaseTimer::start("lexical_search");
            let mut lexical = Vec::new();
            if let Some(expression) = crate::search_lexical::query_expression(&self.db, query)? {
                let mut statement = self.db.prepare(
                    "SELECT head_search_fts.vid,head_search_fts.syntax_id,bm25(head_search_fts)
                     FROM head_search_fts
                     JOIN native_version_search_cards c ON c.vid=head_search_fts.vid AND c.syntax_id=head_search_fts.syntax_id
                     JOIN revision_documents m ON m.vid=c.vid AND m.revision_id=?1
                     WHERE head_search_fts MATCH ?2
                     ORDER BY bm25(head_search_fts),head_search_fts.syntax_id LIMIT ?3"
                )?;
                let mut rows = statement.query(params![selected.key, expression, CANDIDATE_DEPTH as i64])?;
                while let Some(row) = rows.next()? {
                    check()?;
                    let score: f64 = row.get(2)?;
                    ensure!(score.is_finite(), SelectedIntegrity("store_unavailable: invalid lexical score".into()));
                    lexical.push(SearchCandidateKey { version_id: row.get(0)?, syntax_id: stored_id_row(row,1,false)? });
                }
            }
            Ok(McpSearchCandidates { pin: selected.pin, cards, lexical })
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
