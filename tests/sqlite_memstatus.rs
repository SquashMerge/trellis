//! #673: SQLite's global memory statistics are off for the process. This binary holds this
//! one test, so SQLite is not yet initialized when it starts.

#[test]
fn memory_statistics_are_off_before_the_first_connection() {
    assert!(trellis::sqlite_config::disable_memory_statistics());
    assert!(
        trellis::sqlite_config::disable_memory_statistics(),
        "a later call is a no-op"
    );
    let db = rusqlite::Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE t(x);
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<10000)
         INSERT INTO t SELECT randomblob(100) FROM n;",
    )
    .unwrap();
    let count: i64 = db
        .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 10_000);
    // SAFETY: reads SQLite's global counter, which it does not keep with statistics off.
    assert_eq!(unsafe { rusqlite::ffi::sqlite3_memory_used() }, 0);
}
