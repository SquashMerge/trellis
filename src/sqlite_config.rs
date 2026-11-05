//! Process-wide SQLite configuration, set once before the first connection (#673).

/// Turn off SQLite's global memory statistics. With them on, every SQLite allocation and
/// free takes one process-wide mutex, which the S thread and the reader threads contended
/// for (`__psynch_mutexwait` was 22–26% across threads during catch-up). Trellis sets no
/// soft heap limit and relies on no SQLite memory statistic.
///
/// SQLite accepts this only before it initializes, so call it at process start, before any
/// connection opens. Returns whether SQLite accepted it; a later call is a no-op.
pub fn disable_memory_statistics() -> bool {
    static ACCEPTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ACCEPTED.get_or_init(|| {
        // SAFETY: SQLITE_CONFIG_MEMSTATUS takes one int. sqlite3_config must precede other
        // SQLite use; the call-at-start contract keeps it there, and after initialization
        // SQLite refuses it with SQLITE_MISUSE.
        let rc = unsafe {
            rusqlite::ffi::sqlite3_config(
                rusqlite::ffi::SQLITE_CONFIG_MEMSTATUS,
                0 as std::os::raw::c_int,
            )
        };
        rc == rusqlite::ffi::SQLITE_OK
    })
}
