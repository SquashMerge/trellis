//! The staged join backend: claims, native facts, compact global indexes and output rows live
//! in `stage.db`, and the native facts of one document at a time are served from a
//! byte-bounded LRU of document groups. The binding, ambiguity and rejoin rules are the ones
//! the in-memory backend runs; this module streams them one document at a time:
//!
//! 1. captured global discovery (`g_*` side 0: exact definitions and methods; symbol presence
//!    is the admitted claims' presence index);
//! 2. per captured document, its captured join and whole-document eligibility (`w_docs`);
//! 3. eligible destination global discovery (`g_*` side 1). A shared pin writes nothing here:
//!    its destination context is the eligible view of side 0;
//! 4. per destination document, both joins recomputed (once, when they must be equal) and
//!    settled into validated `out_*` rows and its coverage; then the SQL closure.
//!
//! Each pass runs in one stage transaction. No per-occurrence or per-fact corpus is persisted
//! between documents.
use super::{
    CoordinateMap, DestinationMutations, MeasuredFacts, Position, RoleSites, ScopedKey,
    TargetCallable, compose_join, document_coordinates,
    join_store::{
        BasisFor, Bytes, DocumentJoinState, GlobalJoinReads, JoinFacts, JoinSink, JoinStore,
        MethodSummary, OccurrenceRow, ProofIdentities, ProofRef, RelationshipRow,
    },
    mapped_range, occurrence_endpoints, range_endpoints, rejoin_provenance_id, source_role_sites,
};
use crate::{
    model::{CancelFlag, IndexPin},
    native_evidence as native,
    scip_raw::{CapturedClaim, ScipCaptureError},
    semantic_batch::{
        AdmittedBatch, BatchCounters, BatchLimits, JoinedBatch, LookupBudget,
        descriptor::{Descriptor, SourceManifestHasher, basis_artifact_ordinal},
        staging::{JOIN_TABLES, claim_from_stage},
    },
    semantic_evidence::{self as semantic, EmissionHeader, EmissionValidator, RawClaimDocument},
    store::{NativeHeader, NativeReader, Store},
};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet, VecDeque},
    rc::{Rc, Weak},
    sync::atomic::Ordering,
};

mod claim_size;
pub(crate) mod codec;
pub(super) mod group_size;
pub(super) mod heap;
mod live;
pub(crate) mod publication;
mod settle;
pub(super) mod sizes;
mod streaming;
pub(super) mod target_size;
use live::{LiveBudget, Reservation};

const DOCUMENT_SQL: &str =
    "SELECT ord, language, path FROM nat_docs WHERE rev=?1 AND ord>?2 ORDER BY ord LIMIT 128";
const REVERSE_DOCUMENT_SQL: &str =
    "SELECT ord, language, path FROM nat_docs WHERE rev=?1 AND ord<?2 ORDER BY ord DESC LIMIT 128";

/// The ordered cursor queries of the join, for plan tests.
#[cfg(feature = "test-support")]
pub(crate) fn cursor_sql() -> Vec<String> {
    let mut queries = vec![
        document_claim_sql(false),
        document_claim_sql(true),
        DOCUMENT_SQL.to_owned(),
        REVERSE_DOCUMENT_SQL.to_owned(),
    ];
    queries.extend(streaming::cursor_sql());
    queries
}

/// The per-call target-callable probe (#470), for plan tests.
#[cfg(feature = "test-support")]
pub(crate) fn target_callable_sql() -> &'static str {
    streaming::target_callable_sql()
}

fn document_claim_sql(filtered: bool) -> String {
    format!(
        "{} WHERE s.language=?1 AND s.path=?2 ORDER BY c.canonical_bytes,c.claim_id",
        claim_select(filtered)
    )
}

fn to_json<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

fn canonical_target(target: &semantic::Target) -> Result<Vec<u8>> {
    Ok(crate::native_ids::canonical(&serde_json::to_value(target)?))
}

/// Everything one staged join shares: the stage connection, the limits and their meters, the
/// document cache and the destination edits.
pub(super) struct JoinCtx<'a> {
    conn: &'a Connection,
    limits: BatchLimits,
    budget: RefCell<LookupBudget>,
    live: Rc<LiveBudget>,
    counters: RefCell<BatchCounters>,
    cache: RefCell<DocCache>,
    rows: Cell<u64>,
    /// `rows` by owning table: every charged attempt is filed under exactly one owner.
    ledger: RefCell<BTreeMap<&'static str, u64>>,
    /// Cumulative (lookup steps, row attempts) at the end of each pass, for the diagnostic.
    passes: RefCell<Vec<(&'static str, u64, u64)>>,
    /// The stage file size when the join started, the base of the output-bytes meter.
    stage_base: u64,
    mutations: DestinationMutations,
    cancel: CancelFlag,
}

impl JoinCtx<'_> {
    /// Cached groups also retain their unit reservation. On pressure, discard cache ownership;
    /// live callers keep the group and its charge until their last Rc drops.
    fn reserve_native(&self, bytes: u64) -> Result<Reservation> {
        match self.live.reserve(bytes) {
            Ok(reservation) => Ok(reservation),
            Err(_) => {
                let mut cache = self.cache.borrow_mut();
                cache.active = None;
                cache.lru.clear();
                cache.bytes = 0;
                cache.evicted.retain(|held| held.strong_count() > 0);
                self.live.reserve(bytes)
            }
        }
    }

    /// Accounts lookup work before it is done.
    fn charge(&self, units: usize) -> Result<()> {
        self.budget.borrow_mut().charge(units as u64)?;
        Ok(())
    }

    fn check_cancel(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(ScipCaptureError::Cancelled.into());
        }
        Ok(())
    }

    /// Accounts real insertion attempts, including ignored duplicates and replacements, under
    /// their owner. The return value reports crossing a physical-output check boundary.
    fn reserve_rows(&self, owner: &'static str, count: u64) -> Result<bool> {
        let previous = self.rows.get();
        let rows = previous
            .checked_add(count)
            .ok_or_else(|| ScipCaptureError::LimitExceeded("staged rows".into()))?;
        self.rows.set(rows);
        let mut ledger = self.ledger.borrow_mut();
        let entry = ledger.entry(owner).or_default();
        *entry = entry.saturating_add(count);
        if rows > self.limits.staged_rows {
            return Err(ScipCaptureError::LimitExceeded("staged rows".into()).into());
        }
        Ok(previous / OUTPUT_CHECK_ROWS != rows / OUTPUT_CHECK_ROWS)
    }

    /// Records the cumulative lookup steps and row attempts at the end of a pass.
    fn pass(&self, name: &'static str) {
        self.passes
            .borrow_mut()
            .push((name, self.budget.borrow().used(), self.rows.get()));
    }

    /// Charges one insertion attempt into `table` before it runs.
    fn row(&self, table: &'static str) -> Result<()> {
        if self.reserve_rows(table, 1)? {
            self.check_output()?;
        }
        Ok(())
    }

    /// Meters the stage file's growth since the join started (page count times page size, so
    /// indexes and work tables count), against the encoded-output limit.
    fn check_output(&self) -> Result<()> {
        if stage_bytes(self.conn)?.saturating_sub(self.stage_base) > self.limits.output_bytes {
            return Err(ScipCaptureError::LimitExceeded("output bytes".into()).into());
        }
        Ok(())
    }
}

/// Contract `Symbol.key`: a conflicting duplicate key in one producer artifact invalidates it.
const DUPLICATE_SYMBOL_KEY: &str = "duplicate semantic symbol key";

/// Rows between output-bytes checks.
const OUTPUT_CHECK_ROWS: u64 = 10_000;

fn stage_bytes(conn: &Connection) -> Result<u64> {
    let pages: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    let size: i64 = conn.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    Ok(u64::try_from(pages)?.saturating_mul(u64::try_from(size)?))
}

type GroupKey = (u8, i64);

/// Document groups: one active, the rest in an LRU bounded by `document_cache_bytes`. The
/// LRU bound is on the live reservations its groups keep (`DocGroup::bytes`), so the cache
/// never holds more of the unit-expansion budget than its own bound, however many documents
/// pass through it. A group evicted while a caller still holds it stays pinned until the
/// caller drops it.
#[derive(Default)]
struct DocCache {
    budget: u64,
    active: Option<(GroupKey, Rc<DocGroup>)>,
    lru: VecDeque<(GroupKey, Rc<DocGroup>)>,
    bytes: u64,
    evicted: Vec<Weak<DocGroup>>,
}

impl DocCache {
    fn find(&mut self, key: GroupKey, counters: &mut BatchCounters) -> Option<Rc<DocGroup>> {
        if let Some((active, group)) = &self.active
            && *active == key
        {
            return Some(group.clone());
        }
        let position = self.lru.iter().position(|(held, _)| *held == key)?;
        let (_, group) = self.lru.remove(position).expect("position just found");
        self.bytes -= group.bytes;
        self.activate(key, group.clone(), counters);
        Some(group)
    }

    fn activate(&mut self, key: GroupKey, group: Rc<DocGroup>, counters: &mut BatchCounters) {
        if let Some(previous) = self.active.take() {
            self.bytes += previous.1.bytes;
            self.lru.push_back(previous);
            while self.bytes > self.budget {
                let Some((_, evicted)) = self.lru.pop_front() else {
                    break;
                };
                self.bytes -= evicted.bytes;
                if Rc::strong_count(&evicted) > 1 {
                    self.evicted.push(Rc::downgrade(&evicted));
                }
            }
        }
        self.active = Some((key, group));
        self.evicted.retain(|held| held.strong_count() > 0);
        counters.active_document_groups_high_water = counters
            .active_document_groups_high_water
            .max(1 + self.evicted.len() as u64);
        counters.document_cache_high_water_bytes =
            counters.document_cache_high_water_bytes.max(self.bytes);
        counters.document_cache_high_water_entries = counters
            .document_cache_high_water_entries
            .max(self.lru.len() as u64);
    }
}

/// The measured native facts of one document, with the join's per-record indexes.
struct DocGroup {
    document: native::Document,
    text: Rc<str>,
    native: native::Artifact,
    declaration_ids: BTreeMap<String, Vec<usize>>,
    /// Declarations by canonical key bytes, built on first use (only Java method owners ask).
    /// Its bytes are part of the group's build reservation either way.
    declaration_keys: std::cell::OnceCell<BTreeMap<Vec<u8>, Vec<usize>>>,
    declaration_names: BTreeSet<(usize, usize, String)>,
    call_ids: BTreeMap<String, Vec<usize>>,
    reference_ids: BTreeMap<String, Vec<usize>>,
    candidates: BTreeMap<(usize, usize, semantic::JoinKind), BTreeSet<String>>,
    support: BTreeMap<semantic::JoinKind, native::MeasurementSupport>,
    /// The live reservation the group holds, which is what keeping it cached costs: the
    /// document cache's budget bounds the sum of its cached groups' live charges.
    bytes: u64,
    /// The load and build peak while the group is built, then its retained bytes.
    native_reservation: Reservation,
}

