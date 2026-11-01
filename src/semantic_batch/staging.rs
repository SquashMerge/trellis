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

pub const STAGE_PREFIX: &str = "semantic-stage-";
const STAGE_DB: &str = "stage.db";

/// `documents.capped` is 1 for a document admitted under `scip_raw::OCCURRENCE_CAP_EXCEEDED`.
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
CREATE TABLE documents(source_ord INTEGER PRIMARY KEY, artifact INTEGER NOT NULL, path TEXT NOT NULL, wire_digest TEXT NOT NULL, capped INTEGER NOT NULL CHECK(capped IN (0,1)), UNIQUE(artifact,path));
CREATE TABLE claims(claim_id INTEGER PRIMARY KEY, artifact INTEGER NOT NULL, source_ord INTEGER NOT NULL, record_kind TEXT NOT NULL, fact_digest TEXT NOT NULL, proof_id TEXT NOT NULL, canonical_bytes BLOB NOT NULL, symbol TEXT, presence INTEGER NOT NULL, target_root INTEGER, target_lib INTEGER NOT NULL DEFAULT 0, target_package TEXT, target_version TEXT, UNIQUE(artifact,fact_digest));
CREATE INDEX claims_order ON claims(source_ord, canonical_bytes);
CREATE UNIQUE INDEX claims_proof ON claims(proof_id);
CREATE INDEX claims_symbol ON claims(symbol, presence, target_root) WHERE symbol IS NOT NULL;
CREATE TABLE join_state(singleton INTEGER PRIMARY KEY CHECK(singleton=1), generation TEXT NOT NULL, revision INTEGER NOT NULL, attempt INTEGER NOT NULL);
CREATE TABLE nat_done(rev INTEGER PRIMARY KEY);
CREATE TABLE nat_docs(rev INTEGER NOT NULL, ord INTEGER NOT NULL, language TEXT NOT NULL, path TEXT NOT NULL, content_hash TEXT NOT NULL, byte_length INTEGER NOT NULL, reserve INTEGER, PRIMARY KEY(rev, ord), UNIQUE(rev, language, path)) WITHOUT ROWID;
CREATE TABLE nat_declarations(rev INTEGER NOT NULL, doc_ord INTEGER NOT NULL, syntax_id BLOB NOT NULL, n INTEGER NOT NULL, named INTEGER NOT NULL, name_ok INTEGER NOT NULL, PRIMARY KEY(rev, doc_ord, syntax_id, n)) WITHOUT ROWID;
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
}

fn open_connection(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).context("stage open")?;
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
        Ok(self.conn.as_ref().expect("stage connection just opened"))
    }

    /// Drops the connection. The publishing writer attaches `stage.db` only after this.
    pub fn release_connection(&mut self) {
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
        let begun = self.conn.execute_batch("BEGIN");
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
                doc.occurrence_cap_exceeded
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
        let committed = self.conn.execute_batch("COMMIT");
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
