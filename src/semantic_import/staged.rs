//! The staged join backend: claims, native facts, global indexes and output rows live in
//! `stage.db`, and the native facts of one document at a time are served from a byte-bounded
//! LRU of document groups. The binding, ambiguity and rejoin rules are the ones the in-memory
//! backend runs (`join_inventory_indexed`); this module only supplies the storage.
use super::{
    CoordinateMap, DestinationMutations, MeasuredFacts, RoleSites, ScopedKey, compose_join,
    document_coordinates, join_inventory_indexed,
    join_store::{
        BasisFor, Bytes, JoinFacts, JoinIndex, JoinSink, JoinStore, OccurrenceRow, ProofIdentities,
        ProofRef, RelationshipRow,
    },
    mapped_range, occurrence_endpoints, rejoin_provenance_id, source_role_sites,
};
use crate::{
    model::{CancelFlag, IndexPin},
    native_evidence as native,
    scip_raw::{CapturedClaim, CapturedScipInventory, ScipCaptureError},
    semantic_batch::{
        AdmittedBatch, BatchCounters, BatchLimits, JoinedBatch, LookupBudget,
        descriptor::{Descriptor, SourceManifestHasher, basis_artifact_ordinal},
        staging::{JOIN_TABLES, claim_from_stage},
    },
    semantic_evidence::{self as semantic, EmissionHeader, EmissionValidator, RawClaimDocument},
    store::{NativeHeader, NativeReader, Store},
};
use anyhow::{Context, Result, anyhow};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet, VecDeque},
    rc::{Rc, Weak},
    sync::atomic::Ordering,
};

const CLAIM_PAGE: usize = 128;
const DOCUMENT_SQL: &str =
    "SELECT ord, language, path FROM nat_docs WHERE rev=?1 AND ord>?2 ORDER BY ord LIMIT 128";
const DECLARATIONS_SQL: &str = "SELECT owner_language, owner_path, syntax_id FROM ix_declarations \
     WHERE side=?1 AND symbol=?2 AND local_language=?3 AND local_path=?4 \
     ORDER BY owner_language, owner_path, syntax_id";
const PROVENANCE_SQL: &str = "SELECT raw_id, kind FROM w_prov ORDER BY raw_id, kind";
const REFERENCES_SQL: &str = "SELECT ref_id, body FROM w_refrows ORDER BY ref_id";
const RESOLVE_CALLS_SQL: &str = "SELECT raw_id, body FROM w_calls ORDER BY ord";
const CALLS_SQL: &str = "SELECT raw_id, body FROM w_callres ORDER BY ord";
const SITE_TARGETS_SQL: &str =
    "SELECT call_id, canon, target FROM w_sitetargets ORDER BY call_id, canon";
const REFERENCE_CLAIMS_SQL: &str =
    "SELECT proof_id, body FROM f_refclaims WHERE side=1 ORDER BY proof_id";
const SYMBOLS_SQL: &str = "SELECT body FROM f_symbols WHERE side=1 ORDER BY ord";
const BINDINGS_SQL: &str = "SELECT body FROM f_bindings WHERE side=1 ORDER BY ord";

/// The ordered cursor queries of the join, for plan tests.
#[cfg(feature = "test-support")]
pub(crate) fn cursor_sql() -> Vec<String> {
    let mut queries = vec![
        claim_page_sql(false),
        claim_page_sql(true),
        DOCUMENT_SQL.to_owned(),
        DECLARATIONS_SQL.to_owned(),
        PROVENANCE_SQL.to_owned(),
        REFERENCES_SQL.to_owned(),
        RESOLVE_CALLS_SQL.to_owned(),
        SITE_TARGETS_SQL.to_owned(),
        CALLS_SQL.to_owned(),
        REFERENCE_CLAIMS_SQL.to_owned(),
        SYMBOLS_SQL.to_owned(),
        BINDINGS_SQL.to_owned(),
    ];
    queries.push(
        "SELECT body FROM ix_relationships WHERE side=?1 AND language=?2 AND path=?3 ORDER BY ord"
            .into(),
    );
    queries
}

fn claim_page_sql(filtered: bool) -> String {
    format!(
        "{} WHERE (c.source_ord, c.canonical_bytes, c.claim_id) > (?1, ?2, ?3) \
         ORDER BY c.source_ord, c.canonical_bytes, c.claim_id LIMIT {CLAIM_PAGE}",
        claim_select(filtered)
    )
}

fn to_json<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

fn from_json<T: DeserializeOwned>(text: &str) -> Result<T> {
    Ok(serde_json::from_str(text)?)
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
    counters: RefCell<BatchCounters>,
    cache: RefCell<DocCache>,
    rows: Cell<u64>,
    /// The stage file size when the join started, the base of the output-bytes meter.
    stage_base: u64,
    mutations: DestinationMutations,
    cancel: CancelFlag,
}

impl JoinCtx<'_> {
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

    /// Accounts one staged join row.
    fn row(&self) -> Result<()> {
        let rows = self.rows.get() + 1;
        self.rows.set(rows);
        if rows > self.limits.staged_rows {
            return Err(ScipCaptureError::LimitExceeded("staged rows".into()).into());
        }
        if rows.is_multiple_of(OUTPUT_CHECK_ROWS) {
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

/// Rows between output-bytes checks.
const OUTPUT_CHECK_ROWS: u64 = 10_000;

fn stage_bytes(conn: &Connection) -> Result<u64> {
    let pages: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    let size: i64 = conn.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    Ok(u64::try_from(pages)?.saturating_mul(u64::try_from(size)?))
}

type GroupKey = (u8, i64);

/// Document groups: one active, the rest in an LRU bounded by `document_cache_bytes`. The
/// LRU bound is on estimated bytes (`DocGroup::bytes`), not measured heap size. A group evicted
/// while a caller still holds it stays pinned until the caller drops it.
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
    source_ord: Option<i64>,
    text: Rc<str>,
    native: native::Artifact,
    declaration_ids: BTreeMap<String, Vec<usize>>,
    declaration_keys: BTreeMap<Vec<u8>, Vec<usize>>,
    declaration_names: BTreeSet<(usize, usize, String)>,
    call_ids: BTreeMap<String, Vec<usize>>,
    reference_ids: BTreeMap<String, Vec<usize>>,
    candidates: BTreeMap<(usize, usize, semantic::JoinKind), BTreeSet<String>>,
    support: BTreeMap<semantic::JoinKind, native::MeasurementSupport>,
    bytes: u64,
    coordinates: RefCell<Option<BTreeMap<u8, CoordinateMap>>>,
    symbol_kinds: RefCell<Option<BTreeMap<String, Option<i64>>>>,
}

impl DocGroup {
    fn build(
        header: &NativeHeader,
        document: native::Document,
        source_ord: Option<i64>,
        rows: crate::store::NativeDocumentRows,
    ) -> Result<Self> {
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
        let (candidates, support) = {
            let facts = MeasuredFacts::new(&mini, &sources)?;
            facts.into_anchor_parts(&key)
        };
        let bytes = sources.into_values().next().expect("one source");
        let text: Rc<str> = Rc::from(String::from_utf8(bytes).context("non-UTF8 native source")?);
        let (declarations, calls, references) = (&mini.declarations, &mini.calls, &mini.references);
        let mut declaration_ids: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut declaration_keys: BTreeMap<Vec<u8>, Vec<usize>> = BTreeMap::new();
        let mut declaration_names = BTreeSet::new();
        for (index, d) in declarations.iter().enumerate() {
            declaration_ids
                .entry(d.syntax_id.clone())
                .or_default()
                .push(index);
            declaration_keys
                .entry(crate::native_ids::canonical(&serde_json::to_value(&d.key)?))
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
        let bytes = text.len() as u64
            + 512 * declarations.len() as u64
            + 256 * (calls.len() + references.len()) as u64
            + 128 * candidates.len() as u64
            + 1024;
        Ok(Self {
            document,
            source_ord,
            text,
            native: mini,
            declaration_ids,
            declaration_keys,
            declaration_names,
            call_ids,
            reference_ids,
            candidates,
            support,
            bytes,
            coordinates: RefCell::new(None),
            symbol_kinds: RefCell::new(None),
        })
    }

    fn pick<T: Clone>(rows: &[T], indexes: Option<&Vec<usize>>) -> Vec<Cow<'static, T>> {
        indexes
            .into_iter()
            .flatten()
            .map(|index| Cow::Owned(rows[*index].clone()))
            .collect()
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

fn claim_of_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, i64, ClaimParts)> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        (
            row.get::<_, Vec<u8>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
        ),
    ))
}

type ClaimParts = (Vec<u8>, String, String, String, String);

fn build_claim(parts: ClaimParts) -> Result<CapturedClaim> {
    let (canonical, digest, proof, artifact_hash, language) = parts;
    claim_from_stage(canonical, digest, proof, language, artifact_hash)
}

/// The claims in canonical order, read a page at a time.
struct ClaimCursor<'a> {
    conn: &'a Connection,
    filtered: bool,
    last: Option<(i64, Vec<u8>, i64)>,
    page: VecDeque<Result<CapturedClaim>>,
    done: bool,
}

impl<'c> Iterator for ClaimCursor<'c> {
    type Item = Result<Cow<'c, CapturedClaim>>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(item) = self.page.pop_front() {
                return Some(item.map(Cow::Owned));
            }
            if self.done {
                return None;
            }
            if let Err(error) = self.fill() {
                self.done = true;
                return Some(Err(error));
            }
        }
    }
}

impl ClaimCursor<'_> {
    fn fill(&mut self) -> Result<()> {
        let sql = claim_page_sql(self.filtered);
        let (ord, bytes, id) = self.last.clone().unwrap_or((-1, vec![], -1));
        let mut statement = self.conn.prepare_cached(&sql)?;
        let rows = statement
            .query_map(params![ord, bytes, id], claim_of_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if rows.len() < CLAIM_PAGE {
            self.done = true;
        }
        for (claim_id, source_ord, parts) in rows {
            self.last = Some((source_ord, parts.0.clone(), claim_id));
            self.page.push_back(build_claim(parts));
        }
        Ok(())
    }
}

/// The documents of one native revision in stored order, read a page at a time.
struct DocumentCursor<'a> {
    conn: &'a Connection,
    rev: u8,
    source_set_id: String,
    last: i64,
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
                let mut statement = self.conn.prepare_cached(DOCUMENT_SQL)?;
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