impl DocGroup {
    fn build(
        header: &NativeHeader,
        document: native::Document,
        rows: crate::store::NativeDocumentRows,
        mut native_reservation: Reservation,
        live: &Rc<LiveBudget>,
    ) -> Result<Self> {
        native_reservation.grow(group_size::group_build(header, &document, &rows, live)?)?;
        let key = document.key.clone();
        let sources = BTreeMap::from([(key.clone(), rows.source)]);
        let mini = native::Artifact {
            producer: header.producer.clone(),
            source_set: header.source_set.clone(),
            revision: native::Revision {
                documents: vec![document.clone()],
                ..header.revision.clone()
            },
            coverage: vec![rows.coverage],
            provenance: vec![],
            declarations: rows.declarations,
            calls: rows.calls,
            control_regions: vec![],
            references: rows.references,
            capability_reports: vec![rows.capability_report],
            search_cards: vec![],
        };
        let (candidates, support) = MeasuredFacts::anchor_parts(&mini, &sources, &key)?;
        let bytes = sources.into_values().next().expect("one source");
        let text: Rc<str> = Rc::from(String::from_utf8(bytes).context("non-UTF8 native source")?);
        let (declarations, calls, references) = (&mini.declarations, &mini.calls, &mini.references);
        let mut declaration_ids: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut declaration_names = BTreeSet::new();
        for (index, d) in declarations.iter().enumerate() {
            declaration_ids
                .entry(d.syntax_id.clone())
                .or_default()
                .push(index);
            if let (Some(range), Some(name)) = (&d.name_range, &d.name) {
                declaration_names.insert((range.start, range.end, name.clone()));
            }
        }
        let mut call_ids: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (index, c) in calls.iter().enumerate() {
            call_ids.entry(c.id.clone()).or_default().push(index);
        }
        let mut reference_ids: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (index, r) in references.iter().enumerate() {
            reference_ids.entry(r.id.clone()).or_default().push(index);
        }
        // What the group does not keep drops before its charge shrinks to what it keeps.
        drop((
            key,
            rows.provenance,
            rows.control_regions,
            rows.search_cards,
        ));
        let mut group = Self {
            document,
            text,
            native: mini,
            declaration_ids,
            declaration_keys: std::cell::OnceCell::new(),
            declaration_names,
            call_ids,
            reference_ids,
            candidates,
            support,
            bytes: 0,
            native_reservation,
        };
        // The load and build peak was admitted above; what outlives it is the built group.
        // Its charge becomes the group's retained bytes, measured from the real capacities.
        let retained = group_size::retained_group(&group)?;
        let held = group.native_reservation.bytes();
        if retained < held {
            group.native_reservation.release(held - retained)?;
        } else {
            group.native_reservation.grow(retained - held)?;
        }
        // A cached group keeps its whole live reservation, so the cache counts exactly that.
        group.bytes = group.native_reservation.bytes();
        Ok(group)
    }

    fn declaration_keys(&self) -> Result<&BTreeMap<Vec<u8>, Vec<usize>>> {
        if let Some(keys) = self.declaration_keys.get() {
            return Ok(keys);
        }
        let mut keys: BTreeMap<Vec<u8>, Vec<usize>> = BTreeMap::new();
        for (index, d) in self.native.declarations.iter().enumerate() {
            keys.entry(crate::native_ids::canonical(&serde_json::to_value(&d.key)?))
                .or_default()
                .push(index);
        }
        Ok(self.declaration_keys.get_or_init(|| keys))
    }

    fn pick<'s, T: Clone>(
        rows: &'s [T],
        indexes: Option<&Vec<usize>>,
        operation: &mut Reservation,
    ) -> Result<Vec<Cow<'s, T>>> {
        let count = indexes.map_or(0, Vec::len);
        let bytes = (count as u64)
            .checked_mul(std::mem::size_of::<Cow<'s, T>>() as u64)
            .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?;
        operation.grow(sizes::sum(&[
            std::mem::size_of::<Vec<Cow<'s, T>>>() as u64,
            bytes,
        ])?)?;
        let mut result = Vec::with_capacity(count);
        result.extend(
            indexes
                .into_iter()
                .flatten()
                .map(|&index| Cow::Borrowed(&rows[index])),
        );
        Ok(result)
    }
}

fn claim_select(filtered: bool) -> String {
    format!(
        "SELECT c.claim_id, c.source_ord, c.canonical_bytes, c.fact_digest, c.proof_id, a.sha256, s.language \
         FROM claims c JOIN sources s ON s.ord=c.source_ord JOIN artifacts a ON a.ordinal=c.artifact {}",
        if filtered { ELIGIBLE_JOIN } else { "" }
    )
}

const ELIGIBLE_JOIN: &str =
    "JOIN w_docs w ON w.source_ord=c.source_ord AND w.eligible=1 AND w.failed=0";

/// The staged join's claim of one `claim_select` row, decoded from its borrowed canonical
/// bytes.
fn build_claim(row: &rusqlite::Row<'_>) -> Result<CapturedClaim> {
    claim_from_stage(
        row.get_ref(2)?.as_blob()?,
        row.get(3)?,
        row.get(4)?,
        row.get(6)?,
        row.get(5)?,
    )
}

/// The documents of one native revision in stored order (or its reverse), a page at a time.
struct DocumentCursor<'a> {
    conn: &'a Connection,
    rev: u8,
    source_set_id: String,
    last: i64,
    reverse: bool,
    page: VecDeque<native::DocumentKey>,
    done: bool,
}

impl Iterator for DocumentCursor<'_> {
    type Item = Result<native::DocumentKey>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(key) = self.page.pop_front() {
                return Some(Ok(key));
            }
            if self.done {
                return None;
            }
            let fetched = (|| -> Result<Vec<(i64, String, String)>> {
                let mut statement = self.conn.prepare_cached(if self.reverse {
                    REVERSE_DOCUMENT_SQL
                } else {
                    DOCUMENT_SQL
                })?;
                Ok(statement
                    .query_map(params![self.rev, self.last], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })?
                    .collect::<rusqlite::Result<_>>()?)
            })();
            match fetched {
                Err(error) => {
                    self.done = true;
                    return Some(Err(error));
                }
                Ok(rows) => {
                    if rows.len() < 128 {
                        self.done = true;
                    }
                    for (ord, language, path) in rows {
                        self.last = ord;
                        self.page.push_back(native::DocumentKey {
                            source_set_id: self.source_set_id.clone(),
                            language,
                            path,
                        });
                    }
                }
            }
        }
    }
}

fn decode_reserved_claim(
    row: &rusqlite::Row<'_>,
    live: &Rc<LiveBudget>,
    reservation: &mut Reservation,
) -> Result<CapturedClaim> {
    let canonical = row.get_ref(2)?.as_blob()?;
    let metadata = sizes::sum(&[
        row.get_ref(3)?.as_str()?.len() as u64,
        row.get_ref(4)?.as_str()?.len() as u64,
        row.get_ref(5)?.as_str()?.len() as u64,
        row.get_ref(6)?.as_str()?.len() as u64,
    ])?;
    let size = claim_size::measure(canonical, metadata)?;
    reservation.grow(size.retained)?;
    let _scratch = live.reserve(size.scratch)?;
    build_claim(row)
}

/// Parsed claims belong to one document/pass, not to the native cache or a cursor page.
struct DocumentClaims {
    claims: Vec<CapturedClaim>,
    coordinates: RefCell<Option<BTreeMap<u8, CoordinateMap>>>,
    symbol_kinds: BTreeMap<String, Option<i64>>,
    /// Claim index and staged `claim_id`, by proof ID.
    by_proof: BTreeMap<String, (usize, i64)>,
    /// The syntax tree role sites were last read from, and the address and length of the
    /// group text it parsed, until the same document join takes it for the document's direct
    /// type bases; `streaming::join_document` drops any it left. One tree at a time, as when
    /// each step parsed its own (tree-sitter memory is outside the live budget, as before).
    parsed: RefCell<Option<((usize, usize), tree_sitter::Tree)>>,
    reservation: RefCell<Reservation>,
}

impl DocumentClaims {
    /// Claims other than symbol information: those the report tallies.
    fn non_information(&self) -> u64 {
        self.claims
            .iter()
            .filter(|claim| claim.envelope.record_kind != "symbolInformation")
            .count() as u64
    }

    fn coordinate_bytes(&self, admitted_encoding: u8) -> Result<u64> {
        let mut counts = [0usize; 3];
        for claim in &self.claims {
            if claim.envelope.record_kind != "occurrence" {
                continue;
            }
            for enclosing in [false, true] {
                let raw = &claim.envelope.raw[if enclosing { "enclosingRange" } else { "range" }];
                if enclosing && raw.is_null() {
                    continue;
                }
                let (encoding, _) =
                    range_endpoints(raw, claim.envelope.position_encoding, admitted_encoding)?;
                counts[encoding as usize - 1] = counts[encoding as usize - 1]
                    .checked_add(2)
                    .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?;
            }
        }
        counts.into_iter().try_fold(0, |total, count| {
            if count == 0 {
                return Ok(total);
            }
            // Positions Vec push growth, all unique output entries, and their overlap.
            let slots = count
                .checked_mul(2)
                .map(|n| n.max(4))
                .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?;
            sizes::sum(&[
                total,
                sizes::map_entries::<u8, Vec<Position>>(1)?,
                sizes::map_entries::<u8, CoordinateMap>(1)?,
                sizes::map_entries::<Position, usize>(count)?,
                (slots as u64)
                    .checked_mul(std::mem::size_of::<Position>() as u64)
                    .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?,
                std::mem::size_of::<std::sync::atomic::AtomicBool>() as u64
                    + 2 * std::mem::size_of::<usize>() as u64,
            ])
        })
    }
    fn load(side: &StagedSide<'_>, document: &native::DocumentKey) -> Result<Self> {
        let mut reservation = side.ctx.live.reserve(sizes::sum(&[
            std::mem::size_of::<Self>() as u64,
            2 * std::mem::size_of::<usize>() as u64,
        ])?)?;
        let sql = document_claim_sql(side.filtered);
        side.ctx.charge(1)?;
        let mut statement = side.ctx.conn.prepare_cached(&sql)?;
        let mut rows = statement.query(params![document.language, document.path])?;
        let mut claims = Vec::new();
        let mut symbol_kinds = BTreeMap::new();
        let mut by_proof = BTreeMap::new();
        while let Some(row) = rows.next()? {
            side.ctx.charge(1)?;
            let claim_id: i64 = row.get(0)?;
            let claim = decode_reserved_claim(row, &side.ctx.live, &mut reservation)?;
            if claim.envelope.record_kind == "symbolInformation"
                && let Some(symbol) = claim.envelope.raw["symbol"].as_str()
                && !symbol_kinds.contains_key(symbol)
            {
                reservation.grow(sizes::sum(&[
                    symbol.len() as u64,
                    sizes::map_entries::<String, Option<i64>>(1)?,
                ])?)?;
                symbol_kinds.insert(symbol.to_owned(), claim.envelope.raw["kind"].as_i64());
            }
            reservation.grow(sizes::sum(&[
                claim.captured_proof_id.len() as u64,
                sizes::map_entries::<String, (usize, i64)>(1)?,
            ])?)?;
            by_proof
                .entry(claim.captured_proof_id.clone())
                .or_insert((claims.len(), claim_id));
            if claims.len() == claims.capacity() {
                // New slots are covered by retained claim roots; old slots overlap the move.
                let overlap = (claims.capacity() as u64)
                    .checked_mul(std::mem::size_of::<CapturedClaim>() as u64)
                    .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?;
                let _old_buffer = side.ctx.live.reserve(overlap)?;
                claims.reserve_exact(1);
            }
            claims.push(claim);
        }
        Ok(Self {
            claims,
            coordinates: RefCell::new(None),
            symbol_kinds,
            by_proof,
            parsed: RefCell::new(None),
            reservation: RefCell::new(reservation),
        })
    }
}

/// One revision's side of the join: the retained native revision, the claims (all of them, or
/// those of eligible documents), and the fact and index tables of its side.
pub(super) struct StagedSide<'a> {
    ctx: &'a JoinCtx<'a>,
    reader: &'a NativeReader<'a>,
    header: &'a NativeHeader,
    rev: u8,
    side: u8,
    filtered: bool,
    document: Option<native::DocumentKey>,
    local_claims: Option<Rc<DocumentClaims>>,
    local_group: Option<Rc<DocGroup>>,
}

