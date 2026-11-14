use super::*;
use crate::{
    graph_query::{
        DocumentFacts, FrontierReason, GraphAnswer, GraphFacts, GraphRequest, GraphSnapshot,
        evaluate,
    },
    model::IndexPin,
    native_evidence::{self as native, DocumentKey},
    semantic_evidence as sem,
};
use std::{collections::BTreeMap, sync::Arc};

const SS: &str = "source-set:v1:s";
const REV: &str = "revision:v2:r";

/// #660: the call graph names its nodes' languages when pending, including under
/// `compact`, and omits pending languages outside the graph.
#[test]
fn call_graph_carries_semantic_status_for_node_languages() {
    use crate::model::SemanticStatus::{CatchingUp, NeedsReimport};
    let tiers = super::test_support::pending_tiers(&[
        ("rust", CatchingUp, "re-import pending"),
        ("java", NeedsReimport, "re-import required"),
    ]);
    for compact in [false, true] {
        let graph = |tiers: &Tiers| {
            call_graph(
                &json!("graph"),
                &mut answer(1, 1),
                None,
                &Default::default(),
                tiers,
                compact,
                std::path::Path::new("/w"),
                false,
                true,
            )
            .unwrap()
            .unwrap()
        };
        assert_eq!(graph(&Default::default())["semanticStatus"], json!([]));
        assert_eq!(
            graph(&tiers)["semanticStatus"],
            json!([{"language":"rust","state":"catchingUp","reason":"re-import pending"}])
        );
    }
}

fn sid(n: u64) -> String {
    format!("sid:v1:{n:032x}")
}

/// A syntax-only root whose name is `padding` bytes long and which makes `calls` calls.
fn answer(calls: u64, padding: usize) -> GraphAnswer {
    let key = DocumentKey {
        source_set_id: SS.into(),
        language: "rust".into(),
        path: "src/lib.rs".into(),
    };
    let name = "r".repeat(padding.max(1));
    let declaration = native::Declaration {
        role: native::DeclarationRole::Production,
        syntax_id: sid(1),
        document: key.clone(),
        revision_id: REV.into(),
        kind: "function".into(),
        name: Some(name.clone()),
        lookup_key: Some(name.clone()),
        ancestors: vec![],
        key: native::Key {
            kind: "function".into(),
            name: Some(name.clone()),
            signature: None,
            ordinal: 0,
        },
        range: native::Range { start: 0, end: 1 },
        name_range: None,
        header: native::Header {
            kind: "function".into(),
            name: Some(name),
            modifiers: vec![],
            type_parameters: vec![],
            parameters: vec![],
            result_type: None,
            bases: vec![],
        },
        provenance_id: "native:doc".into(),
    };
    let owned = (0..calls)
        .map(|i| native::Call {
            id: format!("occ:v2:{:032x}", 100 + i),
            owner_syntax_id: sid(1),
            ordinal: i as usize,
            document: key.clone(),
            revision_id: REV.into(),
            range: native::Range {
                start: 10 + i as usize,
                end: 11 + i as usize,
            },
            callee_range: None,
            spelling: Some(format!("f{i}")),
            region_ids: vec![],
            provenance_id: "native:doc".into(),
        })
        .collect::<Vec<_>>();
    let document = DocumentFacts {
        projection: None,
        key: key.clone(),
        declarations: BTreeMap::from([(sid(1), declaration)]),
        calls: BTreeMap::from([(sid(1), owned)]),
        native_coverage: sem::Coverage {
            producer_id: "native".into(),
            language: sem::Language::Rust,
            source_set_id: SS.into(),
            document_path: key.path.clone(),
            revision_id: REV.into(),
            requested: true,
            selected: true,
            state: sem::CoverageState::Complete,
            supported_roles: vec![],
            observed_roles: vec![],
            diagnostic: None,
            reasons: None,
        },
        native_provenance: sem::Provenance {
            id: "native:doc".into(),
            producer_id: "native".into(),
            document: key.clone(),
            revision_id: REV.into(),
            content_hash: "h".into(),
            evidence_kind: sem::EvidenceKind::MeasuredSyntax,
            basis: None,
            freshness: sem::Freshness::Fresh,
            derived_from: None,
        },
        semantic: None,
    };
    let mut facts = GraphFacts::complete(GraphSnapshot {
        source_set_id: SS.into(),
        revision_id: REV.into(),
    });
    facts.locations.insert(sid(1), Some(key.clone()));
    facts.documents.insert(key, Arc::new(document));
    let requested = GraphRequest {
        source_set_id: SS.into(),
        revision_id: REV.into(),
        root_syntax_id: sid(1),
        semantic_producer_id: None,
        depth: 2,
        max_nodes: 150,
        max_calls: 500,
        include_external: false,
        include_tests: false,
        app_paths: None,
    };
    let outcome = evaluate(&requested, &mut facts).unwrap();
    GraphAnswer {
        basis: IndexPin {
            index_generation: uuid::Uuid::from_u128(1),
            index_revision: 1,
        },
        requested,
        outcome,
        facts,
    }
}

fn frame_len(envelope: &Value, modern: bool) -> usize {
    let frame = json!({"jsonrpc":"2.0","id":"graph",
        "result":crate::mcp::tools::result("trellis_call_graph", envelope.clone(), modern)});
    serde_json::to_vec(&frame).unwrap().len()
}

