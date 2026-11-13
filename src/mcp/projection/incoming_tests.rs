use super::test_support::{owner, required_call, with_lines};
use super::*;
use crate::store::mcp_read::{McpIncomingCallWithLines, McpIncomingWithLines};

/// #660: the owner and callers' languages are the answer's; others are omitted.
#[test]
fn incoming_carries_semantic_status_for_the_answers_languages() {
    use crate::model::SemanticStatus::{CatchingUp, NeedsReimport};
    let mut pending = super::test_support::pending_tiers(&[
        ("javascript", NeedsReimport, "re-import required"),
        ("python", CatchingUp, "re-import pending"),
    ]);
    let incoming = |tiers: &Tiers| {
        incoming(
            &json!(1),
            fixture(1, false, 0),
            &crate::store::McpSemantic::syntax_only(crate::store::NO_PRODUCER_NOTE, 0),
            tiers,
            false,
            std::path::Path::new("/workspace"),
            false,
            true,
        )
        .unwrap()
        .unwrap()
    };
    assert_eq!(incoming(&Default::default())["semanticStatus"], json!([]));
    assert_eq!(
        incoming(&pending)["semanticStatus"],
        json!([{"language":"javascript","state":"needsReimport","reason":"re-import required"}])
    );
    pending[3].pending = None;
    assert_eq!(incoming(&pending)["semanticStatus"], json!([]));
}

