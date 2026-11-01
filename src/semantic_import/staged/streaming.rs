//! Complete side discovery followed by document-local Common classification.
use super::super::join_store::{ClaimTarget, GlobalDiscoverySink, MethodSummary, RowReserve};
use super::super::{
    JoinedFacts, discover_join_claim, emit_join_symbols, emit_type_relationships, join_occurrence,
    method_target_owner_matches, scoped_key, settle_join_references,
};
use super::codec;
use super::*;

/// The `sources.ord` of a symbol's scope document (a local symbol's document, or the crate
/// root of a Cargo-target-scoped one, #394), `-1` for a global symbol (`?3`, `?4` are its
/// language and path, both empty for a global one).
const LOCAL_DOC: &str =
    "CASE WHEN ?4='' THEN -1 ELSE (SELECT ord FROM sources WHERE language=?3 AND path=?4) END";

/// Restricts a query to rows whose owning document is eligible.
fn eligible(sql: &str, owner: &str, filtered: bool) -> String {
    if filtered {
        format!(
            "{sql} AND EXISTS (SELECT 1 FROM w_docs w WHERE w.source_ord={owner} \
             AND w.eligible=1 AND w.failed=0)"
        )
    } else {
        sql.to_owned()
    }
}

fn definitions_sql(filtered: bool) -> String {
    eligible(
        &format!(
            "SELECT s.language, s.path, g.syntax_id FROM g_definitions g JOIN sources s ON s.ord=g.owner_doc \
             WHERE g.side=?1 AND g.symbol=?2 AND g.local_doc={LOCAL_DOC} AND g.common_compatible=1"
        ),
        "g.owner_doc",
        filtered,
    )
}

/// The callable kind discovery recorded for one exact definition (#470); absent is `Other`.
/// Every row of one target carries the same kind, so any one answers.
const TARGET_CALLABLE_SQL: &str = "SELECT g.callable FROM g_definitions g JOIN sources s \
    ON s.ord=g.owner_doc WHERE g.side=?1 AND s.language=?2 AND s.path=?3 AND g.syntax_id=?4 LIMIT 1";

fn methods_sql(target: bool, filtered: bool) -> String {
    let key = if target {
        format!("local_doc={LOCAL_DOC}")
    } else {
        "owner_doc=(SELECT ord FROM sources WHERE language=?3 AND path=?4)".to_owned()
    };
    format!(
        "{} ORDER BY ord",
        eligible(
            &format!(
                "SELECT body,body_bytes,decoded_bytes FROM g_methods WHERE side=?1 AND symbol=?2 AND {key}"
            ),
            "owner_doc",
            filtered,
        )
    )
}

/// Presence is a function of the claim set alone, so it is read from the admitted claims'
/// presence index: all of them for the captured side, the eligible ones for the destination.
/// One keyed probe per presence bit (`?2`), whatever the number of claims naming the symbol.
fn presence_sql(filtered: bool) -> String {
    format!(
        "{} LIMIT 1",
        eligible(
            "SELECT 1 FROM claims WHERE symbol=?1 AND presence=?2",
            "claims.source_ord",
            filtered,
        )
    )
}

/// The same probe restricted to one Cargo target, by its crate root (`?3` language, `?4` path).
fn target_presence_sql(filtered: bool) -> String {
    format!(
        "{} LIMIT 1",
        eligible(
            "SELECT 1 FROM claims WHERE symbol=?1 AND presence=?2 \
             AND target_root=(SELECT ord FROM sources WHERE language=?3 AND path=?4)",
            "claims.source_ord",
            filtered,
        )
    )
}

/// Crate roots and library flags of the Cargo targets whose claims name `?1` as present, one
/// row per claim.
fn defining_targets_sql(filtered: bool) -> String {
    eligible(
        "SELECT s.language, s.path, c.target_lib, c.target_package, c.target_version FROM claims c \
         JOIN sources s ON s.ord=c.target_root WHERE c.symbol=?1 AND c.presence>0",
        "c.source_ord",
        filtered,
    )
}

/// Captured exact-definition membership of a raw symbol, in any document or scope.
const CAPTURED_SYMBOL_SQL: &str = "SELECT 1 FROM g_definitions WHERE side=0 AND symbol=?1 LIMIT 1";

#[cfg(feature = "test-support")]
pub(super) fn target_callable_sql() -> &'static str {
    TARGET_CALLABLE_SQL
}

#[cfg(feature = "test-support")]
pub(super) fn cursor_sql() -> Vec<String> {
    let mut queries = vec![];
    for filtered in [false, true] {
        queries.push(definitions_sql(filtered));
        queries.push(methods_sql(true, filtered));
        queries.push(methods_sql(false, filtered));
        queries.push(presence_sql(filtered));
        queries.push(target_presence_sql(filtered));
        queries.push(defining_targets_sql(filtered));
    }
    queries
}

/// The completed global context of one side: exact definitions, symbol presence and measured
/// methods in `g_*`, queried by key one row at a time. With `filtered`, the context is the
/// rows of `side` whose owning document is eligible: for a shared pin, the eligible
/// destination context is exactly that restriction of the captured one (see `StagedJoin::run`).
pub(super) struct CompletedGlobal<'a> {
    ctx: &'a JoinCtx<'a>,
    side: u8,
    filtered: bool,
    source_set_id: String,
    _source_reservation: Reservation,
}

/// Pushes one row whose bytes were admitted through `reserve`, admitting a doubled buffer
/// first when the Vec is full. Superseded buffers stay admitted for the caller's operation.
fn push_reserved<T>(
    rows: &mut Vec<T>,
    row_bytes: u64,
    reserve: RowReserve<'_>,
    make_row: impl FnOnce() -> Result<T>,
) -> Result<()> {
    if rows.len() == rows.capacity() {
        let next = rows
            .capacity()
            .checked_mul(2)
            .map(|n| n.max(4))
            .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?;
        reserve(
            (next as u64)
                .checked_mul(std::mem::size_of::<T>() as u64)
                .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?,
        )?;
        rows.reserve_exact(next - rows.len());
    }
    reserve(row_bytes)?;
    rows.push(make_row()?);
    Ok(())
}

impl<'a> CompletedGlobal<'a> {
    fn new(ctx: &'a JoinCtx<'a>, side: u8, filtered: bool, source_set_id: &str) -> Result<Self> {
        Ok(Self {
            ctx,
            side,
            filtered,
            _source_reservation: ctx.live.reserve(source_set_id.len() as u64)?,
            source_set_id: source_set_id.to_owned(),
        })
    }

    /// The restriction of this context to the rows of eligible owner documents.
    pub(super) fn eligible_view(&self) -> Result<CompletedGlobal<'a>> {
        Self::new(self.ctx, self.side, true, &self.source_set_id)
    }

    /// One query over the matching methods; each row is charged and admitted before decode.
    fn methods(
        &self,
        document: &native::DocumentKey,
        symbol: &str,
        target: bool,
        reserve: RowReserve<'_>,
    ) -> Result<Vec<MethodSummary>> {
        self.ctx.charge(1)?;
        let (language, path) = if target && !symbol.starts_with("local ") {
            ("", "")
        } else {
            (document.language.as_str(), document.path.as_str())
        };
        let sql = methods_sql(target, self.filtered);
        let mut statement = self.ctx.conn.prepare_cached(&sql)?;
        let mut rows = statement.query(params![self.side, symbol, language, path])?;
        let mut result = Vec::new();
        reserve(std::mem::size_of::<Vec<MethodSummary>>() as u64)?;
        while let Some(row) = rows.next()? {
            self.ctx.charge(1)?;
            let body_bytes = u64::try_from(row.get::<_, i64>(1)?)?;
            let decoded = u64::try_from(row.get::<_, i64>(2)?)?
                .checked_sub(std::mem::size_of::<MethodSummary>() as u64)
                .context("corrupt staged method size")?;
            // Borrowed SQL text avoids a String copy, but serde still owns escape scratch.
            let escape_bytes = if body_bytes == 0 {
                0
            } else {
                body_bytes
                    .checked_mul(2)
                    .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?
                    .max(8)
            };
            // The row's text buffer lives in SQLite while serde decodes from it.
            let scratch = sizes::sum(&[body_bytes, escape_bytes])?;
            push_reserved(&mut result, decoded, reserve, || {
                let _scratch = self.ctx.live.reserve(scratch)?;
                Ok(serde_json::from_slice(row.get_ref(0)?.as_bytes()?)?)
            })?;
        }
        Ok(result)
    }

    /// Whether any captured exact definition binds `symbol`, the existing internal-symbol
    /// suppression set of the rejoin.
    pub(super) fn captured_symbol(ctx: &JoinCtx<'_>, symbol: &str) -> Result<bool> {
        ctx.charge(1)?;
        exists(ctx.conn, CAPTURED_SYMBOL_SQL, &[&symbol])
    }
}

impl GlobalJoinReads for CompletedGlobal<'_> {
    fn declarations(
        &self,
        key: &ScopedKey,
        reserve: RowReserve<'_>,
    ) -> Result<Vec<(native::DocumentKey, String)>> {
        self.ctx.charge(1)?;
        let (language, path) = local_columns(key);
        let sql = definitions_sql(self.filtered);
        let mut statement = self.ctx.conn.prepare_cached(&sql)?;
        let mut rows = statement.query(params![self.side, key.0, language, path])?;
        let mut result = Vec::new();
        reserve(std::mem::size_of::<Vec<(native::DocumentKey, String)>>() as u64)?;
        while let Some(row) = rows.next()? {
            self.ctx.charge(1)?;
            let bytes = sizes::sum(&[
                self.source_set_id.len() as u64,
                row.get_ref(0)?.as_str()?.len() as u64,
                row.get_ref(1)?.as_str()?.len() as u64,
                codec::sid_len(row.get_ref(2)?)?,
            ])?;
            push_reserved(&mut result, bytes, reserve, || {
                Ok((
                    native::DocumentKey {
                        source_set_id: self.source_set_id.clone(),
                        language: row.get(0)?,
                        path: row.get(1)?,
                    },
                    codec::sid_text(row.get_ref(2)?)?,
                ))
            })?;
        }
        // Owners are stored by ordinal; the lookup's order stays (language, path, syntax ID).
        result.sort_unstable_by(|a, b| {
            (&a.0.language, &a.0.path, &a.1).cmp(&(&b.0.language, &b.0.path, &b.1))
        });
        Ok(result)
    }
    fn target_callable(
        &self,
        document: &native::DocumentKey,
        syntax_id: &str,
    ) -> Result<TargetCallable> {
        self.ctx.charge(1)?;
        let code: Option<i64> = self
            .ctx
            .conn
            .prepare_cached(TARGET_CALLABLE_SQL)?
            .query_row(
                params![
                    self.side,
                    document.language,
                    document.path,
                    codec::sid(syntax_id)
                ],
                |row| row.get(0),
            )
            .optional()?;
        code.map_or(Ok(TargetCallable::Other), TargetCallable::from_code)
    }
    fn presence(&self, symbol: &str, target: Option<&native::DocumentKey>) -> Result<(bool, bool)> {
        use crate::semantic_batch::staging::{PRESENCE_DECLARED, PRESENCE_INFORMATION};
        // Side 1 is always the eligible claim set.
        let filtered = self.filtered || self.side == 1;
        let sql = match target {
            None => presence_sql(filtered),
            Some(_) => target_presence_sql(filtered),
        };
        let probe = |bit: i64| -> Result<bool> {
            self.ctx.charge(1)?;
            match target {
                None => exists(self.ctx.conn, &sql, &[&symbol, &bit]),
                Some(root) => exists(
                    self.ctx.conn,
                    &sql,
                    &[&symbol, &bit, &root.language, &root.path],
                ),
            }
        };
        Ok((probe(PRESENCE_DECLARED)?, probe(PRESENCE_INFORMATION)?))
    }
    fn defining_targets(&self, symbol: &str) -> Result<Vec<ClaimTarget>> {
        self.ctx.charge(1)?;
        let sql = defining_targets_sql(self.filtered || self.side == 1);
        let mut statement = self.ctx.conn.prepare_cached(&sql)?;
        let mut rows = statement.query(params![symbol])?;
        let mut result = BTreeSet::new();
        while let Some(row) = rows.next()? {
            self.ctx.charge(1)?;
            result.insert((
                native::DocumentKey {
                    source_set_id: self.source_set_id.clone(),
                    language: row.get(0)?,
                    path: row.get(1)?,
                },
                (row.get::<_, i64>(2)? == 1)
                    .then(|| Ok::<_, rusqlite::Error>((row.get(3)?, row.get(4)?)))
                    .transpose()?,
            ));
        }
        Ok(result.into_iter().collect())
    }
    fn source_methods(
        &self,
        document: &native::DocumentKey,
        symbol: &str,
        reserve: RowReserve<'_>,
    ) -> Result<Vec<MethodSummary>> {
        self.methods(document, symbol, false, reserve)
    }
    fn target_methods(
        &self,
        document: &native::DocumentKey,
        symbol: &str,
        reserve: RowReserve<'_>,
    ) -> Result<Vec<MethodSummary>> {
        self.methods(document, symbol, true, reserve)
    }
}