impl<'a> StagedSide<'a> {
    fn doc_row(&self, document: &native::DocumentKey) -> Result<Option<(i64, native::Document)>> {
        let found = self
            .ctx
            .conn
            .prepare_cached(
                "SELECT ord, content_hash, byte_length FROM nat_docs WHERE rev=?1 AND language=?2 AND path=?3",
            )?
            .query_row(params![self.rev, document.language, document.path], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
            })
            .optional()?;
        Ok(found.map(|(ord, content_hash, length)| {
            (
                ord,
                native::Document {
                    key: document.clone(),
                    revision_id: self.header.revision.id.clone(),
                    content_hash,
                    byte_length: length as usize,
                },
            )
        }))
    }

    /// The group of `document`, loaded on first use. `None` when the revision lacks it.
    fn mapped_claim_range(
        &self,
        claim: &CapturedClaim,
        admitted_encoding: u8,
        encoding: u8,
        endpoints: super::Endpoints,
    ) -> Result<native::Range> {
        let document = native::DocumentKey {
            source_set_id: self.header.source_set.id.clone(),
            language: claim.language.clone(),
            path: claim.envelope.relative_path.clone(),
        };
        let group = self.require_group(&document)?;
        let local = self
            .local_claims
            .as_ref()
            .context("document claim owner required")?;
        if local.coordinates.borrow().is_none() {
            local
                .reservation
                .borrow_mut()
                .grow(local.coordinate_bytes(admitted_encoding)?)?;
            let maps = document_coordinates(&local.claims, &group.text, admitted_encoding)?;
            *local.coordinates.borrow_mut() = Some(maps);
        }
        let coordinates = local.coordinates.borrow();
        let offsets = coordinates
            .as_ref()
            .and_then(|maps| maps.get(&encoding))
            .context("missing SCIP coordinate map")?;
        mapped_range(claim, endpoints, offsets)
    }

    fn borrowed_group(&self, document: &native::DocumentKey) -> Result<Option<&DocGroup>> {
        anyhow::ensure!(
            self.document.as_ref() == Some(document),
            "native document view required"
        );
        Ok(self.local_group.as_deref())
    }

    fn group(&self, document: &native::DocumentKey) -> Result<Option<Rc<DocGroup>>> {
        if self.document.is_some() {
            return Ok(self
                .borrowed_group(document)?
                .map(|_| self.local_group.as_ref().unwrap().clone()));
        }
        anyhow::ensure!(
            self.document
                .as_ref()
                .is_none_or(|current| current == document),
            "foreign document in local join"
        );
        let Some((ord, row)) = self.doc_row(document)? else {
            return Ok(None);
        };
        let key = (self.rev, ord);
        {
            let mut cache = self.ctx.cache.borrow_mut();
            let mut counters = self.ctx.counters.borrow_mut();
            if let Some(group) = cache.find(key, &mut counters) {
                return Ok(Some(group));
            }
        }
        self.ctx.charge(1)?;
        self.ctx.check_cancel()?;
        let native_bytes = self.reservation_bytes(ord, &row)?;
        let mut reservation = self.ctx.reserve_native(native_bytes)?;
        // Rc<str> construction overlaps the source Vec returned by the reader.
        reservation.grow(row.byte_length as u64)?;
        let mut rows = self.reader.join_document(&row)?;
        // The load's fetch and decode scratch is gone: keep the rows' own storage and the
        // source overlap, measured from their real capacities.
        if let Some(loaded) = group_size::retained_rows(&rows) {
            let loaded = sizes::sum(&[loaded, row.byte_length as u64])?;
            if loaded < reservation.bytes() {
                reservation.release(reservation.bytes() - loaded)?;
            }
        }
        if self.rev == 1 {
            reservation.grow(sizes::mutation(&rows, self.ctx.mutations)?)?;
            self.ctx
                .mutations
                .apply(&mut rows.declarations, &mut rows.references);
        }
        let group = Rc::new(DocGroup::build(
            self.header,
            row,
            rows,
            reservation,
            &self.ctx.live,
        )?);
        let mut cache = self.ctx.cache.borrow_mut();
        let mut counters = self.ctx.counters.borrow_mut();
        cache.activate(key, group.clone(), &mut counters);
        Ok(Some(group))
    }

    /// The native load reservation of document `ord` of this revision: computed by its first
    /// load and kept on its `nat_docs` row, so a later pass that loads it again skips the scan.
    fn reservation_bytes(&self, ord: i64, row: &native::Document) -> Result<u64> {
        let stored: Option<i64> = self
            .ctx
            .conn
            .prepare_cached("SELECT reserve FROM nat_docs WHERE rev=?1 AND ord=?2")?
            .query_row(params![self.rev, ord], |found| found.get(0))?;
        if let Some(bytes) = stored {
            return Ok(u64::try_from(bytes)?);
        }
        let bytes = self.reader.document_reservation_bytes(row)?;
        self.ctx
            .conn
            .prepare_cached("UPDATE nat_docs SET reserve=?3 WHERE rev=?1 AND ord=?2")?
            .execute(params![self.rev, ord, i64::try_from(bytes)?])?;
        Ok(bytes)
    }

    fn require_group(&self, document: &native::DocumentKey) -> Result<Rc<DocGroup>> {
        self.group(document)?.context("foreign anchor document")
    }

    /// The revision's documents in reverse stored order. A pass between two forward passes
    /// walks backwards so the document cache still holds the groups the previous pass loaded
    /// last and the next pass loads first.
    fn documents_reversed(&self) -> DocumentCursor<'a> {
        DocumentCursor {
            conn: self.ctx.conn,
            rev: self.rev,
            source_set_id: self.header.source_set.id.clone(),
            last: i64::MAX,
            reverse: true,
            page: VecDeque::new(),
            done: false,
        }
    }

    /// The decoded claim of this document view with `proof`, `kind` and `digest`, and its
    /// staged `claim_id`. Uncharged: the caller already charged the lookup that found it.
    fn local_claim(&self, proof: &str, kind: &str, digest: &str) -> Result<(&CapturedClaim, i64)> {
        let local = self
            .local_claims
            .as_ref()
            .context("document claim owner required")?;
        let (index, claim_id) = *local
            .by_proof
            .get(proof)
            .ok_or(ScipCaptureError::MissingProof)?;
        let claim = &local.claims[index];
        if claim.envelope.record_kind != kind || claim.fact_digest != digest {
            return Err(ScipCaptureError::MissingProof.into());
        }
        Ok((claim, claim_id))
    }

    fn proof_filter(&self) -> &'static str {
        if self.filtered { ELIGIBLE_JOIN } else { "" }
    }
}

impl JoinFacts for StagedSide<'_> {
    type Operation = Reservation;
    fn operation(&self) -> Result<Reservation> {
        self.ctx.live.reserve(0)
    }
    fn reserve_operation(
        &self,
        operation: &mut Reservation,
        bytes: impl FnOnce() -> Result<u64>,
    ) -> Result<()> {
        operation.grow(bytes()?)
    }
    fn join_reserved(
        &self,
        document: &native::DocumentKey,
        range: native::Range,
        kind: semantic::JoinKind,
        operation: &mut Reservation,
    ) -> Result<semantic::Join> {
        let group = self.require_group(document)?;
        let ids = group.candidates.get(&(range.start, range.end, kind));
        let count = ids.map_or(0, BTreeSet::len);
        let id_bytes = ids
            .into_iter()
            .flatten()
            .try_fold(0, |total, id| sizes::sum(&[total, id.len() as u64]))?;
        let diagnostic = if count > 1 {
            "ambiguous_measured_anchor".len() as u64
        } else if count == 1 {
            0
        } else {
            group
                .support
                .get(&kind)
                .map_or(0, |support| {
                    support
                        .diagnostic
                        .as_ref()
                        .map_or(0, |s| s.len() as u64)
                        .max(
                            support
                                .gaps
                                .iter()
                                .map(|gap| gap.diagnostic.len() as u64)
                                .max()
                                .unwrap_or(0),
                        )
                })
                .max("unmatched_measured_anchor".len() as u64)
        };
        operation.grow(sizes::sum(&[
            std::mem::size_of::<semantic::Join>() as u64,
            sizes::document(&group.document.key)?,
            self.header.revision.id.len() as u64,
            group.document.content_hash.len() as u64,
            id_bytes,
            (count as u64)
                .checked_mul(std::mem::size_of::<String>() as u64)
                .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?,
            diagnostic,
        ])?)?;
        self.join(document, range, kind)
    }
    fn revision_id(&self) -> &str {
        &self.header.revision.id
    }

    fn source_set_id(&self) -> &str {
        &self.header.source_set.id
    }

    fn join(
        &self,
        document: &native::DocumentKey,
        range: native::Range,
        kind: semantic::JoinKind,
    ) -> Result<semantic::Join> {
        let group = self.require_group(document)?;
        let candidates = group.candidates.get(&(range.start, range.end, kind));
        let mut ids = Vec::with_capacity(candidates.map_or(0, BTreeSet::len));
        ids.extend(candidates.into_iter().flatten().cloned());
        {
            let mut counters = self.ctx.counters.borrow_mut();
            counters.candidate_group_high_water =
                counters.candidate_group_high_water.max(ids.len() as u64);
        }
        compose_join(
            &self.header.revision.id,
            &group.document,
            &group.text,
            ids,
            group.support.get(&kind),
            range,
            kind,
        )
    }

    fn bytes_of(&self, document: &native::DocumentKey) -> Result<Option<Bytes<'_>>> {
        Ok(self
            .borrowed_group(document)?
            .map(|group| Bytes::Borrowed(group.text.as_bytes())))
    }

    fn documents(&self) -> Box<dyn Iterator<Item = Result<native::DocumentKey>> + '_> {
        if let Some(document) = &self.document {
            return Box::new(std::iter::once(Ok(document.clone())));
        }
        Box::new(DocumentCursor {
            conn: self.ctx.conn,
            rev: self.rev,
            source_set_id: self.header.source_set.id.clone(),
            last: -1,
            reverse: false,
            page: VecDeque::new(),
            done: false,
        })
    }

    fn document_declarations(
        &self,
        document: &native::DocumentKey,
        operation: &mut Self::Operation,
    ) -> Result<Vec<Cow<'_, native::Declaration>>> {
        let Some(group) = self.borrowed_group(document)? else {
            return Ok(vec![]);
        };
        let count = group.native.declarations.len();
        operation.grow(sizes::sum(&[
            std::mem::size_of::<Vec<Cow<'_, native::Declaration>>>() as u64,
            (count as u64)
                .checked_mul(std::mem::size_of::<Cow<'_, native::Declaration>>() as u64)
                .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?,
        ])?)?;
        let mut result = Vec::with_capacity(count);
        result.extend(group.native.declarations.iter().map(Cow::Borrowed));
        Ok(result)
    }

    fn declaration_of(
        &self,
        document: &native::DocumentKey,
        id: &str,
    ) -> Result<Option<Cow<'_, native::Declaration>>> {
        Ok(self.borrowed_group(document)?.and_then(|group| {
            group
                .declaration_ids
                .get(id)
                .and_then(|ids| ids.first())
                .map(|&index| Cow::Borrowed(&group.native.declarations[index]))
        }))
    }

    fn declarations_with_id(
        &self,
        document: &native::DocumentKey,
        id: &str,
        operation: &mut Self::Operation,
    ) -> Result<Vec<Cow<'_, native::Declaration>>> {
        Ok(match self.borrowed_group(document)? {
            Some(group) => DocGroup::pick(
                &group.native.declarations,
                group.declaration_ids.get(id),
                operation,
            )?,
            None => vec![],
        })
    }

    fn declarations_with_key(
        &self,
        document: &native::DocumentKey,
        key: &[u8],
        operation: &mut Self::Operation,
    ) -> Result<Vec<Cow<'_, native::Declaration>>> {
        Ok(match self.borrowed_group(document)? {
            Some(group) => DocGroup::pick(
                &group.native.declarations,
                group.declaration_keys()?.get(key),
                operation,
            )?,
            None => vec![],
        })
    }

    fn has_declaration_name(
        &self,
        document: &native::DocumentKey,
        start: usize,
        end: usize,
        spelling: &str,
    ) -> Result<bool> {
        Ok(match self.borrowed_group(document)? {
            Some(group) => group
                .declaration_names
                .contains(&(start, end, spelling.to_owned())),
            None => false,
        })
    }

    fn call_of(
        &self,
        document: &native::DocumentKey,
        id: &str,
    ) -> Result<Option<Cow<'_, native::Call>>> {
        Ok(self.borrowed_group(document)?.and_then(|group| {
            group
                .call_ids
                .get(id)
                .and_then(|ids| ids.first())
                .map(|&index| Cow::Borrowed(&group.native.calls[index]))
        }))
    }

    fn calls_with_id(
        &self,
        document: &native::DocumentKey,
        id: &str,
        operation: &mut Self::Operation,
    ) -> Result<Vec<Cow<'_, native::Call>>> {
        Ok(match self.borrowed_group(document)? {
            Some(group) => DocGroup::pick(&group.native.calls, group.call_ids.get(id), operation)?,
            None => vec![],
        })
    }

    fn reference_of(
        &self,
        document: &native::DocumentKey,
        id: &str,
    ) -> Result<Option<Cow<'_, native::Reference>>> {
        Ok(self.borrowed_group(document)?.and_then(|group| {
            group
                .reference_ids
                .get(id)
                .and_then(|ids| ids.first())
                .map(|&index| Cow::Borrowed(&group.native.references[index]))
        }))
    }

    fn references_with_id(
        &self,
        document: &native::DocumentKey,
        id: &str,
        operation: &mut Self::Operation,
    ) -> Result<Vec<Cow<'_, native::Reference>>> {
        Ok(match self.borrowed_group(document)? {
            Some(group) => DocGroup::pick(
                &group.native.references,
                group.reference_ids.get(id),
                operation,
            )?,
            None => vec![],
        })
    }
}