/// One revision's side of the join: the retained native revision, the claims (all of them, or
/// those of eligible documents), and the fact and index tables of its side.
pub(super) struct StagedSide<'a> {
    ctx: &'a JoinCtx<'a>,
    reader: &'a NativeReader<'a>,
    header: &'a NativeHeader,
    rev: u8,
    side: u8,
    filtered: bool,
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
    fn group(&self, document: &native::DocumentKey) -> Result<Option<Rc<DocGroup>>> {
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
        let mut rows = self.reader.document(&row)?;
        if self.rev == 1 {
            self.ctx
                .mutations
                .apply(&mut rows.declarations, &mut rows.references);
        }
        let source_ord: Option<i64> = self
            .ctx
            .conn
            .prepare_cached("SELECT ord FROM sources WHERE language=?1 AND path=?2")?
            .query_row(params![document.language, document.path], |r| r.get(0))
            .optional()?;
        let group = Rc::new(DocGroup::build(self.header, row, source_ord, rows)?);
        let mut cache = self.ctx.cache.borrow_mut();
        let mut counters = self.ctx.counters.borrow_mut();
        cache.activate(key, group.clone(), &mut counters);
        Ok(Some(group))
    }

    fn require_group(&self, document: &native::DocumentKey) -> Result<Rc<DocGroup>> {
        self.group(document)?.context("foreign anchor document")
    }

    fn document_claims(&self, group: &DocGroup) -> Result<Vec<CapturedClaim>> {
        self.ctx.charge(1)?;
        let Some(ord) = group.source_ord else {
            return Ok(vec![]);
        };
        let sql = format!(
            "{} WHERE c.source_ord=?1 ORDER BY c.canonical_bytes, c.claim_id",
            claim_select(false)
        );
        let mut statement = self.ctx.conn.prepare_cached(&sql)?;
        let rows = statement
            .query_map([ord], claim_of_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        self.ctx.charge(rows.len())?;
        rows.into_iter()
            .map(|(_, _, parts)| build_claim(parts))
            .collect()
    }

    fn proof_filter(&self) -> &'static str {
        if self.filtered { ELIGIBLE_JOIN } else { "" }
    }
}

impl JoinFacts for StagedSide<'_> {
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
        let ids: Vec<String> = group
            .candidates
            .get(&(range.start, range.end, kind))
            .into_iter()
            .flatten()
            .cloned()
            .collect();
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
            .group(document)?
            .map(|group| Bytes::Shared(group.text.clone())))
    }

    fn documents(&self) -> Box<dyn Iterator<Item = Result<native::DocumentKey>> + '_> {
        Box::new(DocumentCursor {
            conn: self.ctx.conn,
            rev: self.rev,
            source_set_id: self.header.source_set.id.clone(),
            last: -1,
            page: VecDeque::new(),
            done: false,
        })
    }

    fn document_declarations(
        &self,
        document: &native::DocumentKey,
    ) -> Result<Vec<Cow<'_, native::Declaration>>> {
        Ok(match self.group(document)? {
            Some(group) => group
                .native
                .declarations
                .iter()
                .cloned()
                .map(Cow::Owned)
                .collect(),
            None => vec![],
        })
    }

    fn declaration_of(
        &self,
        document: &native::DocumentKey,
        id: &str,
    ) -> Result<Option<Cow<'_, native::Declaration>>> {
        Ok(self.declarations_with_id(document, id)?.into_iter().next())
    }

    fn declarations_with_id(
        &self,
        document: &native::DocumentKey,
        id: &str,
    ) -> Result<Vec<Cow<'_, native::Declaration>>> {
        Ok(match self.group(document)? {
            Some(group) => {
                DocGroup::pick(&group.native.declarations, group.declaration_ids.get(id))
            }
            None => vec![],
        })
    }

    fn declarations_with_key(
        &self,
        document: &native::DocumentKey,
        key: &[u8],
    ) -> Result<Vec<Cow<'_, native::Declaration>>> {
        Ok(match self.group(document)? {
            Some(group) => {
                DocGroup::pick(&group.native.declarations, group.declaration_keys.get(key))
            }
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
        Ok(match self.group(document)? {
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
        Ok(self.calls_with_id(document, id)?.into_iter().next())
    }

    fn calls_with_id(
        &self,
        document: &native::DocumentKey,
        id: &str,
    ) -> Result<Vec<Cow<'_, native::Call>>> {
        Ok(match self.group(document)? {
            Some(group) => DocGroup::pick(&group.native.calls, group.call_ids.get(id)),
            None => vec![],
        })
    }

    fn reference_of(
        &self,
        document: &native::DocumentKey,
        id: &str,
    ) -> Result<Option<Cow<'_, native::Reference>>> {
        Ok(self.references_with_id(document, id)?.into_iter().next())
    }

    fn references_with_id(
        &self,
        document: &native::DocumentKey,
        id: &str,
    ) -> Result<Vec<Cow<'_, native::Reference>>> {
        Ok(match self.group(document)? {
            Some(group) => DocGroup::pick(&group.native.references, group.reference_ids.get(id)),
            None => vec![],
        })
    }
}

impl ProofIdentities for StagedSide<'_> {
    fn proof(&self, proof_id: &str) -> Result<Option<ProofRef>> {
        let sql = format!(
            "SELECT s.language, s.path, c.record_kind, c.fact_digest, a.sha256 FROM claims c \
             JOIN sources s ON s.ord=c.source_ord JOIN artifacts a ON a.ordinal=c.artifact {} WHERE c.proof_id=?1",
            self.proof_filter()
        );
        Ok(self
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
            .optional()?)
    }

    fn check_proof(&self, proof_id: &str, kind: &str, digest: &str) -> Result<()> {
        let sql = format!(
            "SELECT 1 FROM claims c {} WHERE c.proof_id=?1 AND c.record_kind=?2 AND c.fact_digest=?3",
            self.proof_filter()
        );
        let found = self
            .ctx
            .conn
            .prepare_cached(&sql)?
            .query_row(params![proof_id, kind, digest], |_| Ok(()))
            .optional()?;
        found.ok_or_else(|| ScipCaptureError::MissingProof.into())
    }
}

impl<'a> JoinStore for StagedSide<'a> {
    type Restricted<'i>
        = StagedSide<'i>
    where
        Self: 'i;
    type Index = StagedIndex<'a>;

    fn restrict<'i>(&'i self, _claims: &'i CapturedScipInventory) -> Result<StagedSide<'i>> {
        Err(anyhow!(
            "a staged store is restricted by document eligibility"
        ))
    }

    fn new_index(&self) -> StagedIndex<'a> {
        StagedIndex {
            ctx: self.ctx,
            side: self.side,
            source_set_id: self.header.source_set.id.clone(),
            occurrences: 0,
            relationships: 0,
            endpoints: 0,
            reference_claims: 0,
            methods: 0,
        }
    }

    fn claims(&self) -> Box<dyn Iterator<Item = Result<Cow<'_, CapturedClaim>>> + '_> {
        Box::new(ClaimCursor {
            conn: self.ctx.conn,
            filtered: self.filtered,
            last: None,
            page: VecDeque::new(),
            done: false,
        })
    }

    fn lookup_proof(&self, proof: &str, kind: &str, digest: &str) -> Result<()> {
        self.check_proof(proof, kind, digest)
    }

    fn claim(&self, proof: &str, kind: &str, digest: &str) -> Result<Cow<'_, CapturedClaim>> {
        let sql = format!(
            "{} WHERE c.proof_id=?1 AND c.record_kind=?2 AND c.fact_digest=?3",
            claim_select(self.filtered)
        );
        let parts = self
            .ctx
            .conn
            .prepare_cached(&sql)?
            .query_row(params![proof, kind, digest], claim_of_row)
            .optional()?
            .ok_or(ScipCaptureError::MissingProof)?
            .2;
        Ok(Cow::Owned(build_claim(parts)?))
    }

    fn symbol_kind(&self, language: &str, path: &str, symbol: &str) -> Result<Option<i64>> {
        let document = native::DocumentKey {
            source_set_id: self.header.source_set.id.clone(),
            language: language.to_owned(),
            path: path.to_owned(),
        };
        let group = self.require_group(&document)?;
        if group.symbol_kinds.borrow().is_none() {
            let mut kinds = BTreeMap::new();
            for claim in self.document_claims(&group)? {
                if claim.envelope.record_kind == "symbolInformation"
                    && let Some(name) = claim.envelope.raw["symbol"].as_str()
                {
                    kinds
                        .entry(name.to_owned())
                        .or_insert_with(|| claim.envelope.raw["kind"].as_i64());
                }
            }
            *group.symbol_kinds.borrow_mut() = Some(kinds);
        }
        Ok(group
            .symbol_kinds
            .borrow()
            .as_ref()
            .and_then(|kinds| kinds.get(symbol).copied())
            .flatten())
    }

    fn occurrence_range(
        &self,
        claim: &CapturedClaim,
        admitted_encoding: u8,
    ) -> Result<native::Range> {
        let (encoding, endpoints) = occurrence_endpoints(claim, admitted_encoding)?;
        let document = native::DocumentKey {
            source_set_id: self.header.source_set.id.clone(),
            language: claim.language.clone(),
            path: claim.envelope.relative_path.clone(),
        };
        let group = self.require_group(&document)?;
        if group.coordinates.borrow().is_none() {
            let claims = self.document_claims(&group)?;
            let maps = document_coordinates(&claims, &group.text, admitted_encoding)?;
            *group.coordinates.borrow_mut() = Some(maps);
        }
        let coordinates = group.coordinates.borrow();
        let offsets = coordinates
            .as_ref()
            .and_then(|maps| maps.get(&encoding))
            .context("missing SCIP coordinate map")?;
        mapped_range(claim, endpoints, offsets)
    }

    fn role_sites(&self, document: &native::DocumentKey) -> Result<RoleSites> {
        #[cfg(feature = "test-support")]
        super::note_parsed_role_site_document(document);
        let group = self.require_group(document)?;
        source_role_sites(document.language.as_str(), group.text.as_bytes())
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

/// The global join indexes of one side, as tables keyed on the lookup key.
pub(super) struct StagedIndex<'a> {
    ctx: &'a JoinCtx<'a>,
    side: u8,
    source_set_id: String,
    occurrences: usize,
    relationships: i64,
    endpoints: i64,
    reference_claims: i64,
    methods: i64,
}