/// Writes one side's exact definitions and methods. Presence needs no write: the admitted
/// claims' presence index already holds it (`claim_presence`). `targets` collects the current
/// document's exact-definition syntax IDs: every internal target settlement can name is one of
/// them, so their name status is the only declaration status it ever asks for.
struct GlobalWriter<'a> {
    global: CompletedGlobal<'a>,
    targets: BTreeSet<String>,
    targets_reservation: Reservation,
}

/// Which declaration name status (`nat_declarations`) a discovery pass writes from the document
/// groups it loads.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TargetStatus {
    /// None: an unshared pin's captured side. Its exact definitions are named captured
    /// declarations by construction (`StagedJoin::retarget`); the captured status of the
    /// destination's own targets is written by `remaining_target_status`.
    None,
    /// The side's own exact-definition targets, in its own revision (a shared pin's captured
    /// side, which is also the destination).
    Own,
    /// The side's own targets and the captured side's targets of the same document, in the
    /// destination revision (an unshared pin's eligible destination side).
    OwnAndCaptured,
}

/// The captured exact-definition syntax IDs owned by one source document.
const CAPTURED_TARGETS_SQL: &str = "SELECT DISTINCT syntax_id FROM g_definitions \
    WHERE side=0 AND owner_doc=(SELECT ord FROM sources WHERE language=?1 AND path=?2)";

/// Adds each captured exact-definition syntax ID of `document` to `targets`, admitting every
/// new entry before it is inserted. One charged lookup, and one per row.
fn add_captured_targets(
    ctx: &JoinCtx<'_>,
    document: &native::DocumentKey,
    targets: &mut BTreeSet<String>,
    reservation: &mut Reservation,
) -> Result<()> {
    ctx.charge(1)?;
    let mut statement = ctx.conn.prepare_cached(CAPTURED_TARGETS_SQL)?;
    let mut rows = statement.query(params![document.language, document.path])?;
    while let Some(row) = rows.next()? {
        ctx.charge(1)?;
        let value = row.get_ref(0)?;
        reservation.grow(sizes::sum(&[
            sizes::map_entries::<String, ()>(1)?,
            codec::sid_len(value)?,
        ])?)?;
        targets.insert(codec::sid_text(value)?);
    }
    Ok(())
}

/// Writes the name status (`nat_declarations` in `group`'s revision `rev`, document `ord`) of
/// every declaration of `group` whose syntax ID is in `targets`.
fn write_target_status(
    ctx: &JoinCtx<'_>,
    rev: u8,
    ord: i64,
    group: &DocGroup,
    targets: &BTreeSet<String>,
) -> Result<()> {
    for (n, declaration) in group.native.declarations.iter().enumerate() {
        if !targets.contains(&declaration.syntax_id) {
            continue;
        }
        ctx.row("nat_declarations")?;
        let name_ok = declaration.name_range.as_ref().is_some_and(|range| {
            group.text.get(range.start..range.end) == declaration.name.as_deref()
        });
        ctx.conn
            .prepare_cached("INSERT INTO nat_declarations VALUES(?1,?2,?3,?4,?5,?6)")?
            .execute(params![
                rev,
                ord,
                codec::sid(&declaration.syntax_id),
                i64::try_from(n)?,
                declaration.name_range.is_some(),
                name_ok
            ])?;
    }
    Ok(())
}

impl GlobalWriter<'_> {
    /// Writes the name status of the current document's targets (see `TargetStatus`) from its
    /// loaded native group, and ends the document.
    fn target_declarations(
        &mut self,
        store: &StagedSide<'_>,
        document: &native::DocumentKey,
        status: TargetStatus,
    ) -> Result<()> {
        let mut targets = std::mem::take(&mut self.targets);
        // The moved set keeps its charge until this function drops it.
        let mut targets_charge = std::mem::replace(
            &mut self.targets_reservation,
            self.global.ctx.live.reserve(0)?,
        );
        match status {
            TargetStatus::None => return Ok(()),
            TargetStatus::Own => {}
            TargetStatus::OwnAndCaptured => {
                add_captured_targets(self.global.ctx, document, &mut targets, &mut targets_charge)?
            }
        }
        if targets.is_empty() {
            return Ok(());
        }
        let group = store
            .borrowed_group(document)?
            .context("missing native document")?;
        let (ord, _) = store
            .doc_row(document)?
            .context("missing native document")?;
        write_target_status(self.global.ctx, store.rev, ord, group, &targets)
    }
}

/// The destination status of captured targets whose document the eligible destination
/// discovery did not visit (an ineligible or failed document), and the captured status of
/// destination targets that are not captured targets of the same document. Unshared pins only:
/// with the two discovery passes it completes the status of every key `g_definitions` holds.
pub(super) fn remaining_target_status(
    captured: &StagedSide<'_>,
    destination: &StagedSide<'_>,
) -> Result<()> {
    let ctx = captured.ctx;
    // (status side, revision view, the documents and syntax IDs it lacks)
    let passes: [(&StagedSide<'_>, &str); 2] = [
        (
            destination,
            "SELECT s.language, s.path, g.syntax_id FROM g_definitions g JOIN sources s \
             ON s.ord=g.owner_doc WHERE g.side=0 AND NOT EXISTS (SELECT 1 FROM w_docs w \
             WHERE w.source_ord=g.owner_doc AND w.eligible=1 AND w.failed=0) \
             ORDER BY g.owner_doc, g.syntax_id",
        ),
        (
            captured,
            "SELECT s.language, s.path, g.syntax_id FROM g_definitions g JOIN sources s \
             ON s.ord=g.owner_doc WHERE g.side=1 AND NOT EXISTS (SELECT 1 FROM g_definitions c \
             WHERE c.side=0 AND c.owner_doc=g.owner_doc AND c.syntax_id=g.syntax_id) \
             ORDER BY g.owner_doc, g.syntax_id",
        ),
    ];
    for (side, sql) in passes {
        ctx.charge(1)?;
        let mut statement = ctx.conn.prepare_cached(sql)?;
        let mut rows = statement.query([])?;
        let mut current: Option<(native::DocumentKey, BTreeSet<String>, Reservation)> = None;
        let flush = |entry: Option<(native::DocumentKey, BTreeSet<String>, Reservation)>| {
            let Some((document, targets, _charge)) = entry else {
                return anyhow::Ok(());
            };
            let Some((ord, _)) = side.doc_row(&document)? else {
                return Ok(());
            };
            let group = side.group(&document)?.context("missing native document")?;
            write_target_status(ctx, side.rev, ord, &group, &targets)
        };
        while let Some(row) = rows.next()? {
            ctx.check_cancel()?;
            ctx.charge(1)?;
            let (language, path) = (row.get_ref(0)?.as_str()?, row.get_ref(1)?.as_str()?);
            let value = row.get_ref(2)?;
            if current.as_ref().is_none_or(|(document, _, _)| {
                document.language != language || document.path != path
            }) {
                flush(current.take())?;
                let document = native::DocumentKey {
                    source_set_id: side.source_set_id().to_owned(),
                    language: language.to_owned(),
                    path: path.to_owned(),
                };
                let reservation = ctx.live.reserve(sizes::sum(&[
                    std::mem::size_of::<native::DocumentKey>() as u64,
                    sizes::document(&document)?,
                ])?)?;
                current = Some((document, BTreeSet::new(), reservation));
            }
            let (_, targets, reservation) = current.as_mut().expect("current document");
            reservation.grow(sizes::sum(&[
                sizes::map_entries::<String, ()>(1)?,
                codec::sid_len(value)?,
            ])?)?;
            targets.insert(codec::sid_text(value)?);
        }
        flush(current.take())?;
    }
    Ok(())
}

impl GlobalDiscoverySink for GlobalWriter<'_> {
    fn symbol_presence(
        &mut self,
        symbol: &str,
        target: Option<&ClaimTarget>,
        declared: bool,
        information: bool,
    ) -> Result<()> {
        // The claim's admitted row already records this presence.
        let _ = (symbol, target, declared, information);
        Ok(())
    }
    fn exact_definition(
        &mut self,
        key: ScopedKey,
        document: native::DocumentKey,
        syntax_id: String,
        common_compatible: bool,
        callable: TargetCallable,
    ) -> Result<()> {
        let (language, path) = local_columns(&key);
        if !self.targets.contains(&syntax_id) {
            self.targets_reservation.grow(sizes::sum(&[
                sizes::map_entries::<String, ()>(1)?,
                syntax_id.len() as u64,
            ])?)?;
            self.targets.insert(syntax_id.clone());
        }
        self.global.ctx.row("g_definitions")?;
        self.global.ctx.conn.prepare_cached(&format!(
            "INSERT INTO g_definitions VALUES(?1,?2,{LOCAL_DOC},(SELECT ord FROM sources WHERE language=?5 AND path=?6),?7,?8,?9) \
             ON CONFLICT(side,symbol,local_doc,owner_doc,syntax_id) DO UPDATE SET common_compatible=MAX(common_compatible,excluded.common_compatible)",
        ))?.execute(params![self.global.side,key.0,language,path,document.language,document.path,codec::sid(&syntax_id),common_compatible,callable as i64])?;
        Ok(())
    }
}

