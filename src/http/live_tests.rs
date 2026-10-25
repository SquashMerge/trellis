use super::*;

#[test]
fn outer_http_fence_covers_native_composition() {
    for path in [
        "/api/status",
        "/api/tree",
        "/api/files",
        "/api/methods",
        "/api/sequence",
        "/api/classes",
        "/api/class-diagram",
        "/api/navigation",
        "/api/source",
        "/api/dependencies",
        "/api/dependencies/symbols",
        "/api/dependencies/source",
        "/api/rust-sources/tree",
    ] {
        assert!(native_response_route(path), "{path}");
    }
    for path in [
        "/api/rust-sources/file",
        "/api/healthz",
        "/api/jobs/current",
    ] {
        assert!(!native_response_route(path), "{path}");
    }
}
#[test]
fn api_error_walks_context_but_never_masks_sqlite_ioerr_rdlock() {
    let busy = anyhow::anyhow!("storage_busy: SQLite lock contention").context("durable enqueue");
    let mapped = ApiError::from(busy);
    assert_eq!((mapped.0, mapped.1), (StatusCode::CONFLICT, "storage_busy"));
    let root = anyhow::anyhow!("root_changed: pathname replaced").context("durable enqueue");
    let mapped = ApiError::from(root);
    assert_eq!((mapped.0, mapped.1), (StatusCode::CONFLICT, "root_changed"));
    let ioerr = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR_RDLOCK),
        None,
    );
    let mapped = ApiError::from(
        anyhow::Error::new(ioerr).context("storage_busy: wrapped database I/O failure"),
    );
    assert_eq!(
        (mapped.0, mapped.1),
        (StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
    );
    let rebuild = anyhow::Error::new(crate::store::RebuildInProgress {
        since: std::time::SystemTime::now(),
        documents: 10_000,
    })
    .context("selected evidence");
    let mapped = ApiError::from(rebuild);
    assert_eq!(
        (mapped.0, mapped.1),
        (StatusCode::SERVICE_UNAVAILABLE, "rebuild_in_progress")
    );
}
