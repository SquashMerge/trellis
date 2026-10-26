//! Machine-wide, disposable embedding cache. Only the daemon owns a writer.
//!
//! The budget caps the committed main database file. FULL auto-vacuum reclaims
//! evicted pages at commit. A transaction's temporary rollback journal is extra.
use std::{fs, path::Path, time::Duration};

use anyhow::{Result, ensure};
use rusqlite::{
    OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params, types::ValueRef,
};

use crate::store::{ProtectedSqliteConnection, topology::StorageBusy};

pub const DEFAULT_BYTE_BUDGET: u64 = 1024 * 1024 * 1024;
const MAX_DIMENSIONS: usize = 1024;
const BUSY_WAIT: Duration = Duration::from_secs(3);
const TABLE: &str = "CREATE TABLE vectors (model_id TEXT, card_hash TEXT, vector BLOB, touched INTEGER, PRIMARY KEY (model_id, card_hash)) WITHOUT ROWID";
const LRU_INDEX: &str = "CREATE INDEX vectors_lru ON vectors(touched, model_id, card_hash)";

#[derive(Debug)]
pub struct StoreUnavailable;
impl std::fmt::Display for StoreUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("store_unavailable: vector cache")
    }
}
impl std::error::Error for StoreUnavailable {}

#[derive(Debug)]
pub struct InvalidVector;
impl std::fmt::Display for InvalidVector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid vector cache key or vector")
    }
}
impl std::error::Error for InvalidVector {}

#[derive(Debug)]
pub struct Capacity;
impl std::fmt::Display for Capacity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("vector cache capacity cannot fit schema and entry")
    }
}
impl std::error::Error for Capacity {}

fn storage<T>(result: rusqlite::Result<T>) -> Result<T> {
    result.map_err(|error| match &error {
        rusqlite::Error::SqliteFailure(info, _)
            if matches!(
                info.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) || info.extended_code == rusqlite::ffi::SQLITE_READONLY_ROLLBACK =>
        {
            StorageBusy.into()
        }
        _ => anyhow::Error::new(error).context(StoreUnavailable),
    })
}
fn unavailable<T>(result: Result<T>) -> Result<T> {
    result.map_err(|error| {
        if error.is::<StorageBusy>() || error.is::<StoreUnavailable>() {
            error
        } else if error.chain().any(|cause| matches!(cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(info, _))
                if matches!(info.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                    || info.extended_code == rusqlite::ffi::SQLITE_READONLY_ROLLBACK)) {
            StorageBusy.into()
        } else {
            error.context(StoreUnavailable)
        }
    })
}
fn valid_key(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn keys(model_id: &str, card_hash: &str) -> Result<()> {
    if !valid_key(model_id) || !valid_key(card_hash) {
        return Err(InvalidVector.into());
    }
    Ok(())
}
fn valid_vector(vector: &[i8]) -> bool {
    !vector.is_empty() && vector.len() <= MAX_DIMENSIONS && vector.iter().any(|v| *v != 0)
}
fn present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}
fn open(path: &Path, writable: bool) -> Result<ProtectedSqliteConnection> {
    open_with_wait(path, writable, BUSY_WAIT)
}
fn open_with_wait(
    path: &Path,
    writable: bool,
    busy_wait: Duration,
) -> Result<ProtectedSqliteConnection> {
    unavailable((|| {
        ensure!(path.is_absolute(), "vector cache path must be absolute");
        let path = fs::canonicalize(path.parent().ok_or(StoreUnavailable)?)?
            .join(path.file_name().ok_or(StoreUnavailable)?);
        let path = path.as_path();
        // Reuse the index store's retained witnesses: closing a separate raw FD
        // while any SQLite connection is live would drop POSIX process locks.
        let _file = crate::store::retained_sqlite_file(path, writable, writable, true)?;
        let access = if writable {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        };
        let db =
            crate::store::protected_sqlite_open(path, access | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        storage(db.busy_timeout(busy_wait))?;
        if !writable {
            storage(db.pragma_update(None, "query_only", "ON"))?;
        }
        Ok(db)
    })())
}
#[derive(Debug)]
struct CacheFormat;
impl std::fmt::Display for CacheFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("incompatible vector cache format")
    }
}
impl std::error::Error for CacheFormat {}

/// Only disposable format/corruption failures warrant dropping and recreating
/// the cache. Contention, I/O and capacity failures must preserve it.
pub(crate) fn is_corrupt_cache(error: &anyhow::Error) -> bool {
    error.is::<CacheFormat>()
        || error.chain().any(|cause| {
            matches!(cause.downcast_ref::<rusqlite::Error>(),
                Some(rusqlite::Error::SqliteFailure(info, _))
                if matches!(info.code, rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase))
        })
}

fn validate(db: &rusqlite::Connection) -> Result<()> {
    let version: i64 = storage(db.pragma_query_value(None, "user_version", |row| row.get(0)))?;
    let journal: String = storage(db.pragma_query_value(None, "journal_mode", |row| row.get(0)))?;
    let vacuum: i64 = storage(db.pragma_query_value(None, "auto_vacuum", |row| row.get(0)))?;
    ensure!(
        version == 1 && journal == "delete" && vacuum == 1,
        anyhow::Error::new(CacheFormat).context(StoreUnavailable)
    );
    let mut statement = storage(db.prepare(
        "SELECT name, sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' ORDER BY name",
    ))?;
    let schema = storage(statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }))?;
    let schema = storage(schema.collect::<rusqlite::Result<Vec<_>>>())?;
    ensure!(
        schema
            == [
                ("vectors".to_owned(), TABLE.to_owned()),
                ("vectors_lru".to_owned(), LRU_INDEX.to_owned())
            ],
        anyhow::Error::new(CacheFormat).context(StoreUnavailable)
    );
    Ok(())
}

