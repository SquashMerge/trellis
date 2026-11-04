//! The per-operation stage: a private directory beside `index.db` holding `stage.db` and the
//! copied artifacts. The claiming worker owns it; it is derived, disposable state.
use super::limits::{BatchLimits, STAGE_CACHE_KIB, STAGE_CHECK_ROWS};
use crate::scip_raw::{CapturedClaim, ClaimSink, ScipCaptureError, UnitDocument};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, ErrorCode, OptionalExtension, params};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

/// A reusable witness is unavailable; the caller must run bounded FULL admission/join.
#[derive(Debug)]
pub(crate) struct WitnessFallback(pub &'static str);

impl std::fmt::Display for WitnessFallback {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "witness fallback: {}", self.0)
    }
}

impl std::error::Error for WitnessFallback {}

pub const STAGE_PREFIX: &str = "semantic-stage-";
const STAGE_DB: &str = "stage.db";

/// `documents.capped` is `scip_raw::DocumentCap::code` of the per-document cap a document was
/// admitted under: 0 for none, 1 for `OCCURRENCE_CAP_EXCEEDED`, 2 for `DOCUMENT_CAP_EXCEEDED`,
/// 3 for `INVALID_RANGE`.
///
/// `claims.symbol`/`presence` index each admitted claim's symbol presence: a symbol-information
/// claim's symbol (`PRESENCE_INFORMATION`) and a definition occurrence's (`PRESENCE_DECLARED`),
/// `NULL`/0 otherwise. A side's presence context is this index over its claim set.
/// `claims.target_root` is the `sources.ord` of the crate root of a Rust claim's Cargo target
/// (`NULL` without one), `target_lib` is 1 when that target is a library, and
/// `target_package`/`target_version` are its package's manifest name and version (#394).
///
/// Compact join tables. Documents are integers: `sources.ord` for `g_*` owners and local
/// symbol scopes (`-1` for none), `nat_docs.ord` for native declarations, and the document's
/// `out_coverage.ord` (`doc`) for every output row. Native syntax IDs and 64-hex provenance IDs
/// are stored as their bytes when canonical, as text otherwise (`staged::codec`).
/// `nat_docs.reserve` is a document's native load reservation, filled on its first load.
/// `g_definitions.callable` is the owner declaration's `TargetCallable` code (#470): a function
/// of `(side, owner_doc, syntax_id)`, so every row of one definition target carries the same value.
/// `g_definitions_owner` covers it, so the per-call probe is one keyed index search.
///
/// The join emits `out_*` one document at a time, so every table's `ord` is document-major and
/// the publisher reads each family with one cursor in `ord` order. Within one document each
/// family is in contract order. A document's shared provenance fields (document, revision,
/// content hash, basis, freshness) are stored once, as a template on its coverage row; a
/// provenance row holds only its raw claim, evidence kind and ID. Fact bodies store an empty
/// `provenanceId`; the row's `provenance_id` column is authoritative.
const STAGE_SCHEMA: &str = "
CREATE TABLE header(singleton INTEGER PRIMARY KEY CHECK(singleton=1), descriptor_sha256 TEXT NOT NULL, descriptor_bytes BLOB NOT NULL);
CREATE TABLE sources(ord INTEGER PRIMARY KEY, language TEXT NOT NULL, path TEXT NOT NULL, content_hash TEXT NOT NULL, byte_length INTEGER NOT NULL, UNIQUE(language,path));
CREATE TABLE coverage(source_ord INTEGER PRIMARY KEY REFERENCES sources(ord), artifact INTEGER, exclusion TEXT, CHECK((artifact IS NULL)<>(exclusion IS NULL)));
CREATE INDEX coverage_artifact ON coverage(artifact, source_ord);
CREATE TABLE artifacts(ordinal INTEGER PRIMARY KEY, path TEXT NOT NULL, language TEXT NOT NULL, sha256 TEXT NOT NULL UNIQUE, byte_length INTEGER NOT NULL, documents INTEGER NOT NULL DEFAULT 0, claims INTEGER NOT NULL DEFAULT 0);
CREATE TABLE documents(source_ord INTEGER PRIMARY KEY, artifact INTEGER NOT NULL, path TEXT NOT NULL, wire_digest TEXT NOT NULL, capped INTEGER NOT NULL CHECK(capped IN (0,1,2,3)), UNIQUE(artifact,path));
CREATE TABLE claims(claim_id INTEGER PRIMARY KEY, artifact INTEGER NOT NULL, source_ord INTEGER NOT NULL, record_kind TEXT NOT NULL, fact_digest TEXT NOT NULL, proof_id TEXT NOT NULL, canonical_bytes BLOB NOT NULL, symbol TEXT, presence INTEGER NOT NULL, target_root INTEGER, target_lib INTEGER NOT NULL DEFAULT 0, target_package TEXT, target_version TEXT, UNIQUE(artifact,fact_digest));
CREATE INDEX claims_order ON claims(source_ord, canonical_bytes);
CREATE UNIQUE INDEX claims_proof ON claims(proof_id);
CREATE INDEX claims_symbol ON claims(symbol, presence, target_root) WHERE symbol IS NOT NULL;
CREATE TABLE witness_proofs(identity_id INTEGER PRIMARY KEY CHECK(identity_id<0), artifact INTEGER NOT NULL, source_ord INTEGER NOT NULL, record_kind TEXT NOT NULL, fact_digest TEXT NOT NULL, proof_id TEXT NOT NULL UNIQUE, capture_id INTEGER NOT NULL, UNIQUE(artifact,fact_digest));
CREATE INDEX witness_proofs_source ON witness_proofs(source_ord, record_kind);
CREATE VIEW proof_identities AS SELECT claim_id AS identity_id, claim_id, artifact, source_ord, record_kind, fact_digest, proof_id, NULL AS capture_id FROM claims c UNION ALL SELECT identity_id, identity_id AS claim_id, artifact, source_ord, record_kind, fact_digest, proof_id, capture_id FROM witness_proofs w;
CREATE TABLE witness_state(singleton INTEGER PRIMARY KEY CHECK(singleton=1), internal_symbols_complete INTEGER NOT NULL DEFAULT 0 CHECK(internal_symbols_complete IN (0,1)), staged_rows INTEGER NOT NULL DEFAULT 0);
CREATE TABLE source_witnesses(source_ord INTEGER PRIMARY KEY, decision TEXT NOT NULL CHECK(decision IN ('reused','copied','changedBytes','newDocument')), body TEXT NOT NULL, complete INTEGER NOT NULL DEFAULT 0 CHECK(complete IN (0,1)));
CREATE TABLE original_associations(source_ord INTEGER NOT NULL, ordinal INTEGER NOT NULL, body TEXT NOT NULL, body_bytes INTEGER NOT NULL, PRIMARY KEY(source_ord,ordinal)) WITHOUT ROWID;
CREATE TABLE witness_internal_symbols(scope INTEGER NOT NULL, language TEXT NOT NULL, path TEXT NOT NULL, symbol TEXT NOT NULL, PRIMARY KEY(scope,language,path,symbol)) WITHOUT ROWID;
CREATE INDEX witness_internal_symbols_symbol ON witness_internal_symbols(symbol);
CREATE TABLE join_state(singleton INTEGER PRIMARY KEY CHECK(singleton=1), generation TEXT NOT NULL, revision INTEGER NOT NULL, attempt INTEGER NOT NULL);
CREATE TABLE nat_done(rev INTEGER PRIMARY KEY);
CREATE TABLE nat_docs(rev INTEGER NOT NULL, ord INTEGER NOT NULL, language TEXT NOT NULL, path TEXT NOT NULL, content_hash TEXT NOT NULL, byte_length INTEGER NOT NULL, reserve INTEGER, PRIMARY KEY(rev, ord), UNIQUE(rev, language, path)) WITHOUT ROWID;
CREATE TABLE nat_declarations(rev INTEGER NOT NULL, doc_ord INTEGER NOT NULL, syntax_id BLOB NOT NULL, n INTEGER NOT NULL, named INTEGER NOT NULL, name_ok INTEGER NOT NULL, method INTEGER NOT NULL, PRIMARY KEY(rev, doc_ord, syntax_id, n)) WITHOUT ROWID;
CREATE TABLE g_definitions(side INTEGER NOT NULL, symbol TEXT NOT NULL, local_doc INTEGER NOT NULL, owner_doc INTEGER NOT NULL, syntax_id BLOB NOT NULL, common_compatible INTEGER NOT NULL, callable INTEGER NOT NULL CHECK(callable IN (0,1,2,3,4)), PRIMARY KEY(side,symbol,local_doc,owner_doc,syntax_id)) WITHOUT ROWID;
CREATE INDEX g_definitions_owner ON g_definitions(side,owner_doc,syntax_id,callable);
CREATE TABLE g_methods(ord INTEGER PRIMARY KEY, side INTEGER NOT NULL, symbol TEXT NOT NULL, local_doc INTEGER NOT NULL, owner_doc INTEGER NOT NULL, body TEXT NOT NULL, body_bytes INTEGER NOT NULL, decoded_bytes INTEGER NOT NULL);
CREATE INDEX g_methods_source ON g_methods(side,symbol,owner_doc);
CREATE INDEX g_methods_target ON g_methods(side,symbol,local_doc);
CREATE TABLE w_docs(source_ord INTEGER PRIMARY KEY, requested INTEGER NOT NULL, failed INTEGER NOT NULL, eligible INTEGER NOT NULL, unsupported INTEGER NOT NULL, unsupported_claims INTEGER NOT NULL, claims INTEGER NOT NULL, gaps_macro_invocation INTEGER NOT NULL, gaps_attribute_item INTEGER NOT NULL, gaps_macro_definition INTEGER NOT NULL, gaps_other INTEGER NOT NULL);
CREATE TABLE out_coverage(ord INTEGER PRIMARY KEY, language TEXT NOT NULL, path TEXT NOT NULL, body TEXT NOT NULL, provenance TEXT);
CREATE TABLE out_provenance(ord INTEGER PRIMARY KEY, doc INTEGER NOT NULL, claim_id INTEGER NOT NULL, kind INTEGER NOT NULL, id BLOB NOT NULL UNIQUE);
CREATE TABLE out_symbols(ord INTEGER PRIMARY KEY, doc INTEGER NOT NULL, provenance_id BLOB NOT NULL, body TEXT NOT NULL);
CREATE TABLE out_declaration_bindings(ord INTEGER PRIMARY KEY, doc INTEGER NOT NULL, provenance_id BLOB NOT NULL, body TEXT NOT NULL);
CREATE TABLE out_references(ord INTEGER PRIMARY KEY, doc INTEGER NOT NULL, reference_id TEXT NOT NULL UNIQUE, provenance_id BLOB NOT NULL, body TEXT NOT NULL);
CREATE TABLE out_call_bindings(ord INTEGER PRIMARY KEY, doc INTEGER NOT NULL, provenance_id BLOB NOT NULL, body TEXT NOT NULL);
CREATE TABLE out_type_relationships(ord INTEGER PRIMARY KEY, doc INTEGER NOT NULL, provenance_id BLOB NOT NULL, body TEXT NOT NULL);
";