impl ProofIdentities for StagedSide<'_> {
    fn proof(&self, proof_id: &str) -> Result<Option<ProofRef>> {
        if let Some(local) = &self.local_claims {
            self.ctx.charge(1)?;
            return Ok(local.by_proof.get(proof_id).map(|&(index, _)| {
                let claim = &local.claims[index];
                ProofRef {
                    language: claim.language.clone(),
                    path: claim.envelope.relative_path.clone(),
                    record_kind: claim.envelope.record_kind.clone(),
                    fact_digest: claim.fact_digest.clone(),
                    artifact_hash: claim.artifact_hash.clone(),
                }
            }));
        }
        let sql = format!(
            "SELECT s.language, s.path, c.record_kind, c.fact_digest, a.sha256 FROM claims c \
             JOIN sources s ON s.ord=c.source_ord JOIN artifacts a ON a.ordinal=c.artifact {} WHERE c.proof_id=?1",
            self.proof_filter()
        );
        let proof = self
            .ctx
            .conn
            .prepare_cached(&sql)?
            .query_row([proof_id], |row| {
                Ok(ProofRef {
                    language: row.get(0)?,
                    path: row.get(1)?,
                    record_kind: row.get(2)?,
                    fact_digest: row.get(3)?,
                    artifact_hash: row.get(4)?,
                })
            })
            .optional()?;
        Ok(proof.filter(|proof| {
            self.document.as_ref().is_none_or(|document| {
                document.language == proof.language && document.path == proof.path
            })
        }))
    }

    fn check_proof(&self, proof_id: &str, kind: &str, digest: &str) -> Result<()> {
        if self.local_claims.is_some() {
            self.claim(proof_id, kind, digest)?;
            return Ok(());
        }
        let proof = self
            .proof(proof_id)?
            .ok_or(ScipCaptureError::MissingProof)?;
        if proof.record_kind != kind || proof.fact_digest != digest {
            return Err(ScipCaptureError::MissingProof.into());
        }
        Ok(())
    }
}

impl<'a> JoinStore for StagedSide<'a> {
    fn claims(&self) -> Box<dyn Iterator<Item = Result<Cow<'_, CapturedClaim>>> + '_> {
        match &self.local_claims {
            Some(local) => Box::new(local.claims.iter().map(|claim| Ok(Cow::Borrowed(claim)))),
            None => Box::new(std::iter::once(Err(anyhow::anyhow!(
                "document claim owner required"
            )))),
        }
    }

    fn lookup_proof(&self, proof: &str, kind: &str, digest: &str) -> Result<()> {
        self.check_proof(proof, kind, digest)
    }

    fn claim(&self, proof: &str, kind: &str, digest: &str) -> Result<Cow<'_, CapturedClaim>> {
        let local = self
            .local_claims
            .as_ref()
            .context("document claim owner required")?;
        self.ctx.charge(1)?;
        let claim = local
            .by_proof
            .get(proof)
            .map(|&(index, _)| &local.claims[index])
            .ok_or(ScipCaptureError::MissingProof)?;
        if claim.envelope.record_kind != kind || claim.fact_digest != digest {
            return Err(ScipCaptureError::MissingProof.into());
        }
        Ok(Cow::Borrowed(claim))
    }

    fn symbol_kind(&self, language: &str, path: &str, symbol: &str) -> Result<Option<i64>> {
        let document = self
            .document
            .as_ref()
            .context("document claim owner required")?;
        anyhow::ensure!(
            document.language == language && document.path == path,
            "foreign symbol-kind document"
        );
        let local = self
            .local_claims
            .as_ref()
            .context("document claim owner required")?;
        Ok(local.symbol_kinds.get(symbol).copied().flatten())
    }

    fn occurrence_range(
        &self,
        claim: &CapturedClaim,
        admitted_encoding: u8,
    ) -> Result<native::Range> {
        let (encoding, endpoints) = occurrence_endpoints(claim, admitted_encoding)?;
        self.mapped_claim_range(claim, admitted_encoding, encoding, endpoints)
    }
    fn enclosing_occurrence_range(
        &self,
        claim: &CapturedClaim,
        admitted_encoding: u8,
    ) -> Result<native::Range> {
        let (encoding, endpoints) = range_endpoints(
            &claim.envelope.raw["enclosingRange"],
            claim.envelope.position_encoding,
            admitted_encoding,
        )?;
        self.mapped_claim_range(claim, admitted_encoding, encoding, endpoints)
    }

    fn role_sites(&self, document: &native::DocumentKey) -> Result<RoleSites> {
        #[cfg(feature = "test-support")]
        super::note_parsed_role_site_document(document);
        let group = self.require_group(document)?;
        source_role_sites(document.language.as_str(), group.text.as_bytes())
    }

    fn role_sites_reserved(
        &self,
        document: &native::DocumentKey,
        operation: &mut Self::Operation,
    ) -> Result<RoleSites> {
        #[cfg(feature = "test-support")]
        super::note_parsed_role_site_document(document);
        let group = self.require_group(document)?;
        let (sites, tree) = super::source_role_sites_parsed(
            document.language.as_str(),
            group.text.as_bytes(),
            &mut |bytes| operation.grow(bytes()?),
        )?;
        if let Some(local) = &self.local_claims {
            let text = (group.text.as_ptr() as usize, group.text.len());
            *local.parsed.borrow_mut() = tree.map(|tree| (text, tree));
        }
        Ok(sites)
    }

    fn take_parsed_tree(&self, document: &native::DocumentKey) -> Option<tree_sitter::Tree> {
        let local = self.local_claims.as_ref()?;
        let (text, tree) = local.parsed.borrow_mut().take()?;
        // Only the tree of this view's own document group text.
        let group = self.local_group.as_ref()?;
        let current = (group.text.as_ptr() as usize, group.text.len());
        (self.document.as_ref() == Some(document) && text == current).then_some(tree)
    }

    fn charge(&self, units: usize) -> Result<()> {
        self.ctx.charge(units)?;
        self.ctx.check_cancel()
    }
}

fn local_columns(key: &ScopedKey) -> (&str, &str) {
    match &key.1 {
        Some(document) => (&document.language, &document.path),
        None => ("", ""),
    }
}

fn exists(conn: &Connection, sql: &str, values: &[&dyn rusqlite::ToSql]) -> Result<bool> {
    Ok(conn
        .prepare_cached(sql)?
        .query_row(values, |_| Ok(()))
        .optional()?
        .is_some())
}

fn language_of(name: &str) -> Result<semantic::Language> {
    Ok(serde_json::from_value(serde_json::json!(name))?)
}

/// The captured basis of a document, from the batch header and the document's artifact (§4.4 e).
struct StagedBasis<'a> {
    conn: &'a Connection,
    descriptor: &'a Descriptor,
    producer: &'a semantic::Producer,
    captured: &'a native::Revision,
}

impl BasisFor for StagedBasis<'_> {
    fn basis_for(&self, document: &native::DocumentKey) -> Result<Option<semantic::SemanticBasis>> {
        let language = language_of(&document.language)?;
        if !self.producer.languages.contains(&language) {
            return Ok(None);
        }
        let assigned: Option<Option<i64>> = self
            .conn
            .prepare_cached(
                "SELECT c.artifact FROM sources s JOIN coverage c ON c.source_ord=s.ord WHERE s.language=?1 AND s.path=?2",
            )?
            .query_row(params![document.language, document.path], |row| row.get(0))
            .optional()?;
        let Some(assigned) = assigned else {
            return Ok(None);
        };
        let ordinal = basis_artifact_ordinal(
            &self.descriptor.artifacts,
            &document.language,
            assigned.map(|ordinal| ordinal as u32),
        );
        let Some(artifact) = self
            .descriptor
            .artifacts
            .iter()
            .find(|artifact| artifact.ordinal == ordinal)
        else {
            return Ok(None);
        };
        Ok(Some(semantic::SemanticBasis {
            producer_id: self.producer.id.clone(),
            producer_version: self.producer.version.clone(),
            producer_hash: self.producer.executable_hash.clone(),
            artifact_hash: artifact.sha256.clone(),
            language,
            source_set_id: document.source_set_id.clone(),
            revision_id: self.captured.id.clone(),
            source_manifest_hash: self.descriptor.source_manifest_hash.clone(),
            toolchain_hash: self.captured.toolchain_hash.clone(),
            config_hash: self.captured.config_hash.clone(),
            dependency_hash: self.captured.dependency_hash.clone(),
            lookup_dependencies: vec![],
        }))
    }
}

/// Streams the revision's inventory once into `nat_docs`: the identity of every document the
/// rejoin may ask about. Declaration name status (`nat_declarations`) is not streamed here:
/// settlement only asks about exact-definition targets, whose status discovery writes from the
/// document groups it holds (`streaming::GlobalWriter::target_declarations`), every attempt.
fn native_pass(ctx: &JoinCtx<'_>, reader: &NativeReader<'_>, rev: u8) -> Result<()> {
    let conn = ctx.conn;
    if exists(conn, "SELECT 1 FROM nat_done WHERE rev=?1", &[&rev])? {
        return Ok(());
    }
    for table in ["nat_docs", "nat_done"] {
        conn.execute(&format!("DELETE FROM {table} WHERE rev=?1"), [rev])?;
    }
    conn.execute_batch("BEGIN")?;
    let outcome = (|| -> Result<()> {
        let mut stream = reader.inventory()?;
        let mut ord = 0i64;
        while let Some(document) = stream.next_document()? {
            ctx.check_cancel()?;
            ctx.row("nat_docs")?;
            conn.prepare_cached("INSERT INTO nat_docs VALUES(?1,?2,?3,?4,?5,?6,NULL)")?
                .execute(params![
                    rev,
                    ord,
                    document.key.language,
                    document.key.path,
                    document.content_hash,
                    document.byte_length as i64
                ])?;
            ord += 1;
        }
        conn.execute("INSERT INTO nat_done VALUES(?1)", [rev])?;
        Ok(())
    })();
    match outcome {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK");
            for table in ["nat_docs", "nat_done"] {
                let _ = conn.execute(&format!("DELETE FROM {table} WHERE rev=?1"), [rev]);
            }
            Err(error)
        }
    }
}

