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
        storage(db.busy_timeout(BUSY_WAIT))?;
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
}
impl VectorCacheWriter {
    /// The daemon must prepare the private parent directory and own all writes.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_budget(path, DEFAULT_BYTE_BUDGET)
    }
    pub fn open_with_budget(path: &Path, byte_budget: u64) -> Result<Self> {
        match Self::open_once(path, byte_budget) {
            Err(error) if error.is::<CacheFormat>() || error.chain().any(|cause| {
                matches!(cause.downcast_ref::<rusqlite::Error>(),
                    Some(rusqlite::Error::SqliteFailure(info, _))
                    if matches!(info.code, rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase))
            }) => {
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
        let mut cache = Self { db, byte_budget };
        // A smaller configured budget takes effect even without another insert.
        let tx = storage(
            cache
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate),
        )?;
        evict(&tx, byte_budget, None)?;
        storage(tx.commit())?;
        Ok(cache)
    }
    /// Invalid input is rejected before any transaction. If this entry cannot
    /// fit even alone, the whole operation rolls back with typed Capacity.
    pub fn put(&mut self, model_id: &str, card_hash: &str, vector: &[i8]) -> Result<()> {
        keys(model_id, card_hash)?;
        if !valid_vector(vector) {
            return Err(InvalidVector.into());
        }
        let bytes: Vec<u8> = vector.iter().map(|v| *v as u8).collect();
        let tx = storage(
            self.db
                .transaction_with_behavior(TransactionBehavior::Immediate),
        )?;
        let age = next_age(&tx)?;
        storage(tx.execute("INSERT INTO vectors VALUES (?1, ?2, ?3, ?4) ON CONFLICT(model_id,card_hash) DO UPDATE SET vector=excluded.vector,touched=excluded.touched", params![model_id, card_hash, bytes, age]))?;
        evict(&tx, self.byte_budget, Some((model_id, card_hash)))?;
        storage(tx.commit())
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
        evict(&tx, self.byte_budget, None)?;
        storage(tx.commit())
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
fn evict(tx: &Transaction<'_>, budget: u64, inserted: Option<(&str, &str)>) -> Result<()> {
    while compact_bytes(tx)? > budget {
        let oldest: Option<(String, String)> = storage(tx.query_row("SELECT model_id,card_hash FROM vectors ORDER BY touched,model_id,card_hash LIMIT 1", [], |row| Ok((row.get(0)?,row.get(1)?))).optional())?;
        let Some((model, card)) = oldest else {
            return Err(Capacity.into());
        };
        if inserted.is_some_and(|key| key == (model.as_str(), card.as_str())) {
            return Err(Capacity.into());
        }
        storage(tx.execute(
            "DELETE FROM vectors WHERE model_id=?1 AND card_hash=?2",
            params![model, card],
        ))?;
    }
    Ok(())
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
