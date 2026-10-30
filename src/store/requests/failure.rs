//! The typed cause a failed terminal request records (#445).
//!
//! A failed row stores `<code>: <reason>`, the same shape `recovery_blocked` already uses. The
//! code is an existing typed failure code and the reason comes only from the closed set below,
//! so repository content, paths and error chains never reach `requests.db` or its readers.

/// A typed semantic import failure. `Display` keeps the internal spelling that opt-in
/// diagnostics print; the terminal row records only `code` and a [`REASONS`] entry.
#[derive(Debug)]
pub(crate) struct TypedFailure {
    pub(crate) code: &'static str,
    pub(crate) detail: String,
}

impl std::fmt::Display for TypedFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for TypedFailure {}

/// The reason for a failure that has no typed cause.
pub(crate) const INTERNAL_REASON: &str = "internal error";
/// The reason for a typed failure whose detail is not in [`REASONS`].
pub(crate) const UNLISTED_REASON: &str = "unlisted detail";
const STORAGE_FULL_REASON: &str = "database or disk is full";

/// Every detail Trellis's SCIP capture, admission and import spell for `invalid_request`,
/// `invalid_artifact` and `limit_exceeded` (limit names included). Byte-sorted for lookup.
const REASONS: &[&str] = &[
    "artifact bytes",
    "artifact count",
    "artifact digest",
    "artifact language",
    "artifact length",
    "artifact ordinals",
    "artifact read",
    "artifact totals",
    "artifacts",
    "candidate facts",
    "captured source hash",
    "checked-in producer profile",
    "closed descriptor",
    "closed page",
    "competing range tags",
    "coverage artifact",
    "coverage completion",
    "coverage disposition",
    "decoder context",
    "descriptor bytes",
    "descriptor decoder version",
    "descriptor digest",
    "descriptor format version",
    "diagnostics",
    "document entries",
    "document source binding",
    "duplicate artifact",
    "duplicate coverage row",
    "duplicate document path",
    "duplicate semantic symbol key",
    "duplicate source path",
    "empty or reversed SCIP range",
    "extra captured source",
    "field length",
    "field wire type",
    "index entries",
    "invalid document list",
    "invalid role path",
    "lookup steps",
    "message depth",
    "metadata binding",
    "missing artifact document",
    "missing captured source",
    "missing range",
    "mixed legacy/typed range",
    "mixed ranges",
    "native revision components",
    "negative position",
    "non-UTF8 physical root",
    "non-UTF8 role",
    "non-UTF8 source",
    "operation disk growth",
    "output bytes",
    "packed values",
    "page bytes",
    "page digest",
    "page length",
    "page order",
    "page ordinals",
    "page rows",
    "page totals",
    "pages",
    "position encoding",
    "producer not pinned",
    "producer profile pin",
    "protobuf parse",
    "range column outside source",
    "range components",
    "range line outside source",
    "relationships",
    "reordered page",
    "repeated field",
    "reversed range",
    "role outside root",
    "selected role absent",
    "selected role is not absolute",
    "signature occurrences",
    "signature position encoding",
    "signature relative path",
    "source document hash or duplicate",
    "source documents",
    "source manifest hash",
    "source read",
    "source row",
    "split codepoint",
    "stage storage",
    "staged rows",
    "staging bytes",
    "symbol roles",
    "symlinked selected role",
    "tool binding",
    "total artifact bytes",
    "total page bytes",
    "truncated field",
    "truncated varint",
    "unbound coverage path",
    "unit expansion",
    "unknown enclosing range variant",
    "unknown enum",
    "unknown field",
    "unknown primary range variant",
    "unknown protobuf field",
    "unlisted document",
    "unsorted coverage rows",
    "unsorted source rows",
    "varint overflow",
    "wire fields",
];

/// Classify a terminal failure as `(code, reason)`. A typed semantic failure keeps its code
/// and maps its detail into [`REASONS`]; a full disk is `storage_full`; anything else is
/// `index_failed` with a fixed reason. Only the first typed cause in the chain counts, so the
/// precedence of the error that produced it is unchanged.
pub(crate) fn classify(error: &anyhow::Error) -> (&'static str, &'static str) {
    use crate::store::StorageFull;
    if let Some(typed) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<TypedFailure>())
    {
        let reason = REASONS
            .binary_search(&typed.detail.as_str())
            .map_or(UNLISTED_REASON, |index| REASONS[index]);
        return (typed.code, reason);
    }
    // `typed_storage_full` attaches `StorageFull` as context, which only the error itself
    // downcasts to; a chain element can also be a plain `StorageFull`.
    if error.is::<StorageFull>() || error.chain().any(|cause| cause.is::<StorageFull>()) {
        return ("storage_full", STORAGE_FULL_REASON);
    }
    ("index_failed", INTERNAL_REASON)
}

/// Split a stored terminal error into its code and optional reason.
pub fn split_error(stored: &str) -> (&str, Option<&str>) {
    match stored.split_once(": ") {
        Some((code, reason)) => (code, Some(reason)),
        None => (stored, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(code: &'static str, detail: &str) -> anyhow::Error {
        TypedFailure {
            code,
            detail: detail.to_owned(),
        }
        .into()
    }

    #[test]
    fn reasons_are_sorted_unique_bounded_and_unambiguous() {
        assert!(REASONS.windows(2).all(|pair| pair[0] < pair[1]));
        for reason in
            REASONS
                .iter()
                .chain([&INTERNAL_REASON, &UNLISTED_REASON, &STORAGE_FULL_REASON])
        {
            assert!(!reason.is_empty() && reason.len() <= 64, "{reason}");
            assert!(!reason.contains(':') && reason.is_ascii(), "{reason}");
        }
    }

    #[test]
    fn typed_failure_keeps_its_code_and_listed_reason() {
        assert_eq!(
            classify(&typed("invalid_artifact", "empty or reversed SCIP range")),
            ("invalid_artifact", "empty or reversed SCIP range")
        );
        assert_eq!(
            classify(&typed("limit_exceeded", "output bytes").context("stage write")),
            ("limit_exceeded", "output bytes")
        );
        assert_eq!(
            classify(&typed("invalid_request", "producer not pinned")),
            ("invalid_request", "producer not pinned")
        );
    }

    #[test]
    fn unlisted_detail_and_untyped_errors_never_record_their_text() {
        let artifact_text = "/home/user/secret.rs: fn leaked() {}";
        assert_eq!(
            classify(&typed("invalid_artifact", artifact_text)),
            ("invalid_artifact", UNLISTED_REASON)
        );
        assert_eq!(
            classify(&anyhow::anyhow!("{artifact_text}").context("index publication")),
            ("index_failed", INTERNAL_REASON)
        );
    }

    #[test]
    fn storage_full_is_typed() {
        let full = anyhow::Error::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
            Some("database or disk is full".into()),
        ));
        let typed = crate::store::typed_storage_full(full);
        assert_eq!(classify(&typed), ("storage_full", STORAGE_FULL_REASON));
    }

    #[test]
    fn stored_error_splits_at_the_first_separator() {
        assert_eq!(split_error("index_failed"), ("index_failed", None));
        assert_eq!(
            split_error("invalid_artifact: empty or reversed SCIP range"),
            ("invalid_artifact", Some("empty or reversed SCIP range"))
        );
    }
}