pub struct VectorCacheWriter {
    db: ProtectedSqliteConnection,
    byte_budget: u64,
    eviction_epoch: u64,
}
impl VectorCacheWriter {
    /// The daemon must prepare the private parent directory and own all writes.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_budget(path, DEFAULT_BYTE_BUDGET)
    }
    pub fn open_with_budget(path: &Path, byte_budget: u64) -> Result<Self> {
        match Self::open_once(path, byte_budget) {
            Err(error) if is_corrupt_cache(&error) => {
                unavailable(crate::store::discard_sqlite_cache(path))?;
                Self::open_once(path, byte_budget)
            }
            result => result,
        }
    }
    fn open_once(path: &Path, byte_budget: u64) -> Result<Self> {
        let mut db = open(path, true)?;
        storage(db.pragma_update(None, "synchronous", "FULL"))?;
        let version: i64 = storage(db.pragma_query_value(None, "user_version", |row| row.get(0)))?;
        if version == 0 {
            // This is a disposable database, not a migration of any old schema.
            storage(db.pragma_update(None, "auto_vacuum", "FULL"))?;
            let tx = storage(db.transaction_with_behavior(TransactionBehavior::Immediate))?;
            let count: i64 =
                storage(tx.query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0)))?;
            ensure!(
                count == 0,
                anyhow::Error::new(CacheFormat).context(StoreUnavailable)
            );
            storage(tx.execute_batch(TABLE))?;
            storage(tx.execute_batch(LRU_INDEX))?;
            storage(tx.pragma_update(None, "user_version", 1))?;
            storage(tx.commit())?;
        }
        {
            let tx = storage(db.transaction())?;
            validate(&tx)?;
        }
        let mut cache = Self {
            db,
            byte_budget,
            eviction_epoch: 0,
        };
        // A smaller configured budget takes effect even without another insert.
        let tx = storage(
            cache
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate),
        )?;
        let evicted = evict(&tx, byte_budget, &[])?;
        storage(tx.commit())?;
        cache.eviction_epoch += u64::from(evicted);
        Ok(cache)
    }
    /// Invalid input is rejected before any transaction. If this entry cannot
    /// fit even alone, the whole operation rolls back with typed Capacity.
    pub fn put(&mut self, model_id: &str, card_hash: &str, vector: &[i8]) -> Result<()> {
        self.put_many(&[(model_id, card_hash, vector)])
    }
    /// Atomically store a batch in input LRU order, with one transaction and one
    /// eviction pass. All inserted/replaced keys are retained on success; if
    /// they cannot fit together, typed Capacity rolls back the entire batch.
    /// Duplicate keys and invalid keys/vectors reject the whole batch before
    /// opening a transaction. An empty batch is a no-op.
    pub fn put_many(&mut self, entries: &[(&str, &str, &[i8])]) -> Result<()> {
        let mut retained = Vec::with_capacity(entries.len());
        for &(model_id, card_hash, vector) in entries {
            keys(model_id, card_hash)?;
            if !valid_vector(vector) || retained.contains(&(model_id, card_hash)) {
                return Err(InvalidVector.into());
            }
            retained.push((model_id, card_hash));
        }
        if entries.is_empty() {
            return Ok(());
        }
        let tx = storage(
            self.db
                .transaction_with_behavior(TransactionBehavior::Immediate),
        )?;
        for &(model_id, card_hash, vector) in entries {
            let bytes: Vec<u8> = vector.iter().map(|v| *v as u8).collect();
            let age = next_age(&tx)?;
            storage(tx.execute("INSERT INTO vectors VALUES (?1, ?2, ?3, ?4) ON CONFLICT(model_id,card_hash) DO UPDATE SET vector=excluded.vector,touched=excluded.touched", params![model_id, card_hash, bytes, age]))?;
        }
        let evicted = evict(&tx, self.byte_budget, &retained)?;
        storage(tx.commit())?;
        self.eviction_epoch += u64::from(evicted);
        Ok(())
    }
    /// Explicit owner operation. Reads never call this and never change age.
    pub fn touch(&mut self, model_id: &str, card_hash: &str) -> Result<()> {
        keys(model_id, card_hash)?;
        let tx = storage(
            self.db
                .transaction_with_behavior(TransactionBehavior::Immediate),
        )?;
        let age = next_age(&tx)?;
        storage(tx.execute(
            "UPDATE vectors SET touched=?3 WHERE model_id=?1 AND card_hash=?2",
            params![model_id, card_hash, age],
        ))?;
        let evicted = evict(&tx, self.byte_budget, &[])?;
        storage(tx.commit())?;
        self.eviction_epoch += u64::from(evicted);
        Ok(())
    }
    /// In-process invalidation only; inserts that retain every row do not advance it.
    pub fn eviction_epoch(&self) -> u64 {
        self.eviction_epoch
    }

    pub fn snapshot(&mut self) -> Result<VectorSnapshot<'_>> {
        snapshot(&mut self.db)
    }
}
fn next_age(tx: &Transaction<'_>) -> Result<i64> {
    let age: i64 = storage(tx.query_row(
        "SELECT COALESCE(MAX(touched),0) FROM vectors",
        [],
        |row| row.get(0),
    ))?;
    age.checked_add(1).ok_or_else(|| StoreUnavailable.into())
}
fn compact_bytes(db: &rusqlite::Connection) -> Result<u64> {
    let pages: i64 = storage(db.pragma_query_value(None, "page_count", |row| row.get(0)))?;
    let free: i64 = storage(db.pragma_query_value(None, "freelist_count", |row| row.get(0)))?;
    let size: i64 = storage(db.pragma_query_value(None, "page_size", |row| row.get(0)))?;
    let pages = u64::try_from(pages).map_err(|_| StoreUnavailable)?;
    let free = u64::try_from(free).map_err(|_| StoreUnavailable)?;
    let size = u64::try_from(size).map_err(|_| StoreUnavailable)?;
    // Pointer-map pages are included in page_count and never in freelist_count.
    // FULL auto-vacuum can remove some too, so this is conservative, not low.
    pages
        .checked_sub(free)
        .and_then(|pages| pages.checked_mul(size))
        .ok_or_else(|| StoreUnavailable.into())
}
fn evict(tx: &Transaction<'_>, budget: u64, retained: &[(&str, &str)]) -> Result<bool> {
    let mut evicted = false;
    while compact_bytes(tx)? > budget {
        let oldest = {
            let mut statement = storage(tx.prepare(
                "SELECT model_id,card_hash FROM vectors ORDER BY touched,model_id,card_hash",
            ))?;
            let mut rows = storage(statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            }))?;
            let mut oldest = None;
            for row in &mut rows {
                let (model, card) = storage(row)?;
                if !retained.contains(&(model.as_str(), card.as_str())) {
                    oldest = Some((model, card));
                    break;
                }
            }
            oldest
        };
        let Some((model, card)) = oldest else {
            return Err(Capacity.into());
        };
        storage(tx.execute(
            "DELETE FROM vectors WHERE model_id=?1 AND card_hash=?2",
            params![model, card],
        ))?;
        evicted = true;
    }
    Ok(evicted)
}

