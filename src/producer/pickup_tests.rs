use super::pickup::{Decision, PickupRequest, TRANSIENT_RETRY_MILLIS, decide, decide_with_retry};
use super::state::{LastImported, PickupEntry};
use serde_json::json;

const SHA: &str = "aa";
const HEAD: &str = "rev-1";
const THEN: u64 = 1_000_000;

fn entry(last: Option<(&str, &str, Option<&str>)>) -> PickupEntry {
    PickupEntry {
        producer: "scip-java".into(),
        artifact: "/a.scip".into(),
        last_imported: last.map(|(sha, state, code)| LastImported {
            sha256: sha.into(),
            source_revision: HEAD.into(),
            request_id: None,
            state: state.into(),
            error_code: code.map(Into::into),
            attempted_at: THEN,
        }),
    }
}

#[test]
fn decide_table() {
    let now = THEN + 5;
    // None, or a different digest: import.
    assert_eq!(decide(&entry(None), SHA, HEAD, now), Decision::Import);
    assert_eq!(
        decide(&entry(Some(("bb", "done", None))), SHA, HEAD, now),
        Decision::Import
    );
    // Done: skip, however long ago and whichever head.
    for (head, at) in [(HEAD, now), ("rev-2", THEN + 10 * TRANSIENT_RETRY_MILLIS)] {
        assert_eq!(
            decide(&entry(Some((SHA, "done", None))), SHA, head, at),
            Decision::Skip
        );
    }
    // Deterministic for these bytes against this head: skip until the head changes.
    for code in [
        "invalid_artifact: tool binding",
        "invalid_request: generation origin",
        "limit_exceeded",
        "producer_output_invalid: project root",
    ] {
        let failed = entry(Some((SHA, "failed", Some(code))));
        let late = THEN + 10 * TRANSIENT_RETRY_MILLIS;
        assert_eq!(decide(&failed, SHA, HEAD, late), Decision::Skip, "{code}");
        assert_eq!(
            decide(&failed, SHA, "rev-2", now),
            Decision::Import,
            "{code}"
        );
    }
    // Anything else is transient: retry once the interval has passed, on any head.
    for code in [
        "stale_basis: head revision changed",
        "revision_conflict",
        "capture_unavailable",
        "root_changed",
        "index_failed",
        "storage_busy",
    ] {
        let failed = entry(Some((SHA, "failed", Some(code))));
        let edge = THEN + TRANSIENT_RETRY_MILLIS;
        assert_eq!(
            decide(&failed, SHA, HEAD, edge - 1),
            Decision::Skip,
            "{code}"
        );
        assert_eq!(decide(&failed, SHA, HEAD, edge), Decision::Import, "{code}");
        assert_eq!(
            decide(&failed, SHA, "rev-2", edge),
            Decision::Import,
            "{code}"
        );
    }
    // A failure with no recorded code is transient too.
    let bare = entry(Some((SHA, "failed", None)));
    assert_eq!(decide(&bare, SHA, HEAD, now), Decision::Skip);
    assert_eq!(
        decide(&bare, SHA, HEAD, THEN + TRANSIENT_RETRY_MILLIS),
        Decision::Import
    );
}

#[test]
fn pickup_request_is_closed_and_needs_one_action() {
    let ok = |value| PickupRequest::parse(&value);
    let set = ok(json!({"version":1,"producer":"scip-java","artifact":"/a.scip","clear":false}));
    assert_eq!(set.unwrap().artifact.as_deref(), Some("/a.scip"));
    assert!(ok(json!({"version":1,"producer":"scip-java","artifact":null,"clear":true})).is_ok());
    for bad in [
        json!({"version":1,"producer":"scip-java","artifact":null,"clear":false}),
        json!({"version":1,"producer":"scip-java","artifact":"/a","clear":true}),
        json!({"version":2,"producer":"scip-java","artifact":"/a","clear":false}),
        json!({"version":1,"producer":"scip-java","artifact":"/a","clear":false,"origin":"ownerPickup"}),
        json!({"version":1,"artifact":"/a","clear":false}),
        json!({"version":1,"producer":"scip-java","artifact":"x".repeat(20_000),"clear":false}),
    ] {
        assert!(ok(bad.clone()).is_err(), "{bad}");
    }
}

#[test]
fn decide_honours_a_shorter_retry_interval() {
    let failed = entry(Some((SHA, "failed", Some("stale_basis"))));
    assert_eq!(
        decide_with_retry(&failed, SHA, HEAD, THEN + 199, 200),
        Decision::Skip
    );
    assert_eq!(
        decide_with_retry(&failed, SHA, HEAD, THEN + 200, 200),
        Decision::Import
    );
}