/// Tables one join attempt fills from its claims and destination. Every attempt clears them all;
/// only `nat_docs` (and `nat_done`) outlive an attempt, as its retained native seed.
pub const JOIN_TABLES: &[&str] = &[
    "nat_declarations",
    "g_definitions",
    "g_methods",
    "w_docs",
    "out_coverage",
    "out_provenance",
    "out_symbols",
    "out_declaration_bindings",
    "out_references",
    "out_call_bindings",
    "out_type_relationships",
];

fn valid_component(text: &str) -> bool {
    (1..=64).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// `semantic-stage-<request id>-<claim incarnation>`; both parts are `^[0-9A-Za-z-]{1,64}$`.
pub fn stage_dir_name(request_id: &str, incarnation: &str) -> Result<String> {
    ensure!(
        valid_component(request_id) && valid_component(incarnation),
        "index_failed: stage name"
    );
    Ok(format!("{STAGE_PREFIX}{request_id}-{incarnation}"))
}

pub fn artifact_file(dir: &Path, ordinal: u32) -> PathBuf {
    dir.join(format!("artifact-{ordinal}.scip"))
}

fn tree_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.metadata().ok())
                .filter(|metadata| metadata.is_file())
                .map(|metadata| metadata.len())
                .sum()
        })
        .unwrap_or(0)
}

