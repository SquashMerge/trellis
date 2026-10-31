use super::*;

fn code(kind: RequestKind, message: &str) -> String {
    match CompletionOutcome::from_result(kind, Err(anyhow::anyhow!(message.to_owned()))).unwrap() {
        CompletionOutcome::Failed(code) => code,
        CompletionOutcome::Done(_) => unreachable!(),
    }
}

#[test]
fn semantic_import_keeps_its_closed_codes_and_listed_reasons() {
    for (message, expected) in [
        (
            "invalid_request: role outside root",
            "invalid_request: role outside root",
        ),
        (
            "invalid_request: generation directory",
            "invalid_request: generation directory",
        ),
        (
            "stale_basis: head revision changed",
            "stale_basis: head revision changed",
        ),
        ("capture_unavailable", "capture_unavailable"),
        ("root_changed", "root_changed"),
        ("index_failed", "index_failed: internal error"),
        ("something unclassified", "index_failed: internal error"),
        ("invalid_requestx: nope", "index_failed: internal error"),
    ] {
        assert_eq!(
            code(RequestKind::SemanticImport, message),
            expected,
            "{message}"
        );
    }
}

#[test]
fn unlisted_reason_text_never_reaches_the_row() {
    for message in [
        "invalid_artifact: /tmp/x\nsecond line",
        "invalid_artifact: ",
        "limit_exceeded: Bad Case",
    ] {
        let stored = code(RequestKind::SemanticImport, message);
        assert!(stored.ends_with(": unlisted detail"), "{message}: {stored}");
    }
}

#[test]
fn revision_conflict_prefix_wins_over_classifier() {
    assert_eq!(
        code(
            RequestKind::SemanticImport,
            "revision conflict: request result is not current index"
        ),
        "revision_conflict"
    );
    assert_eq!(
        code(
            RequestKind::Index,
            "revision conflict: cached completion differs"
        ),
        "revision_conflict"
    );
}

#[test]
fn index_kind_keeps_index_failed() {
    assert_eq!(
        code(RequestKind::Index, "invalid_request: role outside root"),
        "index_failed: internal error"
    );
}