pub struct VectorCacheReader {
    db: ProtectedSqliteConnection,
}
impl VectorCacheReader {
    /// Missing cache is ordinary absence; this never creates any pathname.
    pub fn open_existing(path: &Path) -> Result<Option<Self>> {
        if !unavailable(present(path))? {
            return Ok(None);
        }
        let mut db = open(path, false)?;
        {
            let tx = storage(db.transaction())?;
            validate(&tx)?;
        }
        Ok(Some(Self { db }))
    }
    /// Query-only reader with contention waits bounded by the vector budget.
    /// Actual storage failures take precedence over a subsequent budget check.
    pub fn open_existing_with_budget(
        path: &Path,
        budget: &crate::search_embed::QueryBudget,
    ) -> Result<Option<Self>> {
        budget.check()?;
        if !unavailable(present(path))? {
            budget.check()?;
            return Ok(None);
        }
        let mut db = open_with_wait(path, false, BUSY_WAIT.min(budget.remaining()))?;
        budget.check()?;
        storage(db.busy_timeout(BUSY_WAIT.min(budget.remaining())))?;
        {
            let tx = storage(db.transaction())?;
            validate(&tx)?;
        }
        budget.check()?;
        Ok(Some(Self { db }))
    }

    pub fn snapshot_with_budget(
        &mut self,
        budget: &crate::search_embed::QueryBudget,
    ) -> Result<VectorSnapshot<'_>> {
        budget.check()?;
        storage(self.db.busy_timeout(BUSY_WAIT.min(budget.remaining())))?;
        let snapshot = snapshot(&mut self.db)?;
        budget.check()?;
        Ok(snapshot)
    }

    pub fn snapshot(&mut self) -> Result<VectorSnapshot<'_>> {
        snapshot(&mut self.db)
    }
}

