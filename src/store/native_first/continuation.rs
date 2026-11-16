//! The index read connection one S continuation keeps across its yields, and the S
//! diagnostics. The scheduler that uses them is `crate::semantic_catchup`.
use super::*;

/// The index read connection, with its use guard, that one S continuation keeps across its
/// yields (#671): each unit takes a fresh snapshot on it instead of reopening the index.
/// Dropping the continuation closes it. Exclusive use transitions and retirement close it
/// first through [`Store::release_semantic_reads`]; the next unit then reopens.
#[derive(Default)]
pub(crate) struct HeldIndexRead(Mutex<Option<IndexConnection>>);

impl Store {
    /// Close every read connection an S continuation keeps across yields. The worker thread
    /// never runs a transition inside an S unit, so on that thread the connection is always
    /// in its slot here. A unit running on another thread keeps its connection to the end of
    /// its slice, exactly as an unheld connection would.
    pub(crate) fn release_semantic_reads(&self) {
        let held: Vec<_> = self
            .semantic_reads
            .lock()
            .unwrap()
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .collect();
        for read in held {
            let connection = read.0.lock().unwrap().take();
            drop(connection);
        }
    }

    /// How many S continuations keep an open read connection across yields now.
    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn held_semantic_reads_for_tests(&self) -> usize {
        let held: Vec<_> = self
            .semantic_reads
            .lock()
            .unwrap()
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .collect();
        held.iter()
            .filter(|read| read.0.lock().unwrap().is_some())
            .count()
    }

    /// Classify the index for exceptional recreation, as a read finding an obsolete or
    /// exceptional format does.
    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn mark_recreate_pending_for_tests(&self) {
        self.mark_recovery(RecoveryDisposition::RecreatePending);
    }

    /// One read snapshot on a continuation's held connection, opened on first use. A
    /// failed read closes the connection, so the next unit reopens and revalidates it.
    pub(super) fn with_held_read<T>(
        &self,
        held: &HeldIndexRead,
        read: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        let mut connection = held.0.lock().unwrap().take();
        let result = (|| {
            let db = match &mut connection {
                Some(db) => {
                    // What `connect_index` checks after its open, without reopening.
                    (|| {
                        self.ensure_not_recreate_pending()?;
                        self.identity.verify()?;
                        db._use_guard.verify()
                    })()
                    .inspect_err(|error| {
                        self.note_index_failure(error);
                    })?;
                    READ_GATE
                        .with(|gate| *gate.borrow_mut() = Arc::downgrade(&self.publication_gate));
                    db
                }
                None => connection.insert(self.cache()?),
            };
            let tx = storage_result(db.transaction())?;
            begin_read_snapshot(
                &tx,
                &self.roots.index_db(&self.identity),
                Duration::from_secs(5),
            )?;
            read(&tx)
        })()
        .map_err(|error| self.report_index_failure(error));
        if result.is_ok()
            && let Some(connection) = connection
        {
            *held.0.lock().unwrap() = Some(connection);
        }
        result
    }

    /// Opt-in (`TRELLIS_INDEX_DIAGNOSTICS=1`), best-effort S phase marker for profiling;
    /// never carries IDs, paths or pins.
    pub(crate) fn semantic_phase_diagnostic(phase: &'static str, detail: &'static str) {
        if std::env::var("TRELLIS_INDEX_DIAGNOSTICS").as_deref() == Ok("1") {
            use std::io::Write as _;
            let wall_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis());
            // SQLite allocates from the process heap, so `heap_in_use_mib` includes it;
            // its own counter is off (`SQLITE_CONFIG_MEMSTATUS=0`, #673).
            let (in_use, allocated) = crate::heap::bytes();
            let _ = writeln!(
                std::io::stderr().lock(),
                "semantic-s phase={phase} detail={detail} wall_ms={wall_ms} heap_in_use_mib={} heap_allocated_mib={}",
                in_use >> 20,
                allocated >> 20
            );
        }
    }

    /// Opt-in (`TRELLIS_INDEX_DIAGNOSTICS=1`) page-cache use of the S commit's
    /// writer just before COMMIT: whether it spilled to `index.db` (#674).
    pub(super) fn semantic_commit_cache_diagnostic(db: &Connection) {
        if std::env::var("TRELLIS_INDEX_DIAGNOSTICS").as_deref() == Ok("1") {
            use std::io::Write as _;
            let status = |op| {
                let (mut current, mut highwater) = (0, 0);
                // SAFETY: the handle belongs to a live connection used on this
                // thread, and the out-pointers are valid locals.
                unsafe {
                    rusqlite::ffi::sqlite3_db_status(
                        db.handle(),
                        op,
                        &mut current,
                        &mut highwater,
                        0,
                    )
                };
                current
            };
            let used = status(rusqlite::ffi::SQLITE_DBSTATUS_CACHE_USED);
            let spill = status(rusqlite::ffi::SQLITE_DBSTATUS_CACHE_SPILL);
            let written = status(rusqlite::ffi::SQLITE_DBSTATUS_CACHE_WRITE);
            let _ = writeln!(
                std::io::stderr().lock(),
                "semantic-s phase=commit_cache detail=before_commit cache_used_mib={} cache_spill_pages={spill} cache_write_pages={written}",
                used >> 20
            );
        }
    }
}
