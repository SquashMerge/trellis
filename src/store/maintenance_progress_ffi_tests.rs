use super::*;
#[test]
fn publisher_wait_interrupts_sqlite_and_removes_handler() {
    let mut db = Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE rows(id INTEGER PRIMARY KEY);
             WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<600)
             INSERT INTO rows(id) SELECT x FROM n",
    )
    .unwrap();
    let gate = PublicationGate::default();
    let progress = MaintenanceProgress { gate: &gate };
    assert!(progress.check());
    let handler = install_maintenance_progress(&db, &progress);
    let tx = db.transaction().unwrap();
    tx.execute("DELETE FROM rows WHERE id<=300", []).unwrap();
    {
        let mut state = gate.state.lock().unwrap();
        state.waiting = 1;
        gate.publish_waiting(&state);
    }
    let interrupted = tx.execute("DELETE FROM rows WHERE id>300", []).unwrap_err();
    assert!(matches!(
        interrupted,
        rusqlite::Error::SqliteFailure(info, _)
            if info.code == rusqlite::ErrorCode::OperationInterrupted
    ));
    drop(tx);
    drop(handler);
    assert_eq!(
        db.query_row("SELECT count(*) FROM rows", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        600
    );
    assert_eq!(db.execute("DELETE FROM rows", []).unwrap(), 600);
}