fn index_files_bytes(index_dir: &Path) -> u64 {
    ["index.db", "index.db-wal", "index.db-shm"]
        .iter()
        .filter_map(|name| std::fs::metadata(index_dir.join(name)).ok())
        .map(|metadata| metadata.len())
        .sum()
}

/// Measures the stage against the staging and operation-growth limits.
#[derive(Debug, Clone)]
pub struct StageBudget {
    dir: PathBuf,
    index_dir: PathBuf,
    baseline: u64,
    staging_bytes: u64,
    disk_growth_bytes: u64,
}

impl StageBudget {
    /// Charges `reserved` staged bytes and measures the stage once [`WITNESS_CHECK_BYTES`]
    /// (or an eighth of the staging limit, if smaller) have accumulated in `unchecked`.
    pub(crate) fn charge(
        &self,
        unchecked: &mut u64,
        reserved: u64,
    ) -> std::result::Result<(), ScipCaptureError> {
        *unchecked = unchecked.saturating_add(reserved);
        if *unchecked >= WITNESS_CHECK_BYTES.min(self.staging_bytes / 8) {
            *unchecked = 0;
            self.check()?;
        }
        Ok(())
    }

    pub fn check(&self) -> std::result::Result<(), ScipCaptureError> {
        let staged = tree_bytes(&self.dir);
        if staged > self.staging_bytes {
            return Err(ScipCaptureError::LimitExceeded("staging bytes".into()));
        }
        let live = index_files_bytes(&self.index_dir).saturating_add(staged);
        if live.saturating_sub(self.baseline) > self.disk_growth_bytes {
            return Err(ScipCaptureError::LimitExceeded(
                "operation disk growth".into(),
            ));
        }
        Ok(())
    }
}

/// Owner of one operation's stage directory. Dropping it removes only its own exact path.
pub struct Stage {
    dir: PathBuf,
    index_dir: PathBuf,
    baseline: u64,
    conn: Option<Connection>,
    /// Statements run in one stage transaction until [`Stage::commit_batch`] or a release.
    batched: bool,
}

/// A unit's own stage transaction. It is a savepoint so it nests inside a batch
/// ([`Stage::begin_batch`]); outside one it begins and commits like `BEGIN`/`COMMIT`.
pub const UNIT_BEGIN: &str = "SAVEPOINT stage_unit";
/// Ends a unit transaction begun with [`UNIT_BEGIN`].
pub const UNIT_COMMIT: &str = "RELEASE stage_unit";
/// Abandons a unit transaction begun with [`UNIT_BEGIN`].
pub const UNIT_ROLLBACK: &str = "ROLLBACK TO stage_unit; RELEASE stage_unit";

