use super::test_support::{owner, required_call, with_lines};
use super::*;

/// #660: `trellis_outgoing_calls` and `trellis_declaration` name the owner document's
/// language when it is pending, and omit pending languages outside the answer.
#[test]
fn outgoing_and_declaration_carry_semantic_status() {
    use crate::model::SemanticStatus::{CatchingUp, NeedsReimport};
    let root = std::path::Path::new("/workspace");
    let result = || {
        with_lines(McpOutgoing {
            owner: owner(),
            calls: vec![],
            has_more: false,
        })
    };
    let none = Tiers::default();
    let pending = super::test_support::pending_tiers(&[
        ("javascript", CatchingUp, "re-import pending"),
        ("rust", NeedsReimport, "re-import required"),
    ]);
    let expected =
        json!([{"language":"javascript","state":"catchingUp","reason":"re-import pending"}]);
    for (tiers, status) in [(&none, json!([])), (&pending, expected)] {
        let outgoing = outgoing(
            &json!(1),
            result(),
            &syntax_only(),
            tiers,
            false,
            root,
            false,
            true,
        )
        .unwrap()
        .unwrap();
        assert_eq!(outgoing["semanticStatus"], status);
        let declaration = declaration(&json!(1), result().owner, tiers, root, false, true).unwrap();
        assert_eq!(declaration["semanticStatus"], status);
    }
}
use crate::store::mcp_read::{McpOutgoing, McpOutgoingCall, McpOutgoingCandidate};

fn outgoing_fixture(
    id: &Value,
    result: McpOutgoing,
    root: &std::path::Path,
    catching_up: bool,
    modern: bool,
) -> Option<Value> {
    outgoing(
        id,
        with_lines(result),
        &syntax_only(),
        &Default::default(),
        false,
        root,
        catching_up,
        modern,
    )
    .unwrap()
}
pub(super) fn syntax_only() -> crate::store::McpSemantic {
    crate::store::McpSemantic::syntax_only(crate::graph_query::NO_PRODUCER_NOTE, 0)
}