struct CountJson {
    bytes: u64,
    overflow: bool,
}
impl CountJson {
    fn measure(value: &impl Serialize) -> Result<u64> {
        let mut count = Self {
            bytes: 0,
            overflow: false,
        };
        let result = serde_json::to_writer(&mut count, value);
        if count.overflow {
            return Err(ScipCaptureError::LimitExceeded("unit expansion".into()).into());
        }
        result?;
        Ok(count.bytes)
    }
}
impl std::io::Write for CountJson {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let Some(total) = self.bytes.checked_add(bytes.len() as u64) else {
            self.overflow = true;
            return Err(std::io::Error::other("unit expansion"));
        };
        self.bytes = total;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl GlobalWriter<'_> {
    fn method(
        &mut self,
        document: &native::DocumentKey,
        symbol: &str,
        summary: MethodSummary,
    ) -> Result<()> {
        let _key_reservation = self.global.ctx.live.reserve(sizes::sum(&[
            std::mem::size_of::<ScopedKey>() as u64,
            symbol.len() as u64,
            if symbol.starts_with("local ") {
                sizes::document(document)?
            } else {
                0
            },
        ])?)?;
        let key = scoped_key(symbol, document);
        let (language, path) = local_columns(&key);
        let decoded_bytes = sizes::decoded_method(&summary)?;
        let body_bytes = CountJson::measure(&summary)?;
        let _body_reservation = self.global.ctx.live.reserve(body_bytes)?;
        let mut body = Vec::with_capacity(usize::try_from(body_bytes)?);
        serde_json::to_writer(&mut body, &summary)?;
        let body = std::str::from_utf8(&body)?;
        self.global.ctx.row("g_methods")?;
        self.global
            .ctx
            .conn
            .prepare_cached(&format!(
                "INSERT INTO g_methods(side,symbol,local_doc,owner_doc,body,body_bytes,decoded_bytes) \
                 VALUES(?1,?2,{LOCAL_DOC},(SELECT ord FROM sources WHERE language=?5 AND path=?6),?7,?8,?9)"
            ))?
            .execute(params![
                self.global.side,
                symbol,
                language,
                path,
                document.language,
                document.path,
                body,
                i64::try_from(body_bytes)?,
                i64::try_from(decoded_bytes)?,
            ])?;
        Ok(())
    }
}

/// Admit a new requested buffer while the old buffer remains live, then move the row.
pub(super) fn append_reserved<T>(
    rows: &mut Vec<T>,
    row: T,
    payload: u64,
    retained: &mut Reservation,
) -> Result<()> {
    append_reserved_with(rows, payload, retained, || row)
}

pub(super) fn append_reserved_with<T>(
    rows: &mut Vec<T>,
    payload: u64,
    retained: &mut Reservation,
    make_row: impl FnOnce() -> T,
) -> Result<()> {
    let capacity = rows.capacity();
    if rows.len() == capacity {
        let next = capacity
            .checked_mul(2)
            .map(|n| n.max(4))
            .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?;
        let bytes = (next as u64)
            .checked_mul(std::mem::size_of::<T>() as u64)
            .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?;
        retained.grow(sizes::sum(&[payload, bytes])?)?;
        rows.reserve_exact(next - rows.len());
        retained.release((capacity * std::mem::size_of::<T>()) as u64)?;
    } else {
        retained.grow(payload)?;
    }
    rows.push(make_row());
    Ok(())
}

struct DocumentIndex<'g, 'a> {
    global: &'g CompletedGlobal<'a>,
    document: native::DocumentKey,
    retained: Reservation,
    type_uses: BTreeMap<(usize, usize), BTreeSet<String>>,
    occurrences: Vec<OccurrenceRow>,
    relationships: Vec<RelationshipRow>,
    endpoints: BTreeMap<String, BTreeMap<String, Vec<usize>>>,
    reference_claims: BTreeMap<String, Vec<(String, semantic::Target)>>,
    call_references: BTreeMap<String, String>,
    reads: RefCell<ReadCache>,
}

/// Global declaration and presence rows this document already read. A document's occurrences
/// and symbol information name the same symbols many times; each is queried once per join.
struct ReadCache {
    declarations: BTreeMap<ScopedKey, Vec<(native::DocumentKey, String)>>,
    presence: BTreeMap<(String, Option<native::DocumentKey>), (bool, bool)>,
    targets: BTreeMap<String, Vec<ClaimTarget>>,
    retained: Reservation,
}

/// The owned payload of one declaration row, beyond its tuple slot.
fn declaration_row_payload(row: &(native::DocumentKey, String)) -> Result<u64> {
    sizes::sum(&[sizes::document(&row.0)?, row.1.len() as u64])
}

impl<'g, 'a> DocumentIndex<'g, 'a> {
    fn new(global: &'g CompletedGlobal<'a>, document: &native::DocumentKey) -> Result<Self> {
        let retained = global.ctx.live.reserve(sizes::sum(&[
            std::mem::size_of::<Self>() as u64,
            sizes::document(document)?,
        ])?)?;
        Ok(Self {
            global,
            document: document.clone(),
            retained,
            type_uses: BTreeMap::new(),
            occurrences: Vec::new(),
            relationships: Vec::new(),
            endpoints: BTreeMap::new(),
            reference_claims: BTreeMap::new(),
            call_references: BTreeMap::new(),
            reads: RefCell::new(ReadCache {
                declarations: BTreeMap::new(),
                presence: BTreeMap::new(),
                targets: BTreeMap::new(),
                retained: global.ctx.live.reserve(0)?,
            }),
        })
    }

    fn check_document(&self, language: &str, path: &str) -> Result<()> {
        anyhow::ensure!(
            self.document.language == language && self.document.path == path,
            "foreign document in local join state"
        );
        Ok(())
    }

    /// Sizes the occurrence buffer for `count` rows at once, admitted first, so filling it
    /// never doubles the buffer (no growth slack, no old and new buffer live together).
    fn reserve_occurrences(&mut self, count: usize) -> Result<()> {
        let additional = count.saturating_sub(self.occurrences.capacity());
        self.retained.grow(
            (additional as u64)
                .checked_mul(std::mem::size_of::<OccurrenceRow>() as u64)
                .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?,
        )?;
        self.occurrences.reserve_exact(additional);
        Ok(())
    }
}

impl GlobalJoinReads for DocumentIndex<'_, '_> {
    fn declarations(
        &self,
        key: &ScopedKey,
        reserve: RowReserve<'_>,
    ) -> Result<Vec<(native::DocumentKey, String)>> {
        if let Some(rows) = self.reads.borrow().declarations.get(key) {
            // The caller's copy is admitted exactly as a fetched result would be.
            reserve(sizes::sum(&[
                std::mem::size_of::<Vec<(native::DocumentKey, String)>>() as u64,
                (rows.len() as u64)
                    .checked_mul(std::mem::size_of::<(native::DocumentKey, String)>() as u64)
                    .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?,
            ])?)?;
            for row in rows {
                reserve(declaration_row_payload(row)?)?;
            }
            return Ok(rows.clone());
        }
        let rows = self.global.declarations(key, reserve)?;
        let mut reads = self.reads.borrow_mut();
        let mut bytes = sizes::sum(&[
            sizes::map_entries::<ScopedKey, Vec<(native::DocumentKey, String)>>(1)?,
            key.0.len() as u64,
            key.1.as_ref().map_or(Ok(0), sizes::document)?,
            (rows.len() as u64)
                .checked_mul(std::mem::size_of::<(native::DocumentKey, String)>() as u64)
                .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?,
        ])?;
        for row in &rows {
            bytes = sizes::sum(&[bytes, declaration_row_payload(row)?])?;
        }
        // The cache is optional: without room for it the rows are returned uncached.
        if reads.retained.grow(bytes).is_ok() {
            reads.declarations.insert(key.clone(), rows.clone());
        }
        Ok(rows)
    }
    fn target_callable(
        &self,
        document: &native::DocumentKey,
        syntax_id: &str,
    ) -> Result<TargetCallable> {
        self.global.target_callable(document, syntax_id)
    }
    fn presence(&self, symbol: &str, target: Option<&native::DocumentKey>) -> Result<(bool, bool)> {
        let key = (symbol.to_owned(), target.cloned());
        if let Some(found) = self.reads.borrow().presence.get(&key) {
            return Ok(*found);
        }
        let found = self.global.presence(symbol, target)?;
        let mut reads = self.reads.borrow_mut();
        let bytes = sizes::sum(&[
            sizes::map_entries::<(String, Option<native::DocumentKey>), (bool, bool)>(1)?,
            symbol.len() as u64,
            target.map_or(Ok(0), sizes::document)?,
        ])?;
        if reads.retained.grow(bytes).is_ok() {
            reads.presence.insert(key, found);
        }
        Ok(found)
    }
    fn defining_targets(&self, symbol: &str) -> Result<Vec<ClaimTarget>> {
        if let Some(found) = self.reads.borrow().targets.get(symbol) {
            return Ok(found.clone());
        }
        let found = self.global.defining_targets(symbol)?;
        let mut reads = self.reads.borrow_mut();
        let mut bytes = sizes::sum(&[
            sizes::map_entries::<String, Vec<ClaimTarget>>(1)?,
            symbol.len() as u64,
        ])?;
        for (root, package) in &found {
            bytes = sizes::sum(&[
                bytes,
                std::mem::size_of::<ClaimTarget>() as u64,
                sizes::document(root)?,
                package.as_ref().map_or(0, |(name, version)| {
                    (name.len() + version.as_ref().map_or(0, String::len)) as u64
                }),
            ])?;
        }
        // Optional like the presence cache: without room the roots are returned uncached.
        if reads.retained.grow(bytes).is_ok() {
            reads.targets.insert(symbol.to_owned(), found.clone());
        }
        Ok(found)
    }
    fn source_methods(
        &self,
        document: &native::DocumentKey,
        symbol: &str,
        reserve: RowReserve<'_>,
    ) -> Result<Vec<MethodSummary>> {
        self.global.source_methods(document, symbol, reserve)
    }
    fn target_methods(
        &self,
        document: &native::DocumentKey,
        symbol: &str,
        reserve: RowReserve<'_>,
    ) -> Result<Vec<MethodSummary>> {
        self.global.target_methods(document, symbol, reserve)
    }
}

