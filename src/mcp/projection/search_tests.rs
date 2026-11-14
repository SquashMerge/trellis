use super::*;
use crate::mcp::search::SearchResult;
use crate::search_embed::Backend;
use crate::search_model::{LexicalState, SearchIndexState, SearchIndexStatus};

const COMPLETE: [[u64; 5]; 5] = [[0; 5], [0; 5], [0; 5], [3, 3, 0, 0, 0], [0; 5]];

fn fixture(names: &[(&str, &str)], scope: [[u64; 5]; 5], has_more: bool) -> SearchResult {
    let hits: Vec<_> = names
        .iter()
        .map(|(name, path)| ((*name).to_owned(), "source-set:v1:test", (*path).to_owned()))
        .collect();
    SearchResult {
        role: crate::search_rank::SearchRole::default(),
        symbols: super::test_support::fixture(&hits, scope, has_more),
        matched_by: names
            .iter()
            .map(|_| vec!["lexical".into(), "vector".into()])
            .collect(),
        search_index: SearchIndexStatus {
            state: SearchIndexState::Partial,
            model_id: Some("a".repeat(64)),
            model_name: Some("voyage-4-nano".into()),
            backend: Some(Backend::Cpu),
            declarations: 3,
            embedded: 1,
            lexical: LexicalState::Complete,
        },
    }
}

fn project(result: SearchResult, modern: bool) -> Result<Option<Value>, &'static str> {
    search(
        &json!("search"),
        result,
        &Default::default(),
        std::path::Path::new("/workspace"),
        true,
        modern,
        false,
    )
}

fn assert_wire(envelope: &Value, modern: bool) {
    let frame = json!({"jsonrpc":"2.0","id":"search","result":
        crate::mcp::tools::result("trellis_search_symbols", envelope.clone(), modern)});
    let mut line = Vec::new();
    crate::mcp::wire::write_response(&mut line, &frame).unwrap();
    assert!(line.len() <= crate::mcp::wire::RESPONSE_BYTES);
    assert_eq!(line.last(), Some(&b'\n'));
    assert_eq!(frame["result"].get("resultType").is_some(), modern);
}

#[test]
fn search_relevance_is_not_native_completeness() {
    for modern in [false, true] {
        let envelope = project(
            fixture(&[("one", "a.js"), ("two", "a.js")], COMPLETE, false),
            modern,
        )
        .unwrap()
        .unwrap();
        let data = &envelope["data"];
        assert_eq!(data.as_object().unwrap().len(), 8);
        assert_eq!(data["coverageDiagnostics"], json!([]));
        assert_eq!(data["role"], "any");
        assert_eq!(data["searchIndex"]["state"], "partial");
        assert_eq!(data["searchIndex"]["embedded"], 1);
        assert_eq!(envelope["partial"], false);
        assert_eq!(envelope["truncated"], false);
        assert_eq!(
            envelope["warnings"],
            json!([{"code":"syntaxOnly","message":"This answer is syntax-derived; no fresh semantic evidence is selected for javascript","provenanceId":null}])
        );
        assert_eq!(data["hits"][0]["rank"], 1);
        assert_eq!(data["hits"][1]["rank"], 2);
        assert_eq!(data["hits"][0]["matchedBy"], json!(["lexical", "vector"]));
        assert_eq!(data["hits"][0]["lines"], json!({"start":1,"end":1}));
        assert_eq!(data["documents"].as_array().unwrap().len(), 1);
        assert_eq!(data["coverage"].as_array().unwrap().len(), 1);
        assert_eq!(data["provenance"].as_array().unwrap().len(), 1);
        assert_wire(&envelope, modern);

        let mut unavailable = fixture(&[], COMPLETE, false);
        unavailable.search_index.state = SearchIndexState::Unavailable(
            crate::search_model::SearchUnavailableReason::ModelNotConfigured,
        );
        unavailable.search_index.model_id = None;
        unavailable.search_index.model_name = None;
        unavailable.search_index.backend = None;
        unavailable.search_index.embedded = 0;
        let empty = project(unavailable, modern).unwrap().unwrap();
        assert_eq!(empty["partial"], false);
        assert_eq!(empty["data"]["searchIndex"]["backend"], Value::Null);
        for field in [
            "hits",
            "documents",
            "coverage",
            "coverageDiagnostics",
            "provenance",
        ] {
            assert_eq!(empty["data"][field], json!([]));
        }
        let incomplete = [[0; 5], [2, 1, 1, 0, 0], [0; 5], [3, 3, 0, 0, 0], [0; 5]];
        let empty = project(fixture(&[], incomplete, false), modern)
            .unwrap()
            .unwrap();
        assert_eq!(empty["partial"], true);
        assert_eq!(empty["warnings"][0]["code"], "coverageIncomplete");
        assert_eq!(empty["warnings"][1]["code"], "syntaxOnly");
        assert_eq!(empty["data"]["scopeCoverage"][0]["language"], "rust");
        assert_eq!(empty["data"]["scopeCoverage"][0]["coverage"]["partial"], 1);
        assert_wire(&empty, modern);
    }
}

