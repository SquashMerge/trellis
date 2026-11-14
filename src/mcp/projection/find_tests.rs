use super::test_support::fixture;
use super::*;
use crate::mcp::wire::{RESPONSE_BYTES, write_response};
use crate::native_evidence::DocumentKey;
use crate::store::mcp_read::McpDocumentVersion;

fn hit(name: impl Into<String>, path: &str) -> (String, &'static str, String) {
    (name.into(), "source-set:v1:test", path.into())
}
/// The #156 smoke checkout: 270 documents, 111 partial Rust files.
const LARGE_SCOPE: [[u64; 5]; 5] = [
    [4, 4, 0, 0, 0],
    [117, 6, 111, 0, 0],
    [9, 9, 0, 0, 0],
    [140, 140, 0, 0, 0],
    [0; 5],
];
const COMPLETE_SCOPE: [[u64; 5]; 5] = [[1, 1, 0, 0, 0], [0; 5], [0; 5], [3, 3, 0, 0, 0], [0; 5]];

fn assert_wire(envelope: Value, id: &Value, modern: bool) -> Value {
    let result = super::super::tools::result("trellis_find_symbols", envelope.clone(), modern);
    assert_eq!(result["structuredContent"], envelope);
    let count = envelope["data"]["symbols"].as_array().unwrap().len();
    let truncated = if envelope["truncated"] == true {
        format!(
            ", truncated: {}",
            envelope["truncationReason"].as_str().unwrap()
        )
    } else {
        String::new()
    };
    let items = if count == 1 { "symbol" } else { "symbols" };
    assert_eq!(
        result["content"][0]["text"],
        format!(
            "trellis_find_symbols: {count} {items}{truncated}; full result in structuredContent."
        )
    );
    assert_eq!(result.get("resultType").is_some(), modern);
    let frame = json!({"jsonrpc":"2.0","id":id,"result":result});
    let mut bytes = Vec::new();
    write_response(&mut bytes, &frame).unwrap();
    assert!(bytes.len() <= RESPONSE_BYTES);
    assert_eq!(bytes.last(), Some(&b'\n'));
    envelope
}