impl StagedIndex<'_> {
    fn insert(&self, sql: &str, values: &[&dyn rusqlite::ToSql]) -> Result<()> {
        self.ctx.row()?;
        self.ctx.conn.prepare_cached(sql)?.execute(values)?;
        Ok(())
    }

    fn rows<T>(
        &self,
        sql: &str,
        values: &[&dyn rusqlite::ToSql],
        map: impl Fn(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Vec<T>> {
        self.ctx.charge(1)?;
        let mut statement = self.ctx.conn.prepare_cached(sql)?;
        let mut rows = statement.query(values)?;
        let mut items = vec![];
        while let Some(row) = rows.next()? {
            self.ctx.charge(1)?;
            items.push(map(row)?);
        }
        Ok(items)
    }
}

impl JoinIndex for StagedIndex<'_> {
    fn put_declaration(
        &mut self,
        key: ScopedKey,
        document: native::DocumentKey,
        syntax_id: String,
    ) -> Result<()> {
        let (local_language, local_path) = local_columns(&key);
        self.insert(
            "INSERT OR IGNORE INTO ix_declarations VALUES(?1,?2,?3,?4,?5,?6,?7)",
            &[
                &self.side,
                &key.0,
                &local_language,
                &local_path,
                &document.language,
                &document.path,
                &syntax_id,
            ],
        )
    }

    fn declarations(&self, key: &ScopedKey) -> Result<Vec<(native::DocumentKey, String)>> {
        let (local_language, local_path) = local_columns(key);
        let set = &self.source_set_id;
        self.rows(
            DECLARATIONS_SQL,
            &[&self.side, &key.0, &local_language, &local_path],
            |row| {
                Ok((
                    native::DocumentKey {
                        source_set_id: set.clone(),
                        language: row.get(0)?,
                        path: row.get(1)?,
                    },
                    row.get(2)?,
                ))
            },
        )
    }

    fn put_type_use(
        &mut self,
        document: &native::DocumentKey,
        start: usize,
        end: usize,
        symbol: &str,
    ) -> Result<()> {
        self.insert(
            "INSERT OR IGNORE INTO ix_type_uses VALUES(?1,?2,?3,?4,?5,?6)",
            &[
                &self.side,
                &document.language,
                &document.path,
                &(start as i64),
                &(end as i64),
                &symbol,
            ],
        )
    }

    fn type_uses(
        &self,
        document: &native::DocumentKey,
        start: usize,
        end: usize,
    ) -> Result<BTreeSet<String>> {
        Ok(self
            .rows(
                "SELECT symbol FROM ix_type_uses WHERE side=?1 AND language=?2 AND path=?3 AND start=?4 AND end=?5",
                &[
                    &self.side,
                    &document.language,
                    &document.path,
                    &(start as i64),
                    &(end as i64),
                ],
                |row| row.get::<_, String>(0),
            )?
            .into_iter()
            .collect())
    }

    fn put_occurrence(&mut self, row: OccurrenceRow) -> Result<()> {
        let body = to_json(&row)?;
        self.insert(
            "INSERT INTO ix_occurrences VALUES(?1,?2,?3)",
            &[&self.side, &(self.occurrences as i64), &body],
        )?;
        self.occurrences += 1;
        Ok(())
    }

    fn occurrence_count(&self) -> usize {
        self.occurrences
    }

    fn occurrence(&self, ordinal: usize) -> Result<OccurrenceRow> {
        let body: String = self
            .ctx
            .conn
            .prepare_cached("SELECT body FROM ix_occurrences WHERE side=?1 AND ord=?2")?
            .query_row(params![self.side, ordinal as i64], |row| row.get(0))
            .context("missing occurrence row")?;
        from_json(&body)
    }

    fn put_relationship(&mut self, language: &str, path: &str, row: RelationshipRow) -> Result<()> {
        let body = to_json(&row)?;
        self.insert(
            "INSERT INTO ix_relationships VALUES(?1,?2,?3,?4,?5)",
            &[&self.side, &language, &path, &self.relationships, &body],
        )?;
        self.relationships += 1;
        Ok(())
    }

    fn relationships(&self, language: &str, path: &str) -> Result<Vec<RelationshipRow>> {
        self.rows(
            "SELECT body FROM ix_relationships WHERE side=?1 AND language=?2 AND path=?3 ORDER BY ord",
            &[&self.side, &language, &path],
            |row| row.get::<_, String>(0),
        )?
        .iter()
        .map(|body| from_json(body))
        .collect()
    }

    fn put_relationship_endpoint(
        &mut self,
        language: &str,
        path: &str,
        source: &str,
        target: &str,
        row: RelationshipRow,
    ) -> Result<()> {
        let body = to_json(&row)?;
        self.insert(
            "INSERT INTO ix_relationship_endpoints VALUES(?1,?2,?3,?4,?5,?6,?7)",
            &[
                &self.side,
                &language,
                &path,
                &source,
                &target,
                &self.endpoints,
                &body,
            ],
        )?;
        self.endpoints += 1;
        Ok(())
    }

    fn relationship_endpoints(
        &self,
        language: &str,
        path: &str,
        source: &str,
        target: &str,
    ) -> Result<Vec<RelationshipRow>> {
        self.rows(
            "SELECT body FROM ix_relationship_endpoints WHERE side=?1 AND language=?2 AND path=?3 \
             AND source=?4 AND target=?5 ORDER BY ord",
            &[&self.side, &language, &path, &source, &target],
            |row| row.get::<_, String>(0),
        )?
        .iter()
        .map(|body| from_json(body))
        .collect()
    }

    fn put_reference_claim(
        &mut self,
        reference_id: &str,
        proof_id: &str,
        target: semantic::Target,
    ) -> Result<()> {
        let target = to_json(&target)?;
        self.insert(
            "INSERT INTO ix_reference_claims VALUES(?1,?2,?3,?4,?5)",
            &[
                &self.side,
                &reference_id,
                &self.reference_claims,
                &proof_id,
                &target,
            ],
        )?;
        self.reference_claims += 1;
        Ok(())
    }

    fn take_reference_claims(
        &mut self,
        reference_id: &str,
    ) -> Result<Vec<(String, semantic::Target)>> {
        let rows = self.rows(
            "SELECT proof_id, target FROM ix_reference_claims WHERE side=?1 AND reference_id=?2 ORDER BY ord",
            &[&self.side, &reference_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        self.ctx
            .conn
            .prepare_cached("DELETE FROM ix_reference_claims WHERE side=?1 AND reference_id=?2")?
            .execute(params![self.side, reference_id])?;
        rows.into_iter()
            .map(|(proof, target)| Ok((proof, from_json(&target)?)))
            .collect()
    }

    fn put_call_reference(&mut self, proof_id: &str, reference_id: &str) -> Result<()> {
        self.insert(
            "INSERT OR REPLACE INTO ix_call_references VALUES(?1,?2,?3)",
            &[&self.side, &proof_id, &reference_id],
        )
    }

    fn call_reference(&self, proof_id: &str) -> Result<Option<String>> {
        Ok(self
            .ctx
            .conn
            .prepare_cached(
                "SELECT reference_id FROM ix_call_references WHERE side=?1 AND proof_id=?2",
            )?
            .query_row(params![self.side, proof_id], |row| row.get(0))
            .optional()?)
    }

    fn put_symbol(&mut self, symbol: &str, declared: bool, information: bool) -> Result<()> {
        self.insert(
            "INSERT INTO ix_symbols VALUES(?1,?2,?3,?4) ON CONFLICT(side, symbol) DO UPDATE SET \
             declared=declared|excluded.declared, information=information|excluded.information",
            &[&self.side, &symbol, &declared, &information],
        )
    }

    fn is_declared_symbol(&self, symbol: &str) -> Result<bool> {
        Ok(self
            .ctx
            .conn
            .prepare_cached("SELECT declared FROM ix_symbols WHERE side=?1 AND symbol=?2")?
            .query_row(params![self.side, symbol], |row| row.get::<_, bool>(0))
            .optional()?
            .unwrap_or(false))
    }

    fn is_information_symbol(&self, symbol: &str) -> Result<bool> {
        Ok(self
            .ctx
            .conn
            .prepare_cached("SELECT information FROM ix_symbols WHERE side=?1 AND symbol=?2")?
            .query_row(params![self.side, symbol], |row| row.get::<_, bool>(0))
            .optional()?
            .unwrap_or(false))
    }

    fn put_method(
        &mut self,
        document: &native::DocumentKey,
        symbol: &str,
        method: &native::Declaration,
    ) -> Result<()> {
        let body = to_json(method)?;
        self.insert(
            "INSERT INTO ix_methods VALUES(?1,0,?2,?3,?4,?5,?6)",
            &[
                &self.side,
                &symbol,
                &document.language,
                &document.path,
                &self.methods,
                &body,
            ],
        )?;
        let key = (
            symbol.to_owned(),
            symbol.starts_with("local ").then(|| document.clone()),
        );
        let (local_language, local_path) = local_columns(&key);
        self.insert(
            "INSERT INTO ix_methods VALUES(?1,1,?2,?3,?4,?5,?6)",
            &[
                &self.side,
                &symbol,
                &local_language,
                &local_path,
                &self.methods,
                &body,
            ],
        )?;
        self.methods += 1;
        Ok(())
    }

    fn source_methods(
        &self,
        document: &native::DocumentKey,
        symbol: &str,
    ) -> Result<Vec<native::Declaration>> {
        self.rows(
            "SELECT body FROM ix_methods WHERE side=?1 AND kind=0 AND key_a=?2 AND key_b=?3 AND key_c=?4 ORDER BY ord",
            &[&self.side, &symbol, &document.language, &document.path],
            |row| row.get::<_, String>(0),
        )?
        .iter()
        .map(|body| from_json(body))
        .collect()
    }

    fn target_methods(
        &self,
        document: &native::DocumentKey,
        symbol: &str,
    ) -> Result<Vec<native::Declaration>> {
        let key = (
            symbol.to_owned(),
            symbol.starts_with("local ").then(|| document.clone()),
        );
        let (local_language, local_path) = local_columns(&key);
        self.rows(
            "SELECT body FROM ix_methods WHERE side=?1 AND kind=1 AND key_a=?2 AND key_b=?3 AND key_c=?4 ORDER BY ord",
            &[&self.side, &symbol, &local_language, &local_path],
            |row| row.get::<_, String>(0),
        )?
        .iter()
        .map(|body| from_json(body))
        .collect()
    }
}

/// The join's facts for one side as rows in `f_*`; later passes revise them by slot.
pub(super) struct StagedFactSink<'a> {
    ctx: &'a JoinCtx<'a>,
    side: u8,
    symbols: i64,
    bindings: i64,
    references: i64,
    calls: i64,
    relationships: i64,
}

impl<'a> StagedFactSink<'a> {
    pub(super) fn new(ctx: &'a JoinCtx<'a>, side: u8) -> Self {
        Self {
            ctx,
            side,
            symbols: 0,
            bindings: 0,
            references: 0,
            calls: 0,
            relationships: 0,
        }
    }

    fn execute(&self, sql: &str, values: &[&dyn rusqlite::ToSql]) -> Result<()> {
        self.ctx.row()?;
        self.ctx.conn.prepare_cached(sql)?.execute(values)?;
        Ok(())
    }

    fn body(&self, sql: &str, values: &[&dyn rusqlite::ToSql]) -> Result<String> {
        Ok(self
            .ctx
            .conn
            .prepare_cached(sql)?
            .query_row(values, |row| row.get(0))?)
    }
}

impl JoinSink for StagedFactSink<'_> {
    fn symbol(&mut self, row: semantic::Symbol) -> Result<()> {
        let body = to_json(&row)?;
        self.execute(
            "INSERT INTO f_symbols VALUES(?1,?2,?3)",
            &[&self.side, &self.symbols, &body],
        )?;
        self.symbols += 1;
        Ok(())
    }

    fn declaration_binding(&mut self, row: semantic::DeclarationBinding) -> Result<()> {
        let body = to_json(&row)?;
        self.execute(
            "INSERT INTO f_bindings VALUES(?1,?2,?3,?4,?5)",
            &[
                &self.side,
                &self.bindings,
                &row.provenance_id,
                &row.syntax_id,
                &body,
            ],
        )?;
        self.bindings += 1;
        Ok(())
    }

    fn join(&mut self, _proof_id: &str, _join: semantic::Join) -> Result<()> {
        Ok(())
    }

    fn reference_claim(&mut self, proof_id: &str, row: semantic::Reference) -> Result<()> {
        let body = to_json(&row)?;
        self.execute(
            "INSERT OR REPLACE INTO f_refclaims VALUES(?1,?2,?3)",
            &[&self.side, &proof_id, &body],
        )
    }

    fn reference_slot(&self, reference_id: &str) -> Result<Option<usize>> {
        Ok(self
            .ctx
            .conn
            .prepare_cached("SELECT slot FROM f_refs WHERE side=?1 AND ref_id=?2")?
            .query_row(params![self.side, reference_id], |row| row.get::<_, i64>(0))
            .optional()?
            .map(|slot| slot as usize))
    }

    fn reference_count(&self) -> usize {
        self.references as usize
    }

    fn reference(&self, slot: usize) -> Result<semantic::Reference> {
        from_json(&self.body(
            "SELECT body FROM f_refs WHERE side=?1 AND slot=?2",
            &[&self.side, &(slot as i64)],
        )?)
    }

    fn put_reference(&mut self, slot: Option<usize>, row: semantic::Reference) -> Result<usize> {
        let body = to_json(&row)?;
        match slot {
            Some(slot) => {
                self.ctx
                    .conn
                    .prepare_cached("UPDATE f_refs SET body=?3 WHERE side=?1 AND slot=?2")?
                    .execute(params![self.side, slot as i64, body])?;
                Ok(slot)
            }
            None => {
                let slot = self.references;
                self.execute(
                    "INSERT INTO f_refs VALUES(?1,?2,?3,?4)",
                    &[&self.side, &slot, &row.id, &body],
                )?;
                self.references += 1;
                Ok(slot as usize)
            }
        }
    }

    fn call_binding_count(&self) -> usize {
        self.calls as usize
    }

    fn call_binding(&self, slot: usize) -> Result<semantic::CallBinding> {
        from_json(&self.body(
            "SELECT body FROM f_calls WHERE side=?1 AND slot=?2",
            &[&self.side, &(slot as i64)],
        )?)
    }

    fn put_call_binding(
        &mut self,
        slot: Option<usize>,
        row: semantic::CallBinding,
    ) -> Result<usize> {
        let body = to_json(&row)?;
        match slot {
            Some(slot) => {
                self.ctx
                    .conn
                    .prepare_cached("UPDATE f_calls SET body=?3 WHERE side=?1 AND slot=?2")?
                    .execute(params![self.side, slot as i64, body])?;
                Ok(slot)
            }
            None => {
                let slot = self.calls;
                self.execute(
                    "INSERT INTO f_calls VALUES(?1,?2,?3,?4,?5)",
                    &[&self.side, &slot, &row.provenance_id, &row.call_id, &body],
                )?;
                self.calls += 1;
                Ok(slot as usize)
            }
        }
    }

    fn type_relationship(&mut self, row: semantic::TypeRelationship) -> Result<()> {
        let body = to_json(&row)?;
        self.execute(
            "INSERT INTO f_rels VALUES(?1,?2,?3,?4)",
            &[&self.side, &self.relationships, &row.provenance_id, &body],
        )?;
        self.relationships += 1;
        Ok(())
    }

    fn diagnostic(&mut self, document: &native::DocumentKey, code: &str) -> Result<()> {
        self.execute(
            "INSERT OR IGNORE INTO f_diag VALUES(?1,?2,?3,?4)",
            &[&self.side, &document.language, &document.path, &code],
        )
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

fn kind_rank(kind: semantic::EvidenceKind) -> i64 {
    kind as i64
}

fn rank_kind(rank: i64) -> Result<semantic::EvidenceKind> {
    Ok(match rank {
        1 => semantic::EvidenceKind::DeclarationBinding,
        2 => semantic::EvidenceKind::SemanticReference,
        3 => semantic::EvidenceKind::TypeRelationship,
        _ => anyhow::bail!("invalid staged evidence kind"),
    })
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

/// Streams the revision's inventory once into `nat_docs` and `nat_declarations`: the identity
/// and name checks the rejoin needs about declarations of documents it is not reading.
fn native_pass(ctx: &JoinCtx<'_>, reader: &NativeReader<'_>, rev: u8, mutate: bool) -> Result<()> {
    let conn = ctx.conn;
    if exists(conn, "SELECT 1 FROM nat_done WHERE rev=?1", &[&rev])? {
        return Ok(());
    }
    conn.execute_batch("BEGIN")?;
    let outcome = (|| -> Result<()> {
        let mut stream = reader.inventory()?;
        let mut ord = 0i64;
        while let Some(document) = stream.next_document()? {
            ctx.check_cancel()?;
            ctx.row()?;
            conn.prepare_cached("INSERT INTO nat_docs VALUES(?1,?2,?3,?4,?5,?6)")?
                .execute(params![
                    rev,
                    ord,
                    document.key.language,
                    document.key.path,
                    document.content_hash,
                    document.byte_length as i64
                ])?;
            let mut rows = reader.document(&document)?;
            if mutate {
                ctx.mutations
                    .apply(&mut rows.declarations, &mut rows.references);
            }
            let text = std::str::from_utf8(&rows.source).context("non-UTF8 native source")?;
            for declaration in &rows.declarations {
                ctx.row()?;
                let name_ok = declaration.name_range.as_ref().is_some_and(|range| {
                    text.get(range.start..range.end) == declaration.name.as_deref()
                });
                conn.prepare_cached("INSERT INTO nat_declarations VALUES(?1,?2,?3,?4,?5,?6)")?
                    .execute(params![
                        rev,
                        ord,
                        declaration.syntax_id,
                        declaration.name_range.as_ref().map(|r| r.start as i64),
                        declaration.name_range.as_ref().map(|r| r.end as i64),
                        name_ok
                    ])?;
            }
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
            for table in ["nat_docs", "nat_declarations", "nat_done"] {
                let _ = conn.execute(&format!("DELETE FROM {table} WHERE rev=?1"), [rev]);
            }
            Err(error)
        }
    }
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
        for table in ["nat_docs", "nat_declarations", "nat_done"] {
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
    let mut total = 0i64;
    for table in ["nat_docs", "nat_declarations"] {
        total += conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get::<_, i64>(0)
        })?;
    }
    Ok(total as u64)
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

    fn next_ord(&self, table: usize) -> i64 {
        let mut counts = self.emitted.get();
        let ord = counts[table];
        counts[table] += 1;
        self.emitted.set(counts);
        ord
    }

    fn key(&self, language: &str, path: &str) -> native::DocumentKey {
        native::DocumentKey {
            source_set_id: self.captured.header.source_set.id.clone(),
            language: language.to_owned(),
            path: path.to_owned(),
        }
    }

    fn mark(&self, column: &str, language: &str, path: &str) -> Result<()> {
        self.conn()
            .prepare_cached(&format!(
                "UPDATE w_docs SET {column}=1 WHERE source_ord=(SELECT ord FROM sources WHERE language=?1 AND path=?2)"
            ))?
            .execute(params![language, path])?;
        Ok(())
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

    /// The document ordinal and name facts of one declaration in a revision.
    fn declaration_status(
        &self,
        rev: u8,
        document: &native::DocumentKey,
        syntax_id: &str,
    ) -> Result<Vec<(bool, bool)>> {
        self.ctx.charge(1)?;
        let mut statement = self.conn().prepare_cached(
            "SELECT n.name_start IS NOT NULL, n.name_ok FROM nat_declarations n JOIN nat_docs d \
             ON d.rev=n.rev AND d.ord=n.doc_ord WHERE n.rev=?1 AND d.language=?2 AND d.path=?3 AND n.syntax_id=?4",
        )?;
        Ok(statement
            .query_map(
                params![rev, document.language, document.path, syntax_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn captured_has(&self, sql: &str, proof: &str) -> Result<bool> {
        exists(self.conn(), sql, &[&proof])
    }

    fn captured_association(&self, proof: &str) -> Result<bool> {
        for sql in [
            "SELECT 1 FROM f_bindings WHERE side=0 AND provenance_id=?1 AND syntax_id IS NOT NULL",
            "SELECT 1 FROM f_refclaims WHERE side=0 AND proof_id=?1",
            "SELECT 1 FROM f_calls WHERE side=0 AND provenance_id=?1 AND call_id IS NOT NULL",
            "SELECT 1 FROM f_rels WHERE side=0 AND provenance_id=?1",
        ] {
            if self.captured_has(sql, proof)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn captured_call(&self, proof: &str) -> Result<Option<semantic::CallBinding>> {
        let body: Option<String> = self
            .conn()
            .prepare_cached(
                "SELECT body FROM f_calls WHERE side=0 AND provenance_id=?1 AND call_id IS NOT NULL ORDER BY slot DESC LIMIT 1",
            )?
            .query_row([proof], |row| row.get(0))
            .optional()?;
        body.map(|body| from_json(&body)).transpose()
    }

    fn captured_reference(&self, proof: &str) -> Result<Option<semantic::Reference>> {
        let body: Option<String> = self
            .conn()
            .prepare_cached("SELECT body FROM f_refclaims WHERE side=0 AND proof_id=?1")?
            .query_row([proof], |row| row.get(0))
            .optional()?;
        body.map(|body| from_json(&body)).transpose()
    }

    /// Marks which captured documents are requested, failed, unsupported and eligible for the
    /// destination join.
    fn eligibility(&self) -> Result<()> {
        let conn = self.conn();
        let enc = self.admitted_encoding;
        conn.execute("INSERT INTO w_docs(source_ord) SELECT ord FROM sources", [])?;
        for document in self.captured.documents() {
            let document = document?;
            if !self
                .producer
                .languages
                .contains(&language_of(&document.language)?)
            {
                continue;
            }
            let reason: Option<String> = conn
                .prepare_cached(
                    "SELECT c.exclusion FROM sources s JOIN coverage c ON c.source_ord=s.ord WHERE s.language=?1 AND s.path=?2",
                )?
                .query_row(params![document.language, document.path], |row| row.get(0))
                .optional()?
                .flatten();
            if reason.as_deref() != Some("notRequested") {
                self.mark("requested", &document.language, &document.path)?;
            }
            if reason.as_deref() == Some("failed")
                || (reason.as_deref() != Some("notRequested") && !self.unchanged(&document)?)
            {
                self.mark("failed", &document.language, &document.path)?;
            }
        }
        for claim in self.captured.claims() {
            let claim = claim?;
            self.captured.charge(1)?;
            self.captured.lookup_proof(
                &claim.captured_proof_id,
                &claim.envelope.record_kind,
                &claim.fact_digest,
            )?;
            let doc = self.key(&claim.language, &claim.envelope.relative_path);
            self.mark("requested", &doc.language, &doc.path)?;
            self.captured
                .doc_row(&doc)?
                .context("captured claim outside source manifest")?;
            self.captured
                .bytes_of(&doc)?
                .context("missing captured claim source")?;
            if !self.unchanged(&doc)? {
                self.mark("failed", &doc.language, &doc.path)?;
                continue;
            }
            if claim.envelope.record_kind != "occurrence" {
                self.mark("eligible", &doc.language, &doc.path)?;
                continue;
            }
            let range = self.captured.occurrence_range(&claim, enc)?;
            if range.start == range.end {
                self.mark("eligible", &doc.language, &doc.path)?;
                continue;
            }
            let roles = claim.envelope.raw["symbolRoles"]
                .as_u64()
                .context("invalid raw roles")?;
            let proof = claim.captured_proof_id.as_str();
            let captured_declaration = self.captured_has(
                "SELECT 1 FROM f_bindings WHERE side=0 AND provenance_id=?1 AND syntax_id IS NOT NULL",
                proof,
            )?;
            let captured_reference = self.captured_reference(proof)?.is_some();
            let required = if captured_declaration || (!captured_reference && roles & 1 != 0) {
                semantic::JoinKind::DeclarationName
            } else {
                semantic::JoinKind::Reference
            };
            let first = self.captured.join(&doc, range.clone(), required)?;
            let second = self.destination.join(&doc, range, required)?;
            let authored = self.captured_association(proof)?;
            if !authored
                && first.status == semantic::JoinStatus::Unsupported
                && second.status == semantic::JoinStatus::Unsupported
            {
                conn.prepare_cached("INSERT OR IGNORE INTO w_unsupported_claims VALUES(?1)")?
                    .execute([proof])?;
                self.mark("unsupported", &doc.language, &doc.path)?;
                self.mark("eligible", &doc.language, &doc.path)?;
                continue;
            }
            if !authored
                && first.status == semantic::JoinStatus::Unmatched
                && second.status == semantic::JoinStatus::Unmatched
            {
                self.mark("eligible", &doc.language, &doc.path)?;
                continue;
            }
            let mut exact = first.status == semantic::JoinStatus::Exact
                && second.status == semantic::JoinStatus::Exact;
            if captured_reference && required != semantic::JoinKind::Reference {
                let first_reference = self.captured.join(
                    &doc,
                    first.anchor.range.clone(),
                    semantic::JoinKind::Reference,
                )?;
                let second_reference = self.destination.join(
                    &doc,
                    first.anchor.range.clone(),
                    semantic::JoinKind::Reference,
                )?;
                exact &= first_reference.status == semantic::JoinStatus::Exact
                    && second_reference.status == semantic::JoinStatus::Exact;
            }
            if let Some(binding) = self.captured_call(proof)? {
                let call = self.destination.join(
                    &doc,
                    binding.join.anchor.range.clone(),
                    binding.join.anchor.kind,
                )?;
                let first_call = self.captured.join(
                    &doc,
                    binding.join.anchor.range.clone(),
                    binding.join.anchor.kind,
                )?;
                exact &= call.status == semantic::JoinStatus::Exact
                    && first_call.status == semantic::JoinStatus::Exact
                    && first_call.candidate_ids == binding.join.candidate_ids;
                let mut same_call = false;
                if let Some(id) = second.candidate_ids.first() {
                    let calls = match call.candidate_ids.first() {
                        Some(call_id) => self.destination.calls_with_id(&doc, call_id)?,
                        None => vec![],
                    };
                    same_call =
                        self.destination
                            .references_with_id(&doc, id)?
                            .iter()
                            .any(|reference| {
                                &reference.id == id
                                    && calls.iter().any(|candidate| {
                                        call.candidate_ids == vec![candidate.id.clone()]
                                            && candidate.owner_syntax_id
                                                == reference.owner_syntax_id
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
            self.mark(
                if exact { "eligible" } else { "failed" },
                &doc.language,
                &doc.path,
            )?;
        }
        Ok(())
    }
}

impl StagedJoin<'_> {
    fn fill_captured_targets(&self) -> Result<()> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT body FROM f_bindings WHERE side=0 AND syntax_id IS NOT NULL ORDER BY ord",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let binding: semantic::DeclarationBinding = from_json(&row.get::<_, String>(0)?)?;
            let Some(syntax_id) = &binding.syntax_id else {
                continue;
            };
            let document = &binding.join.anchor.document;
            for symbol in &binding.symbols {
                let target = semantic::Target::Internal {
                    syntax_id: syntax_id.clone(),
                    document: document.clone(),
                    revision_id: self.captured_id().to_owned(),
                };
                self.ctx.row()?;
                conn.prepare_cached("INSERT OR IGNORE INTO w_cap_targets VALUES(?1,?2,?3,?4)")?
                    .execute(params![
                        document.language,
                        document.path,
                        symbol.symbol,
                        canonical_target(&target)?
                    ])?;
            }
        }
        Ok(())
    }

    fn captured_targets(
        &self,
        document: &native::DocumentKey,
        symbol: &str,
    ) -> Result<Vec<semantic::Target>> {
        self.ctx.charge(1)?;
        let mut statement = self.conn().prepare_cached(
            "SELECT target FROM w_cap_targets WHERE language=?1 AND path=?2 AND symbol=?3 ORDER BY target",
        )?;
        let rows = statement
            .query_map(params![document.language, document.path, symbol], |row| {
                row.get::<_, Vec<u8>>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        self.ctx.charge(rows.len())?;
        rows.iter()
            .map(|bytes| Ok(serde_json::from_slice(bytes)?))
            .collect()
    }

    /// Whether a rejoined target still names a measured, unchanged declaration.
    fn retarget(&self, target: &mut semantic::Target) -> Result<bool> {
        match target {
            semantic::Target::External { symbol }
                if exists(
                    self.conn(),
                    "SELECT 1 FROM w_cap_targets WHERE symbol=?1 LIMIT 1",
                    &[&symbol.symbol],
                )? =>
            {
                Ok(false)
            }
            semantic::Target::Internal {
                syntax_id,
                document,
                revision_id,
            } => {
                let original = self
                    .declaration_status(self.captured.rev, document, syntax_id)?
                    .iter()
                    .any(|(named, _)| *named);
                if !original {
                    return Ok(false);
                }
                let candidates: Vec<_> = self
                    .declaration_status(self.destination.rev, document, syntax_id)?
                    .into_iter()
                    .filter(|(named, _)| *named)
                    .collect();
                if candidates.len() != 1
                    || self.destination.doc_row(document)?.is_none()
                    || !candidates[0].1
                {
                    return Ok(false);
                }
                *revision_id = self.destination_id().to_owned();
                Ok(true)
            }
            _ => Ok(true),
        }
    }

    /// Runs the emission validator's row-local checks for the row stemming from raw proof
    /// `raw`: the proof is the captured claim's document and the facts are that document's
    /// destination group. `anchor` is the document the row itself names.
    fn check_row(
        &self,
        raw: &str,
        kind: semantic::EvidenceKind,
        anchor: Option<&native::DocumentKey>,
        check: impl for<'a> FnOnce(
            &'a EmissionValidator<'_>,
            &semantic::Provenance,
            &semantic::DocumentFacts<'a>,
        ) -> Result<()>,
    ) -> Result<()> {
        let claim = self
            .captured
            .proof(raw)?
            .context("dangling semantic fact provenance")?;
        let document = self.key(&claim.language, &claim.path);
        anyhow::ensure!(
            anchor.is_none_or(|anchor| anchor == &document),
            "semantic fact/provenance tuple mismatch"
        );
        let group = self
            .destination
            .group(&document)?
            .context("missing destination document")?;
        let proof = self.validator.fact_proof(
            &self.id_for(raw, kind),
            document,
            kind,
            &group.document.content_hash,
        );
        let source = group.text.as_bytes();
        let key = &group.document.key;
        let sources = |candidate: &native::DocumentKey| (candidate == key).then_some(source);
        let declaration = |id: &str| {
            group
                .declaration_ids
                .get(id)
                .and_then(|found| found.first())
                .map(|index| &group.native.declarations[*index])
        };
        let reference = |id: &str| {
            group
                .reference_ids
                .get(id)
                .and_then(|found| found.first())
                .map(|index| &group.native.references[*index])
        };
        let known = |id: &str, document: &native::DocumentKey| -> Result<bool> {
            Ok(!self
                .declaration_status(self.destination.rev, document, id)?
                .is_empty())
        };
        check(
            &self.validator,
            &proof,
            &semantic::DocumentFacts {
                native: &group.native,
                sources: &sources,
                declaration: &declaration,
                reference: &reference,
                known: &known,
            },
        )
    }

    fn id_for(&self, raw: &str, kind: semantic::EvidenceKind) -> String {
        rejoin_provenance_id(raw, self.destination_id(), kind)
    }

    fn source(&self, raw: &str, kind: semantic::EvidenceKind) -> Result<()> {
        self.ctx.row()?;
        self.conn()
            .prepare_cached("INSERT OR IGNORE INTO w_prov VALUES(?1,?2)")?
            .execute(params![raw, kind_rank(kind)])?;
        Ok(())
    }

    fn observe(&self, document: &native::DocumentKey, role: semantic::Role) -> Result<()> {
        self.conn()
            .prepare_cached("INSERT OR IGNORE INTO w_observed VALUES(?1,?2,?3)")?
            .execute(params![
                document.language,
                document.path,
                serde_json::to_value(role)?.as_str().unwrap_or_default()
            ])?;
        Ok(())
    }

    fn outcome(&self, raw: &str, outcome: i64) -> Result<()> {
        self.conn()
            .prepare_cached("INSERT OR REPLACE INTO w_outcomes VALUES(?1,?2)")?
            .execute(params![raw, outcome])?;
        Ok(())
    }

    fn omit(&self, language: &str, path: &str) -> Result<()> {
        self.mark("omitted", language, path)
    }

    fn proof_document(&self, raw: &str) -> Result<Option<native::DocumentKey>> {
        Ok(self
            .filtered
            .proof(raw)?
            .map(|claim| self.key(&claim.language, &claim.path)))
    }

    fn emit(&self, sql: &str, values: &[&dyn rusqlite::ToSql]) -> Result<()> {
        self.ctx.row()?;
        self.conn().prepare_cached(sql)?.execute(values)?;
        Ok(())
    }

    fn symbols(&self) -> Result<()> {
        let conn = self.conn();
        let mut statement = conn.prepare(SYMBOLS_SQL)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            self.ctx.check_cancel()?;
            let mut symbol: semantic::Symbol = from_json(&row.get::<_, String>(0)?)?;
            if let Some(claim) = self.filtered.proof(&symbol.provenance_id)? {
                let doc = self.key(&claim.language, &claim.path);
                let targets = self.captured_targets(&doc, &symbol.key.symbol)?;
                if !targets.is_empty() {
                    symbol.declarations = targets;
                }
            }
            let mut all = true;
            for target in &mut symbol.declarations {
                if !self.retarget(target)? {
                    all = false;
                    break;
                }
            }
            if all {
                let raw = symbol.provenance_id.clone();
                self.source(&raw, semantic::EvidenceKind::DeclarationBinding)?;
                symbol.provenance_id =
                    self.id_for(&raw, semantic::EvidenceKind::DeclarationBinding);
                #[cfg(feature = "test-support")]
                crate::semantic_batch::testing::edit_row("symbol", &mut symbol)?;
                self.check_row(
                    &raw,
                    semantic::EvidenceKind::DeclarationBinding,
                    None,
                    |v, p, f| v.symbol(&symbol, p, f),
                )?;
                let body = to_json(&symbol)?;
                self.emit(
                    "INSERT INTO out_symbols VALUES(?1,?2,?3)",
                    &[&self.next_ord(SYMBOLS), &symbol.provenance_id, &body],
                )?;
            } else if let Some(claim) = self.filtered.proof(&symbol.provenance_id)? {
                self.omit(&claim.language, &claim.path)?;
            }
        }
        Ok(())
    }

    fn bindings(&self) -> Result<()> {
        let conn = self.conn();
        let mut statement = conn.prepare(BINDINGS_SQL)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let mut binding: semantic::DeclarationBinding = from_json(&row.get::<_, String>(0)?)?;
            let Some(syntax_id) = binding.syntax_id.clone() else {
                continue;
            };
            let captured: Option<String> = conn
                .prepare_cached(
                    "SELECT syntax_id FROM f_bindings WHERE side=0 AND provenance_id=?1 AND syntax_id IS NOT NULL ORDER BY ord DESC LIMIT 1",
                )?
                .query_row([&binding.provenance_id], |row| row.get(0))
                .optional()?;
            if captured.as_deref() != Some(syntax_id.as_str()) {
                continue;
            }
            let raw = binding.provenance_id.clone();
            self.source(&raw, semantic::EvidenceKind::DeclarationBinding)?;
            self.outcome(&raw, 1)?;
            self.observe(&binding.join.anchor.document, semantic::Role::Definition)?;
            binding.provenance_id = self.id_for(&raw, semantic::EvidenceKind::DeclarationBinding);
            #[cfg(feature = "test-support")]
            crate::semantic_batch::testing::edit_row("declarationBinding", &mut binding)?;
            self.check_row(
                &raw,
                semantic::EvidenceKind::DeclarationBinding,
                Some(&binding.join.anchor.document),
                |v, p, f| v.declaration_binding(&binding, p, f),
            )?;
            let body = to_json(&binding)?;
            self.emit(
                "INSERT INTO out_declaration_bindings VALUES(?1,?2,?3)",
                &[&self.next_ord(BINDINGS), &binding.provenance_id, &body],
            )?;
        }
        Ok(())
    }

    /// Calls whose captured reference still retargets, held until references are final.
    fn stage_calls(&self) -> Result<()> {
        let conn = self.conn();
        let mut statement = conn.prepare("SELECT body FROM f_calls WHERE side=1 ORDER BY slot")?;
        let mut rows = statement.query([])?;
        let mut ord = 0i64;
        while let Some(row) = rows.next()? {
            let mut call: semantic::CallBinding = from_json(&row.get::<_, String>(0)?)?;
            let Some(old) = self.captured_reference(&call.provenance_id)? else {
                continue;
            };
            call.resolution = old.resolution;
            call.declared_target = old.declared_target.clone();
            call.candidates = old.candidates.clone();
            let mut all = true;
            for target in call
                .declared_target
                .iter_mut()
                .chain(call.candidates.iter_mut())
            {
                if !self.retarget(target)? {
                    all = false;
                    break;
                }
            }
            if all {
                call.stale_target = matches!(
                    call.declared_target,
                    Some(semantic::Target::Internal { .. })
                )
                .then_some(false);
                self.source(
                    &call.provenance_id,
                    semantic::EvidenceKind::SemanticReference,
                )?;
                self.ctx.row()?;
                conn.prepare_cached("INSERT INTO w_calls VALUES(?1,?2,?3)")?
                    .execute(params![ord, call.provenance_id, to_json(&call)?])?;
                ord += 1;
            } else {
                let document = &call.join.anchor.document;
                self.omit(&document.language, &document.path)?;
            }
        }
        Ok(())
    }

    /// Merges the destination's reference claims into one row per reference.
    fn reference_claims(&self) -> Result<()> {
        let conn = self.conn();
        let mut statement = conn.prepare(REFERENCE_CLAIMS_SQL)?;
        let mut rows = statement.query([])?;
        let mut target_ord = 0i64;
        while let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            let mut reference: semantic::Reference = from_json(&row.get::<_, String>(1)?)?;
            let Some(prior) = self.captured_reference(&raw)? else {
                continue;
            };
            anyhow::ensure!(
                prior.document == reference.document && prior.range == reference.range,
                "captured reference anchor differs"
            );
            reference.roles.retain(|role| prior.roles.contains(role));
            if reference.roles.is_empty() {
                continue;
            }
            reference.resolution = prior.resolution;
            reference.declared_target = prior.declared_target.clone();
            reference.candidates = prior.candidates.clone();
            let mut all = true;
            for target in reference
                .declared_target
                .iter_mut()
                .chain(reference.candidates.iter_mut())
            {
                if !self.retarget(target)? {
                    all = false;
                    break;
                }
            }
            if !all {
                self.omit(&reference.document.language, &reference.document.path)?;
                continue;
            }
            for target in reference
                .declared_target
                .iter()
                .chain(&reference.candidates)
            {
                self.ctx.row()?;
                conn.prepare_cached("INSERT INTO w_reftargets VALUES(?1,?2,?3)")?
                    .execute(params![reference.id, target_ord, to_json(target)?])?;
                target_ord += 1;
            }
            conn.prepare_cached("INSERT OR IGNORE INTO w_refproofs VALUES(?1,?2)")?
                .execute(params![reference.id, raw])?;
            conn.prepare_cached("INSERT OR REPLACE INTO w_accepted VALUES(?1,?2)")?
                .execute(params![raw, reference.id])?;
            self.source(&raw, semantic::EvidenceKind::SemanticReference)?;
            let stored: Option<String> = conn
                .prepare_cached("SELECT body FROM w_refrows WHERE ref_id=?1")?
                .query_row([&reference.id], |row| row.get(0))
                .optional()?;
            if let Some(stored) = stored {
                let mut row: semantic::Reference = from_json(&stored)?;
                let roles: BTreeSet<_> =
                    row.roles.iter().chain(&reference.roles).copied().collect();
                row.roles = roles.into_iter().collect();
                if row.roles.contains(&semantic::Role::Definition) {
                    row.site = semantic::ReferenceSite::Declaration;
                }
                conn.prepare_cached("UPDATE w_refrows SET body=?2 WHERE ref_id=?1")?
                    .execute(params![row.id, to_json(&row)?])?;
            } else {
                self.ctx.row()?;
                conn.prepare_cached("INSERT INTO w_refrows VALUES(?1,?2)")?
                    .execute(params![reference.id, to_json(&reference)?])?;
            }
        }
        Ok(())
    }

    fn references(&self) -> Result<()> {
        let conn = self.conn();
        let mut statement = conn.prepare(REFERENCES_SQL)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let mut reference: semantic::Reference = from_json(&row.get::<_, String>(1)?)?;
            let mut targets: Vec<semantic::Target> = {
                self.ctx.charge(1)?;
                let mut statement =
                    conn.prepare_cached("SELECT target FROM w_reftargets WHERE ref_id=?1")?;
                let found = statement
                    .query_map([&id], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                self.ctx.charge(found.len())?;
                found
                    .iter()
                    .map(|target| from_json(target))
                    .collect::<Result<_>>()?
            };
            targets.sort_by_key(|target| {
                crate::native_ids::canonical(
                    &serde_json::to_value(target).expect("serializable target"),
                )
            });
            targets.dedup();
            reference.declared_target = None;
            reference.candidates.clear();
            if targets.len() > 1 {
                reference.resolution = semantic::Resolution::Ambiguous;
                reference.candidates = targets;
            } else if let Some(target) = targets.pop() {
                reference.resolution = if matches!(target, semantic::Target::Internal { .. }) {
                    semantic::Resolution::Resolved
                } else {
                    semantic::Resolution::External
                };
                reference.declared_target = Some(target);
            } else {
                reference.resolution = semantic::Resolution::Unresolved;
            }
            let outcome = if reference.resolution == semantic::Resolution::Ambiguous {
                3
            } else {
                1
            };
            let proofs = {
                self.ctx.charge(1)?;
                let mut statement =
                    conn.prepare_cached("SELECT raw_id FROM w_refproofs WHERE ref_id=?1")?;
                let found = statement
                    .query_map([&id], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                self.ctx.charge(found.len())?;
                found
            };
            self.outcome(&reference.provenance_id, outcome)?;
            for proof in proofs {
                self.outcome(&proof, outcome)?;
            }
            for role in &reference.roles {
                self.observe(&reference.document, *role)?;
            }
            let raw = reference.provenance_id.clone();
            reference.provenance_id = self.id_for(&raw, semantic::EvidenceKind::SemanticReference);
            #[cfg(feature = "test-support")]
            crate::semantic_batch::testing::edit_row("reference", &mut reference)?;
            self.check_row(
                &raw,
                semantic::EvidenceKind::SemanticReference,
                Some(&reference.document),
                |v, p, f| v.reference(&reference, p, f),
            )?;
            let body = to_json(&reference)?;
            self.emit(
                "INSERT INTO out_references VALUES(?1,?2,?3,?4)",
                &[
                    &self.next_ord(REFERENCES),
                    &reference.id,
                    &reference.provenance_id,
                    &body,
                ],
            )?;
        }
        Ok(())
    }

    /// Resolves each staged call binding from its final reference, then settles every native
    /// call site that more than one binding reaches. Contradictory exact evidence becomes
    /// `ambiguous` over the union of the asserted targets (contract-v1 CallBinding), exactly as
    /// the whole-publication validator requires, so no contradictory site reaches the stage.
    fn resolve_calls(&self) -> Result<()> {
        let conn = self.conn();
        let mut statement = conn.prepare(RESOLVE_CALLS_SQL)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            let mut call: semantic::CallBinding = from_json(&row.get::<_, String>(1)?)?;
            let body: Option<String> = conn
                .prepare_cached(
                    "SELECT r.body FROM w_accepted a JOIN out_references r ON r.reference_id=a.ref_id WHERE a.raw_id=?1",
                )?
                .query_row([&raw], |row| row.get(0))
                .optional()?;
            let Some(body) = body else {
                continue;
            };
            let reference: semantic::Reference = from_json(&body)?;
            call.resolution = reference.resolution;
            call.declared_target = reference.declared_target.clone();
            call.candidates = reference.candidates.clone();
            call.stale_target = call.declared_target.as_ref().and_then(|target| {
                matches!(target, semantic::Target::Internal { .. }).then_some(false)
            });
            #[cfg(feature = "test-support")]
            crate::semantic_batch::testing::edit_row("callResolution", &mut call)?;
            self.ctx.row()?;
            conn.prepare_cached("INSERT INTO w_callres(raw_id, call_id, body) VALUES(?1,?2,?3)")?
                .execute(params![raw, call.call_id, to_json(&call)?])?;
            if let Some(site) = &call.call_id {
                for target in call.declared_target.iter().chain(&call.candidates) {
                    self.ctx.row()?;
                    conn.prepare_cached("INSERT OR IGNORE INTO w_sitetargets VALUES(?1,?2,?3)")?
                        .execute(params![
                            site,
                            crate::native_ids::canonical(&serde_json::to_value(target)?),
                            to_json(target)?
                        ])?;
                }
            }
        }
        self.settle_call_sites()
    }

    /// One pass over the site targets in `(site, canonical target)` order. A site asserting more
    /// than one distinct target is rewritten to the ambiguous union on every binding that
    /// reaches it; only that one site's targets are held at a time.
    fn settle_call_sites(&self) -> Result<()> {
        let conn = self.conn();
        let mut statement = conn.prepare(SITE_TARGETS_SQL)?;
        let mut rows = statement.query([])?;
        let mut site: Option<String> = None;
        let mut asserted: Vec<(Vec<u8>, String)> = Vec::new();
        while let Some(row) = rows.next()? {
            self.ctx.charge(1)?;
            let id: String = row.get(0)?;
            if site.as_ref() != Some(&id)
                && let Some(done) = site.replace(id)
            {
                self.settle_call_site(&done, std::mem::take(&mut asserted))?;
            }
            asserted.push((row.get(1)?, row.get(2)?));
        }
        if let Some(done) = site {
            self.settle_call_site(&done, asserted)?;
        }
        Ok(())
    }

    fn settle_call_site(&self, site: &str, asserted: Vec<(Vec<u8>, String)>) -> Result<()> {
        if asserted.len() < 2 {
            return Ok(());
        }
        let conn = self.conn();
        let union = asserted
            .iter()
            .map(|(_, target)| from_json(target))
            .collect::<Result<Vec<semantic::Target>>>()?;
        let bindings = {
            self.ctx.charge(1)?;
            let mut statement =
                conn.prepare_cached("SELECT ord, body FROM w_callres WHERE call_id=?1")?;
            let found = statement
                .query_map([site], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            self.ctx.charge(found.len())?;
            found
        };
        for (ord, body) in bindings {
            let mut call: semantic::CallBinding = from_json(&body)?;
            if call.resolution == semantic::Resolution::Ambiguous
                && call.declared_target.is_none()
                && call.candidates == union
            {
                continue;
            }
            call.resolution = semantic::Resolution::Ambiguous;
            call.declared_target = None;
            call.candidates = union.clone();
            call.stale_target = None;
            conn.prepare_cached("UPDATE w_callres SET body=?2 WHERE ord=?1")?
                .execute(params![ord, to_json(&call)?])?;
        }
        Ok(())
    }

    fn calls(&self) -> Result<()> {
        let conn = self.conn();
        let mut statement = conn.prepare(CALLS_SQL)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            let mut call: semantic::CallBinding = from_json(&row.get::<_, String>(1)?)?;
            self.outcome(
                &raw,
                if call.resolution == semantic::Resolution::Ambiguous {
                    3
                } else {
                    1
                },
            )?;
            self.observe(&call.join.anchor.document, semantic::Role::Call)?;
            call.provenance_id = self.id_for(&raw, semantic::EvidenceKind::SemanticReference);
            #[cfg(feature = "test-support")]
            crate::semantic_batch::testing::edit_row("callBinding", &mut call)?;
            self.check_row(
                &raw,
                semantic::EvidenceKind::SemanticReference,
                Some(&call.join.anchor.document),
                |v, p, f| v.call_binding(&call, p, f),
            )?;
            let body = to_json(&call)?;
            self.emit(
                "INSERT INTO out_call_bindings VALUES(?1,?2,?3)",
                &[&self.next_ord(CALLS), &call.provenance_id, &body],
            )?;
        }
        Ok(())
    }

    fn relationships(&self) -> Result<()> {
        let conn = self.conn();
        let mut statement = conn.prepare("SELECT body FROM f_rels WHERE side=0 ORDER BY ord")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let mut relationship: semantic::TypeRelationship =
                from_json(&row.get::<_, String>(0)?)?;
            let raw = relationship.provenance_id.clone();
            let Some(document) = self.proof_document(&raw)? else {
                continue;
            };
            if self.retarget(&mut relationship.source)?
                && self.retarget(&mut relationship.target)?
            {
                self.source(&raw, semantic::EvidenceKind::TypeRelationship)?;
                self.outcome(&raw, 1)?;
                self.observe(&document, semantic::Role::Type)?;
                relationship.provenance_id =
                    self.id_for(&raw, semantic::EvidenceKind::TypeRelationship);
                #[cfg(feature = "test-support")]
                crate::semantic_batch::testing::edit_row("typeRelationship", &mut relationship)?;
                self.check_row(
                    &raw,
                    semantic::EvidenceKind::TypeRelationship,
                    None,
                    |v, p, f| v.type_relationship(&relationship, p, f),
                )?;
                let body = to_json(&relationship)?;
                self.emit(
                    "INSERT INTO out_type_relationships VALUES(?1,?2,?3)",
                    &[
                        &self.next_ord(RELATIONSHIPS),
                        &relationship.provenance_id,
                        &body,
                    ],
                )?;
            } else {
                self.omit(&document.language, &document.path)?;
            }
        }
        Ok(())
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
}

impl StagedJoin<'_> {
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

    fn provenance(&self) -> Result<()> {
        let conn = self.conn();
        let destination_manifest_hash = self.destination_manifest_hash()?;
        let lookup = |proof: &str, kind: &str, digest: &str| -> Result<RawClaimDocument> {
            conn.prepare_cached(
                "SELECT s.language, s.path, a.sha256 FROM claims c JOIN sources s ON s.ord=c.source_ord \
                 JOIN artifacts a ON a.ordinal=c.artifact WHERE c.proof_id=?1 AND c.record_kind=?2 AND c.fact_digest=?3",
            )?
            .query_row(params![proof, kind, digest], |row| {
                Ok(RawClaimDocument {
                    language: row.get(0)?,
                    path: row.get(1)?,
                    artifact_hash: row.get(2)?,
                })
            })
            .optional()?
            .ok_or_else(|| ScipCaptureError::MissingProof.into())
        };
        let mut statement = conn.prepare(PROVENANCE_SQL)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            self.ctx.check_cancel()?;
            let raw: String = row.get(0)?;
            let kind = rank_kind(row.get(1)?)?;
            let claim = self
                .captured
                .proof(&raw)?
                .context("missing raw fact for joined provenance")?;
            self.captured
                .check_proof(&raw, &claim.record_kind, &claim.fact_digest)?;
            let doc = self.key(&claim.language, &claim.path);
            let basis = self
                .basis
                .basis_for(&doc)?
                .context("missing immutable captured basis")?;
            anyhow::ensure!(
                basis.revision_id == self.captured_id()
                    && basis.artifact_hash == claim.artifact_hash
                    && basis.source_manifest_hash == self.descriptor.source_manifest_hash
                    && basis.producer_id == self.producer.id
                    && basis.language == language_of(&doc.language)?,
                "captured semantic basis differs from authenticated inventory"
            );
            let (_, destination) = self
                .destination
                .doc_row(&doc)?
                .context("missing destination document")?;
            let captured_hash = self.captured.doc_row(&doc)?.map(|(_, d)| d.content_hash);
            let header = &self.destination_header.revision;
            let freshness = if captured_hash.is_none_or(|hash| hash != destination.content_hash) {
                semantic::Freshness::Stale
            } else if basis.revision_id != header.id
                || basis.source_manifest_hash != destination_manifest_hash
                || basis.toolchain_hash != header.toolchain_hash
                || basis.config_hash != header.config_hash
                || basis.dependency_hash != header.dependency_hash
                || basis.producer_version != self.producer.version
                || basis.producer_hash != self.producer.executable_hash
                || !self.producer.languages.contains(&basis.language)
            {
                semantic::Freshness::PossiblyStale
            } else {
                semantic::Freshness::Fresh
            };
            let derived_kind: semantic::RecordKind =
                serde_json::from_value(serde_json::json!(claim.record_kind))?;
            let provenance = semantic::Provenance {
                id: self.id_for(&raw, kind),
                producer_id: self.producer.id.clone(),
                document: doc,
                revision_id: header.id.clone(),
                content_hash: destination.content_hash.clone(),
                evidence_kind: kind,
                basis: Some(basis),
                freshness,
                derived_from: Some(semantic::DerivedFrom {
                    provenance_id: raw.clone(),
                    record_kind: derived_kind,
                    fact_digest: claim.fact_digest.clone(),
                }),
            };
            self.validator.provenance(&provenance, &lookup)?;
            let artifact: i64 = conn
                .prepare_cached("SELECT artifact FROM claims WHERE proof_id=?1")?
                .query_row([&raw], |row| row.get(0))?;
            let body = to_json(&provenance)?;
            self.emit(
                "INSERT INTO out_provenance VALUES(?1,?2,?3,?4,?5,?6,?7)",
                &[
                    &self.next_ord(PROVENANCE),
                    &provenance.id,
                    &artifact,
                    &raw,
                    &claim.record_kind,
                    &claim.fact_digest,
                    &body,
                ],
            )?;
        }
        Ok(())
    }

    fn coverage(&self) -> Result<BTreeMap<semantic::Language, LanguageTally>> {
        let conn = self.conn();
        let mut tallies: BTreeMap<semantic::Language, LanguageTally> = BTreeMap::new();
        let all_roles = |language: semantic::Language| -> Vec<semantic::Role> {
            [
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
            .collect()
        };
        for doc in self.destination.documents() {
            let doc = doc?;
            let language = language_of(&doc.language)?;
            if !self.producer.languages.contains(&language) {
                continue;
            }
            let flags: Option<(bool, bool, bool, bool)> = conn
                .prepare_cached(
                    "SELECT w.requested, w.failed, w.unsupported, w.omitted FROM sources s \
                     JOIN w_docs w ON w.source_ord=s.ord WHERE s.language=?1 AND s.path=?2",
                )?
                .query_row(params![doc.language, doc.path], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })
                .optional()?;
            let (requested, failed, unsupported, omitted) = flags.unwrap_or_default();
            let excluded: Option<String> = conn
                .prepare_cached(
                    "SELECT c.exclusion FROM sources s JOIN coverage c ON c.source_ord=s.ord WHERE s.language=?1 AND s.path=?2",
                )?
                .query_row(params![doc.language, doc.path], |row| row.get(0))
                .optional()?
                .flatten();
            let diagnostics = exists(
                conn,
                "SELECT 1 FROM f_diag WHERE side=0 AND language=?1 AND path=?2",
                &[&doc.language, &doc.path],
            )?;
            let excluded = excluded.as_deref();
            let partial = excluded == Some("unsupported") || unsupported || omitted || diagnostics;
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
            let observed: BTreeSet<semantic::Role> = {
                self.ctx.charge(1)?;
                let mut statement = conn
                    .prepare_cached("SELECT role FROM w_observed WHERE language=?1 AND path=?2")?;
                let names = statement
                    .query_map(params![doc.language, doc.path], |row| {
                        row.get::<_, String>(0)
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                names
                    .iter()
                    .map(|name| Ok(serde_json::from_value(serde_json::json!(name))?))
                    .collect::<Result<_>>()?
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
                supported_roles: all_roles(language),
                observed_roles: observed.into_iter().collect(),
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
                } else if partial {
                    Some("unmeasurable_internal_target".into())
                } else {
                    None
                },
            };
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
                "INSERT INTO out_coverage VALUES(?1,?2,?3,?4)",
                &[&self.next_ord(COVERAGE), &doc.language, &doc.path, &body],
            )?;
            let mut counts = [0u64; 5];
            let mut statement = conn.prepare_cached(
                "SELECT o.outcome, COUNT(*) FROM claims c JOIN sources s ON s.ord=c.source_ord \
                 JOIN w_outcomes o ON o.proof_id=c.proof_id WHERE s.language=?1 AND s.path=?2 \
                 AND c.record_kind<>'symbolInformation' GROUP BY o.outcome",
            )?;
            let grouped = statement
                .query_map(params![doc.language, doc.path], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)? as u64))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (outcome, count) in grouped {
                counts[0] += count;
                counts[if state == semantic::CoverageState::Failed {
                    2
                } else {
                    outcome as usize
                }] += count;
            }
            anyhow::ensure!(
                counts[0] == counts[1] + counts[2] + counts[3] + counts[4],
                "incomplete semantic import claim tally"
            );
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
        }
        Ok(tallies)
    }

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
                     (SELECT 1 FROM out_provenance p WHERE p.provenance_id=o.provenance_id)"
                ))? == 0,
                "semantic row without provenance"
            );
        }
        anyhow::ensure!(
            count(
                "SELECT COUNT(*) FROM out_provenance o WHERE o.derived_from IS NOT NULL AND NOT EXISTS \
                 (SELECT 1 FROM claims c WHERE c.proof_id=o.derived_from AND c.record_kind=o.derived_kind \
                  AND c.fact_digest=o.derived_digest)"
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
                 ON p.provenance_id=s.provenance_id \
                 GROUP BY p.artifact, json_extract(s.body,'$.key') HAVING COUNT(*)>1)",
                "duplicate semantic symbol key",
            ),
            (
                "SELECT COUNT(*) FROM out_provenance p WHERE NOT EXISTS (SELECT 1 FROM out_coverage c \
                 WHERE c.language=json_extract(p.body,'$.document.language') \
                 AND c.path=json_extract(p.body,'$.document.path') \
                 AND json_extract(c.body,'$.state') IN ('complete','partial'))",
                "proof on non-fact-bearing coverage",
            ),
            (
                "SELECT COUNT(*) FROM out_provenance p WHERE NOT EXISTS (SELECT 1 FROM nat_docs d \
                 WHERE d.rev=0 AND d.language=json_extract(p.body,'$.document.language') \
                 AND d.path=json_extract(p.body,'$.document.path') \
                 AND d.content_hash=json_extract(p.body,'$.contentHash'))",
                "captured proof document hash mismatch",
            ),
        ] {
            anyhow::ensure!(count(sql)? == 0, message);
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

    fn run(&self) -> Result<serde_json::Value> {
        let conn = self.conn();
        let mut sink = StagedFactSink::new(self.ctx, 0);
        join_inventory_indexed(self.admitted_encoding, &self.captured, &mut sink)?;
        self.eligibility()?;
        let mut sink = StagedFactSink::new(self.ctx, 1);
        join_inventory_indexed(self.admitted_encoding, &self.filtered, &mut sink)?;
        self.fill_captured_targets()?;
        conn.execute(
            "INSERT INTO w_outcomes SELECT proof_id, 2 FROM claims WHERE record_kind<>'symbolInformation'",
            [],
        )?;
        conn.execute(
            "UPDATE w_outcomes SET outcome=4 WHERE proof_id IN (SELECT proof_id FROM w_unsupported_claims)",
            [],
        )?;
        self.symbols()?;
        self.bindings()?;
        self.stage_calls()?;
        self.reference_claims()?;
        self.references()?;
        self.resolve_calls()?;
        self.calls()?;
        self.relationships()?;
        self.provenance()?;
        let tallies = self.coverage()?;
        #[cfg(feature = "test-support")]
        crate::semantic_batch::testing::run_before_closure_hook(conn);
        self.closure()?;
        self.report(tallies)
    }
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
        counters: RefCell::new(BatchCounters::default()),
        cache: RefCell::new(DocCache {
            budget: limits.document_cache_bytes,
            ..DocCache::default()
        }),
        rows: Cell::new(0),
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
            native_pass(&ctx, captured, 0, false)?;
            if !shared {
                native_pass(&ctx, destination_reader, 1, !ctx.mutations.is_empty())?;
            }
            let destination_rev = if shared { 0 } else { 1 };
            let side = |reader, header, rev, side, filtered| StagedSide {
                ctx: &ctx,
                reader,
                header,
                rev,
                side,
                filtered,
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
                identity_kind: descriptor.producer.identity_kind.as_str(),
                descriptor_sha256: parts.descriptor_sha256,
            };
            let report = join.run()?;
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
        ctx.rows.set(retained_rows(ctx.conn)?);
        if shared {
            store.with_native_reader(captured_pin, |reader| body(attempt, reader, reader))
        } else {
            store.with_native_reader(captured_pin, |captured| {
                store.with_native_reader(destination, |reader| body(attempt, captured, reader))
            })
        }
    });
    *budget = ctx.budget.into_inner();
    counters.absorb_join(&ctx.counters.borrow());
    result
}