impl DocumentJoinState for DocumentIndex<'_, '_> {
    type ClaimReservation = Reservation;
    fn put_type_use(
        &mut self,
        document: &native::DocumentKey,
        start: usize,
        end: usize,
        symbol: &str,
    ) -> Result<()> {
        self.check_document(&document.language, &document.path)?;
        let key = (start, end);
        if self
            .type_uses
            .get(&key)
            .is_some_and(|symbols| symbols.contains(symbol))
        {
            return Ok(());
        }
        let entry = if self.type_uses.contains_key(&key) {
            0
        } else {
            sizes::map_entries::<(usize, usize), BTreeSet<String>>(1)?
        };
        self.retained.grow(sizes::sum(&[
            entry,
            sizes::map_entries::<String, ()>(1)?,
            symbol.len() as u64,
        ])?)?;
        self.type_uses.entry(key).or_default().insert(symbol.into());
        Ok(())
    }
    fn type_uses_clone_bytes(
        &self,
        document: &native::DocumentKey,
        start: usize,
        end: usize,
    ) -> Result<u64> {
        self.check_document(&document.language, &document.path)?;
        self.type_uses
            .get(&(start, end))
            .into_iter()
            .flatten()
            .try_fold(
                std::mem::size_of::<BTreeSet<String>>() as u64,
                |total, symbol| {
                    sizes::sum(&[
                        total,
                        sizes::map_entries::<String, ()>(1)?,
                        symbol.len() as u64,
                    ])
                },
            )
    }
    fn type_uses(
        &self,
        document: &native::DocumentKey,
        start: usize,
        end: usize,
    ) -> Result<BTreeSet<String>> {
        self.check_document(&document.language, &document.path)?;
        Ok(self
            .type_uses
            .get(&(start, end))
            .cloned()
            .unwrap_or_default())
    }
    fn put_occurrence(&mut self, row: OccurrenceRow) -> Result<()> {
        self.check_document(&row.document.language, &row.document.path)?;
        let payload = sizes::occurrence(&row)? - std::mem::size_of::<OccurrenceRow>() as u64;
        append_reserved(&mut self.occurrences, row, payload, &mut self.retained)
    }
    fn occurrence_count(&self) -> usize {
        self.occurrences.len()
    }
    fn occurrence_clone_bytes(&self, ordinal: usize) -> Result<u64> {
        sizes::occurrence(
            self.occurrences
                .get(ordinal)
                .context("missing document occurrence")?,
        )
    }
    fn occurrence(&self, ordinal: usize) -> Result<OccurrenceRow> {
        let row = self
            .occurrences
            .get(ordinal)
            .context("missing document occurrence")?;
        Ok(row.clone())
    }
    fn put_relationship(&mut self, language: &str, path: &str, row: RelationshipRow) -> Result<()> {
        self.check_document(language, path)?;
        let payload = sizes::relationship(&row)? - std::mem::size_of::<RelationshipRow>() as u64;
        append_reserved(&mut self.relationships, row, payload, &mut self.retained)
    }
    fn relationships_clone_bytes(&self, language: &str, path: &str) -> Result<u64> {
        self.check_document(language, path)?;
        self.relationships.iter().try_fold(
            std::mem::size_of::<Vec<RelationshipRow>>() as u64,
            |total, row| sizes::sum(&[total, sizes::relationship(row)?]),
        )
    }
    fn relationships(&self, language: &str, path: &str) -> Result<Vec<RelationshipRow>> {
        self.check_document(language, path)?;
        Ok(self.relationships.clone())
    }
    fn put_relationship_endpoint(
        &mut self,
        language: &str,
        path: &str,
        source: &str,
        target: &str,
        _row: &RelationshipRow,
    ) -> Result<()> {
        self.check_document(language, path)?;
        let ordinal = self.relationships.len() - 1;
        if let Some(rows) = self
            .endpoints
            .get_mut(source)
            .and_then(|targets| targets.get_mut(target))
        {
            return append_reserved(rows, ordinal, 0, &mut self.retained);
        }
        let outer = if self.endpoints.contains_key(source) {
            0
        } else {
            sizes::sum(&[
                source.len() as u64,
                sizes::map_entries::<String, BTreeMap<String, Vec<usize>>>(1)?,
            ])?
        };
        let entry = sizes::sum(&[
            outer,
            target.len() as u64,
            sizes::map_entries::<String, Vec<usize>>(1)?,
        ])?;
        let mut rows = Vec::new();
        append_reserved(&mut rows, ordinal, entry, &mut self.retained)?;
        if let Some(targets) = self.endpoints.get_mut(source) {
            targets.insert(target.into(), rows);
        } else {
            self.endpoints
                .insert(source.into(), BTreeMap::from([(target.into(), rows)]));
        }
        Ok(())
    }
    fn relationship_endpoints_clone_bytes(
        &self,
        language: &str,
        path: &str,
        source: &str,
        target: &str,
    ) -> Result<u64> {
        self.check_document(language, path)?;
        self.endpoints
            .get(source)
            .and_then(|targets| targets.get(target))
            .into_iter()
            .flatten()
            .try_fold(
                std::mem::size_of::<Vec<RelationshipRow>>() as u64,
                |total, ordinal| {
                    sizes::sum(&[total, sizes::relationship(&self.relationships[*ordinal])?])
                },
            )
    }
    fn relationship_endpoints(
        &self,
        language: &str,
        path: &str,
        source: &str,
        target: &str,
    ) -> Result<Vec<RelationshipRow>> {
        self.check_document(language, path)?;
        let ordinals = self
            .endpoints
            .get(source)
            .and_then(|targets| targets.get(target));
        let mut result = Vec::with_capacity(ordinals.map_or(0, Vec::len));
        result.extend(
            ordinals
                .into_iter()
                .flatten()
                .map(|ordinal| self.relationships[*ordinal].clone()),
        );
        Ok(result)
    }
    fn put_reference_claim(
        &mut self,
        reference_id: &str,
        proof_id: &str,
        target: semantic::Target,
    ) -> Result<()> {
        let entry = if self.reference_claims.contains_key(reference_id) {
            0
        } else {
            sizes::sum(&[
                reference_id.len() as u64,
                sizes::map_entries::<String, Vec<(String, semantic::Target)>>(1)?,
            ])?
        };
        let payload = sizes::sum(&[sizes::target_payload(&target)?, proof_id.len() as u64])?;
        if let Some(rows) = self.reference_claims.get_mut(reference_id) {
            append_reserved_with(rows, payload, &mut self.retained, || {
                (proof_id.into(), target)
            })?;
        } else {
            let mut rows = Vec::new();
            append_reserved_with(
                &mut rows,
                sizes::sum(&[entry, payload])?,
                &mut self.retained,
                || (proof_id.into(), target),
            )?;
            self.reference_claims.insert(reference_id.into(), rows);
        }
        Ok(())
    }
    fn take_reference_claims(
        &mut self,
        reference_id: &str,
    ) -> Result<(Vec<(String, semantic::Target)>, Reservation)> {
        let mut moved = self.global.ctx.live.reserve(0)?;
        let Some(claims) = self.reference_claims.get(reference_id) else {
            return Ok((vec![], moved));
        };
        let bytes = claims.iter().try_fold(
            sizes::sum(&[
                std::mem::size_of::<Vec<(String, semantic::Target)>>() as u64,
                (claims.capacity() * std::mem::size_of::<(String, semantic::Target)>()) as u64,
            ])?,
            |total, (proof, target)| {
                sizes::sum(&[total, proof.len() as u64, sizes::target_payload(target)?])
            },
        )?;
        // The returned guard follows moved payload until this settlement item ends.
        self.retained.transfer_to(&mut moved, bytes)?;
        let result = self
            .reference_claims
            .remove(reference_id)
            .expect("entry just found");
        self.retained.release(sizes::sum(&[
            reference_id.len() as u64,
            sizes::map_entries::<String, Vec<(String, semantic::Target)>>(1)?
                - std::mem::size_of::<Vec<(String, semantic::Target)>>() as u64,
        ])?)?;
        Ok((result, moved))
    }
    fn put_call_reference(&mut self, proof_id: &str, reference_id: &str) -> Result<()> {
        if let Some(value) = self.call_references.get_mut(proof_id) {
            let previous = value.len() as u64;
            self.retained.grow(reference_id.len() as u64)?;
            *value = reference_id.into();
            self.retained.release(previous)?;
        } else {
            self.retained.grow(sizes::sum(&[
                sizes::map_entries::<String, String>(1)?,
                proof_id.len() as u64,
                reference_id.len() as u64,
            ])?)?;
            self.call_references
                .insert(proof_id.into(), reference_id.into());
        }
        Ok(())
    }
    fn call_reference_clone_bytes(&self, proof_id: &str) -> Result<u64> {
        Ok(self.call_references.get(proof_id).map_or(0, |value| {
            std::mem::size_of::<String>() as u64 + value.len() as u64
        }))
    }
    fn call_reference(&self, proof_id: &str) -> Result<Option<String>> {
        Ok(self.call_references.get(proof_id).cloned())
    }
}

struct DocumentSink {
    facts: JoinedFacts,
    retained: Reservation,
    reference_indices: BTreeMap<String, usize>,
}

impl DocumentSink {
    fn new(global: &CompletedGlobal<'_>) -> Result<Self> {
        let retained = global
            .ctx
            .live
            .reserve(std::mem::size_of::<Self>() as u64)?;
        Ok(Self {
            facts: JoinedFacts::default(),
            retained,
            reference_indices: BTreeMap::new(),
        })
    }

    fn into_facts(self) -> (JoinedFacts, Reservation) {
        (self.facts, self.retained)
    }
}

impl JoinSink for DocumentSink {
    fn symbol(&mut self, row: semantic::Symbol) -> Result<()> {
        let payload = sizes::symbol(&row)? - std::mem::size_of::<semantic::Symbol>() as u64;
        append_reserved(&mut self.facts.symbols, row, payload, &mut self.retained)
    }

    fn declaration_binding(&mut self, row: semantic::DeclarationBinding) -> Result<()> {
        let payload =
            sizes::binding(&row)? - std::mem::size_of::<semantic::DeclarationBinding>() as u64;
        append_reserved(
            &mut self.facts.declaration_bindings,
            row,
            payload,
            &mut self.retained,
        )
    }

    fn join(&mut self, _proof_id: &str, _join: &semantic::Join) -> Result<()> {
        Ok(())
    }

    fn reference_claim(&mut self, proof_id: &str, row: semantic::Reference) -> Result<()> {
        if let Some(previous) = self.facts.reference_claims.get_mut(proof_id) {
            let old = sizes::reference(previous)?;
            self.retained.grow(sizes::reference(&row)?)?;
            *previous = row;
            self.retained.release(old)?;
        } else {
            self.retained.grow(sizes::sum(&[
                sizes::map_entries::<String, semantic::Reference>(1)?,
                proof_id.len() as u64,
                sizes::reference(&row)? - std::mem::size_of::<semantic::Reference>() as u64,
            ])?)?;
            self.facts.reference_claims.insert(proof_id.into(), row);
        }
        Ok(())
    }
    fn reference_slot(&self, reference_id: &str) -> Result<Option<usize>> {
        Ok(self.reference_indices.get(reference_id).copied())
    }

    fn reference_count(&self) -> usize {
        self.facts.references.len()
    }

    fn reference_clone_bytes(&self, slot: usize) -> Result<u64> {
        sizes::reference(
            self.facts
                .references
                .get(slot)
                .context("missing reference row")?,
        )
    }
    fn reference(&self, slot: usize) -> Result<semantic::Reference> {
        let row = self
            .facts
            .references
            .get(slot)
            .context("missing reference row")?;
        Ok(row.clone())
    }