/// Runs one pass in a single stage transaction. The stage has no journal, so a failed pass
/// commits what it wrote rather than rolling back; the next attempt clears every join table.
fn in_transaction<T>(conn: &Connection, pass: impl FnOnce() -> Result<T>) -> Result<T> {
    conn.execute_batch("BEGIN")?;
    let result = pass();
    let committed = conn.execute_batch("COMMIT");
    if committed.is_err() && !conn.is_autocommit() {
        let _ = conn.execute_batch("ROLLBACK");
    }
    let value = result?;
    committed?;
    Ok(value)
}

/// Clears the destination-dependent tables when the destination differs from the last attempt.
fn reset_for_destination(conn: &Connection, destination: IndexPin) -> Result<i64> {
    for table in JOIN_TABLES {
        conn.execute(&format!("DELETE FROM {table}"), [])?;
    }
    let generation = destination.index_generation.to_string();
    let previous: Option<(String, i64)> = conn
        .query_row("SELECT generation, revision FROM join_state", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()?;
    if previous.as_ref() != Some(&(generation.clone(), destination.index_revision as i64)) {
        for table in ["nat_docs", "nat_done"] {
            conn.execute(&format!("DELETE FROM {table} WHERE rev=1"), [])?;
        }
    }
    // Every attempt takes a fresh nonce, so a `JoinedBatch` sealed by an earlier attempt no
    // longer matches once a later attempt has cleared or partly refilled the output tables.
    let attempt = NEXT_ATTEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as i64;
    conn.execute(
        "INSERT OR REPLACE INTO join_state VALUES(1,?1,?2,?3)",
        params![generation, destination.index_revision as i64, attempt],
    )?;
    Ok(attempt)
}

static NEXT_ATTEMPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// The native rows an earlier attempt left in the stage. They count against `staged_rows`
/// together with the rows this attempt writes.
fn retained_rows(conn: &Connection) -> Result<u64> {
    let mut total = 0u64;
    let count = conn.query_row("SELECT COUNT(*) FROM nat_docs", [], |row| {
        row.get::<_, i64>(0)
    })?;
    total = total
        .checked_add(u64::try_from(count)?)
        .ok_or_else(|| ScipCaptureError::LimitExceeded("staged rows".into()))?;
    Ok(total)
}

/// One staged join: the sides, the captured basis and the emission validator.
struct StagedJoin<'a> {
    ctx: &'a JoinCtx<'a>,
    producer: &'a semantic::Producer,
    descriptor: &'a Descriptor,
    admitted_encoding: u8,
    captured: StagedSide<'a>,
    destination: StagedSide<'a>,
    filtered: StagedSide<'a>,
    destination_header: &'a NativeHeader,
    validator: EmissionValidator<'a>,
    basis: StagedBasis<'a>,
    emitted: Cell<[i64; 7]>,
    /// Set after eligibility when a document's destination join must equal its captured join:
    /// a shared pin, and every document with claims eligible, so the eligible view of the
    /// captured global rows is all of them.
    identical_joins: Cell<bool>,
    identity_kind: &'static str,
    descriptor_sha256: &'a str,
}

const COVERAGE: usize = 0;
const PROVENANCE: usize = 1;
const SYMBOLS: usize = 2;
const BINDINGS: usize = 3;
const REFERENCES: usize = 4;
const CALLS: usize = 5;
const RELATIONSHIPS: usize = 6;

/// A captured document's join as eligibility computed it: its decoded claims, facts and charge.
struct CapturedJoin {
    claims: Rc<DocumentClaims>,
    facts: super::JoinedFacts,
    reservation: Reservation,
}

/// The output tables of settlement, which a discarded speculative settlement clears.
const OUTPUT_TABLES: [&str; 7] = [
    "out_coverage",
    "out_provenance",
    "out_symbols",
    "out_declaration_bindings",
    "out_references",
    "out_call_bindings",
    "out_type_relationships",
];

/// The source-eligibility decision of one captured document: the single `w_docs` row its
/// eligibility pass writes, and everything coverage later needs from that pass.
#[derive(Default, Clone, Copy)]
struct DocFlags {
    requested: bool,
    failed: bool,
    eligible: bool,
    unsupported: bool,
    /// Claims whose captured and destination anchors are both unsupported. No settled fact
    /// can claim one of them, so they keep the `unsupported` outcome.
    unsupported_claims: u64,
    /// Claims other than symbol information: the document's report denominator.
    claims: u64,
    /// Admitted under `scip_raw::OCCURRENCE_CAP_EXCEEDED`.
    capped: bool,
    /// The unsupported claims by the kind of native gap that hid them (#520).
    gaps: [u64; 4],
}

/// Admitted claims whose source is not a document of the captured revision. Admission binds
/// every staged claim to a listed source, so this guards against a broken stage, not input.
const UNLISTED_CLAIM_DOCUMENT_SQL: &str = "SELECT 1 FROM sources s WHERE EXISTS \
    (SELECT 1 FROM claims c WHERE c.source_ord=s.ord) AND NOT EXISTS (SELECT 1 FROM nat_docs d \
    WHERE d.rev=0 AND d.language=s.language AND d.path=s.path) LIMIT 1";

/// A captured document with claims that is not eligible: its global rows leave the eligible
/// view of a shared pin.
const INELIGIBLE_CLAIM_DOCUMENT_SQL: &str = "SELECT 1 FROM w_docs w WHERE (w.eligible=0 OR w.failed=1) \
    AND EXISTS (SELECT 1 FROM claims c WHERE c.source_ord=w.source_ord) LIMIT 1";

const DOCUMENT_FLAGS_SQL: &str = "SELECT w.requested, w.failed, w.eligible, w.unsupported, \
    w.unsupported_claims, w.claims, c.exclusion, d.capped, w.gaps_macro_invocation, \
    w.gaps_attribute_item, w.gaps_macro_definition, w.gaps_other FROM sources s LEFT JOIN w_docs w ON \
    w.source_ord=s.ord LEFT JOIN coverage c ON c.source_ord=s.ord LEFT JOIN documents d ON \
    d.source_ord=s.ord WHERE s.language=?1 AND s.path=?2";