fn open_connection(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).context("stage open")?;
    conn.set_prepared_statement_cache_capacity(128);
    // Stage statements are cached and rebound per row; a plan fixed at prepare time avoids
    // re-preparing them whenever a bound LIMIT or small integer changes.
    conn.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_ENABLE_QPSG,
        true,
    )
    .context("stage pragmas")?;
    conn.execute_batch(&format!(
        "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA temp_store=FILE; PRAGMA cache_size=-{STAGE_CACHE_KIB};"
    ))
    .context("stage pragmas")?;
    Ok(conn)
}

impl Stage {
    /// Creates the stage directory under `index_dir` and `stage.db` with its full schema.
    pub fn create(index_dir: &Path, request_id: &str, incarnation: &str) -> Result<Self> {
        let dir = index_dir.join(stage_dir_name(request_id, incarnation)?);
        let baseline = index_files_bytes(index_dir);
        std::fs::create_dir(&dir).context("stage directory")?;
        let mut stage = Stage {
            dir,
            index_dir: index_dir.to_owned(),
            baseline,
            conn: None,
            batched: false,
        };
        let conn = open_connection(&stage.db_path())?;
        conn.execute_batch(STAGE_SCHEMA).context("stage schema")?;
        stage.conn = Some(conn);
        Ok(stage)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn db_path(&self) -> PathBuf {
        self.dir.join(STAGE_DB)
    }

    pub fn artifact_path(&self, ordinal: u32) -> PathBuf {
        artifact_file(&self.dir, ordinal)
    }

    /// The stage connection, opened on first use and again after `release_connection`.
    pub fn conn(&mut self) -> Result<&Connection> {
        if self.conn.is_none() {
            self.conn = Some(open_connection(&self.db_path())?);
        }
        let conn = self.conn.as_ref().expect("stage connection just opened");
        if self.batched && conn.is_autocommit() {
            conn.execute_batch("BEGIN")?;
        }
        Ok(conn)
    }

    /// Runs every later statement in one stage transaction until [`Self::commit_batch`] or
    /// [`Self::release_connection`]. The stage has no journal: this saves a lock, page write
    /// and unlock per row, and a commit keeps every statement executed, as autocommit would.
    pub fn begin_batch(&mut self) {
        self.batched = true;
    }

    /// Commits the batch transaction, if one is open, and ends batching.
    pub fn commit_batch(&mut self) -> rusqlite::Result<()> {
        self.batched = false;
        match self.conn.as_ref() {
            Some(conn) if !conn.is_autocommit() => conn.execute_batch("COMMIT"),
            _ => Ok(()),
        }
    }

    /// Measures the stage against its limits after writing an open batch's dirty pages to
    /// the file: inside a batch the file lengths do not yet include them. The batch goes on;
    /// the next statement begins a new one.
    pub fn check_flushed(&mut self, limits: &BatchLimits) -> Result<()> {
        if let Some(conn) = self.conn.as_ref()
            && !conn.is_autocommit()
        {
            conn.execute_batch("COMMIT")?;
        }
        self.budget(limits).check()?;
        Ok(())
    }

    /// [`StageBudget::charge`] for this stage, measuring it with [`Self::check_flushed`].
    pub fn charge_flushed(
        &mut self,
        limits: &BatchLimits,
        unchecked: &mut u64,
        reserved: u64,
    ) -> Result<()> {
        *unchecked = unchecked.saturating_add(reserved);
        if *unchecked >= WITNESS_CHECK_BYTES.min(limits.staging_bytes / 8) {
            *unchecked = 0;
            self.check_flushed(limits)?;
        }
        Ok(())
    }

    /// Drops the connection. The publishing writer attaches `stage.db` only after this.
    /// An open batch is committed first; its error is the owner's to read from
    /// [`Self::commit_batch`], which a slice owner calls before releasing.
    pub fn release_connection(&mut self) {
        let _ = self.commit_batch();
        self.conn = None;
    }

    pub fn has_connection(&self) -> bool {
        self.conn.is_some()
    }

    pub fn bytes(&self) -> u64 {
        tree_bytes(&self.dir)
    }

    pub fn budget(&self, limits: &BatchLimits) -> StageBudget {
        StageBudget {
            dir: self.dir.clone(),
            index_dir: self.index_dir.clone(),
            baseline: self.baseline,
            staging_bytes: limits.staging_bytes,
            disk_growth_bytes: limits.disk_growth_bytes,
        }
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        self.conn = None;
        if let Err(error) = std::fs::remove_dir_all(&self.dir)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            let _ = writeln!(
                std::io::stderr(),
                "trellis: cannot remove semantic stage {}: {error}",
                self.dir.display()
            );
        }
    }
}