/// `src/a.ts` sorts before `src/b.js` by path bytes, so only the #22 language rank puts
/// the JavaScript coverage row first.
#[test]
fn javascript_coverage_sorts_before_typescript() {
    let mut typescript = owner();
    typescript.declaration.document.language = "typescript".into();
    typescript.declaration.document.path = "src/a.ts".into();
    typescript.coverage[0].language = "typescript".into();
    typescript.coverage[0].document_path = "src/a.ts".into();
    typescript.provenance[0].document = typescript.declaration.document.clone();
    let mut target = owner();
    target.declaration.syntax_id = "sid:v1:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into();
    target.declaration.document.path = "src/b.js".into();
    target.declaration.provenance_id = "native-proof:v1:target".into();
    target.coverage[0].document_path = "src/b.js".into();
    target.provenance[0].document = target.declaration.document.clone();
    target.provenance[0].id = target.declaration.provenance_id.clone();
    let mut call = required_call(false);
    call.call.document = typescript.declaration.document.clone();
    call.candidates.push(McpOutgoingCandidate {
        declaration: target.declaration,
        coverage: target.coverage.remove(0),
        provenance: target.provenance.remove(0),
    });
    let answer = outgoing_fixture(
        &json!(1),
        McpOutgoing {
            owner: typescript,
            calls: vec![call],
            has_more: false,
        },
        std::path::Path::new("/workspace"),
        false,
        true,
    )
    .unwrap();
    let rows: Vec<(&str, &str)> = answer["data"]["coverage"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["language"].as_str().unwrap(),
                row["documentPath"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [("javascript", "src/b.js"), ("typescript", "src/a.ts")]
    );
    assert_eq!(
        answer["data"]["documents"][0]["document"]["language"],
        "javascript"
    );
}

#[test]
fn shown_candidate_uses_its_own_proof_not_another_valid_proof() {
    let make_result = || {
        let mut target = owner();
        target.declaration.syntax_id = "sid:v1:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into();
        target.declaration.document.path = "target.js".into();
        target.declaration.provenance_id = "native-proof:v1:target".into();
        target.coverage[0].document_path = "target.js".into();
        target.provenance[0].document = target.declaration.document.clone();
        target.provenance[0].id = target.declaration.provenance_id.clone();
        let mut call = required_call(false);
        call.candidates.push(McpOutgoingCandidate {
            declaration: target.declaration,
            coverage: target.coverage.remove(0),
            provenance: target.provenance.remove(0),
        });
        with_lines(McpOutgoing {
            owner: owner(),
            calls: vec![call],
            has_more: false,
        })
    };
    let root = std::path::Path::new("/workspace");
    let valid = outgoing(
        &json!(1),
        make_result(),
        &syntax_only(),
        &Default::default(),
        false,
        root,
        false,
        true,
    )
    .unwrap()
    .unwrap();
    let entry = &valid["data"]["documents"][valid["data"]["calls"][0]["nativeCandidates"]["items"]
        [0]["doc"]
        .as_u64()
        .unwrap() as usize];
    assert_eq!(entry["provenanceId"], "native-proof:v1:target");
    assert_ne!(
        entry["provenanceId"],
        valid["data"]["documents"][0]["provenanceId"]
    );
    let mut wrong = make_result();
    // The owner proof is real and present in the response, but it is not
    // the independent target declaration's proof.
    wrong.calls[0].candidate_documents[0].provenance_id = owner().provenance[0].id.clone();
    let failure = outgoing(
        &json!(1),
        wrong,
        &syntax_only(),
        &Default::default(),
        false,
        root,
        false,
        true,
    )
    .unwrap_err();
    assert!(failure.starts_with("store_unavailable"));
}

#[test]
fn outgoing_empty_complete_vs_incomplete_coverage_and_unresolved_boundary() {
    let root = std::path::Path::new("/workspace");
    let incomplete = outgoing_fixture(
        &json!(1),
        McpOutgoing {
            owner: owner(),
            calls: vec![],
            has_more: false,
        },
        root,
        false,
        true,
    )
    .unwrap();
    assert_eq!(incomplete["data"]["calls"], json!([]));
    assert_eq!(incomplete["data"]["documents"], json!([]));
    assert_eq!(incomplete["data"]["coverage"].as_array().unwrap().len(), 1);
    assert_eq!(
        incomplete["data"]["provenance"].as_array().unwrap().len(),
        1
    );
    assert_eq!(incomplete["warnings"][0]["code"], "coverageIncomplete");
    assert_eq!(incomplete["partial"], true);
    assert_eq!(incomplete["truncated"], false);
    let mut complete = owner();
    complete.coverage[0].state = "complete".into();
    complete.coverage[0].diagnostic = None;
    let complete = outgoing_fixture(
        &json!(1),
        McpOutgoing {
            owner: complete,
            calls: vec![],
            has_more: false,
        },
        root,
        false,
        true,
    )
    .unwrap();
    assert_eq!(complete["warnings"][0]["code"], "syntaxOnly");
    assert_eq!(complete["partial"], false);
    let call = serde_json::from_value(
        json!({"id":"cid:v1:1","ownerSyntaxId":owner().declaration.syntax_id,
        "ordinal":0,"document":owner().declaration.document,"revisionId":"revision:v2:test",
        "range":{"start":21,"end":30},"calleeRange":{"start":21,"end":25},
        "spelling":"Target","regionIds":[],"provenanceId":"native-proof:v1:test"}),
    )
    .unwrap();
    let data = McpOutgoingCall {
        call,
        content_hash: "a".repeat(64),
        callee_text: Some("é".repeat(512)),
        callee_text_truncated: true,
        candidates: vec![],
        omitted: Some(0),
    };
    let clipped = outgoing_fixture(
        &json!(1),
        McpOutgoing {
            owner: owner(),
            calls: vec![data],
            has_more: false,
        },
        root,
        false,
        true,
    )
    .unwrap();
    assert_eq!(clipped["truncationReason"], "callee_text");
    assert_eq!(clipped["partial"], true);
    assert_eq!(
        clipped["data"]["calls"][0]["calleeText"]
            .as_str()
            .unwrap()
            .len(),
        1024
    );
    assert_eq!(clipped["data"]["calls"][0]["targetId"], Value::Null);
}

#[test]
fn fixed_closure_and_first_required_call_never_become_empty_success() {
    let root = std::path::Path::new("/workspace");
    for modern in [true, false] {
        let fits_empty = |diagnostic_size| {
            let mut selected = owner();
            selected.coverage[0].diagnostic = Some("x".repeat(diagnostic_size));
            outgoing_fixture(
                &json!(1),
                McpOutgoing {
                    owner: selected,
                    calls: vec![],
                    has_more: false,
                },
                root,
                false,
                modern,
            )
            .is_some()
        };
        let mut low = 0;
        let mut high = super::super::wire::RESPONSE_BYTES;
        while low + 1 < high {
            let middle = low + (high - low) / 2;
            if fits_empty(middle) {
                low = middle;
            } else {
                high = middle;
            }
        }
        assert!(fits_empty(low));
        assert!(
            !fits_empty(high),
            "fixed selected coverage must not be clipped"
        );
        let mut selected = owner();
        selected.coverage[0].diagnostic = Some("x".repeat(low));
        assert!(
            outgoing_fixture(
                &json!(1),
                McpOutgoing {
                    owner: selected,
                    calls: vec![required_call(false)],
                    has_more: false,
                },
                root,
                false,
                modern
            )
            .is_none(),
            "a required call must not be replaced by an empty success"
        );
    }
}

#[test]
fn outgoing_truncation_prioritizes_bytes_over_limit_over_callee_text() {
    let root = std::path::Path::new("/workspace");
    for modern in [true, false] {
        let limited = outgoing_fixture(
            &json!(1),
            McpOutgoing {
                owner: owner(),
                calls: vec![required_call(true)],
                has_more: true,
            },
            root,
            false,
            modern,
        )
        .unwrap();
        assert_eq!(limited["truncationReason"], "limit");
        let mut calls = Vec::new();
        for ordinal in 0..50 {
            let mut item = required_call(true);
            item.call.id = format!("cid:v1:{ordinal}");
            item.call.ordinal = ordinal;
            calls.push(item);
        }
        // A hint attached only to the excluded last call must never leak
        // a table entry, coverage row, or proof into the selected prefix.
        let mut candidate = owner();
        candidate.declaration.document.path = "excluded.js".into();
        candidate.declaration.revision_id = "revision:v2:excluded".into();
        candidate.declaration.provenance_id = "native-proof:v1:excluded".into();
        candidate.coverage[0].document_path = "excluded.js".into();
        candidate.coverage[0].revision_id = "revision:v2:excluded".into();
        candidate.provenance[0].document = candidate.declaration.document.clone();
        candidate.provenance[0].revision_id = "revision:v2:excluded".into();
        candidate.provenance[0].id = "native-proof:v1:excluded".into();
        calls[49].candidates.push(McpOutgoingCandidate {
            declaration: candidate.declaration,
            coverage: candidate.coverage.remove(0),
            provenance: candidate.provenance.remove(0),
        });
        let answer = || {
            outgoing_fixture(
                &json!(1),
                McpOutgoing {
                    owner: owner(),
                    calls: (0..50)
                        .map(|ordinal| {
                            let mut item = required_call(true);
                            item.call.id = format!("cid:v1:{ordinal}");
                            item.call.ordinal = ordinal;
                            if ordinal == 49 {
                                item.candidates = calls[49].candidates.clone();
                            }
                            item
                        })
                        .collect(),
                    has_more: true,
                },
                root,
                false,
                modern,
            )
            .unwrap()
        };
        let bytes = answer();
        assert_eq!(
            serde_json::to_vec(&bytes).unwrap(),
            serde_json::to_vec(&answer()).unwrap()
        );
        let emitted = bytes["data"]["calls"].as_array().unwrap();
        assert!(!emitted.is_empty() && emitted.len() < 50);
        assert_eq!(
            emitted
                .iter()
                .map(|call| call["ordinal"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            (0..emitted.len() as u64).collect::<Vec<_>>()
        );
        assert_eq!(bytes["data"]["documents"].as_array().unwrap().len(), 1);
        assert_eq!(bytes["data"]["coverage"].as_array().unwrap().len(), 1);
        assert_eq!(bytes["data"]["provenance"].as_array().unwrap().len(), 1);
        assert!(emitted.iter().all(|call| call["doc"] == 0));
        assert_eq!(bytes["data"]["documents"][0]["document"]["path"], "a.js");
        assert_eq!(bytes["truncationReason"], "response_bytes");
        let required_prefix = |count: usize| {
            outgoing_fixture(
                &json!(1),
                McpOutgoing {
                    owner: owner(),
                    calls: (0..count)
                        .map(|ordinal| {
                            let mut call = required_call(true);
                            call.call.id = format!("cid:v1:{ordinal}");
                            call.call.ordinal = ordinal;
                            call
                        })
                        .collect(),
                    has_more: false,
                },
                root,
                false,
                modern,
            )
            .unwrap()
        };
        let exactly_fits = required_prefix(emitted.len());
        assert_eq!(exactly_fits["data"]["calls"], bytes["data"]["calls"]);
        assert_eq!(exactly_fits["truncationReason"], "callee_text");
        let first_excluded = required_prefix(emitted.len() + 1);
        assert_eq!(first_excluded["data"]["calls"], bytes["data"]["calls"]);
        assert_eq!(first_excluded["truncationReason"], "response_bytes");
        assert!(bytes["truncated"].as_bool().unwrap());
        assert!(bytes["partial"].as_bool().unwrap());
        let frame = json!({"jsonrpc":"2.0","id":1,
            "result":super::super::tools::result("trellis_outgoing_calls", bytes,modern)});
        let mut line = Vec::new();
        super::super::wire::write_response(&mut line, &frame).unwrap();
        assert!(line.len() <= super::super::wire::RESPONSE_BYTES);
        assert_eq!(line.last(), Some(&b'\n'));
    }
}