#[test]
fn exact_lookup_candidates_are_complete_or_atomic_failure() {
    let root = std::path::Path::new("/workspace");
    let key = "src/a.rs::a";
    let hits: Vec<_> = (0..51).map(|_| hit("a", "a.js")).collect();
    let result = || {
        let mut result = fixture(&hits, COMPLETE_SCOPE, false);
        for symbol in &mut result.symbols {
            symbol.source.stable_lookup_key = Some(key.into());
        }
        result
    };
    for modern in [false, true] {
        let envelope = lookup_candidates(
            &json!(1),
            "trellis_call_graph",
            key,
            result(),
            &Default::default(),
            root,
            false,
            modern,
        )
        .unwrap()
        .unwrap();
        assert_eq!(envelope["data"]["candidates"].as_array().unwrap().len(), 51);
        assert_eq!(envelope["data"]["candidates"][0]["stableLookupKey"], key);
        assert!(envelope["data"].get("symbols").is_none());
        assert_eq!(envelope["truncated"], false);
        let symbols = find_lookup(
            &json!(1),
            result(),
            &Default::default(),
            root,
            false,
            modern,
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(symbols["data"]["symbols"].as_array().unwrap().len(), 51);
    }
    let oversized = || fixture(&[hit("x".repeat(70_000), "a.js")], COMPLETE_SCOPE, false);
    assert!(
        lookup_candidates(
            &json!(1),
            "trellis_declaration",
            key,
            oversized(),
            &Default::default(),
            root,
            false,
            true
        )
        .unwrap()
        .is_none()
    );
    assert!(
        find_lookup(
            &json!(1),
            oversized(),
            &Default::default(),
            root,
            false,
            true,
            false
        )
        .unwrap()
        .is_none()
    );
    let incomplete = fixture(&[], COMPLETE_SCOPE, true);
    assert!(
        find_lookup(
            &json!(1),
            incomplete,
            &Default::default(),
            root,
            false,
            true,
            false
        )
        .is_err()
    );
    let empty = find_lookup(
        &json!(1),
        fixture(&[], COMPLETE_SCOPE, false),
        &Default::default(),
        root,
        false,
        true,
        false,
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty["data"]["symbols"], json!([]));
    assert_eq!(empty["truncated"], false);
}

#[test]
fn zero_hits_report_scope_counts_without_document_rows() {
    let root = std::path::Path::new("/workspace");
    let id = json!(7);
    for modern in [false, true] {
        // The scope that overflowed the cap with per-document rows now fits.
        let large = find(
            &id,
            fixture(&[], LARGE_SCOPE, false),
            &Default::default(),
            root,
            false,
            modern,
            false,
        )
        .unwrap()
        .unwrap();
        let data = &large["data"];
        assert_eq!(data["symbols"], json!([]));
        assert_eq!(data["documents"], json!([]));
        assert_eq!(data["coverage"], json!([]));
        assert_eq!(data["provenance"], json!([]));
        assert_eq!(
            data["scopeCoverage"],
            json!([
                {"language":"java","nativeCoverageTier":"syntax","coverage":{"selected":4,"complete":4,"partial":0,"failed":0,"unselected":0},"semantic":{"available":false,"producerId":null,"freshness":"unavailable","coverage":{"selected":0,"complete":0,"partial":0,"failed":0,"unselected":0},"note":null,"reimport":null,"documentStatus":{"olderBasis":0,"catchingUp":0,"needsReimport":0}}},
                {"language":"rust","nativeCoverageTier":"syntax","coverage":{"selected":117,"complete":6,"partial":111,"failed":0,"unselected":0},"semantic":{"available":false,"producerId":null,"freshness":"unavailable","coverage":{"selected":0,"complete":0,"partial":0,"failed":0,"unselected":0},"note":null,"reimport":null,"documentStatus":{"olderBasis":0,"catchingUp":0,"needsReimport":0}}},
                {"language":"python","nativeCoverageTier":"syntax","coverage":{"selected":9,"complete":9,"partial":0,"failed":0,"unselected":0},"semantic":{"available":false,"producerId":null,"freshness":"unavailable","coverage":{"selected":0,"complete":0,"partial":0,"failed":0,"unselected":0},"note":null,"reimport":null,"documentStatus":{"olderBasis":0,"catchingUp":0,"needsReimport":0}}},
                {"language":"javascript","nativeCoverageTier":"syntax","coverage":{"selected":140,"complete":140,"partial":0,"failed":0,"unselected":0},"semantic":{"available":false,"producerId":null,"freshness":"unavailable","coverage":{"selected":0,"complete":0,"partial":0,"failed":0,"unselected":0},"note":null,"reimport":null,"documentStatus":{"olderBasis":0,"catchingUp":0,"needsReimport":0}}}
            ])
        );
        // Incomplete selected scope: partial, one aggregate warning first.
        assert_eq!(large["partial"], true);
        assert_eq!(large["truncated"], false);
        assert_eq!(large["warnings"][0]["code"], "coverageIncomplete");
        assert_eq!(large["warnings"][1]["code"], "syntaxOnly");
        assert_wire(large, &id, modern);

        // A failed selected tuple alone also flags the scope.
        let failed = [[0; 5], [0; 5], [1, 0, 0, 1, 0], [0; 5], [0; 5]];
        let envelope = find(
            &id,
            fixture(&[], failed, false),
            &Default::default(),
            root,
            false,
            modern,
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(envelope["partial"], true);
        assert_eq!(envelope["warnings"][0]["code"], "coverageIncomplete");

        // Unselected-only tuples (and zero-language rows) flag nothing.
        let unselected = [[0; 5], [0, 0, 0, 0, 3], [0; 5], [2, 2, 0, 0, 0], [0; 5]];
        let envelope = find(
            &id,
            fixture(&[], unselected, false),
            &Default::default(),
            root,
            false,
            modern,
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(envelope["partial"], false);
        assert_eq!(envelope["warnings"].as_array().unwrap().len(), 1);
        assert_eq!(envelope["warnings"][0]["code"], "syntaxOnly");
        // #715: with no hits, only the searched languages with selected tuples.
        assert_eq!(
            envelope["data"]["scopeCoverage"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            envelope["data"]["scopeCoverage"][0]["language"],
            "javascript"
        );
    }
}

#[test]
fn coverage_is_the_closure_of_emitted_symbols_by_full_document_key() {
    let root = std::path::Path::new("/workspace");
    let id = json!(1);
    let hits = vec![
        hit("one", "src/a.js"),
        hit("two", "src/a.js"),
        ("three".into(), "source-set:v1:other", "src/a.js".into()),
        hit("four", "src/b.js"),
    ];
    for modern in [false, true] {
        let envelope = find(
            &id,
            fixture(&hits, COMPLETE_SCOPE, false),
            &Default::default(),
            root,
            false,
            modern,
            false,
        )
        .unwrap()
        .unwrap();
        let rows = envelope["data"]["coverage"].as_array().unwrap();
        // Same path in two source sets is two documents; a repeated document is one row.
        let keys: Vec<_> = rows
            .iter()
            .map(|c| {
                (
                    c["sourceSetId"].as_str().unwrap(),
                    c["documentPath"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            keys,
            vec![
                ("source-set:v1:other", "src/a.js"),
                ("source-set:v1:test", "src/a.js"),
                ("source-set:v1:test", "src/b.js"),
            ]
        );
        assert_eq!(envelope["data"]["provenance"].as_array().unwrap().len(), 3);
        let repeated = find(
            &id,
            fixture(&hits, COMPLETE_SCOPE, false),
            &Default::default(),
            root,
            false,
            modern,
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            serde_json::to_vec(&envelope).unwrap(),
            serde_json::to_vec(&repeated).unwrap()
        );
        let documents = envelope["data"]["documents"].as_array().unwrap();
        assert_eq!(documents.len(), 3);
        let refs: Vec<_> = envelope["data"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|symbol| symbol["doc"].as_u64().unwrap())
            .collect();
        assert_eq!(refs, vec![1, 1, 0, 2]);
        for (index, document) in documents.iter().enumerate() {
            assert_eq!(document["ref"], index);
            assert!(refs.contains(&(index as u64)));
        }
        assert_eq!(
            envelope["data"]["symbols"][0]["lines"],
            json!({"start":1,"end":1})
        );
        assert_eq!(envelope["partial"], false);
        assert_wire(envelope, &id, modern);
    }
}

#[test]
fn document_refs_order_revisions_and_full_keys() {
    let version = |source_set: &str, revision: &str| McpDocumentVersion {
        document: DocumentKey {
            source_set_id: source_set.into(),
            language: "javascript".into(),
            path: "a.js".into(),
        },
        revision_id: revision.into(),
        content_hash: "a".repeat(64),
        provenance_id: "proof".into(),
    };
    let other = version("other", "rev:1");
    let later = version("same", "rev:2");
    let earlier = version("same", "rev:1");
    let (documents, refs) = document_table([&later, &other, &earlier, &other]).unwrap();
    assert_eq!(documents.len(), 3);
    assert_eq!(document_ref(&refs, &other), Some(0));
    assert_eq!(document_ref(&refs, &earlier), Some(1));
    assert_eq!(document_ref(&refs, &later), Some(2));
    assert!(document_table(std::iter::empty()).unwrap().0.is_empty());
}

#[test]
fn conflicting_selected_document_version_is_store_unavailable() {
    for differing_proof in [false, true] {
        let mut conflicting = fixture(
            &[hit("one", "a.js"), hit("two", "a.js")],
            COMPLETE_SCOPE,
            false,
        );
        if differing_proof {
            conflicting.symbols[1].source.document.provenance_id = "other-proof".into();
        } else {
            conflicting.symbols[1].source.document.content_hash = "b".repeat(64);
        }
        let failure = find(
            &json!(1),
            conflicting,
            &Default::default(),
            std::path::Path::new("/workspace"),
            false,
            true,
            false,
        )
        .unwrap_err();
        assert!(failure.starts_with("store_unavailable"));
    }
}

#[test]
fn find_keeps_longest_fitting_hit_prefix_and_only_its_closure() {
    let root = std::path::Path::new("/workspace");
    let id = json!("id\n\"\\");
    // Each name appears in a symbol and its displayKey, not in the summary.
    // Two fit; the third would overrun the complete JSON-RPC frame.
    let hits = vec![
        hit("A".repeat(12_000), "src/0.js"),
        hit("B".repeat(12_000), "src/1.js"),
        hit("C".repeat(12_000), "src/2.js"),
    ];
    for modern in [false, true] {
        let envelope = find(
            &id,
            fixture(&hits, COMPLETE_SCOPE, false),
            &Default::default(),
            root,
            true,
            modern,
            false,
        )
        .unwrap()
        .unwrap();
        assert_wire(envelope.clone(), &id, modern);
        let data = &envelope["data"];
        assert_eq!(data["symbols"].as_array().unwrap().len(), 2);
        assert_eq!(data["symbols"][0]["symbolId"], "sid:0");
        assert_eq!(data["symbols"][1]["symbolId"], "sid:1");
        let paths: Vec<_> = data["coverage"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["documentPath"].clone())
            .collect();
        assert_eq!(paths, vec![json!("src/0.js"), json!("src/1.js")]);
        assert_eq!(data["provenance"].as_array().unwrap().len(), 2);
        let documents = data["documents"].as_array().unwrap();
        assert_eq!(documents.len(), 2);
        for (index, document) in documents.iter().enumerate() {
            assert_eq!(document["ref"], index);
            assert_eq!(document["document"]["path"], format!("src/{index}.js"));
            assert!(
                data["symbols"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|symbol| symbol["doc"] == index)
            );
        }
        // #715: only the emitted (JavaScript) row.
        assert_eq!(data["scopeCoverage"].as_array().unwrap().len(), 1);
        assert_eq!(envelope["catchingUp"], true);
        assert_eq!(envelope["truncated"], true);
        assert_eq!(envelope["truncationReason"], "response_bytes");
        assert_eq!(envelope["partial"], true);

        let two = find(
            &id,
            fixture(&hits[..2], COMPLETE_SCOPE, false),
            &Default::default(),
            root,
            true,
            modern,
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(two["data"]["symbols"], data["symbols"]);
        assert_eq!(two["truncated"], false);
        assert_wire(two, &id, modern);
    }
}

#[test]
fn a_first_hit_too_large_returns_an_empty_truncated_prefix() {
    let root = std::path::Path::new("/workspace");
    let id = json!(3);
    for modern in [false, true] {
        let hits = vec![hit("Z".repeat(40_000), "src/z.js")];
        let envelope = find(
            &id,
            fixture(&hits, COMPLETE_SCOPE, false),
            &Default::default(),
            root,
            false,
            modern,
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(envelope["data"]["symbols"], json!([]));
        assert_eq!(envelope["data"]["documents"], json!([]));
        assert_eq!(envelope["data"]["coverage"], json!([]));
        assert_eq!(envelope["data"]["provenance"], json!([]));
        assert_eq!(envelope["truncated"], true);
        assert_eq!(envelope["truncationReason"], "response_bytes");
        assert_eq!(envelope["partial"], true);
        assert_wire(envelope, &id, modern);
    }
}

#[test]
fn all_hits_fit_with_limit_truncation_and_full_proof_closure() {
    let hits = vec![hit("first", "src/0.js"), hit("second", "src/1.js")];
    let id = json!(1);
    for modern in [false, true] {
        let envelope = find(
            &id,
            fixture(&hits, COMPLETE_SCOPE, true),
            &Default::default(),
            std::path::Path::new("/workspace"),
            false,
            modern,
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(envelope["data"]["symbols"].as_array().unwrap().len(), 2);
        assert_eq!(envelope["data"]["provenance"].as_array().unwrap().len(), 2);
        assert_eq!(envelope["data"]["coverage"].as_array().unwrap().len(), 2);
        assert_eq!(envelope["truncationReason"], "limit");
        assert_eq!(envelope["truncated"], true);
        assert_wire(envelope, &id, modern);
    }
}