/// Removes every `semantic-stage-*` entry under `index_dir`. Called once at startup, before
/// the first claim, when no attempt can be active. Failures go to stderr and do not stop the
/// worker. Returns how many entries were removed.
pub fn remove_abandoned_semantic_stages(index_dir: &Path) -> usize {
    let entries = match std::fs::read_dir(index_dir) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                let _ = writeln!(
                    std::io::stderr(),
                    "trellis: cannot scan {} for abandoned semantic stages: {error}",
                    index_dir.display()
                );
            }
            return 0;
        }
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(STAGE_PREFIX)
        {
            continue;
        }
        let path = entry.path();
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        let result = if is_dir {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match result {
            Ok(()) => removed += 1,
            Err(error) => {
                let _ = writeln!(
                    std::io::stderr(),
                    "trellis: cannot remove abandoned semantic stage {}: {error}",
                    path.display()
                );
            }
        }
    }
    removed
}

/// Charges serialization before allocating the private event payload.
fn serialized_len(value: &impl serde::Serialize, limit: u64) -> Result<u64> {
    struct Counter {
        bytes: u64,
        limit: u64,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes = self.bytes.saturating_add(bytes.len() as u64);
            if self.bytes > self.limit {
                return Err(std::io::Error::other("witness serialization limit"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { bytes: 0, limit };
    let result = serde_json::to_writer(&mut counter, value);
    if counter.bytes > limit {
        return Err(ScipCaptureError::LimitExceeded("unit expansion bytes".into()).into());
    }
    result?;
    Ok(counter.bytes)
}

/// Serializes `value` into a buffer that may grow only to `limit` bytes; past it the payload
/// is `unit expansion bytes`, as [`serialized_len`] would have charged it.
fn serialized_bounded(value: &impl serde::Serialize, limit: u64) -> Result<String> {
    struct Bounded {
        bytes: Vec<u8>,
        limit: u64,
        exceeded: bool,
    }
    impl std::io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if (self.bytes.len() as u64).saturating_add(bytes.len() as u64) > self.limit {
                self.exceeded = true;
                return Err(std::io::Error::other("witness serialization limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut buffer = Bounded {
        bytes: Vec::new(),
        limit,
        exceeded: false,
    };
    let result = serde_json::to_writer(&mut buffer, value);
    if buffer.exceeded {
        return Err(ScipCaptureError::LimitExceeded("unit expansion bytes".into()).into());
    }
    result?;
    Ok(String::from_utf8(buffer.bytes)?)
}

pub(crate) fn charge_witness_row(conn: &Connection, limits: &BatchLimits) -> Result<()> {
    let changed = conn
        .prepare_cached(
            "UPDATE witness_state SET staged_rows=staged_rows+1 WHERE singleton=1 AND staged_rows<?1",
        )?
        .execute([i64::try_from(limits.staged_rows)?])?;
    if changed != 1 {
        return Err(ScipCaptureError::LimitExceeded("staged rows".into()).into());
    }
    Ok(())
}

/// One bounded page writer. It owns no main-index connection or source reconstruction.
pub(crate) struct WitnessStageWriter<'a> {
    conn: &'a Connection,
    budget: StageBudget,
    limits: &'a BatchLimits,
    counters: &'a mut super::BatchCounters,
    changes_before: u64,
    /// Reserved bytes staged since the last directory-size check.
    unchecked: u64,
    /// The reused source this writer has seen open; its later rows skip the probe.
    open_source: Option<i64>,
}

/// Rows re-measure the stage directory after this many reserved bytes (or an eighth of the
/// staging limit, if smaller); each writer and every completion measures it too. Walking
/// the directory per row cost more than staging the row.
pub(crate) const WITNESS_CHECK_BYTES: u64 = 1 << 20;

impl<'a> WitnessStageWriter<'a> {
    pub(crate) fn new(
        stage: &'a mut Stage,
        limits: &'a BatchLimits,
        counters: &'a mut super::BatchCounters,
    ) -> Result<Self> {
        let budget = stage.budget(limits);
        budget.check()?;
        let conn = stage.conn()?;
        Ok(Self {
            conn,
            budget,
            limits,
            counters,
            changes_before: conn.total_changes(),
            unchecked: 0,
            open_source: None,
        })
    }

    fn row_check(&mut self, reserved: u64) -> Result<()> {
        self.budget.charge(&mut self.unchecked, reserved)?;
        Ok(())
    }

    fn payload(&mut self, value: &impl serde::Serialize) -> Result<(String, u64)> {
        // One serialization: the buffer may grow only to the largest payload whose doubled
        // reservation still fits the unit expansion limit.
        let body = serialized_bounded(value, self.limits.unit_expansion_bytes / 2)?;
        let length = body.len() as u64;
        let reservation = length.saturating_mul(2);
        charge_witness_row(self.conn, self.limits)?;
        self.row_check(reservation)?;
        self.counters.retained_admission_bytes_high_water = self
            .counters
            .retained_admission_bytes_high_water
            .max(reservation);
        Ok((body, length))
    }

    pub(crate) fn stage_source_witness(
        &mut self,
        source_ord: i64,
        witness: &crate::store::native_first::witness::OriginalSourceWitness,
    ) -> Result<()> {
        ensure!(
            witness.captured_version == witness.current_version,
            "witness source version mismatch"
        );
        self.header(source_ord, "reused", witness)
    }

    /// A reused source whose published rows S copies (#669 H1): no original rows are staged
    /// and the join does not settle it; the publisher copies its basis tuple.
    pub(crate) fn stage_copied_source(
        &mut self,
        source_ord: i64,
        witness: &crate::store::native_first::witness::OriginalSourceWitness,
    ) -> Result<()> {
        ensure!(
            witness.captured_version == witness.current_version,
            "witness source version mismatch"
        );
        self.header(source_ord, "copied", witness)?;
        self.finish_source_witness(source_ord)
    }

    pub(crate) fn stage_witness_coverage(
        &mut self,
        source_ord: i64,
        coverage: &crate::store::native_first::witness::CoverageInput,
        decision: &str,
    ) -> Result<()> {
        ensure!(
            matches!(decision, "changedBytes" | "newDocument"),
            "witness coverage decision"
        );
        self.header(source_ord, decision, coverage)
    }

    fn header(
        &mut self,
        source_ord: i64,
        decision: &str,
        value: &impl serde::Serialize,
    ) -> Result<()> {
        let (body, bytes) = self.payload(value)?;
        self.conn.execute(
            "INSERT INTO source_witnesses(source_ord,decision,body) VALUES(?1,?2,?3)",
            params![source_ord, decision, body],
        )?;
        self.row_check(bytes)?;
        Ok(())
    }

    pub(crate) fn stage_original_row(
        &mut self,
        source_ord: i64,
        ordinal: i64,
        row: &crate::store::native_first::witness::OriginalRow,
    ) -> Result<()> {
        // Only this writer completes a source, so one probe per source and writer holds.
        let open = self.open_source == Some(source_ord)
            || self.conn.prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM source_witnesses WHERE source_ord=?1 AND decision='reused' AND complete=0)",
            )?.query_row([source_ord], |row| row.get(0))?;
        ensure!(open && ordinal >= 0, "witness association source not open");
        self.open_source = Some(source_ord);
        let (body, bytes) = self.payload(row)?;
        self.conn
            .prepare_cached("INSERT INTO original_associations VALUES(?1,?2,?3,?4)")?
            .execute(params![source_ord, ordinal, body, i64::try_from(bytes)?])?;
        self.row_check(bytes)?;
        Ok(())
    }

    pub(crate) fn finish_source_witness(&mut self, source_ord: i64) -> Result<()> {
        ensure!(
            self.conn.execute(
                "UPDATE source_witnesses SET complete=1 WHERE source_ord=?1 AND complete=0",
                [source_ord],
            )? == 1,
            "witness source completion"
        );
        self.open_source = None;
        self.budget.check()?;
        Ok(())
    }

    pub(crate) fn stage_internal_symbol(
        &mut self,
        key: &crate::semantic_evidence::SymbolKey,
    ) -> Result<()> {
        // Scopes are represented exactly as in the original semantic SymbolKey.
        let scope = match key.scope {
            crate::semantic_evidence::SymbolScope::Global => 0,
            crate::semantic_evidence::SymbolScope::Document => 1,
            crate::semantic_evidence::SymbolScope::Target => 2,
        };
        let (language, path) = key
            .document
            .as_ref()
            .map(|document| (document.language.as_str(), document.path.as_str()))
            .unwrap_or(("", ""));
        let bytes = serialized_len(key, self.limits.unit_expansion_bytes)?;
        self.counters.retained_admission_bytes_high_water =
            self.counters.retained_admission_bytes_high_water.max(bytes);
        charge_witness_row(self.conn, self.limits)?;
        self.row_check(bytes.saturating_mul(2))?;
        self.conn
            .prepare_cached(
                "INSERT INTO witness_internal_symbols VALUES(?1,?2,?3,?4) ON CONFLICT DO NOTHING",
            )?
            .execute(params![scope, language, path, key.symbol])?;
        Ok(())
    }

    pub(crate) fn finish_internal_symbols(&mut self) -> Result<()> {
        ensure!(self.conn.execute(
            "UPDATE witness_state SET internal_symbols_complete=1 WHERE singleton=1 AND internal_symbols_complete=0", [],
        )? == 1, "witness internal symbol completion");
        self.budget.check()?;
        Ok(())
    }
}

impl Drop for WitnessStageWriter<'_> {
    fn drop(&mut self) {
        self.counters.retained_admission_units += 1;
        self.counters.retained_admission_rows_high_water = self
            .counters
            .retained_admission_rows_high_water
            .max(self.conn.total_changes() - self.changes_before);
    }
}

/// Conservative owned decoding allowance, queried before reading event payloads.
pub(crate) fn original_rows_reservation(conn: &Connection, source_ord: i64) -> Result<u64> {
    let (bytes, rows): (i64, i64) = conn.prepare_cached("SELECT COALESCE(SUM(body_bytes),0),COUNT(*) FROM original_associations WHERE source_ord=?1")?.query_row(
        [source_ord], |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    u64::try_from(bytes)?
        .checked_mul(8)
        .and_then(|bytes| {
            u64::try_from(rows)
                .ok()?
                .checked_mul(512)
                .and_then(|rows| bytes.checked_add(rows))
        })
        .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion bytes".into()).into())
}

/// Both metadata routes must resolve every emitted provenance row, never silently drop it.
pub(crate) fn check_proof_identity_closure(conn: &Connection) -> Result<()> {
    let invalid: bool = conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM claims WHERE claim_id<=0) OR
         EXISTS(SELECT 1 FROM claims c WHERE EXISTS(SELECT 1 FROM witness_proofs w WHERE w.proof_id=c.proof_id)) OR
         EXISTS(SELECT 1 FROM claims c WHERE EXISTS(SELECT 1 FROM witness_proofs w WHERE w.artifact=c.artifact AND w.fact_digest=c.fact_digest)) OR
         EXISTS(SELECT 1 FROM out_provenance p WHERE NOT EXISTS(SELECT 1 FROM proof_identities i WHERE i.identity_id=p.claim_id))")?.query_row(
        [], |row| row.get(0),
    )?;
    ensure!(!invalid, "staged proof identity closure");
    Ok(())
}

/// `claims.presence` bit of a definition occurrence's symbol.
pub const PRESENCE_DECLARED: i64 = 1;
/// `claims.presence` bit of a symbol-information claim's symbol.
pub const PRESENCE_INFORMATION: i64 = 2;

/// The symbol presence a claim contributes, as the join's discovery records it: the symbol of
/// a symbol-information claim, and of an occurrence whose roles include the definition bit. A
/// claim the join would reject (no symbol, invalid roles) contributes none; the join fails it.
pub fn claim_presence(claim: &CapturedClaim) -> (Option<&str>, i64) {
    let raw = &claim.envelope.raw;
    match claim.envelope.record_kind.as_str() {
        "symbolInformation" => match raw["symbol"].as_str() {
            Some(symbol) => (Some(symbol), PRESENCE_INFORMATION),
            None => (None, 0),
        },
        "occurrence" => match (raw["symbol"].as_str(), raw["symbolRoles"].as_u64()) {
            (Some(symbol), Some(roles)) if !symbol.is_empty() && roles & 1 != 0 => {
                (Some(symbol), PRESENCE_DECLARED)
            }
            _ => (None, 0),
        },
        _ => (None, 0),
    }
}

/// Rebuilds the staged join's view of a claim from its staged row: the parsed envelope and
/// identity, decoded from the row's borrowed canonical bytes. The join never reads the
/// canonical bytes again (they stay in the stage), so the claim does not keep a copy.
pub(crate) fn claim_from_stage(
    canonical_bytes: &[u8],
    fact_digest: String,
    proof_id: String,
    language: String,
    artifact_hash: String,
) -> Result<CapturedClaim> {
    let mut value: serde_json::Value =
        serde_json::from_slice(canonical_bytes).context("staged claim")?;
    let text = |name: &str| {
        value[name]
            .as_str()
            .map(str::to_owned)
            .context("staged claim")
    };
    let record_kind = text("recordKind")?;
    let relative_path = text("relativePath")?;
    let position_encoding = u8::try_from(value["positionEncoding"].as_u64().unwrap_or(0))?;
    let cargo_target = crate::scip_raw::CapturedScipFactV1::cargo_target_from(&value)?;
    let raw = value["raw"].take();
    Ok(CapturedClaim {
        envelope: crate::scip_raw::CapturedScipFactV1 {
            format_version: 1,
            record_kind,
            relative_path,
            position_encoding,
            raw,
            cargo_target,
        },
        canonical_bytes: Vec::new(),
        fact_digest,
        captured_proof_id: proof_id,
        language,
        artifact_hash,
    })
}

/// A staged source row that a coverage row assigned to one artifact.
pub struct Assigned {
    pub ord: i64,
    pub content_hash: String,
    pub byte_length: usize,
}

/// The source `(language, path)` if coverage assigned it to `artifact`.
pub fn lookup_assigned(
    conn: &Connection,
    artifact: u32,
    language: &str,
    path: &str,
) -> rusqlite::Result<Option<Assigned>> {
    conn.prepare_cached(
        "SELECT s.ord, s.content_hash, s.byte_length FROM sources s JOIN coverage c ON c.source_ord=s.ord \
         WHERE s.language=?1 AND s.path=?2 AND c.artifact=?3",
    )?
    .query_row(params![language, path, artifact], |row| {
        Ok(Assigned {
            ord: row.get(0)?,
            content_hash: row.get(1)?,
            byte_length: row.get::<_, i64>(2)? as usize,
        })
    })
    .optional()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkTotals {
    pub documents: u64,
    pub claims: u64,
}

/// Stages one artifact's claims. `UNIQUE(artifact, fact_digest)` deduplicates; an ignored
/// insert compares the stored canonical bytes so a digest collision stays typed, and a proof ID
/// clash with no matching digest is `NonUniqueProof`.
pub struct StageClaimSink<'c> {
    conn: &'c Connection,
    budget: &'c StageBudget,
    artifact: u32,
    language: String,
    current: Option<i64>,
    totals: SinkTotals,
    rows_since_check: u64,
    fault: Option<anyhow::Error>,
}

fn stage_storage() -> ScipCaptureError {
    ScipCaptureError::InvalidArtifact("stage storage".into())
}

impl<'c> StageClaimSink<'c> {
    pub fn new(
        conn: &'c Connection,
        budget: &'c StageBudget,
        artifact: u32,
        language: &str,
    ) -> Self {
        Self {
            conn,
            budget,
            artifact,
            language: language.to_owned(),
            current: None,
            totals: SinkTotals {
                documents: 0,
                claims: 0,
            },
            rows_since_check: 0,
            fault: None,
        }
    }

    /// The storage error behind a `stage storage` rejection, if the sink hit one.
    pub fn take_fault(&mut self) -> Option<anyhow::Error> {
        self.fault.take()
    }

    fn record<T>(
        &mut self,
        result: rusqlite::Result<T>,
    ) -> std::result::Result<T, ScipCaptureError> {
        result.map_err(|error| {
            self.fault = Some(anyhow::Error::new(error).context("stage write"));
            stage_storage()
        })
    }

    /// Writes the artifact's document and claim counts and ends the sink.
    pub fn finish(self) -> Result<SinkTotals> {
        self.conn
            .execute(
                "UPDATE artifacts SET documents=?2, claims=?3 WHERE ordinal=?1",
                params![
                    self.artifact,
                    self.totals.documents as i64,
                    self.totals.claims as i64
                ],
            )
            .context("stage artifact totals")?;
        Ok(self.totals)
    }
}

impl ClaimSink for StageClaimSink<'_> {
    fn begin_document(
        &mut self,
        doc: &UnitDocument<'_>,
    ) -> std::result::Result<(), ScipCaptureError> {
        let begun = self.conn.execute_batch(UNIT_BEGIN);
        self.record(begun)?;
        let found = lookup_assigned(self.conn, self.artifact, &self.language, doc.path);
        self.current = self.record(found)?.map(|assigned| assigned.ord);
        let Some(ord) = self.current else {
            return Ok(());
        };
        let inserted = self.conn.prepare_cached(
            "INSERT INTO documents(source_ord, artifact, path, wire_digest, capped) VALUES(?1,?2,?3,?4,?5)",
        );
        let inserted = inserted.and_then(|mut statement| {
            statement.execute(params![
                ord,
                self.artifact,
                doc.path,
                doc.wire_digest,
                crate::scip_raw::DocumentCap::code(doc.cap)
            ])
        });
        match inserted {
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == ErrorCode::ConstraintViolation =>
            {
                Err(ScipCaptureError::InvalidArtifact(
                    "duplicate document path".into(),
                ))
            }
            other => self.record(other).map(|_| ()),
        }
    }

    fn claim(&mut self, claim: CapturedClaim) -> std::result::Result<(), ScipCaptureError> {
        let Some(ord) = self.current else {
            return Err(stage_storage());
        };
        let inserted = self.conn.prepare_cached(
            "INSERT INTO claims(artifact, source_ord, record_kind, fact_digest, proof_id, canonical_bytes, symbol, presence, target_root, target_lib, target_package, target_version) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,CASE WHEN ?9 IS NULL THEN NULL ELSE (SELECT ord FROM sources WHERE language=?10 AND path=?9) END,?11,?12,?13) ON CONFLICT DO NOTHING",
        );
        let (symbol, presence) = claim_presence(&claim);
        let target = claim.envelope.cargo_target.as_ref();
        let changed = inserted.and_then(|mut statement| {
            statement.execute(params![
                self.artifact,
                ord,
                claim.envelope.record_kind,
                claim.fact_digest,
                claim.captured_proof_id,
                claim.canonical_bytes,
                symbol,
                presence,
                target.map(|t| t.root.as_str()),
                claim.language,
                target.is_some_and(|t| t.is_lib()),
                target.map(|t| t.package.as_str()),
                target.and_then(|t| t.version.as_deref())
            ])
        });
        if self.record(changed)? == 0 {
            let stored = self.conn.prepare_cached(
                "SELECT canonical_bytes FROM claims WHERE artifact=?1 AND fact_digest=?2",
            );
            let stored = stored.and_then(|mut statement| {
                statement
                    .query_row(params![self.artifact, claim.fact_digest], |row| {
                        row.get::<_, Vec<u8>>(0)
                    })
                    .optional()
            });
            // An ignored insert is a repeat of the same claim, a digest collision, or a proof
            // ID already held by a different claim (`claims_proof`), as the in-memory path types.
            match self.record(stored)? {
                Some(bytes) if bytes == claim.canonical_bytes => {}
                Some(_) => return Err(ScipCaptureError::DigestCollision),
                None => return Err(ScipCaptureError::NonUniqueProof),
            }
        } else {
            self.totals.claims += 1;
            self.rows_since_check += 1;
        }
        Ok(())
    }

    fn end_document(&mut self) -> std::result::Result<(), ScipCaptureError> {
        let committed = self.conn.execute_batch(UNIT_COMMIT);
        self.record(committed)?;
        self.current = None;
        self.totals.documents += 1;
        if self.rows_since_check >= STAGE_CHECK_ROWS {
            self.rows_since_check = 0;
            self.budget.check()?;
        }
        Ok(())
    }
}