fn fixture(calls: usize, has_more: bool, diagnostic: usize) -> McpIncomingWithLines {
    let outgoing = with_lines(crate::store::mcp_read::McpOutgoing {
        owner: owner(),
        calls: (0..calls).map(|_| required_call(false)).collect(),
        has_more,
    });
    let mut selected = outgoing.owner;
    selected.selected.coverage[0].state = "complete".into();
    selected.selected.coverage[0].diagnostic = Some("x".repeat(diagnostic));
    let calls = outgoing
        .calls
        .into_iter()
        .enumerate()
        .map(|(i, item)| {
            let mut call = item.selected.call;
            call.id = format!("cid:v1:{i}");
            McpIncomingCallWithLines {
                call,
                caller: selected.selected.declaration.clone(),
                content_hash: selected.selected.content_hash.clone(),
                source: item.source,
                callee_text: "Target".into(),
                coverage: selected.selected.coverage[0].clone(),
                provenance: selected.selected.provenance[0].clone(),
            }
        })
        .collect();
    McpIncomingWithLines {
        selected,
        calls,
        same_name_declarations: 3,
        scope: [[0; 5], [0; 5], [0; 5], [2, 2, 0, 0, 4], [0; 5]],
        has_more,
        bound_elsewhere_omitted: 0,
    }
}
fn project(result: McpIncomingWithLines, modern: bool) -> Option<Value> {
    incoming(
        &json!(1),
        result,
        &crate::store::McpSemantic::syntax_only(crate::store::NO_PRODUCER_NOTE, 0),
        &Default::default(),
        false,
        std::path::Path::new("/workspace"),
        false,
        modern,
    )
    .unwrap()
}
#[test]
fn incoming_empty_and_required_item_closed_shape() {
    for modern in [false, true] {
        let empty = project(fixture(0, false, 0), modern).unwrap();
        let data = &empty["data"];
        assert_eq!(data.as_object().unwrap().len(), 11);
        assert!(data.get("stableLookupKey").is_some_and(Value::is_null));
        assert_eq!(data["semanticBases"], json!([]));
        assert_eq!(data["documents"], json!([]));
        assert_eq!(data["coverage"].as_array().unwrap().len(), 1);
        assert_eq!(data["provenance"].as_array().unwrap().len(), 1);
        assert_eq!(
            data["match"]["limitations"],
            json!([
                "aliases",
                "functionValues",
                "reExports",
                "dynamicDispatch",
                "opaqueMacros",
                "sameNameDeclarations"
            ])
        );
        assert_eq!(data["match"]["sameNameDeclarations"], 3);
        assert_eq!(
            data["scopeCoverage"][3]["coverage"],
            json!({"selected":2,
            "complete":2,"partial":0,"failed":0,"unselected":4})
        );
        assert_eq!(empty["partial"], false);
        assert_eq!(empty["warnings"].as_array().unwrap().len(), 1);
        assert_eq!(empty["warnings"][0]["code"], "syntaxOnly");
        let full = project(fixture(1, false, 0), modern).unwrap();
        let caller = &full["data"]["callers"][0];
        assert_eq!(caller.as_object().unwrap().len(), 20);
        assert_eq!(caller["callerRole"], "production");
        assert_eq!(caller["callerDisplayKey"], "a.js#Owner");
        assert_eq!(caller["calleeText"], "Target");
        assert_eq!(caller["binding"], Value::Null);
        assert_eq!(caller["matchedBy"], "name");
        assert_eq!(full["partial"], true);
        assert_eq!(full["data"]["documents"].as_array().unwrap().len(), 1);
        assert_eq!(full["data"]["callers"][0]["doc"], 0);
        assert_eq!(full["warnings"].as_array().unwrap().len(), 1);
        let frame = json!({"jsonrpc":"2.0","id":1,
            "result":crate::mcp::tools::result("trellis_incoming_calls",full,modern)});
        assert!(serde_json::to_vec(&frame).unwrap().len() <= crate::mcp::wire::RESPONSE_BYTES);
    }
}
/// `src/a.ts` sorts before `src/b.js` by path bytes, so only the #22 language rank puts
/// the JavaScript coverage row first.
#[test]
fn javascript_coverage_sorts_before_typescript() {
    let mut result = fixture(1, false, 0);
    result.selected.selected.coverage[0].language = "typescript".into();
    result.selected.selected.coverage[0].document_path = "src/a.ts".into();
    let item = &mut result.calls[0];
    item.coverage.language = "javascript".into();
    item.coverage.document_path = "src/b.js".into();
    for document in [
        &mut item.call.document,
        &mut item.caller.document,
        &mut item.source.document.document,
        &mut item.provenance.document,
    ] {
        document.language = "javascript".into();
        document.path = "src/b.js".into();
    }
    let envelope = project(result, true).unwrap();
    let rows: Vec<(&str, &str)> = envelope["data"]["coverage"]
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
}
#[test]
fn incoming_scope_has_five_rows() {
    for modern in [false, true] {
        let mut result = fixture(1, false, 0);
        result.scope[4] = [1, 1, 0, 0, 0];
        let envelope = project(result, modern).unwrap();
        let rows = envelope["data"]["scopeCoverage"].as_array().unwrap();
        let languages: Vec<&str> = rows
            .iter()
            .map(|row| row["language"].as_str().unwrap())
            .collect();
        assert_eq!(
            languages,
            ["java", "rust", "python", "javascript", "typescript"]
        );
        assert_eq!(rows[4]["coverage"]["selected"], 1);
    }
}
#[test]
fn incoming_scope_and_limit_are_independent_of_emitted_calls() {
    let mut result = fixture(1, true, 0);
    result.scope[3] = [2, 1, 1, 0, 4];
    result.calls[0].caller.role = crate::native_evidence::DeclarationRole::Test;
    let envelope = project(result, true).unwrap();
    assert_eq!(envelope["warnings"][0]["code"], "coverageIncomplete");
    assert_eq!(envelope["warnings"][1]["code"], "syntaxOnly");
    assert_eq!(envelope["data"]["callers"][0]["callerRole"], "test");
    assert_eq!(envelope["truncationReason"], "limit");
    assert_eq!(
        envelope["data"]["scopeCoverage"][3]["coverage"]["partial"],
        1
    );
}
#[test]
fn incoming_byte_clipping_keeps_order_and_scope() {
    for modern in [false, true] {
        let mut result = fixture(50, false, 0);
        for (index, item) in result.calls.iter_mut().enumerate() {
            item.callee_text = format!("{index:02}{}", "t".repeat(1000));
        }
        let envelope = project(result, modern).unwrap();
        let items = envelope["data"]["callers"].as_array().unwrap();
        assert!(!items.is_empty());
        assert!(items.len() < 50);
        assert_eq!(envelope["truncationReason"], "response_bytes");
        assert_eq!(
            envelope["data"]["scopeCoverage"][3]["coverage"]["complete"],
            2
        );
        assert_eq!(envelope["data"]["documents"].as_array().unwrap().len(), 1);
        assert_eq!(envelope["data"]["coverage"].as_array().unwrap().len(), 1);
        for (index, item) in items.iter().enumerate() {
            assert_eq!(item["callId"], format!("cid:v1:{index}"));
            assert_eq!(
                item["calleeText"],
                format!("{index:02}{}", "t".repeat(1000))
            );
        }
        let frame = json!({"jsonrpc":"2.0","id":1,
            "result":crate::mcp::tools::result("trellis_incoming_calls",envelope,modern)});
        assert!(serde_json::to_vec(&frame).unwrap().len() < crate::mcp::wire::RESPONSE_BYTES);
    }
}
#[test]
fn incoming_exact_wire_budget_and_first_item_refusal() {
    for modern in [false, true] {
        let fits = |n| project(fixture(0, false, n), modern).is_some();
        let (mut low, mut high) = (0, crate::mcp::wire::RESPONSE_BYTES);
        while low + 1 < high {
            let mid = low + (high - low) / 2;
            if fits(mid) { low = mid } else { high = mid }
        }
        assert!(fits(low));
        assert!(!fits(high));
        let exact = project(fixture(0, false, low), modern).unwrap();
        let frame = json!({"jsonrpc":"2.0","id":1,
            "result":crate::mcp::tools::result("trellis_incoming_calls",exact,modern)});
        assert_eq!(
            serde_json::to_vec(&frame).unwrap().len() + 1,
            crate::mcp::wire::RESPONSE_BYTES
        );
        assert!(project(fixture(1, false, low), modern).is_none());
        let mut result = fixture(1, false, 0);
        result.calls[0].callee_text = "x".repeat(crate::mcp::wire::RESPONSE_BYTES);
        assert!(project(result, modern).is_none());
    }
}
