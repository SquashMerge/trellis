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

const STAGE_SCHEMA: &str = "
CREATE TABLE header(singleton INTEGER PRIMARY KEY CHECK(singleton=1), descriptor_sha256 TEXT NOT NULL, descriptor_bytes BLOB NOT NULL);
CREATE TABLE sources(ord INTEGER PRIMARY KEY, language TEXT NOT NULL, path TEXT NOT NULL, content_hash TEXT NOT NULL, byte_length INTEGER NOT NULL, UNIQUE(language,path));
CREATE TABLE coverage(source_ord INTEGER PRIMARY KEY REFERENCES sources(ord), artifact INTEGER, exclusion TEXT, CHECK((artifact IS NULL)<>(exclusion IS NULL)));
CREATE INDEX coverage_artifact ON coverage(artifact, source_ord);
CREATE TABLE artifacts(ordinal INTEGER PRIMARY KEY, path TEXT NOT NULL, language TEXT NOT NULL, sha256 TEXT NOT NULL UNIQUE, byte_length INTEGER NOT NULL, documents INTEGER NOT NULL DEFAULT 0, claims INTEGER NOT NULL DEFAULT 0);
CREATE TABLE documents(source_ord INTEGER PRIMARY KEY, artifact INTEGER NOT NULL, path TEXT NOT NULL, wire_digest TEXT NOT NULL, UNIQUE(artifact,path));
CREATE TABLE claims(claim_id INTEGER PRIMARY KEY, artifact INTEGER NOT NULL, source_ord INTEGER NOT NULL, record_kind TEXT NOT NULL, fact_digest TEXT NOT NULL, proof_id TEXT NOT NULL, canonical_bytes BLOB NOT NULL, UNIQUE(artifact,fact_digest));
CREATE INDEX claims_order ON claims(source_ord, canonical_bytes);
CREATE UNIQUE INDEX claims_proof ON claims(proof_id);
";

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
/// insert compares the stored canonical bytes so a digest collision stays typed.
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
            "INSERT INTO documents(source_ord, artifact, path, wire_digest) VALUES(?1,?2,?3,?4)",
        );
        let inserted = inserted.and_then(|mut statement| {
            statement.execute(params![ord, self.artifact, doc.path, doc.wire_digest])
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
            "INSERT INTO claims(artifact, source_ord, record_kind, fact_digest, proof_id, canonical_bytes) \
             VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT DO NOTHING",
        );
        let changed = inserted.and_then(|mut statement| {
            statement.execute(params![
                self.artifact,
                ord,
                claim.envelope.record_kind,
                claim.fact_digest,
                claim.captured_proof_id,
                claim.canonical_bytes
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
            if self.record(stored)?.as_deref() != Some(claim.canonical_bytes.as_slice()) {
                return Err(ScipCaptureError::DigestCollision);
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
