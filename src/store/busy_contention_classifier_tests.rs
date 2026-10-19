use super::*;
#[test]
fn real_rollback_journal_sqlite_busy_is_transient_but_invariant_busy_is_not() {
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join("contended.db");
    let writer = Connection::open(&file).unwrap();
    writer.execute_batch("PRAGMA journal_mode=DELETE; CREATE TABLE t (value INTEGER); BEGIN IMMEDIATE; INSERT INTO t VALUES (1)").unwrap();
    let reader = Connection::open(&file).unwrap();
    reader.busy_timeout(std::time::Duration::ZERO).unwrap();
    let sqlite = reader.execute("INSERT INTO t VALUES (2)", []).unwrap_err();
    let wrapped = anyhow::Error::new(sqlite).context("publish commit");
    assert!(super::transient_storage_contention(&wrapped));
    assert!(super::nonterminal_storage_busy(&wrapped));
    assert!(super::transient_storage_contention(&anyhow::anyhow!(
        "storage_busy: SQLite lock contention"
    )));
    let invariant = anyhow::anyhow!("storage_busy: cached claim changed");
    assert!(!super::transient_storage_contention(&invariant));
    assert!(super::nonterminal_storage_busy(&invariant));
    writer.execute_batch("ROLLBACK").unwrap();
}