#[test]
fn search_byte_prefix_keeps_only_emitted_closure_and_precedes_limit() {
    let first = "a".repeat(12_000);
    let second = "b".repeat(12_000);
    let third = "c".repeat(12_000);
    for modern in [false, true] {
        let envelope = project(
            fixture(
                &[(&first, "a.js"), (&second, "b.js"), (&third, "c.js")],
                COMPLETE,
                true,
            ),
            modern,
        )
        .unwrap()
        .unwrap();
        assert_eq!(envelope["data"]["hits"].as_array().unwrap().len(), 2);
        assert_eq!(envelope["truncationReason"], "response_bytes");
        assert_eq!(envelope["partial"], true);
        for field in ["documents", "coverage", "provenance"] {
            assert_eq!(envelope["data"][field].as_array().unwrap().len(), 2);
        }
        assert_eq!(envelope["data"]["documents"][1]["document"]["path"], "b.js");
        assert_wire(&envelope, modern);
        let limited = project(fixture(&[("one", "a.js")], COMPLETE, true), modern)
            .unwrap()
            .unwrap();
        assert_eq!(limited["truncationReason"], "limit");
        let oversized = "x".repeat(40_000);
        let empty = project(fixture(&[(&oversized, "big.js")], COMPLETE, false), modern)
            .unwrap()
            .unwrap();
        assert_eq!(empty["truncationReason"], "response_bytes");
        for field in [
            "hits",
            "documents",
            "coverage",
            "coverageDiagnostics",
            "provenance",
        ] {
            assert_eq!(empty["data"][field], json!([]));
        }
        assert_wire(&empty, modern);
    }
}

#[test]
fn search_role_survives_empty_limit_and_byte_trimmed_prefixes() {
    use crate::search_rank::SearchRole;
    let oversized = "x".repeat(40_000);
    for modern in [false, true] {
        for (role, expected) in [
            (SearchRole::Any, "any"),
            (SearchRole::Production, "production"),
            (SearchRole::Test, "test"),
        ] {
            for (names, has_more, reason) in [
                (vec![], false, Value::Null),
                (vec![("one", "a.js")], true, json!("limit")),
                (
                    vec![(oversized.as_str(), "big.js")],
                    false,
                    json!("response_bytes"),
                ),
            ] {
                let mut result = fixture(&names, COMPLETE, has_more);
                result.role = role;
                let envelope = project(result, modern).unwrap().unwrap();
                assert_eq!(envelope["data"]["role"], expected);
                assert_eq!(envelope["truncationReason"], reason);
                if reason == "response_bytes" {
                    assert_eq!(envelope["data"]["hits"], json!([]));
                }
                assert_wire(&envelope, modern);
            }
        }
    }
}

#[test]
fn invalid_arm_membership_and_missing_required_proof_are_typed_failures() {
    for arms in [
        vec![],
        vec!["vector", "lexical"],
        vec!["lexical", "lexical"],
        vec!["unknown"],
    ] {
        let mut result = fixture(&[("one", "a.js")], COMPLETE, false);
        result.matched_by = vec![arms.into_iter().map(str::to_owned).collect()];
        assert!(
            project(result, true)
                .unwrap_err()
                .starts_with("store_unavailable")
        );
    }
    let mut wrong_length = fixture(&[("one", "a.js")], COMPLETE, false);
    wrong_length.matched_by.clear();
    assert!(
        project(wrong_length, true)
            .unwrap_err()
            .starts_with("store_unavailable")
    );
    let mut missing = fixture(&[("one", "a.js")], COMPLETE, false);
    missing.symbols.provenance.clear();
    assert!(
        project(missing, true)
            .unwrap_err()
            .starts_with("store_unavailable")
    );
}

#[test]
fn search_budgets_status_summary_and_final_lf_at_exact_boundary() {
    for modern in [false, true] {
        let make = |size| {
            let mut result = fixture(&[], COMPLETE, false);
            result.search_index.model_name = Some("x".repeat(size));
            result
        };
        let (mut low, mut high) = (0, crate::mcp::wire::RESPONSE_BYTES);
        while low + 1 < high {
            let mid = low + (high - low) / 2;
            if project(make(mid), modern).unwrap().is_some() {
                low = mid;
            } else {
                high = mid;
            }
        }
        assert!(project(make(high), modern).unwrap().is_none());
        let envelope = project(make(low), modern).unwrap().unwrap();
        let frame = json!({"jsonrpc":"2.0","id":"search","result":
            crate::mcp::tools::result("trellis_search_symbols", envelope, modern)});
        assert_eq!(
            serde_json::to_vec(&frame).unwrap().len() + 1,
            crate::mcp::wire::RESPONSE_BYTES
        );
    }
}