pub struct VectorSnapshot<'a> {
    tx: Transaction<'a>,
    entries: u64,
}
fn snapshot(db: &mut rusqlite::Connection) -> Result<VectorSnapshot<'_>> {
    let tx = storage(db.transaction())?;
    // Pin the SQLite read snapshot immediately, before returning it to callers.
    let count: i64 = storage(tx.query_row("SELECT count(*) FROM vectors", [], |row| row.get(0)))?;
    let entries = u64::try_from(count).map_err(|_| StoreUnavailable)?;
    Ok(VectorSnapshot { tx, entries })
}
impl VectorSnapshot<'_> {
    /// Physical row count, including rows whose vector cannot be decoded.
    pub fn entry_count(&self) -> u64 {
        self.entries
    }
    /// Visit the active model in primary-key order without copying vector rows.
    /// Non-TEXT keys cannot match a valid card hash. Non-BLOB vectors are misses.
    /// Borrowed values live only for the callback; callback failures retain their
    /// original type, including caller cancellation and vector sub-budget errors.
    pub(crate) fn scan_model(
        &self,
        model_id: &str,
        visit: &mut impl FnMut(Option<&[u8]>, Option<&[u8]>) -> Result<()>,
    ) -> Result<()> {
        if !valid_key(model_id) {
            return Err(InvalidVector.into());
        }
        let mut statement =
            storage(self.tx.prepare(
                "SELECT card_hash,vector FROM vectors WHERE model_id=?1 ORDER BY card_hash",
            ))?;
        let mut rows = storage(statement.query([model_id]))?;
        while let Some(row) = storage(rows.next())? {
            let hash = match storage(row.get_ref(0))? {
                ValueRef::Text(bytes) => Some(bytes),
                _ => None,
            };
            let vector = match storage(row.get_ref(1))? {
                ValueRef::Blob(bytes) => Some(bytes),
                _ => None,
            };
            visit(hash, vector)?;
        }
        Ok(())
    }

    /// Invalid stored vector bytes are a miss. SQLite stepping/I/O failures are
    /// store_unavailable, never a fabricated hit or a partial vector.
    pub fn get(&self, model_id: &str, card_hash: &str) -> Result<Option<Vec<i8>>> {
        keys(model_id, card_hash)?;
        let vector = storage(
            self.tx
                .query_row(
                    "SELECT vector FROM vectors WHERE model_id=?1 AND card_hash=?2",
                    params![model_id, card_hash],
                    |row| {
                        Ok(match row.get_ref(0)? {
                            ValueRef::Blob(bytes) => {
                                Some(bytes.iter().map(|value| *value as i8).collect::<Vec<_>>())
                            }
                            _ => None,
                        })
                    },
                )
                .optional(),
        )?
        .flatten();
        Ok(vector.filter(|vector| valid_vector(vector)))
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VectorCacheReport {
    pub bytes: u64,
    pub entries: u64,
}
/// Read-only inventory. Counts stored rows (including decode misses) and the
/// main database plus any recognized sidecar bytes, without opening raw FDs.
pub fn report_existing(path: &Path) -> Result<VectorCacheReport> {
    let Some(mut reader) = VectorCacheReader::open_existing(path)? else {
        return Ok(VectorCacheReport::default());
    };
    let snapshot = reader.snapshot()?;
    let bytes = unavailable((|| {
        let mut bytes = fs::symlink_metadata(path)?.len();
        for suffix in ["-journal", "-wal", "-shm"] {
            let sidecar = path.with_file_name(format!(
                "{}{suffix}",
                path.file_name().unwrap().to_string_lossy()
            ));
            match fs::symlink_metadata(sidecar) {
                Ok(metadata) => {
                    bytes = bytes.checked_add(metadata.len()).ok_or(StoreUnavailable)?
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(bytes)
    })())?;
    Ok(VectorCacheReport {
        bytes,
        entries: snapshot.entry_count(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::types::Value;

    fn key(value: u8) -> String {
        format!("{value:064x}")
    }
    fn cache(budget: u64) -> (tempfile::TempDir, VectorCacheWriter) {
        let directory = tempfile::tempdir().unwrap();
        let writer =
            VectorCacheWriter::open_with_budget(&directory.path().join("vectors.db"), budget)
                .unwrap();
        (directory, writer)
    }

    #[test]
    fn query_reader_budget_bounds_wait_without_changing_storage_errors() {
        use crate::search_embed::{EmbedError, QueryBudget};
        use std::sync::Arc;
        use std::time::Instant;

        let (directory, writer) = cache(32 * 1024);
        let path = directory.path().join("vectors.db");
        drop(writer);
        let now = Instant::now();
        let expired = QueryBudget::new(now, Arc::new(move || now));
        let error = match VectorCacheReader::open_existing_with_budget(&path, &expired) {
            Err(error) => error,
            Ok(_) => panic!("expired budget opened the vector cache"),
        };
        assert!(matches!(
            error.downcast_ref::<EmbedError>(),
            Some(EmbedError::BudgetExceeded)
        ));
        let allowance = Duration::from_millis(250);
        let budget = QueryBudget::new(now + allowance, Arc::new(move || now));
        let mut reader = VectorCacheReader::open_existing_with_budget(&path, &budget)
            .unwrap()
            .unwrap();
        let wait: i64 = reader
            .db
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .unwrap();
        assert_eq!(wait, allowance.as_millis() as i64);
        assert_eq!(
            reader.snapshot_with_budget(&budget).unwrap().entry_count(),
            0
        );
        assert!(reader.snapshot_with_budget(&expired).is_err());
        drop(reader);
        let db = open(&path, true).unwrap();
        db.pragma_update(None, "user_version", 2).unwrap();
        drop(db);
        let error = match VectorCacheReader::open_existing_with_budget(&path, &budget) {
            Err(error) => error,
            Ok(_) => panic!("incompatible format became a budget fallback"),
        };
        assert!(error.is::<StoreUnavailable>());
    }

    #[test]
    fn ordered_scan_borrows_current_model_rows_and_keeps_malformed_misses() {
        let (_directory, mut writer) = cache(128 * 1024);
        let model = key(1);
        let other = key(2);
        for card in [6, 3, 5, 4] {
            writer.put(&model, &key(card), &[-128, 127]).unwrap();
        }
        writer.put(&other, &key(3), &[1, 2]).unwrap();
        writer
            .db
            .execute(
                "UPDATE vectors SET vector=CAST(X'80' AS TEXT) WHERE model_id=?1 AND card_hash=?2",
                params![model, key(4)],
            )
            .unwrap();
        writer
            .db
            .execute(
                "UPDATE vectors SET vector=zeroblob(2) WHERE model_id=?1 AND card_hash=?2",
                params![model, key(5)],
            )
            .unwrap();
        writer
            .db
            .execute(
                "UPDATE vectors SET vector=X'' WHERE model_id=?1 AND card_hash=?2",
                params![model, key(6)],
            )
            .unwrap();
        let ages = |db: &rusqlite::Connection| -> Vec<i64> {
            db.prepare("SELECT touched FROM vectors ORDER BY model_id,card_hash")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        let before = ages(&writer.db);
        {
            let snapshot = writer.snapshot().unwrap();
            let mut seen = Vec::new();
            snapshot
                .scan_model(&model, &mut |hash, bytes| {
                    seen.push((hash.unwrap().to_vec(), bytes.map(<[u8]>::to_vec)));
                    Ok(())
                })
                .unwrap();
            assert_eq!(
                seen.iter()
                    .map(|(hash, _)| hash.as_slice())
                    .collect::<Vec<_>>(),
                [3, 4, 5, 6]
                    .iter()
                    .map(|card| key(*card).into_bytes())
                    .collect::<Vec<_>>()
                    .iter()
                    .map(Vec::as_slice)
                    .collect::<Vec<_>>()
            );
            assert_eq!(seen[0].1.as_deref(), Some(&[128, 127][..]));
            assert_eq!(seen[1].1, None);
            assert_eq!(snapshot.get(&model, &key(5)).unwrap(), None);
            assert_eq!(snapshot.get(&model, &key(6)).unwrap(), None);
        }
        assert_eq!(ages(&writer.db), before);
    }

    #[test]
    fn scan_failure_after_rows_is_typed_and_snapshot_stays_pinned() {
        let (directory, mut writer) = cache(128 * 1024);
        let model = key(1);
        for card in 2..5 {
            writer.put(&model, &key(card), &[1, -2]).unwrap();
        }
        let mut reader = VectorCacheReader::open_existing(&directory.path().join("vectors.db"))
            .unwrap()
            .unwrap();
        let snapshot = reader.snapshot().unwrap();
        let mut seen = 0;
        let error = snapshot
            .scan_model(&model, &mut |_, _| {
                seen += 1;
                if seen == 2 {
                    return Err(StorageBusy.into());
                }
                Ok(())
            })
            .unwrap_err();
        assert_eq!(seen, 2);
        assert!(error.is::<StorageBusy>());
        // A callback failure never commits or releases the caller's snapshot.
        assert!(
            writer
                .put(&model, &key(2), &[3])
                .unwrap_err()
                .is::<StorageBusy>()
        );
        let mut old = Vec::new();
        snapshot
            .scan_model(&model, &mut |_, vector| {
                old.push(vector.unwrap().to_vec());
                Ok(())
            })
            .unwrap();
        assert_eq!(old, vec![vec![1, 254]; 3]);
        drop(snapshot);
        writer.put(&model, &key(2), &[3]).unwrap();
        let next = reader.snapshot().unwrap();
        let mut first = None;
        next.scan_model(&model, &mut |_, vector| {
            if first.is_none() {
                first = Some(vector.unwrap().to_vec());
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(first, Some(vec![3]));
    }

    #[test]
    fn bulk_real_cache_misses_and_late_cancellation_never_return_partial_ranking() {
        use std::cell::Cell;

        let (_directory, mut writer) = cache(128 * 1024);
        let model = key(1);
        let candidates: Vec<_> = (2..8)
            .map(|card| crate::search_rank::SearchCandidate {
                key: crate::search_rank::SearchCandidateKey {
                    version_id: i64::from(card),
                    syntax_id: format!("sid:v1:{card:032x}"),
                },
                card_hash: key(card),
            })
            .collect();
        let mut vector = vec![0; crate::search_embed::DIMENSIONS];
        vector[0] = -128;
        vector[1] = 127;
        for card in 2..7 {
            writer.put(&model, &key(card), &vector).unwrap();
        }
        for (card, value) in [
            (3, Value::Text("bad".into())),
            (4, Value::Blob(vec![0; crate::search_embed::DIMENSIONS])),
            (5, Value::Blob(vec![1; crate::search_embed::DIMENSIONS - 1])),
            (6, Value::Blob(vec![])),
        ] {
            writer
                .db
                .execute(
                    "UPDATE vectors SET vector=?1 WHERE model_id=?2 AND card_hash=?3",
                    params![value, model, key(card)],
                )
                .unwrap();
        }
        // This unmatched row is still stepped and checked, never admitted.
        writer.put(&model, &key(8), &vector).unwrap();
        let snapshot = writer.snapshot().unwrap();
        let checks = Cell::new(0);
        let ranking = crate::search_rank::rank_snapshot(
            &candidates,
            &[],
            Some(&snapshot),
            Some(&model),
            Some(&vector),
            &mut || {
                checks.set(checks.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(ranking.embedded, 1);
        assert_eq!(ranking.hits.len(), 1);
        assert_eq!(ranking.hits[0].key, candidates[0].key);
        let final_check = checks.get();
        checks.set(0);
        #[derive(Debug)]
        struct VectorBudget;
        impl std::fmt::Display for VectorBudget {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("vector budget exhausted")
            }
        }
        impl std::error::Error for VectorBudget {}
        let error = crate::search_rank::rank_snapshot(
            &candidates,
            &[],
            Some(&snapshot),
            Some(&model),
            Some(&vector),
            &mut || {
                checks.set(checks.get() + 1);
                if checks.get() == final_check {
                    return Err(VectorBudget.into());
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(error.is::<VectorBudget>());
    }

    #[test]
    fn scan_storage_fault_is_not_an_empty_success() {
        let (_directory, mut writer) = cache(32 * 1024);
        writer.db.execute("DROP TABLE vectors", []).unwrap();
        let tx = writer.db.transaction().unwrap();
        let snapshot = VectorSnapshot { tx, entries: 0 };
        let error = snapshot
            .scan_model(&key(1), &mut |_, _| Ok(()))
            .unwrap_err();
        assert!(error.is::<StoreUnavailable>());
    }

    #[test]
    fn bad_inputs_never_publish_and_bad_rows_are_misses() {
        let (_directory, mut writer) = cache(32 * 1024);
        let model = key(1);
        let card = key(2);
        for vector in [vec![], vec![0, 0], vec![1; 1025]] {
            assert!(
                writer
                    .put(&model, &card, &vector)
                    .unwrap_err()
                    .is::<InvalidVector>()
            );
        }
        assert!(
            writer
                .put("bad", &card, &[1])
                .unwrap_err()
                .is::<InvalidVector>()
        );
        assert!(
            writer
                .put(&model.to_uppercase().replace('1', "A"), &card, &[1])
                .unwrap_err()
                .is::<InvalidVector>()
        );
        assert_eq!(writer.snapshot().unwrap().entry_count(), 0);
        writer.put(&model, &card, &[1, -2]).unwrap();
        for value in [
            Value::Text("not a blob".into()),
            Value::Blob(vec![]),
            Value::Blob(vec![0, 0]),
            Value::Blob(vec![1; 1025]),
        ] {
            writer
                .db
                .execute("UPDATE vectors SET vector=?1", params![value])
                .unwrap();
            let snapshot = writer.snapshot().unwrap();
            assert_eq!(snapshot.entry_count(), 1);
            assert_eq!(snapshot.get(&model, &card).unwrap(), None);
        }
    }

    #[test]
    fn batch_validation_precedes_transaction_and_duplicate_keys_are_rejected() {
        let (directory, mut writer) = cache(32 * 1024);
        let model = key(1);
        let card = key(2);
        writer.put(&model, &card, &[1]).unwrap();
        let mut reader = VectorCacheReader::open_existing(&directory.path().join("vectors.db"))
            .unwrap()
            .unwrap();
        let snapshot = reader.snapshot().unwrap();
        // A write would fail busy while this snapshot is live. Validation and
        // an empty batch must not open a write transaction at all.
        writer.put_many(&[]).unwrap();
        for entries in [
            vec![
                (model.as_str(), card.as_str(), &[2][..]),
                ("bad", card.as_str(), &[1][..]),
            ],
            vec![
                (model.as_str(), card.as_str(), &[2][..]),
                (model.as_str(), card.as_str(), &[3][..]),
            ],
            vec![(model.as_str(), card.as_str(), &[0][..])],
        ] {
            assert!(writer.put_many(&entries).unwrap_err().is::<InvalidVector>());
        }
        assert_eq!(snapshot.get(&model, &card).unwrap(), Some(vec![1]));
    }

    #[test]
    fn batch_success_assigns_input_order_ages_and_busy_commit_rolls_back() {
        let (directory, mut writer) = cache(128 * 1024);
        let model = key(1);
        let cards: Vec<_> = (2..18).map(key).collect();
        let vector = [3; 1024];
        let entries: Vec<_> = cards
            .iter()
            .map(|card| (model.as_str(), card.as_str(), vector.as_slice()))
            .collect();
        writer.put_many(&entries).unwrap();
        let ages: Vec<i64> = writer
            .db
            .prepare("SELECT touched FROM vectors ORDER BY card_hash")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ages, (1..=16).collect::<Vec<_>>());
        let mut reader = VectorCacheReader::open_existing(&directory.path().join("vectors.db"))
            .unwrap()
            .unwrap();
        let snapshot = reader.snapshot().unwrap();
        assert!(
            writer
                .put_many(&[(&model, &cards[0], &[-1]), (&model, &key(18), &[2]),])
                .unwrap_err()
                .is::<StorageBusy>()
        );
        drop(snapshot);
        let snapshot = writer.snapshot().unwrap();
        assert_eq!(snapshot.entry_count(), 16);
        assert_eq!(
            snapshot.get(&model, &cards[0]).unwrap(),
            Some(vector.to_vec())
        );
        assert_eq!(snapshot.get(&model, &key(18)).unwrap(), None);
    }

    #[test]
    fn batch_capacity_rolls_back_replacement_new_rows_and_lru_ages() {
        let (_directory, mut writer) = cache(32 * 1024);
        let model = key(1);
        let original = key(2);
        writer.put(&model, &original, &[-1]).unwrap();
        let before: i64 = writer
            .db
            .query_row("SELECT touched FROM vectors", [], |row| row.get(0))
            .unwrap();
        let cards: Vec<_> = (2..18).map(key).collect();
        let vector = [3; 1024];
        let entries: Vec<_> = cards
            .iter()
            .map(|card| (model.as_str(), card.as_str(), vector.as_slice()))
            .collect();
        assert!(writer.put_many(&entries).unwrap_err().is::<Capacity>());
        let after: i64 = writer
            .db
            .query_row("SELECT touched FROM vectors", [], |row| row.get(0))
            .unwrap();
        assert_eq!(before, after);
        let snapshot = writer.snapshot().unwrap();
        assert_eq!(snapshot.entry_count(), 1);
        assert_eq!(snapshot.get(&model, &original).unwrap(), Some(vec![-1]));
        assert_eq!(snapshot.get(&model, &cards[1]).unwrap(), None);
    }

    #[test]
    fn corruption_predicate_excludes_busy_io_and_capacity() {
        assert!(is_corrupt_cache(
            &anyhow::Error::new(CacheFormat).context(StoreUnavailable)
        ));
        for code in [rusqlite::ffi::SQLITE_CORRUPT, rusqlite::ffi::SQLITE_NOTADB] {
            let error = storage::<()>(Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            )))
            .unwrap_err();
            assert!(is_corrupt_cache(&error));
        }
        for code in [
            rusqlite::ffi::SQLITE_BUSY,
            rusqlite::ffi::SQLITE_IOERR,
            rusqlite::ffi::SQLITE_FULL,
        ] {
            let error = storage::<()>(Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            )))
            .unwrap_err();
            assert!(!is_corrupt_cache(&error));
        }
        assert!(!is_corrupt_cache(&Capacity.into()));
        assert!(!is_corrupt_cache(&anyhow::Error::new(
            std::io::Error::from(std::io::ErrorKind::Other)
        )));
    }

    #[test]
    fn invalid_utf8_text_vector_is_a_cache_miss() {
        let (_directory, mut writer) = cache(32 * 1024);
        let model = key(1);
        let card = key(2);
        writer.put(&model, &card, &[1, -2]).unwrap();
        writer
            .db
            .execute("UPDATE vectors SET vector=CAST(X'80' AS TEXT)", [])
            .unwrap();
        let snapshot = writer.snapshot().unwrap();
        assert_eq!(snapshot.get(&model, &card).unwrap(), None);
    }

    #[test]
    fn lru_is_model_wide_and_only_owner_touch_changes_age() {
        let (directory, mut writer) = cache(32 * 1024);
        let model_a = key(1);
        let model_b = key(2);
        let card = key(3);
        writer.put(&model_a, &card, &[1; 1024]).unwrap();
        writer.put(&model_b, &card, &[-1; 1024]).unwrap();
        assert_eq!(
            writer.eviction_epoch(),
            0,
            "non-evicting inserts preserve completed heads"
        );
        let mut evicted = false;
        for number in 4..40 {
            // Reading model A must not refresh its LRU age; the owner explicitly
            // protects model B while filling the small physical page budget.
            assert!(
                writer
                    .snapshot()
                    .unwrap()
                    .get(&model_a, &card)
                    .unwrap()
                    .is_some()
            );
            writer.touch(&model_b, &card).unwrap();
            assert!(
                fs::metadata(directory.path().join("vectors.db"))
                    .unwrap()
                    .len()
                    <= 32 * 1024
            );
            writer.put(&model_a, &key(number), &[3; 1024]).unwrap();
            assert!(
                fs::metadata(directory.path().join("vectors.db"))
                    .unwrap()
                    .len()
                    <= 32 * 1024
            );
            if writer
                .snapshot()
                .unwrap()
                .get(&model_a, &card)
                .unwrap()
                .is_none()
            {
                evicted = true;
                break;
            }
        }
        assert!(evicted);
        assert!(
            writer.eviction_epoch() > 0,
            "only committed eviction invalidates completed heads"
        );
        assert_eq!(
            writer.snapshot().unwrap().get(&model_b, &card).unwrap(),
            Some(vec![-1; 1024])
        );
    }

    #[test]
    fn touch_growth_evicts_before_committing_above_budget() {
        let (directory, mut writer) = cache(32 * 1024);
        let path = directory.path().join("vectors.db");
        let model = key(1);
        let card = key(2);
        // At a 4096-byte SQLite page, this record is just below the inline
        // payload limit for a WITHOUT ROWID table. Growing touched from 127
        // to 128 needs one extra byte and allocates an overflow page.
        let page_size: i64 = writer
            .db
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .unwrap();
        assert_eq!(page_size, 4096);
        writer.put(&model, &card, &[1; 865]).unwrap();
        writer
            .db
            .execute("UPDATE vectors SET touched=127", [])
            .unwrap();
        let budget = fs::metadata(&path).unwrap().len();
        writer.byte_budget = budget;

        // Prove the fixture crosses a physical page boundary without trimming.
        // Roll it back so the owner touch starts from exactly the same state.
        {
            let tx = writer.db.transaction().unwrap();
            tx.execute("UPDATE vectors SET touched=128", []).unwrap();
            assert!(compact_bytes(&tx).unwrap() > budget);
            tx.rollback().unwrap();
        }
        assert_eq!(fs::metadata(&path).unwrap().len(), budget);
        assert_eq!(writer.snapshot().unwrap().entry_count(), 1);

        writer.touch(&model, &card).unwrap();
        assert!(fs::metadata(&path).unwrap().len() <= budget);
        // Touch may evict even the touched entry: this optional cache promises
        // the committed cap, not retention of any particular row.
        let snapshot = writer.snapshot().unwrap();
        assert_eq!(snapshot.entry_count(), 0);
        assert_eq!(snapshot.get(&model, &card).unwrap(), None);
    }

    #[test]
    fn reader_pins_snapshot_and_writer_contention_is_typed() {
        let (directory, mut writer) = cache(32 * 1024);
        let path = directory.path().join("vectors.db");
        let model = key(1);
        let card = key(2);
        writer.put(&model, &card, &[1]).unwrap();
        let mut reader = VectorCacheReader::open_existing(&path).unwrap().unwrap();
        let snapshot = reader.snapshot().unwrap();
        assert_eq!(snapshot.entry_count(), 1);
        let error = writer.put(&model, &card, &[2]).unwrap_err();
        assert!(error.is::<StorageBusy>(), "{error:#}");
        assert_eq!(snapshot.get(&model, &card).unwrap(), Some(vec![1]));
        drop(snapshot);
        writer.put(&model, &card, &[2]).unwrap();
        assert_eq!(
            reader.snapshot().unwrap().get(&model, &card).unwrap(),
            Some(vec![2])
        );
    }

    #[test]
    fn writer_recreates_invalid_cache_while_readers_fail_typed() {
        let (directory, writer) = cache(32 * 1024);
        let path = directory.path().join("vectors.db");
        drop(writer);
        for damage in ["version", "schema", "corrupt"] {
            if damage == "corrupt" {
                fs::write(&path, b"not a SQLite database").unwrap();
            } else {
                let db = open(&path, true).unwrap();
                if damage == "version" {
                    db.pragma_update(None, "user_version", 2).unwrap();
                } else {
                    db.execute_batch("DROP INDEX vectors_lru").unwrap();
                }
            }
            let error = match VectorCacheReader::open_existing(&path) {
                Err(error) => error,
                Ok(_) => panic!("invalid cache accepted: {damage}"),
            };
            assert!(error.is::<StoreUnavailable>(), "{damage}: {error:#}");
            let mut writer = VectorCacheWriter::open(&path).unwrap();
            assert_eq!(writer.snapshot().unwrap().entry_count(), 0);
            writer.put(&key(1), &key(2), &[1, -2]).unwrap();
            assert_eq!(
                writer.snapshot().unwrap().get(&key(1), &key(2)).unwrap(),
                Some(vec![1, -2]),
                "{damage}"
            );
        }
    }

    #[test]
    fn recreation_defers_while_a_reader_connection_is_live() {
        let (directory, writer) = cache(32 * 1024);
        let path = directory.path().join("vectors.db");
        writer.db.pragma_update(None, "user_version", 2).unwrap();
        drop(writer);
        let reader = open(&path, false).unwrap();
        let error = match VectorCacheWriter::open(&path) {
            Err(error) => error,
            Ok(_) => panic!("recreated a cache with a live reader"),
        };
        assert!(error.is::<StorageBusy>(), "{error:#}");
        let version: i64 = reader
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
        drop(reader);
        let mut writer = VectorCacheWriter::open(&path).unwrap();
        writer.put(&key(1), &key(2), &[3]).unwrap();
        assert_eq!(
            writer.snapshot().unwrap().get(&key(1), &key(2)).unwrap(),
            Some(vec![3])
        );
    }

    #[test]
    fn replacement_and_smaller_budget_reclaim_pages_and_capacity_rolls_back() {
        let (directory, mut writer) = cache(128 * 1024);
        let path = directory.path().join("vectors.db");
        let model = key(1);
        for card in 0..20 {
            writer.put(&model, &key(card), &[1; 1024]).unwrap();
        }
        let full_size = fs::metadata(&path).unwrap().len();
        drop(writer);
        let mut writer = VectorCacheWriter::open_with_budget(&path, 32 * 1024).unwrap();
        assert!(fs::metadata(&path).unwrap().len() < full_size);
        assert!(fs::metadata(&path).unwrap().len() <= 32 * 1024);
        let count = writer.snapshot().unwrap().entry_count();
        writer.put(&model, &key(19), &[-128]).unwrap();
        assert_eq!(writer.snapshot().unwrap().entry_count(), count);
        // One page cannot fit this database's schema. Measure it rather than
        // pinning an arbitrary tiny byte value in the capacity fault fixture.
        let page_size: i64 = writer
            .db
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .unwrap();
        let page_size = u64::try_from(page_size).unwrap();
        writer.byte_budget = page_size;
        assert!(
            writer
                .put(&model, &key(21), &[1])
                .unwrap_err()
                .is::<Capacity>()
        );
        assert_eq!(writer.snapshot().unwrap().entry_count(), count);
        assert_eq!(
            writer.snapshot().unwrap().get(&model, &key(19)).unwrap(),
            Some(vec![-128])
        );
        drop(writer);
        assert!(
            matches!(VectorCacheWriter::open_with_budget(&path, page_size), Err(error) if error.is::<Capacity>())
        );
    }
}
