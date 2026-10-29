use super::*;

fn code(kind: RequestKind, message: &str) -> String {
    match CompletionOutcome::from_result(kind, Err(anyhow::anyhow!(message.to_owned()))).unwrap() {
        CompletionOutcome::Failed(code) => code,
        CompletionOutcome::Done(_) => unreachable!(),
    }
}

#[test]
fn classifies_closed_classes() {
    for (message, expected) in [
        (
            "invalid_request: role outside root",
            "invalid_request: role outside root",
        ),
        (
            "invalid_artifact: bad coordinate",
            "invalid_artifact: bad coordinate",
        ),
        (
            "limit_exceeded: descriptor too large",
            "limit_exceeded: descriptor too large",
        ),
        ("capture_unavailable", "capture_unavailable"),
        ("stale_basis: source changed", "stale_basis: source changed"),
        ("root_changed", "root_changed"),
        ("index_failed", "index_failed"),
        ("something unclassified", "index_failed"),
        ("invalid_requestx: nope", "index_failed"),
    ] {
        assert_eq!(semantic_import_code(message), expected, "{message}");
        assert_eq!(
            code(RequestKind::SemanticImport, message),
            expected,
            "{message}"
        );
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
fn drops_unstable_reason() {
    for message in [
        "invalid_artifact: Bad Case",
        "invalid_artifact: /tmp/x\nsecond line",
        "invalid_artifact: ",
        "invalid_artifact: -leading",
        &format!("invalid_artifact: {}", "a".repeat(65)),
    ] {
        assert_eq!(
            semantic_import_code(message),
            "invalid_artifact",
            "{message}"
        );
    }
    let longest = format!("limit_exceeded: {}", "a".repeat(64));
    assert_eq!(semantic_import_code(&longest), longest);
}

#[test]
fn index_kind_keeps_index_failed() {
    assert_eq!(
        code(RequestKind::Index, "invalid_request: role outside root"),
        "index_failed"
    );
}