impl StagedJoin<'_> {
    fn conn(&self) -> &Connection {
        self.ctx.conn
    }

    fn captured_id(&self) -> &str {
        &self.captured.header.revision.id
    }

    fn destination_id(&self) -> &str {
        &self.destination_header.revision.id
    }

    /// The `out_coverage.ord` of the document being settled: its coverage row follows its facts.
    fn current_document(&self) -> i64 {
        self.emitted.get()[COVERAGE]
    }

    fn next_ord(&self, table: usize) -> i64 {
        let mut counts = self.emitted.get();
        let ord = counts[table];
        counts[table] += 1;
        self.emitted.set(counts);
        ord
    }

    fn unchanged(&self, document: &native::DocumentKey) -> Result<bool> {
        if self.destination.rev == self.captured.rev {
            return Ok(true);
        }
        exists(
            self.conn(),
            "SELECT 1 FROM nat_docs a JOIN nat_docs b ON b.rev=1 AND b.language=a.language AND b.path=a.path \
             WHERE a.rev=0 AND a.language=?1 AND a.path=?2 AND b.content_hash=a.content_hash",
            &[&document.language, &document.path],
        )
    }

    /// Whether `syntax_id` is a captured exact definition of `document`: the anchor of an exact
    /// declaration-name join of the captured revision, so a named captured declaration.
    fn captured_definition(&self, document: &native::DocumentKey, syntax_id: &str) -> Result<bool> {
        self.ctx.charge(1)?;
        exists(
            self.conn(),
            "SELECT 1 FROM g_definitions WHERE side=0 AND owner_doc=\
             (SELECT ord FROM sources WHERE language=?1 AND path=?2) AND syntax_id=?3",
            &[&document.language, &document.path, &codec::sid(syntax_id)],
        )
    }

    /// The document ordinal and name facts of one declaration in a revision.
    fn declaration_status(
        &self,
        rev: u8,
        document: &native::DocumentKey,
        syntax_id: &str,
    ) -> Result<Vec<(bool, bool)>> {
        self.ctx.charge(1)?;
        let mut statement = self.conn().prepare_cached(
            "SELECT n.named, n.name_ok FROM nat_declarations n JOIN nat_docs d \
             ON d.rev=n.rev AND d.ord=n.doc_ord WHERE n.rev=?1 AND d.language=?2 AND d.path=?3 AND n.syntax_id=?4",
        )?;
        Ok(statement
            .query_map(
                params![rev, document.language, document.path, codec::sid(syntax_id)],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Prepays the document-key copies of `views` document-local views before they are made.
    fn view_keys(&self, document: &native::DocumentKey, views: u64) -> Result<Reservation> {
        let key = sizes::sum(&[
            std::mem::size_of::<native::DocumentKey>() as u64,
            sizes::document(document)?,
        ])?;
        self.ctx.live.reserve(
            key.checked_mul(views)
                .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?,
        )
    }

    fn exclusion(&self, document: &native::DocumentKey) -> Result<Option<String>> {
        Ok(self
            .conn()
            .prepare_cached(
                "SELECT c.exclusion FROM sources s JOIN coverage c ON c.source_ord=s.ord WHERE s.language=?1 AND s.path=?2",
            )?
            .query_row(params![document.language, document.path], |row| row.get(0))
            .optional()?
            .flatten())
    }

    /// Pass 2: with the complete captured context, joins one captured document at a time and
    /// decides its whole-document source eligibility. Only the decision row survives; the
    /// document's captured facts are released before the next document.
    fn eligibility(&self, global: &streaming::CompletedGlobal<'_>) -> Result<()> {
        if exists(self.conn(), UNLISTED_CLAIM_DOCUMENT_SQL, &[])? {
            anyhow::bail!("missing admitted claim document");
        }
        // Each document's decision is independent of the others, so the order is free.
        for document in self.captured.documents_reversed() {
            self.eligibility_document(&document?, global)?;
        }
        Ok(())
    }

    /// Decides one captured document's source eligibility and writes its `w_docs` row. Returns
    /// the decision and, for a document with claims, the captured join it was decided on, with
    /// its decoded claims and the join's charge.
    fn eligibility_document(
        &self,
        document: &native::DocumentKey,
        global: &streaming::CompletedGlobal<'_>,
    ) -> Result<(DocFlags, Option<CapturedJoin>)> {
        self.ctx.check_cancel()?;
        let mut flags = DocFlags::default();
        if self
            .producer
            .languages
            .contains(&language_of(&document.language)?)
        {
            let reason = self.exclusion(document)?;
            if reason.as_deref() != Some("notRequested") {
                flags.requested = true;
            }
            if reason.as_deref() == Some("failed")
                || (reason.as_deref() != Some("notRequested") && !self.unchanged(document)?)
            {
                flags.failed = true;
            }
        }
        let claims = DocumentClaims::load(&self.captured, document)?;
        let mut joined = None;
        if !claims.claims.is_empty() {
            flags.claims = claims.non_information();
            let claims = Rc::new(claims);
            let _keys = self.view_keys(document, 1)?;
            let local = streaming::claims_side(&self.captured, document.clone(), claims.clone())?;
            let (facts, reservation) =
                streaming::join_document(&local, global, self.admitted_encoding)?;
            #[cfg(feature = "test-support")]
            crate::semantic_batch::testing::run_document_common_observer(
                0,
                document,
                &facts,
                self.conn(),
            );
            self.classify(document, &local, &facts, &mut flags)?;
            #[cfg(feature = "test-support")]
            if crate::semantic_batch::testing::eligibility_forced_failed(document) {
                flags.failed = true;
            }
            joined = Some(CapturedJoin {
                claims,
                facts,
                reservation,
            });
        }
        self.ctx.row("w_docs")?;
        self.conn()
            .prepare_cached(
                "INSERT INTO w_docs SELECT ord,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12 FROM sources WHERE language=?1 AND path=?2",
            )?
            .execute(params![
                document.language,
                document.path,
                flags.requested,
                flags.failed,
                flags.eligible,
                flags.unsupported,
                i64::try_from(flags.unsupported_claims)?,
                i64::try_from(flags.claims)?,
                i64::try_from(flags.gaps[0])?,
                i64::try_from(flags.gaps[1])?,
                i64::try_from(flags.gaps[2])?,
                i64::try_from(flags.gaps[3])?,
            ])?;
        Ok((flags, joined))
    }

    /// Passes 2 and 4 in one walk, for a shared pin: each document's eligibility, then at
    /// once its settlement and coverage, reusing the captured join eligibility was decided
    /// on. Settling a document before every decision is made assumes every document with
    /// claims is eligible, so that the eligible destination context is the whole captured
    /// one and each destination join equals its captured join (`identical_joins`). The first
    /// document with claims that is not eligible ends the assumption: the remaining decisions
    /// are still made, the settled rows are discarded (their row attempts stay charged) and
    /// `None` asks the caller for the separate settlement pass.
    fn eligibility_and_settlement(
        &self,
        global: &streaming::CompletedGlobal<'_>,
    ) -> Result<Option<BTreeMap<semantic::Language, LanguageTally>>> {
        if exists(self.conn(), UNLISTED_CLAIM_DOCUMENT_SQL, &[])? {
            anyhow::bail!("missing admitted claim document");
        }
        let manifest_hash = self.destination_manifest_hash()?;
        let mut tallies = BTreeMap::new();
        let mut settling = true;
        self.identical_joins.set(true);
        for document in self.destination.documents() {
            let document = document?;
            let (_, joined) = self.eligibility_document(&document, global)?;
            if !settling {
                continue;
            }
            let (flags, excluded) = self.document_flags(&document)?;
            let settled = flags.eligible && !flags.failed;
            if joined.is_some() && !settled {
                settling = false;
                continue;
            }
            let outcome = if settled {
                self.settle_document(&document, global, global, &manifest_hash, joined)?
            } else {
                settle::DocumentOutcome::default()
            };
            self.document_coverage(
                &document,
                flags,
                excluded.as_deref(),
                &outcome,
                &mut tallies,
            )?;
        }
        if settling {
            return Ok(Some(tallies));
        }
        self.identical_joins.set(false);
        for table in OUTPUT_TABLES {
            self.conn().execute(&format!("DELETE FROM {table}"), [])?;
        }
        self.emitted.set([0; 7]);
        Ok(None)
    }

    /// Classifies each claim of one captured document against its captured facts and the
    /// destination's measured anchors. A later failing claim fails the whole document.
    fn classify(
        &self,
        document: &native::DocumentKey,
        local: &StagedSide<'_>,
        facts: &super::JoinedFacts,
        flags: &mut DocFlags,
    ) -> Result<()> {
        let enc = self.admitted_encoding;
        let mut sets = local.operation()?;
        let entry = sizes::map_entries::<&str, ()>(1)?;
        let mut declarations = BTreeSet::new();
        let mut calls = BTreeMap::new();
        let mut associations = BTreeSet::new();
        for binding in &facts.declaration_bindings {
            if binding.syntax_id.is_some() {
                sets.grow(sizes::sum(&[entry, entry])?)?;
                declarations.insert(binding.provenance_id.as_str());
                associations.insert(binding.provenance_id.as_str());
            }
        }
        for proof in facts.reference_claims.keys() {
            sets.grow(entry)?;
            associations.insert(proof.as_str());
        }
        for binding in &facts.call_bindings {
            if binding.call_id.is_some() {
                sets.grow(sizes::sum(&[
                    entry,
                    sizes::map_entries::<&str, &semantic::CallBinding>(1)?,
                ])?)?;
                // The last binding of a proof is the one the whole-corpus map kept.
                calls.insert(binding.provenance_id.as_str(), binding);
                associations.insert(binding.provenance_id.as_str());
            }
        }
        for relationship in &facts.type_relationships {
            sets.grow(entry)?;
            associations.insert(relationship.provenance_id.as_str());
        }
        let unchanged = self.unchanged(document)?;
        let mut destination: Option<(Reservation, StagedSide<'_>)> = None;
        for claim in local.claims() {
            let claim = claim?;
            anyhow::ensure!(
                claim.language == document.language
                    && claim.envelope.relative_path == document.path,
                "captured claim outside its document"
            );
            flags.requested = true;
            local
                .bytes_of(document)?
                .context("missing captured claim source")?;
            if !unchanged {
                flags.failed = true;
                continue;
            }
            if claim.envelope.record_kind != "occurrence" {
                flags.eligible = true;
                continue;
            }
            let range = local.occurrence_range(&claim, enc)?;
            if range.start == range.end {
                flags.eligible = true;
                continue;
            }
            let roles = claim.envelope.raw["symbolRoles"]
                .as_u64()
                .context("invalid raw roles")?;
            let proof = claim.captured_proof_id.as_str();
            let captured_declaration = declarations.contains(proof);
            let captured_reference = facts.reference_claims.contains_key(proof);
            let required = if captured_declaration || (!captured_reference && roles & 1 != 0) {
                semantic::JoinKind::DeclarationName
            } else {
                semantic::JoinKind::Reference
            };
            if destination.is_none() {
                let keys = self.view_keys(document, 1)?;
                destination = Some((
                    keys,
                    streaming::native_document_side(&self.destination, document.clone())?,
                ));
            }
            let destination = &destination.as_ref().expect("destination native view").1;
            let mut captured_operation = local.operation()?;
            let mut destination_operation = destination.operation()?;
            let first =
                local.join_reserved(document, range.clone(), required, &mut captured_operation)?;
            let second =
                destination.join_reserved(document, range, required, &mut destination_operation)?;
            let authored = associations.contains(proof);
            if !authored
                && first.status == semantic::JoinStatus::Unsupported
                && second.status == semantic::JoinStatus::Unsupported
            {
                flags.unsupported = true;
                flags.unsupported_claims += 1;
                flags.gaps[semantic::CoverageReasons::gap_kind(first.diagnostic.as_deref())] += 1;
                flags.eligible = true;
                continue;
            }
            if !authored
                && first.status == semantic::JoinStatus::Unmatched
                && second.status == semantic::JoinStatus::Unmatched
            {
                flags.eligible = true;
                continue;
            }
            let mut exact = first.status == semantic::JoinStatus::Exact
                && second.status == semantic::JoinStatus::Exact;
            if captured_reference && required != semantic::JoinKind::Reference {
                let first_reference = local.join_reserved(
                    document,
                    first.anchor.range.clone(),
                    semantic::JoinKind::Reference,
                    &mut captured_operation,
                )?;
                let second_reference = destination.join_reserved(
                    document,
                    first.anchor.range.clone(),
                    semantic::JoinKind::Reference,
                    &mut destination_operation,
                )?;
                exact &= first_reference.status == semantic::JoinStatus::Exact
                    && second_reference.status == semantic::JoinStatus::Exact;
            }
            if let Some(binding) = calls.get(proof) {
                let call = destination.join_reserved(
                    document,
                    binding.join.anchor.range.clone(),
                    binding.join.anchor.kind,
                    &mut destination_operation,
                )?;
                let first_call = local.join_reserved(
                    document,
                    binding.join.anchor.range.clone(),
                    binding.join.anchor.kind,
                    &mut captured_operation,
                )?;
                exact &= call.status == semantic::JoinStatus::Exact
                    && first_call.status == semantic::JoinStatus::Exact
                    && first_call.candidate_ids == binding.join.candidate_ids;
                let mut same_call = false;
                if let Some(id) = second.candidate_ids.first() {
                    let calls = match call.candidate_ids.first() {
                        Some(call_id) => destination.calls_with_id(
                            document,
                            call_id,
                            &mut destination_operation,
                        )?,
                        None => vec![],
                    };
                    same_call = destination
                        .references_with_id(document, id, &mut destination_operation)?
                        .iter()
                        .any(|reference| {
                            &reference.id == id
                                && calls.iter().any(|candidate| {
                                    call.candidate_ids == vec![candidate.id.clone()]
                                        && candidate.owner_syntax_id == reference.owner_syntax_id
                                        && candidate.range.start <= reference.range.start
                                        && reference.range.end <= candidate.range.end
                                        && candidate
                                            .callee_range
                                            .as_ref()
                                            .is_none_or(|callee| callee == &reference.range)
                                })
                        });
                }
                exact &= same_call;
            }
            if exact {
                flags.eligible = true;
            } else {
                flags.failed = true;
            }
        }
        Ok(())
    }

    /// The eligibility decision and producer exclusion recorded for one source document.
    fn document_flags(&self, document: &native::DocumentKey) -> Result<(DocFlags, Option<String>)> {
        type Row = (
            Option<bool>,
            Option<bool>,
            Option<bool>,
            Option<bool>,
            Option<i64>,
            Option<i64>,
            Option<String>,
            Option<bool>,
            [Option<i64>; 4],
        );
        let row: Option<Row> = self
            .conn()
            .prepare_cached(DOCUMENT_FLAGS_SQL)?
            .query_row(params![document.language, document.path], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    [row.get(8)?, row.get(9)?, row.get(10)?, row.get(11)?],
                ))
            })
            .optional()?;
        let Some((
            requested,
            failed,
            eligible,
            unsupported,
            unsupported_claims,
            claims,
            excluded,
            capped,
            stored_gaps,
        )) = row
        else {
            return Ok((DocFlags::default(), None));
        };
        let mut gaps = [0u64; 4];
        for (gap, stored) in gaps.iter_mut().zip(stored_gaps) {
            *gap = u64::try_from(stored.unwrap_or_default())?;
        }
        Ok((
            DocFlags {
                requested: requested.unwrap_or_default(),
                failed: failed.unwrap_or_default(),
                eligible: eligible.unwrap_or_default(),
                unsupported: unsupported.unwrap_or_default(),
                unsupported_claims: u64::try_from(unsupported_claims.unwrap_or_default())?,
                claims: u64::try_from(claims.unwrap_or_default())?,
                capped: capped.unwrap_or_default(),
                gaps,
            },
            excluded,
        ))
    }

    /// Pass 4: in destination document order, settles each eligible document's final rows and
    /// emits every document's coverage. Nothing of a document outlives its iteration except
    /// its emitted `out_*` rows and the bounded per-language tally.
    fn settle(
        &self,
        captured: &streaming::CompletedGlobal<'_>,
        destination: &streaming::CompletedGlobal<'_>,
    ) -> Result<BTreeMap<semantic::Language, LanguageTally>> {
        let manifest_hash = self.destination_manifest_hash()?;
        let mut tallies = BTreeMap::new();
        for document in self.destination.documents() {
            let document = document?;
            self.ctx.check_cancel()?;
            let (flags, excluded) = self.document_flags(&document)?;
            let outcome = if flags.eligible && !flags.failed {
                self.settle_document(&document, captured, destination, &manifest_hash, None)?
            } else {
                settle::DocumentOutcome::default()
            };
            self.document_coverage(
                &document,
                flags,
                excluded.as_deref(),
                &outcome,
                &mut tallies,
            )?;
        }
        Ok(tallies)
    }

    /// Emits a document's coverage row when the producer covers its language.
    fn document_coverage(
        &self,
        document: &native::DocumentKey,
        flags: DocFlags,
        excluded: Option<&str>,
        outcome: &settle::DocumentOutcome,
        tallies: &mut BTreeMap<semantic::Language, LanguageTally>,
    ) -> Result<()> {
        let language = language_of(&document.language)?;
        if self.producer.languages.contains(&language) {
            self.coverage(document, language, flags, excluded, outcome, tallies)?;
        }
        Ok(())
    }

    fn destination_manifest_hash(&self) -> Result<String> {
        let mut hasher = SourceManifestHasher::new(&self.destination_header.source_set.id);
        let mut statement = self.conn().prepare_cached(
            "SELECT language, path, content_hash FROM nat_docs WHERE rev=?1 ORDER BY ord",
        )?;
        let mut rows = statement.query([self.destination.rev])?;
        while let Some(row) = rows.next()? {
            hasher.push(
                &row.get::<_, String>(0)?,
                &row.get::<_, String>(1)?,
                &row.get::<_, String>(2)?,
            );
        }
        Ok(hasher.finish())
    }

    fn coverage(
        &self,
        doc: &native::DocumentKey,
        language: semantic::Language,
        flags: DocFlags,
        excluded: Option<&str>,
        outcome: &settle::DocumentOutcome,
        tallies: &mut BTreeMap<semantic::Language, LanguageTally>,
    ) -> Result<()> {
        let DocFlags {
            requested,
            failed,
            unsupported,
            ..
        } = flags;
        let reasons = semantic::CoverageReasons::new(
            flags.gaps,
            flags.capped,
            outcome.omitted,
            outcome.diagnostics,
        );
        anyhow::ensure!(
            unsupported == (reasons.native_gaps() > 0),
            "unsupported document without a native gap count"
        );
        let partial = excluded == Some("unsupported") || !reasons.is_empty();
        let state = if !requested {
            semantic::CoverageState::NotRequested
        } else if failed {
            semantic::CoverageState::Failed
        } else if excluded == Some("unsupported") {
            semantic::CoverageState::Omitted
        } else if partial {
            semantic::CoverageState::Partial
        } else {
            semantic::CoverageState::Complete
        };
        let row = semantic::Coverage {
            producer_id: self.producer.id.clone(),
            language,
            source_set_id: doc.source_set_id.clone(),
            document_path: doc.path.clone(),
            revision_id: self.destination_id().to_owned(),
            requested,
            selected: requested && state != semantic::CoverageState::Omitted,
            state,
            supported_roles: [
                semantic::Role::Definition,
                semantic::Role::Read,
                semantic::Role::Write,
                semantic::Role::Call,
                semantic::Role::Type,
                semantic::Role::Import,
                semantic::Role::Alias,
            ]
            .into_iter()
            .filter(|r| language != semantic::Language::Java || *r != semantic::Role::Alias)
            .collect(),
            observed_roles: outcome.observed.iter().copied().collect(),
            diagnostic: if failed {
                Some(
                    if excluded == Some("failed") {
                        "excluded_failed_by_producer"
                    } else {
                        "unmeasurable_source_anchor"
                    }
                    .into(),
                )
            } else if excluded == Some("unsupported") {
                Some("excluded_by_producer".into())
            } else if unsupported {
                Some("unsupported_native_gap".into())
            } else if flags.capped {
                Some(crate::scip_raw::OCCURRENCE_CAP_EXCEEDED.into())
            } else if partial {
                Some("unmeasurable_internal_target".into())
            } else {
                None
            },
            reasons: (state == semantic::CoverageState::Partial).then_some(reasons),
        };
        let conn = self.conn();
        self.validator.coverage(&row, &|language, path| {
            exists(
                conn,
                "SELECT 1 FROM nat_docs WHERE rev=?1 AND language=?2 AND path=?3",
                &[&self.destination.rev, &language, &path],
            )
            .unwrap_or(false)
        })?;
        let body = to_json(&row)?;
        self.emit(
            "out_coverage",
            "INSERT INTO out_coverage VALUES(?1,?2,?3,?4,?5)",
            &[
                &self.next_ord(COVERAGE),
                &doc.language,
                &doc.path,
                &body,
                &outcome.provenance,
            ],
        )?;
        // [requested, matched, skipped, ambiguous, unsupported]; every claim without a settled
        // outcome is skipped, and a failed document skips all of its claims.
        let mut counts = [flags.claims, 0, 0, 0, 0];
        if state == semantic::CoverageState::Failed {
            counts[2] = flags.claims;
        } else {
            counts[1] = outcome.matched;
            counts[3] = outcome.ambiguous;
            counts[4] = flags.unsupported_claims;
            counts[2] = counts[1]
                .checked_add(counts[3])
                .and_then(|settled| settled.checked_add(counts[4]))
                .and_then(|settled| flags.claims.checked_sub(settled))
                .context("incomplete semantic import claim tally")?;
        }
        let tally = tallies.entry(language).or_default();
        tally.documents += 1;
        for (total, count) in tally.counts.iter_mut().zip(counts) {
            *total += count;
        }
        match state {
            semantic::CoverageState::Complete => tally.complete += 1,
            semantic::CoverageState::Partial => tally.partial += 1,
            semantic::CoverageState::Failed => tally.failed += 1,
            semantic::CoverageState::Omitted => tally.omitted += 1,
            _ => {}
        }
        tally.unsupported_documents += u64::from(unsupported);
        tally.reasons.add(&row);
        Ok(())
    }

    fn emit(&self, table: &'static str, sql: &str, values: &[&dyn rusqlite::ToSql]) -> Result<()> {
        self.ctx.row(table)?;
        self.conn().prepare_cached(sql)?.execute(values)?;
        Ok(())
    }

    fn run(&self) -> Result<serde_json::Value> {
        let encoding = self.admitted_encoding;
        self.ctx.pass("native");
        let shared = self.destination.rev == self.captured.rev;
        let captured = in_transaction(self.conn(), || {
            streaming::discover_side(
                &self.captured,
                encoding,
                if shared {
                    streaming::TargetStatus::Own
                } else {
                    streaming::TargetStatus::None
                },
            )
        })?;
        self.ctx.pass("captured discovery");
        #[cfg(feature = "test-support")]
        let fuse = shared && !crate::semantic_batch::testing::settlement_separated();
        #[cfg(not(feature = "test-support"))]
        let fuse = shared;
        if fuse {
            let fused = in_transaction(self.conn(), || self.eligibility_and_settlement(&captured))?;
            self.ctx.pass("eligibility and settlement");
            if let Some(tallies) = fused {
                #[cfg(feature = "test-support")]
                crate::semantic_batch::testing::run_before_closure_hook(self.conn());
                self.closure()?;
                return self.report(tallies);
            }
        } else {
            in_transaction(self.conn(), || self.eligibility(&captured))?;
            self.ctx.pass("eligibility");
        }
        // With a shared pin both sides read the same native revision without destination
        // edits, and every global row stems from one claim's own document. Discovery over the
        // eligible claims would therefore write exactly the captured rows of eligible owner
        // documents: the eligible view of side 0 is that context, and no side-1 rows exist.
        // No known producer input leaves a document with claims ineligible on a shared pin
        // (that needs duplicate native anchors), so the filtering view is a fallback that only
        // a unit test exercises; the common case reuses the captured join.
        let destination = if shared {
            self.identical_joins
                .set(!exists(self.conn(), INELIGIBLE_CLAIM_DOCUMENT_SQL, &[])?);
            captured.eligible_view()?
        } else {
            in_transaction(self.conn(), || {
                let destination = streaming::discover_side(
                    &self.filtered,
                    encoding,
                    streaming::TargetStatus::OwnAndCaptured,
                )?;
                streaming::remaining_target_status(&self.captured, &self.destination)?;
                Ok(destination)
            })?
        };
        self.ctx.pass("destination discovery");
        let tallies = in_transaction(self.conn(), || self.settle(&captured, &destination))?;
        self.ctx.pass("settlement");
        #[cfg(feature = "test-support")]
        crate::semantic_batch::testing::run_before_closure_hook(self.conn());
        self.closure()?;
        self.report(tallies)
    }
}

