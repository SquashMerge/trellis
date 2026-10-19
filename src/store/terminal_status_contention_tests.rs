use super::*;
use anyhow::Context;

#[test]
fn only_direct_sqlite_busy_is_terminal_read_retryable() {
    let direct: anyhow::Error = SqliteContention(()).into();
    assert!(terminal_status_sqlite_contention(&direct));
    for code in [rusqlite::ffi::SQLITE_BUSY, rusqlite::ffi::SQLITE_LOCKED] {
        let error: anyhow::Error =
            rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None).into();
        assert!(terminal_status_sqlite_contention(&error));
        let wrapped = Err::<(), _>(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            None,
        ))
        .context("root_changed: publication guard failed")
        .unwrap_err();
        assert!(!terminal_status_sqlite_contention(&wrapped));
    }
    for guard in [
        "root_changed",
        "index_not_ready",
        "schema_invalid",
        "pin_mismatch",
    ] {
        let nested = Err::<(), _>(SqliteContention(()))
            .context(guard)
            .unwrap_err();
        assert!(!terminal_status_sqlite_contention(&nested), "{guard}");
    }
    let selected = Err::<(), _>(SqliteContention(()))
        .context(SelectedIntegrity(
            "incompatible_index: selected evidence".into(),
        ))
        .unwrap_err();
    assert!(!terminal_status_sqlite_contention(&selected));
    let leader: anyhow::Error = topology::StorageBusy.into();
    assert!(!terminal_status_sqlite_contention(&leader));
    assert!(!terminal_status_sqlite_contention(&anyhow::anyhow!(
        "storage_busy: SQLite lock contention"
    )));
}

#[test]
fn cli_read_busy_retry_is_direct_and_expiry_is_typed() {
    let raw: anyhow::Error =
        rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY), None)
            .into();
    assert!(cli_read_retryable_contention(&raw));
    assert!(cli_read_retryable_contention(&SqliteContention(()).into()));
    assert!(cli_read_retryable_contention(&topology::StorageBusy.into()));
    for exact in [
        "storage_busy: index journal sidecar present",
        "storage_busy: requests.db initialization in progress",
    ] {
        assert!(cli_read_retryable_contention(&anyhow::anyhow!(exact)));
        assert!(!cli_read_retryable_contention(
            &Err::<(), _>(anyhow::anyhow!(exact))
                .context("root_changed: guard failed")
                .unwrap_err()
        ));
    }
    assert!(!cli_read_retryable_contention(
        &Err::<(), _>(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        ))
        .context("incompatible_index: selected proof failed")
        .unwrap_err()
    ));
    assert!(!cli_read_retryable_contention(&anyhow::anyhow!(
        "storage_busy: cached claim changed"
    )));
    let expiry = cli_read_busy_expired();
    assert!(expiry.downcast_ref::<CliReadBusy>().is_some());
    assert_eq!(
        expiry.to_string(),
        "storage_busy: CLI read contention wait expired"
    );
}