    fn put_reference(&mut self, slot: Option<usize>, row: semantic::Reference) -> Result<usize> {
        let key = if slot.is_none() {
            sizes::sum(&[row.id.len() as u64, sizes::map_entries::<String, usize>(1)?])?
        } else {
            0
        };
        let payload = sizes::reference(&row)? - std::mem::size_of::<semantic::Reference>() as u64;
        match slot {
            Some(slot) => {
                let destination = self
                    .facts
                    .references
                    .get_mut(slot)
                    .context("missing reference row")?;
                let previous = sizes::reference(destination)?
                    - std::mem::size_of::<semantic::Reference>() as u64;
                self.retained.grow(payload)?;
                *destination = row;
                self.retained.release(previous)?;
                Ok(slot)
            }
            None => {
                let slot = self.facts.references.len();
                append_reserved(
                    &mut self.facts.references,
                    row,
                    sizes::sum(&[key, payload])?,
                    &mut self.retained,
                )?;
                self.reference_indices
                    .insert(self.facts.references[slot].id.clone(), slot);
                Ok(slot)
            }
        }
    }

    fn call_binding_count(&self) -> usize {
        self.facts.call_bindings.len()
    }

    fn call_binding_clone_bytes(&self, slot: usize) -> Result<u64> {
        sizes::call(
            self.facts
                .call_bindings
                .get(slot)
                .context("missing call binding row")?,
        )
    }
    fn call_binding(&self, slot: usize) -> Result<semantic::CallBinding> {
        let row = self
            .facts
            .call_bindings
            .get(slot)
            .context("missing call binding row")?;
        Ok(row.clone())
    }

    fn put_call_binding(
        &mut self,
        slot: Option<usize>,
        row: semantic::CallBinding,
    ) -> Result<usize> {
        let payload = sizes::call(&row)? - std::mem::size_of::<semantic::CallBinding>() as u64;
        match slot {
            Some(slot) => {
                let destination = self
                    .facts
                    .call_bindings
                    .get_mut(slot)
                    .context("missing call binding row")?;
                let previous =
                    sizes::call(destination)? - std::mem::size_of::<semantic::CallBinding>() as u64;
                self.retained.grow(payload)?;
                *destination = row;
                self.retained.release(previous)?;
                Ok(slot)
            }
            None => {
                append_reserved(
                    &mut self.facts.call_bindings,
                    row,
                    payload,
                    &mut self.retained,
                )?;
                Ok(self.facts.call_bindings.len() - 1)
            }
        }
    }

    fn type_relationship(&mut self, row: semantic::TypeRelationship) -> Result<()> {
        let payload = sizes::type_relationship(&row)?
            - std::mem::size_of::<semantic::TypeRelationship>() as u64;
        append_reserved(
            &mut self.facts.type_relationships,
            row,
            payload,
            &mut self.retained,
        )
    }

    fn diagnostic(&mut self, document: &native::DocumentKey, code: &str) -> Result<()> {
        if self
            .facts
            .diagnostics
            .get(document)
            .is_some_and(|codes| codes.contains(code))
        {
            return Ok(());
        }
        let entry = if self.facts.diagnostics.contains_key(document) {
            0
        } else {
            sizes::sum(&[
                sizes::document(document)?,
                sizes::map_entries::<native::DocumentKey, BTreeSet<String>>(1)?,
            ])?
        };
        self.retained.grow(sizes::sum(&[
            entry,
            code.len() as u64,
            sizes::map_entries::<String, ()>(1)?,
        ])?)?;
        if let Some(codes) = self.facts.diagnostics.get_mut(document) {
            codes.insert(code.into());
        } else {
            self.facts
                .diagnostics
                .insert(document.clone(), BTreeSet::from([code.into()]));
        }
        Ok(())
    }
}

struct DiscardDiscovery;
impl GlobalDiscoverySink for DiscardDiscovery {
    fn symbol_presence(
        &mut self,
        _symbol: &str,
        _target: Option<&ClaimTarget>,
        _declared: bool,
        _information: bool,
    ) -> Result<()> {
        Ok(())
    }
    fn exact_definition(
        &mut self,
        _key: ScopedKey,
        _document: native::DocumentKey,
        _syntax_id: String,
        _compatible: bool,
        _callable: TargetCallable,
    ) -> Result<()> {
        Ok(())
    }
}

pub(super) fn native_document_side<'a>(
    side: &StagedSide<'a>,
    document: native::DocumentKey,
) -> Result<StagedSide<'a>> {
    let group = side.group(&document)?;
    Ok(StagedSide {
        ctx: side.ctx,
        reader: side.reader,
        header: side.header,
        rev: side.rev,
        side: side.side,
        filtered: side.filtered,
        document: Some(document),
        local_claims: None,
        local_group: group,
    })
}

/// A view of one document of `side` over an already decoded claim set, which captured and
/// destination views of the same document may share.
pub(super) fn claims_side<'a>(
    side: &StagedSide<'a>,
    document: native::DocumentKey,
    claims: Rc<DocumentClaims>,
) -> Result<StagedSide<'a>> {
    let mut local = native_document_side(side, document)?;
    local.local_claims = Some(claims);
    Ok(local)
}

/// Pass 1 (captured) or 3 (eligible destination): the complete global context of one side.
/// A document without claims contributes nothing, so its native facts are never loaded.
pub(super) fn discover_side<'a>(
    side: &StagedSide<'a>,
    encoding: u8,
    target_status: TargetStatus,
) -> Result<CompletedGlobal<'a>> {
    let global = || -> Result<CompletedGlobal<'a>> {
        CompletedGlobal::new(side.ctx, side.side, false, side.source_set_id())
    };
    let mut writer = GlobalWriter {
        global: global()?,
        targets: BTreeSet::new(),
        targets_reservation: side.ctx.live.reserve(0)?,
    };
    for document in side.documents() {
        let document = document?;
        let _view_document_reservation = side.ctx.live.reserve(sizes::sum(&[
            std::mem::size_of::<native::DocumentKey>() as u64,
            sizes::document(&document)?,
        ])?)?;
        let claims = DocumentClaims::load(side, &document)?;
        if claims.claims.is_empty() {
            continue;
        }
        let local_store = claims_side(side, document.clone(), Rc::new(claims))?;
        let reads = global()?;
        let mut local = DocumentIndex::new(&reads, &document)?;
        for claim in local_store.claims() {
            let claim = claim?;
            let mut operation = local_store.operation()?;
            if let Some((range, row)) =
                discover_join_claim(encoding, &local_store, &mut writer, &claim, &mut operation)?
            {
                local.put_type_use(&document, range.start, range.end, &row.symbol)?;
            }
        }
        for declaration in &local_store
            .borrowed_group(&document)?
            .context("missing native document")?
            .native
            .declarations
        {
            if declaration.kind != "method" {
                continue;
            }
            let Some(range) = &declaration.name_range else {
                continue;
            };
            let mut symbols_operation = local_store.operation()?;
            local_store.reserve_operation(&mut symbols_operation, || {
                local.type_uses_clone_bytes(&document, range.start, range.end)
            })?;
            for symbol in local.type_uses(&document, range.start, range.end)? {
                let _summary_reservation = side.ctx.live.reserve(sizes::sum(&[
                    sizes::declaration(declaration)?,
                    (std::mem::size_of::<MethodSummary>()
                        - std::mem::size_of::<native::Declaration>()) as u64,
                ])?)?;
                let summary = MethodSummary {
                    target_owner_matches: method_target_owner_matches(
                        &local_store,
                        &local,
                        declaration,
                        &symbol,
                    )?,
                    method: declaration.clone(),
                };
                writer.method(&document, &symbol, summary)?;
            }
        }
        writer.target_declarations(&local_store, &document, target_status)?;
    }
    Ok(writer.global)
}

/// One document's Common join against a completed global context. The returned guard holds
/// the facts' charge until the caller drops them.
pub(super) fn join_document(
    store: &StagedSide<'_>,
    global: &CompletedGlobal<'_>,
    encoding: u8,
) -> Result<(JoinedFacts, Reservation)> {
    let document = store.document.as_ref().context("document view required")?;
    let mut index = DocumentIndex::new(global, document)?;
    // Each occurrence claim yields at most one occurrence row.
    let local = store
        .local_claims
        .as_ref()
        .context("document claim owner required")?;
    index.reserve_occurrences(
        local
            .claims
            .iter()
            .filter(|claim| claim.envelope.record_kind == "occurrence")
            .count(),
    )?;
    for claim in store.claims() {
        let claim = claim?;
        let mut operation = store.operation()?;
        if let Some((range, row)) = discover_join_claim(
            encoding,
            store,
            &mut DiscardDiscovery,
            &claim,
            &mut operation,
        )? {
            index.put_type_use(&row.document, range.start, range.end, &row.symbol)?;
            index.put_occurrence(row)?;
        }
    }
    super::super::discover_relationship_claims(store, &mut index)?;
    let mut sink = DocumentSink::new(global)?;
    emit_join_symbols(store, &index, &mut sink)?;
    let mut role_sites = None;
    // Each row is read once, in order, so it moves out of the index instead of being copied;
    // its payload is released once the row is dropped. The buffer keeps its charge until the
    // loop ends.
    let occurrences = std::mem::take(&mut index.occurrences);
    for row in occurrences {
        let mut operation = store.operation()?;
        let payload = sizes::occurrence(&row)? - std::mem::size_of::<OccurrenceRow>() as u64;
        join_occurrence(
            encoding,
            store,
            &mut index,
            &mut sink,
            row,
            &mut role_sites,
            &mut operation,
        )?;
        index.retained.release(payload)?;
    }
    settle_join_references(store, &mut index, &mut sink)?;
    let related = emit_type_relationships(store, &index, &mut sink);
    // A role-site tree the relationships did not take ends with this join.
    if let Some(local) = &store.local_claims {
        local.parsed.borrow_mut().take();
    }
    related?;
    Ok(sink.into_facts())
}

#[cfg(test)]
mod reservation_tests {
    use super::*;
    fn method_fixture() -> MethodSummary {
        MethodSummary {
            target_owner_matches: true,
            method: native::Declaration {
                role: native::DeclarationRole::Production,
                syntax_id: "method-é".into(),
                document: native::DocumentKey {
                    source_set_id: "source".into(),
                    language: "java".into(),
                    path: "é.java".into(),
                },
                revision_id: "revision".into(),
                kind: "method".into(),
                name: Some("f\n".into()),
                lookup_key: Some("f".into()),
                ancestors: vec![],
                key: native::Key {
                    kind: "method".into(),
                    name: Some("f".into()),
                    signature: Some(native::Signature {
                        parameter_types: vec!["Té".into()],
                        type_parameter_count: 0,
                        variadic: false,
                    }),
                    ordinal: 0,
                },
                range: native::Range { start: 0, end: 1 },
                name_range: Some(native::Range { start: 0, end: 1 }),
                header: native::Header {
                    kind: "method".into(),
                    name: Some("f".into()),
                    modifiers: vec!["public".into()],
                    type_parameters: vec![],
                    parameters: vec![native::Parameter {
                        name: Some("p".into()),
                        type_name: Some("Té".into()),
                        variadic: false,
                    }],
                    result_type: None,
                    bases: vec![],
                },
                provenance_id: "proof".into(),
            },
        }
    }