fn project(answer: &mut GraphAnswer, modern: bool) -> Option<Value> {
    call_graph(
        &json!("graph"),
        answer,
        None,
        &Default::default(),
        &Default::default(),
        false,
        std::path::Path::new("/w"),
        false,
        modern,
    )
    .unwrap()
}

#[test]
fn fitting_answer_is_unreduced() {
    for modern in [false, true] {
        let mut small = answer(3, 1);
        let envelope = project(&mut small, modern).unwrap();
        let result = &envelope["data"]["result"];
        assert_eq!(result["edges"].as_array().unwrap().len(), 3);
        assert_eq!(result["request"]["maxCalls"], 500);
        assert_eq!(envelope["data"]["requested"], result["request"]);
        assert_eq!(envelope["truncated"], false);
        assert_eq!(envelope["truncationReason"], Value::Null);
        assert_eq!(envelope["warnings"], result["warnings"]);
    }
}

#[test]
fn complete_frame_is_checked_once_without_building_prefixes() {
    for edges in [0, 1, 300, 500] {
        let complete = json!({"complete": edges});
        let mut checks = 0;
        let result = fit_call_graph(
            complete.clone(),
            edges,
            |_| panic!("a fitting complete answer must not build prefixes"),
            |frame| {
                checks += 1;
                frame == &complete
            },
        )
        .unwrap();
        assert_eq!(result, Some(complete));
        assert_eq!(checks, 1);
    }
}

#[test]
fn a_nonfitting_intermediate_prefix_cannot_hide_a_fitting_complete_frame() {
    let complete = json!({"jsonrpc":"2.0","id":"graph","result":"fits"});
    let intermediate = json!({"jsonrpc":"2.0","id":"graph",
        "result":{"disclosure":"x".repeat(crate::mcp::wire::RESPONSE_BYTES)}});
    let fits =
        |frame: &Value| serde_json::to_vec(frame).unwrap().len() < crate::mcp::wire::RESPONSE_BYTES;
    assert!(fits(&complete));
    assert!(!fits(&intermediate));
    assert_eq!(
        fit_call_graph(complete.clone(), 3, |_| Ok(intermediate.clone()), fits).unwrap(),
        Some(complete)
    );
}

#[test]
fn overflow_checks_complete_then_visits_each_prefix_once_until_first_refusal() {
    let mut prefixes = Vec::new();
    let mut checks = 0;
    let result = fit_call_graph(
        json!(500),
        500,
        |count| {
            prefixes.push(count);
            Ok(json!(count))
        },
        |frame| {
            checks += 1;
            frame.as_u64().unwrap() < 3
        },
    )
    .unwrap();
    assert_eq!(result, Some(json!(2)));
    assert_eq!(prefixes, [0, 1, 2, 3]);
    assert_eq!(checks, 5);
}

#[test]
fn oversized_answer_fits_a_frozen_prefix_without_changing_limits() {
    for modern in [false, true] {
        let mut big = answer(600, 1);
        let full = big.outcome.clone().unwrap();
        assert_eq!(full.edges.len(), 500);
        let envelope = project(&mut big, modern).unwrap();
        let result = &envelope["data"]["result"];
        let k = result["edges"].as_array().unwrap().len();
        assert!(k < 500);
        assert_eq!(result["request"]["maxCalls"], 500);
        assert_eq!(envelope["data"]["requested"], result["request"]);
        assert_eq!(envelope["truncated"], true);
        assert_eq!(envelope["truncationReason"], "response_bytes");
        assert_eq!(envelope["partial"], true);
        let frontier = result["frontier"].as_array().unwrap();
        assert!(frontier.iter().all(|row| row["nextOrdinal"].is_null()));
        let omitted = |reason| {
            frontier
                .iter()
                .filter(|row| row["reason"] == reason)
                .map(|row| row["omittedCalls"].as_u64().unwrap())
                .sum::<u64>()
        };
        assert_eq!(omitted(json!(FrontierReason::CallLimit)), 100);
        assert_eq!(
            omitted(json!(FrontierReason::ResponseBytes)),
            500 - k as u64
        );
        assert_eq!(result["omitted"]["callLimit"], 100);
        assert_eq!(result["omitted"]["responseBytes"], 500 - k as u64);
        assert!(frame_len(&envelope, modern) < crate::mcp::wire::RESPONSE_BYTES);
        // Syntax-only proofs project `basis: null` as `basisRef: null` (#665).
        let wire = |result: crate::graph_query::GraphResult| {
            let mut value = serde_json::to_value(result).unwrap();
            for proof in value["provenance"].as_array_mut().unwrap() {
                let proof = proof.as_object_mut().unwrap();
                assert_eq!(proof.remove("basis"), Some(Value::Null));
                proof.insert("basisRef".into(), Value::Null);
            }
            value
        };
        assert_eq!(wire(big.prefix(k).unwrap()), *result);
        let mut probe = envelope.clone();
        probe["data"]["result"] = wire(big.prefix(k + 1).unwrap());
        assert!(frame_len(&probe, modern) >= crate::mcp::wire::RESPONSE_BYTES);
        assert_eq!(project(&mut big, modern).unwrap(), envelope);
    }
}

#[test]
fn root_alone_over_cap_is_range_too_large() {
    let mut huge = answer(2, 70_000);
    assert!(project(&mut huge, true).is_none());
    let mut no_calls = answer(0, 70_000);
    assert!(project(&mut no_calls, false).is_none());
}