/// The claim counts and document states of one language, for the bounded report.
#[derive(Default)]
struct LanguageTally {
    documents: u64,
    counts: [u64; 5],
    complete: u64,
    partial: u64,
    failed: u64,
    omitted: u64,
    unsupported_documents: u64,
    reasons: super::LanguageReasons,
}

impl StagedJoin<'_> {
    fn closure(&self) -> Result<()> {
        self.ctx.check_output()?;
        let conn = self.conn();
        let count = |sql: &str| -> Result<i64> { Ok(conn.query_row(sql, [], |row| row.get(0))?) };
        for table in [
            "out_symbols",
            "out_declaration_bindings",
            "out_references",
            "out_call_bindings",
            "out_type_relationships",
        ] {
            anyhow::ensure!(
                count(&format!(
                    "SELECT COUNT(*) FROM {table} o WHERE NOT EXISTS \
                     (SELECT 1 FROM out_provenance p WHERE p.id=o.provenance_id AND p.doc=o.doc)"
                ))? == 0,
                "semantic row without provenance"
            );
        }
        // A provenance row derives from a staged claim of its own coverage document.
        anyhow::ensure!(
            count(
                "SELECT COUNT(*) FROM out_provenance o LEFT JOIN claims c ON c.claim_id=o.claim_id \
                 LEFT JOIN sources s ON s.ord=c.source_ord LEFT JOIN out_coverage v ON v.ord=o.doc \
                 WHERE s.ord IS NULL OR v.ord IS NULL OR s.language<>v.language OR s.path<>v.path \
                 OR o.kind NOT IN (0,1,2,3)"
            )? == 0,
            "derived provenance without staged claim"
        );
        for (sql, message) in [
            (
                "SELECT COUNT(*) FROM (SELECT 1 FROM out_declaration_bindings \
                 GROUP BY provenance_id, json_extract(body,'$.join.anchor') HAVING COUNT(*)>1)",
                "duplicate declaration binding fact",
            ),
            (
                "SELECT COUNT(*) FROM (SELECT 1 FROM out_call_bindings \
                 GROUP BY provenance_id, json_extract(body,'$.join.anchor') HAVING COUNT(*)>1)",
                "duplicate call binding fact",
            ),
            (
                "SELECT COUNT(*) FROM (SELECT 1 FROM out_type_relationships GROUP BY provenance_id, \
                 json_extract(body,'$.kind'), json_extract(body,'$.source'), json_extract(body,'$.target') HAVING COUNT(*)>1)",
                "duplicate relationship fact",
            ),
            (
                "SELECT COUNT(*) FROM (SELECT 1 FROM out_symbols s JOIN out_provenance p \
                 ON p.id=s.provenance_id JOIN claims c ON c.claim_id=p.claim_id \
                 GROUP BY c.artifact, json_extract(s.body,'$.key') HAVING COUNT(*)>1)",
                DUPLICATE_SYMBOL_KEY,
            ),
            // Keys are unique per Cargo target (#394), but a global key still conflicts with
            // the same key in any target.
            (
                "SELECT COUNT(*) FROM (SELECT 1 FROM out_symbols s JOIN out_provenance p \
                 ON p.id=s.provenance_id JOIN claims c ON c.claim_id=p.claim_id \
                 WHERE json_extract(s.body,'$.key.scope') IN ('global','target') \
                 GROUP BY c.artifact, json_extract(s.body,'$.key.symbol') \
                 HAVING SUM(json_extract(s.body,'$.key.scope')='global')>0 AND COUNT(*)>1)",
                DUPLICATE_SYMBOL_KEY,
            ),
            (
                "SELECT COUNT(*) FROM out_provenance p WHERE NOT EXISTS (SELECT 1 FROM out_coverage c \
                 WHERE c.ord=p.doc AND c.provenance IS NOT NULL \
                 AND json_extract(c.body,'$.state') IN ('complete','partial'))",
                "proof on non-fact-bearing coverage",
            ),
            (
                "SELECT COUNT(*) FROM out_coverage c WHERE c.provenance IS NOT NULL AND NOT EXISTS \
                 (SELECT 1 FROM nat_docs d WHERE d.rev=0 AND d.language=c.language AND d.path=c.path \
                 AND json_extract(c.provenance,'$.document.language')=c.language \
                 AND json_extract(c.provenance,'$.document.path')=c.path \
                 AND d.content_hash=json_extract(c.provenance,'$.contentHash'))",
                "captured proof document hash mismatch",
            ),
        ] {
            if count(sql)? != 0 {
                // A producer that gives one global symbol information in two documents of an
                // artifact (rust-analyzer does across Cargo targets) is an invalid artifact,
                // reported typed like any other, not an internal error.
                if message == DUPLICATE_SYMBOL_KEY {
                    return Err(ScipCaptureError::InvalidArtifact(message.into()).into());
                }
                anyhow::bail!(message);
            }
        }
        let languages = self
            .producer
            .languages
            .iter()
            .map(|language| {
                Ok(serde_json::to_value(language)?
                    .as_str()
                    .unwrap_or_default()
                    .to_owned())
            })
            .collect::<Result<Vec<String>>>()?;
        let requested: i64 = {
            let mut total = 0;
            for language in &languages {
                total += conn.query_row(
                    "SELECT COUNT(*) FROM nat_docs WHERE rev=?1 AND language=?2",
                    params![self.destination.rev, language],
                    |row| row.get::<_, i64>(0),
                )?;
            }
            total
        };
        anyhow::ensure!(
            count("SELECT COUNT(*) FROM out_coverage")? == requested,
            "coverage count differs from the requested-language source count"
        );
        anyhow::ensure!(
            count(
                "SELECT COUNT(*) FROM (SELECT 1 FROM out_coverage GROUP BY language, path HAVING COUNT(*)>1)"
            )? == 0,
            "duplicate coverage row"
        );
        Ok(())
    }

    fn report(
        &self,
        tallies: BTreeMap<semantic::Language, LanguageTally>,
    ) -> Result<serde_json::Value> {
        let conn = self.conn();
        let artifacts = {
            let mut statement = conn.prepare(
                "SELECT ordinal, language, sha256, documents, claims FROM artifacts ORDER BY ordinal",
            )?;
            statement
                .query_map([], |row| {
                    Ok(serde_json::json!({
                        "ordinal": row.get::<_, i64>(0)?, "language": row.get::<_, String>(1)?,
                        "artifactHash": row.get::<_, String>(2)?, "documents": row.get::<_, i64>(3)?,
                        "claims": row.get::<_, i64>(4)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let languages: Vec<_> = tallies
            .into_iter()
            .map(|(language, tally)| {
                serde_json::json!({
                    "language": language, "documents": tally.documents, "requested": tally.counts[0],
                    "matched": tally.counts[1], "skipped": tally.counts[2],
                    "ambiguous": tally.counts[3], "unsupported": tally.counts[4],
                    "complete": tally.complete, "partial": tally.partial, "failed": tally.failed,
                    "omitted": tally.omitted, "unsupportedDocuments": tally.unsupported_documents,
                    "partialReasons": tally.reasons.json(),
                })
            })
            .collect();
        #[allow(unused_mut)]
        let mut report = serde_json::json!({
            "producerId": self.producer.id,
            "identityKind": self.identity_kind,
            "sourceSetId": self.destination_header.revision.source_set_id,
            "capturedRevision": self.captured_id(),
            "destinationRevision": self.destination_id(),
            "descriptorSha256": self.descriptor_sha256,
            "artifacts": artifacts,
            "languages": languages,
        });
        #[cfg(feature = "test-support")]
        if let Ok(bytes) = std::env::var("TRELLIS_TEST_SEMANTIC_REPORT_PADDING")
            && let Ok(bytes) = bytes.parse::<usize>()
            && bytes <= 1024 * 1024
        {
            report["padding"] = serde_json::Value::String("x".repeat(bytes));
        }
        anyhow::ensure!(
            serde_json::to_vec(&report)?.len() <= 1024 * 1024,
            "semantic import report exceeds response bound"
        );
        Ok(report)
    }
}

/// The seed owner: native rows an earlier attempt left in the stage.
const RETAINED_SEED: &str = "seed:retained-native";

/// Opt-in (`TRELLIS_INDEX_DIAGNOSTICS=1`) table-by-table ledger of one successful join
/// attempt: charged row attempts by owner and the physical stage bytes of each table with its
/// indexes. Best effort; carries no IDs, paths or pins, and never stops the join.
fn emit_ledger_diagnostic(ctx: &JoinCtx<'_>, lookup_steps: u64) {
    if std::env::var("TRELLIS_INDEX_DIAGNOSTICS").as_deref() != Ok("1") {
        return;
    }
    let physical = (|| -> Result<BTreeMap<String, i64>> {
        let mut statement = ctx.conn.prepare(
            "SELECT COALESCE(s.tbl_name, d.name), SUM(d.pgsize) FROM dbstat d \
             LEFT JOIN sqlite_schema s ON s.name=d.name GROUP BY 1",
        )?;
        let rows = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    })();
    let pages = (|| -> Result<(u64, i64)> {
        let freelist: i64 = ctx
            .conn
            .query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
        Ok((stage_bytes(ctx.conn)?, freelist))
    })();
    let line = serde_json::json!({
        "rowAttempts": *ctx.ledger.borrow(),
        "rowAttemptTotal": ctx.rows.get(),
        "passes": ctx.passes.borrow().iter().map(|(pass, lookup, rows)| {
            serde_json::json!({"pass": pass, "lookupSteps": lookup, "rowAttempts": rows})
        }).collect::<Vec<_>>(),
        "lookupSteps": lookup_steps,
        "liveHighWaterBytes": ctx.live.high_water(),
        "stageBaseBytes": ctx.stage_base,
        "stageBytes": pages.as_ref().ok().map(|(bytes, _)| *bytes),
        "freelistPages": pages.as_ref().ok().map(|(_, pages)| *pages),
        "physicalBytes": physical.as_ref().ok(),
        "physicalBytesError": physical.as_ref().err().map(|error| format!("{error:#}")),
    });
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr().lock(), "semantic-join-ledger {line}");
}

/// Joins an admitted batch against the captured revision and `destination`, writing only
/// validated rows to the stage's `out_*` tables. The lookup budget and the counters outlive the
/// call, so a retry after a destination change keeps charging the same budget. Destination
/// edits apply only when the two revisions differ.
#[allow(clippy::too_many_arguments)]
pub fn join_staged(
    admitted: &mut AdmittedBatch,
    store: &Store,
    destination: IndexPin,
    limits: &BatchLimits,
    budget: &mut LookupBudget,
    counters: &mut BatchCounters,
    mutations: DestinationMutations,
    cancel: &CancelFlag,
) -> Result<JoinedBatch> {
    let captured_pin = admitted.captured_pin();
    let shared = captured_pin == destination;
    let parts = admitted.join_parts()?;
    let ctx = JoinCtx {
        conn: parts.conn,
        limits: limits.clone(),
        budget: RefCell::new(budget.clone()),
        live: LiveBudget::new(limits.unit_expansion_bytes),
        counters: RefCell::new(BatchCounters::default()),
        cache: RefCell::new(DocCache {
            budget: limits.document_cache_bytes,
            ..DocCache::default()
        }),
        rows: Cell::new(0),
        ledger: RefCell::new(BTreeMap::new()),
        passes: RefCell::new(Vec::new()),
        stage_base: stage_bytes(parts.conn)?,
        mutations: if shared {
            DestinationMutations::default()
        } else {
            mutations
        },
        cancel: cancel.clone(),
    };
    let producer = parts.producer.producer();
    let body =
        |attempt: i64, captured: &NativeReader<'_>, destination_reader: &NativeReader<'_>| {
            let captured_header = captured.header()?;
            let destination_header = destination_reader.header()?;
            let descriptor = parts.descriptor;
            native_pass(&ctx, captured, 0)?;
            if !shared {
                native_pass(&ctx, destination_reader, 1)?;
            }
            let destination_rev = if shared { 0 } else { 1 };
            let side = |reader, header, rev, side, filtered| StagedSide {
                ctx: &ctx,
                reader,
                header,
                rev,
                side,
                filtered,
                document: None,
                local_claims: None,
                local_group: None,
            };
            let join = StagedJoin {
                ctx: &ctx,
                producer: &producer,
                descriptor,
                admitted_encoding: descriptor.producer.position_encoding,
                captured: side(captured, &captured_header, 0, 0, false),
                destination: side(
                    destination_reader,
                    &destination_header,
                    destination_rev,
                    1,
                    false,
                ),
                filtered: side(
                    destination_reader,
                    &destination_header,
                    destination_rev,
                    1,
                    true,
                ),
                destination_header: &destination_header,
                validator: EmissionValidator::new(EmissionHeader {
                    producer: &producer,
                    captured_revision: captured_header.revision.clone(),
                    destination_revision: destination_header.revision.clone(),
                    source_set_languages: destination_header.source_set.languages.clone(),
                    artifact_hashes: descriptor
                        .artifacts
                        .iter()
                        .map(|artifact| artifact.sha256.clone())
                        .collect(),
                    source_manifest_hash: descriptor.source_manifest_hash.clone(),
                }),
                basis: StagedBasis {
                    conn: ctx.conn,
                    descriptor,
                    producer: &producer,
                    captured: &captured_header.revision,
                },
                emitted: Cell::new([0; 7]),
                identical_joins: Cell::new(false),
                identity_kind: descriptor.producer.identity_kind.as_str(),
                descriptor_sha256: parts.descriptor_sha256,
            };
            let report = join.run()?;
            emit_ledger_diagnostic(&ctx, ctx.budget.borrow().used());
            Ok(JoinedBatch::seal(
                captured_pin,
                destination,
                destination_header.revision.id.clone(),
                parts.descriptor_sha256.to_owned(),
                attempt,
                report,
            ))
        };
    let result = reset_for_destination(ctx.conn, destination).and_then(|attempt| {
        ctx.reserve_rows(RETAINED_SEED, retained_rows(ctx.conn)?)?;
        ctx.check_output()?;
        if shared {
            store.with_native_reader(captured_pin, |reader| body(attempt, reader, reader))
        } else {
            store.with_native_reader(captured_pin, |captured| {
                store.with_native_reader(destination, |reader| body(attempt, captured, reader))
            })
        }
    });
    *budget = ctx.budget.into_inner();
    {
        let mut join_counters = ctx.counters.borrow_mut();
        join_counters.join_reservation_high_water_bytes = ctx.live.high_water();
        join_counters.staged_row_attempts = ctx
            .ledger
            .borrow()
            .iter()
            .map(|(owner, rows)| ((*owner).to_owned(), *rows))
            .collect();
    }
    counters.absorb_join(&ctx.counters.borrow());
    result
}