    #[test]
    fn sql_declarations_are_one_charged_pass_admitting_each_row_before_it_is_built() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!("{GLOBAL_TEST_SCHEMA} INSERT INTO sources VALUES(0,'java','β.java'),(1,'java','é.java');")).unwrap();
        for (path, id) in [("é.java", "id-é"), ("β.java", "id-β")] {
            conn.execute(
                "INSERT INTO g_definitions VALUES(0,'pkg Té#',-1,(SELECT ord FROM sources WHERE path=?1),?2,1)",
                params![path, id],
            )
            .unwrap();
        }
        let ctx = row_context(&conn, BatchLimits::PRODUCTION);
        let global = local_global(&ctx);
        let key = ("pkg Té#".into(), None);
        let row =
            |path: &str, id: &str| ("source".len() + "java".len() + path.len() + id.len()) as u64;
        let expected = vec![
            std::mem::size_of::<Vec<(native::DocumentKey, String)>>() as u64,
            4 * std::mem::size_of::<(native::DocumentKey, String)>() as u64,
            row("é.java", "id-é"),
            row("β.java", "id-β"),
        ];
        let mut admitted = Vec::new();
        let before = ctx.budget.borrow().used();
        let rows = global
            .declarations(&key, &mut |bytes| {
                admitted.push(bytes);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            admitted, expected,
            "UTF-8 byte lengths, admitted in row order"
        );
        assert_eq!(
            ctx.budget.borrow().used() - before,
            3,
            "one query plus one per row"
        );
        assert_eq!(
            rows.iter()
                .map(|(doc, id)| (doc.path.as_str(), id.as_str()))
                .collect::<Vec<_>>(),
            [("é.java", "id-é"), ("β.java", "id-β")]
        );
        // A denial at the second row's admission builds nothing further.
        let mut calls = 0;
        let error = global
            .declarations(&key, &mut |_| {
                calls += 1;
                if calls == 4 {
                    Err(ScipCaptureError::LimitExceeded("unit expansion".into()).into())
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<ScipCaptureError>(),Some(ScipCaptureError::LimitExceeded(name)) if name=="unit expansion")
        );
    }

    #[test]
    fn sql_methods_are_one_charged_pass_and_scratch_denial_precedes_decode() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "{GLOBAL_TEST_SCHEMA} INSERT INTO sources VALUES(0,'java','é.java');"
        ))
        .unwrap();
        let first = method_fixture();
        let mut second = method_fixture();
        second.method.syntax_id = "method-β".into();
        for (ordinal, summary) in [(0, &first), (1, &second)] {
            let body = serde_json::to_string(summary).unwrap();
            conn.execute(
                "INSERT INTO g_methods VALUES(?1,0,'pkg Té#',-1,0,?2,?3,?4)",
                params![
                    ordinal,
                    body,
                    body.len() as i64,
                    sizes::decoded_method(summary).unwrap() as i64
                ],
            )
            .unwrap();
        }
        let ctx = row_context(&conn, BatchLimits::PRODUCTION);
        let global = local_global(&ctx);
        let document = first.method.document.clone();
        let root = std::mem::size_of::<MethodSummary>() as u64;
        let expected = vec![
            std::mem::size_of::<Vec<MethodSummary>>() as u64,
            4 * root,
            sizes::decoded_method(&first).unwrap() - root,
            sizes::decoded_method(&second).unwrap() - root,
        ];
        for target in [true, false] {
            let mut admitted = Vec::new();
            let before = ctx.budget.borrow().used();
            let rows = global
                .methods(&document, "pkg Té#", target, &mut |bytes| {
                    admitted.push(bytes);
                    Ok(())
                })
                .unwrap();
            assert_eq!(admitted, expected);
            assert_eq!(ctx.budget.borrow().used() - before, 3);
            assert_eq!(rows[0].method, first.method);
            assert_eq!(rows[1].method, second.method);
            assert!(rows.iter().all(|row| row.target_owner_matches));
        }
        conn.execute("DELETE FROM g_methods", []).unwrap();
        conn.execute(
            "INSERT INTO g_methods VALUES(0,0,'pkg Té#',-1,0,'{',1,?1)",
            [sizes::decoded_method(&first).unwrap() as i64],
        )
        .unwrap();
        let scratch = 1 + 8;
        let blocker = ctx
            .live
            .reserve(ctx.limits.unit_expansion_bytes - scratch + 1)
            .unwrap();
        let error = global
            .target_methods(&document, "pkg Té#", &mut |_| Ok(()))
            .err()
            .expect("scratch denial");
        assert!(
            matches!(error.downcast_ref::<ScipCaptureError>(),Some(ScipCaptureError::LimitExceeded(name)) if name=="unit expansion")
        );
        drop(blocker);
        let error = global
            .target_methods(&document, "pkg Té#", &mut |_| Ok(()))
            .err()
            .expect("corrupt body");
        assert!(error.downcast_ref::<serde_json::Error>().is_some());
        drop(ctx.live.reserve(ctx.limits.unit_expansion_bytes).unwrap());
    }

    #[test]
    fn local_getter_owners_release_between_calls_and_overlap_when_live() {
        let conn = Connection::open_in_memory().unwrap();
        let ctx = row_context(&conn, BatchLimits::PRODUCTION);
        let global = local_global(&ctx);
        let document = native::DocumentKey {
            source_set_id: "source".into(),
            language: "java".into(),
            path: "A.java".into(),
        };
        let mut index = DocumentIndex::new(&global, &document).unwrap();
        index.put_type_use(&document, 1, 2, "pkg A#").unwrap();
        let occupied = std::mem::size_of_val(&index) as u64
            + sizes::document(&document).unwrap()
            + sizes::map_entries::<(usize, usize), BTreeSet<String>>(1).unwrap()
            + sizes::map_entries::<String, ()>(1).unwrap()
            + "pkg A#".len() as u64;
        let bytes = index.type_uses_clone_bytes(&document, 1, 2).unwrap();
        let blocker = ctx
            .live
            .reserve(ctx.limits.unit_expansion_bytes - occupied - bytes + 1)
            .unwrap();
        let error = ctx
            .live
            .reserve(bytes)
            .err()
            .expect("one-byte-short lookup admission");
        assert!(
            matches!(error.downcast_ref::<ScipCaptureError>(),Some(ScipCaptureError::LimitExceeded(name)) if name=="unit expansion")
        );
        drop(blocker);
        for _ in 0..2 {
            let operation = ctx.live.reserve(bytes).unwrap();
            let symbols = index.type_uses(&document, 1, 2).unwrap();
            assert_eq!(symbols, BTreeSet::from(["pkg A#".into()]));
            let probe = ctx
                .live
                .reserve(ctx.limits.unit_expansion_bytes - occupied - bytes)
                .unwrap();
            assert!(ctx.live.reserve(1).is_err());
            drop(probe);
            drop(symbols);
            drop(operation);
        }
        let first = ctx.live.reserve(bytes).unwrap();
        let first_rows = index.type_uses(&document, 1, 2).unwrap();
        let second = ctx.live.reserve(bytes).unwrap();
        let second_rows = index.type_uses(&document, 1, 2).unwrap();
        let probe = ctx
            .live
            .reserve(ctx.limits.unit_expansion_bytes - occupied - 2 * bytes)
            .unwrap();
        assert!(ctx.live.reserve(1).is_err());
        drop(probe);
        drop(first_rows);
        drop(second_rows);
        drop(first);
        drop(second);
        drop(index);
        drop(ctx.live.reserve(ctx.limits.unit_expansion_bytes).unwrap());
    }

    #[test]
    fn denied_reference_append_leaves_no_reference_slot_or_key_charge() {
        let conn = Connection::open_in_memory().unwrap();
        let ctx = row_context(&conn, BatchLimits::PRODUCTION);
        let global = local_global(&ctx);
        let mut sink = DocumentSink::new(&global).unwrap();
        let row = semantic::Reference {
            id: "ref".into(),
            owner_syntax_id: "owner".into(),
            ordinal: 0,
            document: native::DocumentKey {
                source_set_id: "source".into(),
                language: "java".into(),
                path: "A.java".into(),
            },
            revision_id: "revision".into(),
            range: native::Range { start: 1, end: 2 },
            spelling: "A".into(),
            lookup_key: "A".into(),
            site: semantic::ReferenceSite::Use,
            roles: vec![],
            resolution: semantic::Resolution::Unresolved,
            declared_target: None,
            candidates: vec![],
            provenance_id: "proof".into(),
        };
        let payload =
            sizes::reference(&row).unwrap() - std::mem::size_of::<semantic::Reference>() as u64;
        let key = row.id.len() as u64 + sizes::map_entries::<String, usize>(1).unwrap();
        let room = key + payload + std::mem::size_of::<semantic::Reference>() as u64;
        let base = std::mem::size_of_val(&sink) as u64;
        let blocker = ctx
            .live
            .reserve(ctx.limits.unit_expansion_bytes - base - room)
            .unwrap();
        let error = sink.put_reference(None, row).unwrap_err();
        assert!(
            matches!(error.downcast_ref::<ScipCaptureError>(),Some(ScipCaptureError::LimitExceeded(name)) if name=="unit expansion")
        );
        assert!(sink.facts.references.is_empty());
        assert!(sink.reference_indices.is_empty());
        let probe = ctx.live.reserve(room).unwrap();
        assert!(ctx.live.reserve(1).is_err());
        drop(probe);
        drop(blocker);
        drop(sink);
        drop(ctx.live.reserve(ctx.limits.unit_expansion_bytes).unwrap());
    }

    /// The tables the global reads touch, as the stage creates them.
    const GLOBAL_TEST_SCHEMA: &str = "CREATE TABLE sources(ord INTEGER PRIMARY KEY, language TEXT NOT NULL, path TEXT NOT NULL); \
        CREATE TABLE w_docs(source_ord INTEGER PRIMARY KEY, eligible INTEGER NOT NULL, failed INTEGER NOT NULL); \
        CREATE TABLE g_definitions(side INTEGER NOT NULL, symbol TEXT NOT NULL, local_doc INTEGER NOT NULL, owner_doc INTEGER NOT NULL, syntax_id BLOB NOT NULL, common_compatible INTEGER NOT NULL, PRIMARY KEY(side,symbol,local_doc,owner_doc,syntax_id)) WITHOUT ROWID; \
        CREATE TABLE g_methods(ord INTEGER PRIMARY KEY, side INTEGER NOT NULL, symbol TEXT NOT NULL, local_doc INTEGER NOT NULL, owner_doc INTEGER NOT NULL, body TEXT NOT NULL, body_bytes INTEGER NOT NULL, decoded_bytes INTEGER NOT NULL); \
        CREATE TABLE claims(claim_id INTEGER PRIMARY KEY, source_ord INTEGER NOT NULL, symbol TEXT, presence INTEGER NOT NULL, target_root INTEGER, target_lib INTEGER NOT NULL DEFAULT 0, target_package TEXT, target_version TEXT); \
        CREATE INDEX claims_symbol ON claims(symbol, presence, target_root) WHERE symbol IS NOT NULL;";

    #[test]
    fn eligible_view_keeps_exactly_the_rows_of_eligible_owner_documents() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "{GLOBAL_TEST_SCHEMA} INSERT INTO sources VALUES(0,'java','A.java'),(1,'java','B.java'),(2,'java','C.java'); \
             INSERT INTO w_docs VALUES(0,1,0),(1,1,1),(2,0,0); \
             INSERT INTO g_definitions VALUES(0,'pkg S#',-1,0,'a',1),(0,'pkg S#',-1,1,'b',1),(0,'pkg S#',-1,2,'c',1); \
             INSERT INTO claims(claim_id,source_ord,symbol,presence) VALUES(1,0,NULL,0),(2,1,'pkg D#',2),(3,2,'pkg D#',1),(4,0,'pkg E#',1);"
        ))
        .unwrap();
        let summary = method_fixture();
        let body = serde_json::to_string(&summary).unwrap();
        for (ord, owner) in [(0, 0), (1, 1), (2, 2)] {
            conn.execute(
                "INSERT INTO g_methods VALUES(?1,0,'pkg S#',-1,?2,?3,?4,?5)",
                params![
                    ord,
                    owner,
                    body,
                    body.len() as i64,
                    sizes::decoded_method(&summary).unwrap() as i64
                ],
            )
            .unwrap();
        }
        let ctx = row_context(&conn, BatchLimits::PRODUCTION);
        let captured = local_global(&ctx);
        let view = captured.eligible_view().unwrap();
        let key = ("pkg S#".to_owned(), None);
        let ids = |global: &CompletedGlobal<'_>| {
            global
                .declarations(&key, &mut |_| Ok(()))
                .unwrap()
                .into_iter()
                .map(|(_, id)| id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&captured), ["a", "b", "c"]);
        // B failed and C is not eligible: only A's rows remain.
        assert_eq!(ids(&view), ["a"]);
        let document = summary.method.document.clone();
        assert_eq!(
            captured
                .target_methods(&document, "pkg S#", &mut |_| Ok(()))
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            view.target_methods(&document, "pkg S#", &mut |_| Ok(()))
                .unwrap()
                .len(),
            1
        );
        // Presence is the claims' own: all of them captured, the eligible ones in the view.
        assert_eq!(captured.presence("pkg D#", None).unwrap(), (true, true));
        assert_eq!(view.presence("pkg D#", None).unwrap(), (false, false));
        assert_eq!(view.presence("pkg E#", None).unwrap(), (true, false));
        assert_eq!(captured.presence("pkg F#", None).unwrap(), (false, false));
        // A lookup is two keyed probes, however many claims name the symbol.
        for claim in 10..60 {
            conn.execute(
                "INSERT INTO claims(claim_id,source_ord,symbol,presence) VALUES(?1,0,'pkg D#',1)",
                [claim],
            )
            .unwrap();
        }
        let before = ctx.budget.borrow().used();
        assert_eq!(captured.presence("pkg D#", None).unwrap(), (true, true));
        assert_eq!(ctx.budget.borrow().used() - before, 2);
    }

    fn local_global<'a>(ctx: &'a JoinCtx<'a>) -> CompletedGlobal<'a> {
        CompletedGlobal {
            ctx,
            side: 0,
            filtered: false,
            source_set_id: "source".into(),
            _source_reservation: ctx.live.reserve(0).unwrap(),
        }
    }

    #[test]
    fn denied_local_vector_insertions_leave_maps_and_charges_unchanged() {
        let conn = Connection::open_in_memory().unwrap();
        let ctx = row_context(&conn, BatchLimits::PRODUCTION);
        let global = local_global(&ctx);
        let document = native::DocumentKey {
            source_set_id: "source".into(),
            language: "java".into(),
            path: "A.java".into(),
        };
        let mut index = DocumentIndex::new(&global, &document).unwrap();
        let base = std::mem::size_of_val(&index) as u64 + sizes::document(&document).unwrap();
        let target = semantic::Target::External {
            symbol: super::super::super::symbol_key("pkg A#", &document),
        };
        let payload = 1 + sizes::target_payload(&target).unwrap();
        let entry = 1 + sizes::map_entries::<String, Vec<(String, semantic::Target)>>(1).unwrap();
        let room = entry + payload + std::mem::size_of::<(String, semantic::Target)>() as u64;
        let blocker = ctx
            .live
            .reserve(ctx.limits.unit_expansion_bytes - base - room)
            .unwrap();
        let error = index.put_reference_claim("r", "p", target).unwrap_err();
        assert!(
            matches!(error.downcast_ref::<ScipCaptureError>(),Some(ScipCaptureError::LimitExceeded(name)) if name=="unit expansion")
        );
        assert!(index.reference_claims.is_empty());
        let probe = ctx.live.reserve(room).unwrap();
        assert!(ctx.live.reserve(1).is_err());
        drop(probe);
        drop(blocker);
        let relationship = RelationshipRow {
            proof_id: "p".into(),
            source_symbol: Some("s".into()),
            target_symbol: Some("t".into()),
        };
        let row_payload = sizes::relationship(&relationship).unwrap()
            - std::mem::size_of::<RelationshipRow>() as u64;
        index
            .put_relationship("java", "A.java", relationship.clone())
            .unwrap();
        let occupied = base
            + row_payload
            + (index.relationships.capacity() * std::mem::size_of::<RelationshipRow>()) as u64;
        let entry = sizes::sum(&[
            2,
            sizes::map_entries::<String, BTreeMap<String, Vec<usize>>>(1).unwrap(),
            sizes::map_entries::<String, Vec<usize>>(1).unwrap(),
        ])
        .unwrap();
        let room = entry + std::mem::size_of::<usize>() as u64;
        let blocker = ctx
            .live
            .reserve(ctx.limits.unit_expansion_bytes - occupied - room)
            .unwrap();
        assert!(
            index
                .put_relationship_endpoint("java", "A.java", "s", "t", &relationship)
                .is_err()
        );
        assert!(index.endpoints.is_empty());
        assert_eq!(index.relationships.len(), 1);
        let probe = ctx.live.reserve(room).unwrap();
        assert!(ctx.live.reserve(1).is_err());
        drop(probe);
        drop(blocker);
        drop(index);
        drop(ctx.live.reserve(ctx.limits.unit_expansion_bytes).unwrap());
    }

    #[test]
    fn moved_claims_keep_spare_buffer_slots_at_exhausted_budget() {
        let conn = Connection::open_in_memory().unwrap();
        let ctx = row_context(&conn, BatchLimits::PRODUCTION);
        let global = local_global(&ctx);
        let document = native::DocumentKey {
            source_set_id: "source".into(),
            language: "java".into(),
            path: "A.java".into(),
        };
        let mut index = DocumentIndex::new(&global, &document).unwrap();
        let target = semantic::Target::External {
            symbol: super::super::super::symbol_key("pkg A#", &document),
        };
        let payload = 1 + sizes::target_payload(&target).unwrap();
        index.put_reference_claim("r", "p", target).unwrap();
        let capacity = index.reference_claims["r"].capacity();
        let buffer = (capacity * std::mem::size_of::<(String, semantic::Target)>()) as u64;
        let base = std::mem::size_of_val(&index) as u64 + sizes::document(&document).unwrap();
        let entry = 1 + sizes::map_entries::<String, Vec<(String, semantic::Target)>>(1).unwrap();
        let blocker = ctx
            .live
            .reserve(ctx.limits.unit_expansion_bytes - base - entry - buffer - payload)
            .unwrap();
        assert!(ctx.live.reserve(1).is_err());
        let (claims, moved) = index.take_reference_claims("r").unwrap();
        assert_eq!(claims.capacity(), capacity);
        assert_eq!(claims[0].0, "p");
        assert!(index.reference_claims.is_empty());
        drop(index);
        drop(blocker);
        let held = std::mem::size_of::<Vec<(String, semantic::Target)>>() as u64 + buffer + payload;
        let probe = ctx
            .live
            .reserve(ctx.limits.unit_expansion_bytes - held)
            .unwrap();
        assert!(ctx.live.reserve(1).is_err());
        drop(probe);
        drop(claims);
        drop(moved);
        drop(ctx.live.reserve(ctx.limits.unit_expansion_bytes).unwrap());
    }

    #[test]
    fn vector_admission_pays_spare_slots_and_old_new_overlap() {
        let slot = std::mem::size_of::<usize>() as u64;
        let budget = LiveBudget::new(slot);
        let mut retained = budget.reserve(0).unwrap();
        let mut rows = Vec::new();
        assert!(append_reserved(&mut rows, 1usize, 0, &mut retained).is_err());
        assert!(rows.is_empty());
        assert_eq!(rows.capacity(), 0);
        drop(retained);
        drop(budget.reserve(slot).unwrap());

        let budget = LiveBudget::new(8 * slot);
        let mut retained = budget.reserve(0).unwrap();
        for value in 0usize..4 {
            append_reserved(&mut rows, value, 0, &mut retained).unwrap();
        }
        assert_eq!(rows.capacity(), 4);
        assert!(append_reserved(&mut rows, 4, 0, &mut retained).is_err());
        assert_eq!(rows, [0, 1, 2, 3]);
        assert_eq!(rows.capacity(), 4);
        drop(rows);
        drop(retained);
        drop(budget.reserve(8 * slot).unwrap());

        let budget = LiveBudget::new(12 * slot);
        let mut retained = budget.reserve(0).unwrap();
        let mut rows = Vec::new();
        for value in 0usize..5 {
            append_reserved(&mut rows, value, 0, &mut retained).unwrap();
        }
        assert_eq!(rows.capacity(), 8);
        let spare = budget.reserve(4 * slot).unwrap();
        assert!(budget.reserve(1).is_err());
        drop(spare);
        drop(rows);
        drop(retained);
        drop(budget.reserve(12 * slot).unwrap());
    }

    fn row_context(conn: &Connection, limits: BatchLimits) -> JoinCtx<'_> {
        JoinCtx {
            conn,
            budget: RefCell::new(LookupBudget::new(&limits)),
            live: LiveBudget::new(limits.unit_expansion_bytes),
            limits,
            counters: RefCell::new(BatchCounters::default()),
            cache: RefCell::new(DocCache::default()),
            rows: Cell::new(0),
            ledger: RefCell::new(BTreeMap::new()),
            passes: RefCell::new(Vec::new()),
            stage_base: stage_bytes(conn).unwrap(),
            mutations: DestinationMutations::default(),
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    #[test]
    fn row_attempts_are_filed_by_owner_and_never_refunded() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE output(id INTEGER PRIMARY KEY);")
            .unwrap();
        let mut limits = BatchLimits::PRODUCTION;
        limits.staged_rows = 3;
        let ctx = row_context(&conn, limits);
        for _ in 0..2 {
            ctx.row("output").unwrap();
            conn.execute("INSERT OR IGNORE INTO output VALUES(1)", [])
                .unwrap();
        }
        ctx.row("other").unwrap();
        let error = ctx.row("output").unwrap_err();
        assert!(
            matches!(error.downcast_ref::<ScipCaptureError>(),Some(ScipCaptureError::LimitExceeded(name)) if name=="staged rows")
        );
        conn.execute("DELETE FROM output WHERE id=1", []).unwrap();
        assert_eq!(
            ctx.rows.get(),
            4,
            "ignored, failed and deleted attempts are not refunded"
        );
        assert_eq!(
            *ctx.ledger.borrow(),
            BTreeMap::from([("other", 1), ("output", 3)]),
            "every attempt has exactly one owner"
        );
    }

    #[test]
    fn crossing_checks_physical_growth_and_seed_is_native_only() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE output(body TEXT); CREATE TABLE nat_docs(id); CREATE TABLE nat_declarations(id); INSERT INTO nat_docs VALUES(1),(2),(3); INSERT INTO nat_declarations VALUES(1),(2); CREATE TABLE claims(id); INSERT INTO claims VALUES(1),(2);").unwrap();
        assert_eq!(retained_rows(&conn).unwrap(), 3);
        let mut limits = BatchLimits::PRODUCTION;
        limits.output_bytes = 0;
        let ctx = row_context(&conn, limits);
        ctx.reserve_rows("seed", OUTPUT_CHECK_ROWS - 1).unwrap();
        conn.execute("INSERT INTO output VALUES(zeroblob(8000))", [])
            .unwrap();
        let error = ctx.row("output").unwrap_err();
        assert!(
            matches!(error.downcast_ref::<ScipCaptureError>(),Some(ScipCaptureError::LimitExceeded(name)) if name=="output bytes")
        );
        assert_eq!(ctx.rows.get(), OUTPUT_CHECK_ROWS);
        let mut limits = BatchLimits::PRODUCTION;
        limits.staged_rows = 2;
        let ctx = row_context(&conn, limits);
        assert!(
            ctx.reserve_rows("seed", retained_rows(&conn).unwrap())
                .is_err()
        );
        ctx.rows.set(u64::MAX);
        assert!(ctx.reserve_rows("seed", 1).is_err());
        assert_eq!(ctx.rows.get(), u64::MAX);
    }

    #[test]
    fn role_span_admission_precedes_insert_and_duplicate_needs_no_charge() {
        let bytes = sizes::map_entries::<(usize, usize), ()>(1).unwrap();
        let denied = LiveBudget::new(bytes - 1);
        let mut operation = denied.reserve(0).unwrap();
        let mut spans = BTreeSet::new();
        let error = super::super::super::reserve_role_span(&mut spans, (1, 2), &mut |bytes| {
            operation.grow(bytes()?)
        })
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<ScipCaptureError>(),Some(ScipCaptureError::LimitExceeded(name)) if name=="unit expansion")
        );
        assert!(spans.is_empty());
        drop(operation);
        drop(denied.reserve(bytes - 1).unwrap());

        let budget = LiveBudget::new(bytes);
        let mut operation = budget.reserve(0).unwrap();
        super::super::super::reserve_role_span(&mut spans, (1, 2), &mut |bytes| {
            operation.grow(bytes()?)
        })
        .unwrap();
        super::super::super::reserve_role_span(&mut spans, (1, 2), &mut |_| {
            panic!("duplicate admission")
        })
        .unwrap();
        assert_eq!(spans.len(), 1);
        assert!(budget.reserve(1).is_err());
        drop(spans);
        drop(operation);
        drop(budget.reserve(bytes).unwrap());
    }

    #[test]
    fn role_parser_owners_release_on_success_and_partial_denial() {
        let source = b"import p.T; class A { T x; T y; }";
        let root = std::mem::size_of::<RoleSites>() as u64;
        let entry = sizes::map_entries::<(usize, usize), ()>(1).unwrap();
        let budget = LiveBudget::new(BatchLimits::PRODUCTION.unit_expansion_bytes);
        let mut operation = budget.reserve(0).unwrap();
        let sites =
            super::super::super::source_role_sites_reserved("java", source, None, &mut |bytes| {
                operation.grow(bytes()?)
            })
            .unwrap();
        let expected = super::super::super::source_role_sites("java", source, None).unwrap();
        assert_eq!(sites.types, expected.types);
        assert_eq!(sites.imports, expected.imports);
        assert!(!sites.types.is_empty());
        assert!(!sites.imports.is_empty());
        let charged = root + (sites.types.len() + sites.imports.len()) as u64 * entry;
        let remaining = budget
            .reserve(BatchLimits::PRODUCTION.unit_expansion_bytes - charged)
            .unwrap();
        assert!(budget.reserve(1).is_err());
        drop(remaining);
        drop(sites);
        drop(operation);
        drop(
            budget
                .reserve(BatchLimits::PRODUCTION.unit_expansion_bytes)
                .unwrap(),
        );

        let limit = root + entry - 1;
        let denied = LiveBudget::new(limit);
        let error = (|| -> Result<RoleSites> {
            let mut operation = denied.reserve(0)?;
            super::super::super::source_role_sites_reserved("java", source, None, &mut |bytes| {
                operation.grow(bytes()?)
            })
        })()
        .err()
        .expect("role entry should be denied");
        assert!(
            matches!(error.downcast_ref::<ScipCaptureError>(),Some(ScipCaptureError::LimitExceeded(name)) if name=="unit expansion")
        );
        drop(denied.reserve(limit).unwrap());
    }

    #[test]
    fn document_read_cache_degrades_to_queries_when_the_live_budget_is_full() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!("{GLOBAL_TEST_SCHEMA} INSERT INTO sources VALUES(0,'java','A.java'); INSERT INTO g_definitions VALUES(0,'pkg A#',-1,0,'id',1);")).unwrap();
        let ctx = row_context(&conn, BatchLimits::PRODUCTION);
        let global = local_global(&ctx);
        let document = native::DocumentKey {
            source_set_id: "source".into(),
            language: "java".into(),
            path: "A.java".into(),
        };
        let index = DocumentIndex::new(&global, &document).unwrap();
        let key = ("pkg A#".to_owned(), None);
        let used = |ctx: &JoinCtx<'_>| ctx.budget.borrow().used();
        let full = ctx
            .live
            .reserve(ctx.limits.unit_expansion_bytes - ctx.live.high_water());
        let full = full.unwrap();
        for _ in 0..2 {
            let before = used(&ctx);
            let rows = index.declarations(&key, &mut |_| Ok(())).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(used(&ctx) - before, 2, "uncached: every call queries");
        }
        drop(full);
        index.declarations(&key, &mut |_| Ok(())).unwrap();
        let before = used(&ctx);
        index.declarations(&key, &mut |_| Ok(())).unwrap();
        assert_eq!(used(&ctx), before, "cached once the budget has room");
    }

    #[test]
    fn claim_whose_source_is_not_a_captured_document_is_detected() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sources(ord INTEGER PRIMARY KEY, language TEXT, path TEXT);
             CREATE TABLE claims(claim_id INTEGER PRIMARY KEY, source_ord INTEGER);
             CREATE TABLE nat_docs(rev INTEGER, ord INTEGER, language TEXT, path TEXT);
             INSERT INTO sources VALUES(0,'java','A.java'),(1,'java','B.java');
             INSERT INTO claims VALUES(0,0);
             INSERT INTO nat_docs VALUES(0,0,'java','A.java'),(1,0,'java','B.java');",
        )
        .unwrap();
        // A source without claims, and a revision-1 document, never satisfy the guard.
        assert!(!exists(&conn, UNLISTED_CLAIM_DOCUMENT_SQL, &[]).unwrap());
        conn.execute("INSERT INTO claims VALUES(1,1)", []).unwrap();
        assert!(exists(&conn, UNLISTED_CLAIM_DOCUMENT_SQL, &[]).unwrap());
    }

    #[test]
    fn json_counter_reports_typed_overflow_without_wrapping() {
        use std::io::Write;
        let mut count = CountJson {
            bytes: u64::MAX,
            overflow: false,
        };
        assert!(count.write(b"x").is_err());
        assert!(count.overflow);
        assert_eq!(count.bytes, u64::MAX);
        let value = serde_json::json!({"name":"é\n"});
        assert_eq!(
            CountJson::measure(&value).unwrap(),
            serde_json::to_vec(&value).unwrap().len() as u64
        );
    }

    #[test]
    fn repeated_local_updates_only_charge_their_live_owners() {
        let conn = Connection::open_in_memory().unwrap();
        let limits = BatchLimits::PRODUCTION;
        let ctx = JoinCtx {
            conn: &conn,
            limits: limits.clone(),
            budget: RefCell::new(LookupBudget::new(&limits)),
            live: LiveBudget::new(limits.unit_expansion_bytes),
            counters: RefCell::new(BatchCounters::default()),
            cache: RefCell::new(DocCache::default()),
            rows: Cell::new(0),
            ledger: RefCell::new(BTreeMap::new()),
            passes: RefCell::new(Vec::new()),
            stage_base: 0,
            mutations: DestinationMutations::default(),
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let global = CompletedGlobal {
            ctx: &ctx,
            side: 0,
            filtered: false,
            source_set_id: "source".into(),
            _source_reservation: ctx.live.reserve(0).unwrap(),
        };
        let document = native::DocumentKey {
            source_set_id: "source".into(),
            language: "java".into(),
            path: "A.java".into(),
        };
        let mut index = DocumentIndex::new(&global, &document).unwrap();
        index.put_type_use(&document, 1, 2, "pkg A#").unwrap();
        let type_high = ctx.live.high_water();
        for _ in 0..20 {
            index.put_type_use(&document, 1, 2, "pkg A#").unwrap();
        }
        assert_eq!(ctx.live.high_water(), type_high);
        let mut sink = DocumentSink::new(&global).unwrap();
        sink.diagnostic(&document, "unmeasurable_override_endpoint")
            .unwrap();
        let diagnostic_high = ctx.live.high_water();
        for _ in 0..20 {
            sink.diagnostic(&document, "unmeasurable_override_endpoint")
                .unwrap();
        }
        assert_eq!(ctx.live.high_water(), diagnostic_high);
        index.put_call_reference("proof", "reference").unwrap();
        index.put_call_reference("proof", "reference").unwrap();
        let replacement_high = ctx.live.high_water();
        for _ in 0..20 {
            index.put_call_reference("proof", "reference").unwrap();
        }
        assert_eq!(ctx.live.high_water(), replacement_high);
        for _ in 0..2 {
            let _lookup = ctx
                .live
                .reserve(index.type_uses_clone_bytes(&document, 1, 2).unwrap())
                .unwrap();
            assert_eq!(index.type_uses(&document, 1, 2).unwrap().len(), 1);
        }
        let scratch_before = ctx.live.high_water();
        let target = semantic::Target::External {
            symbol: super::super::super::symbol_key("pkg A#", &document),
        };
        index
            .put_reference_claim("reference", "proof", target)
            .unwrap();
        let after_insert = ctx.live.high_water();
        let (claims, moved) = index.take_reference_claims("reference").unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(ctx.live.high_water(), after_insert);
        drop(claims);
        drop(moved);
        // Only existing type/call indexes and the diagnostic remain; no moved owner.
        drop(index);
        drop(sink);
        drop(ctx.live.reserve(limits.unit_expansion_bytes).unwrap());
        assert!(after_insert >= scratch_before);
    }
}
