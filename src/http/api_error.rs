//! The typed API error and the mapping from internal failures to it.

use super::*;

pub(super) fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"error":{"code":code,"message":message}})),
    )
        .into_response()
}
#[derive(Debug)]
pub(super) struct ApiError(
    pub(super) StatusCode,
    pub(super) &'static str,
    pub(super) &'static str,
);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        error(self.0, self.1, self.2)
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        // IOERR is broken SQLite locking/storage, not ordinary BUSY. Even an
        // outer storage_busy context must never mask an IOERR_RDLOCK as 409.
        if e.chain().any(|cause| cause.downcast_ref::<rusqlite::Error>().is_some_and(|error| {
            matches!(error, rusqlite::Error::SqliteFailure(info, _) if info.code == rusqlite::ErrorCode::SystemIoFailure)
        })) {
            return Self(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", "Operation failed");
        }
        if e.chain().any(|cause| cause.downcast_ref::<rusqlite::Error>().is_some_and(|error| {
            matches!(error, rusqlite::Error::SqliteFailure(info, _) if matches!(info.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
        })) {
            return Self(StatusCode::CONFLICT, "storage_busy", "Storage is busy");
        }
        // A spilled large publication holds every reader out until COMMIT,
        // and the read's bounded wait for it ran out (#674); say so instead of
        // a generic busy (#210).
        if e.chain().any(|cause| {
            cause
                .downcast_ref::<crate::store::RebuildInProgress>()
                .is_some()
        }) {
            return Self(
                StatusCode::SERVICE_UNAVAILABLE,
                "rebuild_in_progress",
                "The index is being rebuilt; retry after it finishes",
            );
        }
        if let Some(directory) = e.downcast_ref::<BrowseDirectoryError>() {
            return browse_directory_error(&directory.0);
        }
        if let Some(invalid) = e.downcast_ref::<crate::navigation::InvalidRequest>() {
            Self(
                StatusCode::BAD_REQUEST,
                "invalid_navigation_request",
                invalid.0,
            )
        } else if e
            .downcast_ref::<crate::files_view::InvalidRequest>()
            .is_some()
        {
            invalid()
        } else if let Some(invalid) = e.downcast_ref::<crate::class_diagram::InvalidRequest>() {
            Self(StatusCode::BAD_REQUEST, "invalid_class_request", invalid.0)
        } else if e
            .chain()
            .any(|cause| cause.downcast_ref::<crate::store::PinExpired>().is_some())
        {
            Self(StatusCode::CONFLICT, "pin_expired", "The index pin expired")
        } else if e
            .chain()
            .any(|cause| cause.to_string().starts_with("revision conflict"))
        {
            Self(
                StatusCode::CONFLICT,
                "revision_conflict",
                "The index revision changed",
            )
        } else if e
            .chain()
            .any(|cause| cause.to_string() == "native declaration target missing")
        {
            invalid()
        } else {
            // Context must not hide an exact allowlisted refusal from the public API.
            for cause in e.chain() {
                let text = cause.to_string();
                for (prefix, status, code) in [
                    ("root_changed", StatusCode::CONFLICT, "root_changed"),
                    (
                        "root_key_collision",
                        StatusCode::SERVICE_UNAVAILABLE,
                        "root_key_collision",
                    ),
                    (
                        "workspace_id_changed",
                        StatusCode::CONFLICT,
                        "workspace_id_changed",
                    ),
                    ("storage_busy", StatusCode::CONFLICT, "storage_busy"),
                    (
                        "index_not_ready",
                        StatusCode::SERVICE_UNAVAILABLE,
                        "index_not_ready",
                    ),
                    (
                        "incompatible_index",
                        StatusCode::SERVICE_UNAVAILABLE,
                        "incompatible_index",
                    ),
                    (
                        "recovery_required",
                        StatusCode::SERVICE_UNAVAILABLE,
                        "recovery_required",
                    ),
                    (
                        "unsafe_index",
                        StatusCode::SERVICE_UNAVAILABLE,
                        "unsafe_index",
                    ),
                    ("not_found", StatusCode::NOT_FOUND, "not_found"),
                    (
                        "deadline_exceeded",
                        StatusCode::REQUEST_TIMEOUT,
                        "deadline_exceeded",
                    ),
                    ("cancelled", StatusCode::REQUEST_TIMEOUT, "cancelled"),
                    (
                        "bounded_work",
                        StatusCode::SERVICE_UNAVAILABLE,
                        "bounded_work",
                    ),
                    (
                        "response_too_large",
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "response_too_large",
                    ),
                    ("corrupt", StatusCode::SERVICE_UNAVAILABLE, "corrupt"),
                ] {
                    if text.starts_with(prefix) {
                        return Self(status, code, "Storage is unavailable");
                    }
                }
            }
            Self(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Operation failed",
            )
        }
    }
}
pub(super) fn invalid() -> ApiError {
    ApiError(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        "Invalid request",
    )
}
pub(super) fn missing() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not_found", "Not found")
}
