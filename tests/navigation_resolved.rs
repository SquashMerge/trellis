#![cfg(feature = "test-support")]
//! Navigation uses real native publication and validated semantic fixture publications.
use serde_json::json;
use trellis::{
    model::IndexPin,
    navigation::{InvalidRequest, NavigationRequest, NavigationResult},
    semantic_evidence::{CoverageState, Dispatch, Resolution},
    store::Store,
};
#[path = "common/graph_fixtures.rs"]
mod graph_fixtures;
use graph_fixtures::{
    Bind, PRODUCER, Publisher, Spec, add_second_producer, bind, calls_of, syntax_id,
};

fn request(pin: IndexPin, path: &str, line: usize, producer: Option<&str>) -> NavigationRequest {
    serde_json::from_value(
        json!({"expectedRevision":pin,"path":path,"line":line,"semanticProducerId":producer}),
    )
    .unwrap()
}
fn navigate(store: &Store, pin: IndexPin, line: usize, producer: Option<&str>) -> NavigationResult {
    store
        .navigation_at(&request(pin, "a.rs", line, producer))
        .unwrap()
}
fn callees(result: &NavigationResult) -> Vec<&str> {
    result
        .targets
        .iter()
        .filter(|target| target.reason == "call")
        .map(|target| target.symbol.name.as_str())
        .collect()
}
fn workspace() -> (tempfile::TempDir, tempfile::TempDir) {
    (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap())
}

#[test]
fn fresh_direct_constructor_and_distinct_calls_are_explicit_choices() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[
        ("a.rs", "fn a() { b(); b(); c(); }\n"),
        ("b.rs", "fn b() {}\nfn c() {}\n"),
    ]);
    spec.bindings = vec![
        bind("a", 0, "b"),
        bind("a", 1, "b"),
        Bind {
            dispatch: Dispatch::Constructor,
            ..bind("a", 2, "c")
        },
    ];
    let pin = publisher.publish(&spec);
    let result = navigate(&publisher.store, pin, 1, Some(PRODUCER));
    assert_eq!(result.revision, pin);
    assert_eq!(callees(&result), vec!["b", "b", "c"]);
    let calls = calls_of(publisher.native(), "a");
    for (target, call) in result
        .targets
        .iter()
        .filter(|target| target.reason == "call")
        .zip(calls)
    {
        assert_eq!(target.action, "sequence");
        assert_eq!(target.match_kind, "resolved");
        assert_eq!(target.call_id.as_deref(), Some(call.id.as_str()));
        assert_eq!(target.range.as_ref(), Some(&call.range));
        assert_eq!(target.symbol.path, "b.rs");
        assert_eq!(
            target.symbol.id,
            syntax_id(publisher.native(), &target.symbol.name)
        );
    }
    assert!(!result.truncated);
    assert!(
        result
            .targets
            .iter()
            .any(|target| target.reason == "declaration" && target.symbol.name == "a")
    );
}

#[test]
fn absent_and_null_producer_are_syntax_only_unknown_and_empty_are_invalid() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[("a.rs", "fn a() { b(); }\nfn b() {}\n")]);
    spec.bindings = vec![bind("a", 0, "b")];
    let pin = publisher.publish(&spec);
    let absent: NavigationRequest =
        serde_json::from_value(json!({"expectedRevision":pin,"path":"a.rs","line":1})).unwrap();
    assert!(callees(&publisher.store.navigation_at(&absent).unwrap()).is_empty());
    assert!(callees(&navigate(&publisher.store, pin, 1, None)).is_empty());
    for producer in ["", "semantic.other"] {
        let error = publisher
            .store
            .navigation_at(&request(pin, "a.rs", 1, Some(producer)))
            .unwrap_err();
        assert!(
            error.downcast_ref::<InvalidRequest>().is_some(),
            "{error:#}"
        );
    }
    assert!(callees(&navigate(&publisher.store, pin, 2, Some(PRODUCER))).is_empty());
}

#[test]
fn unresolved_ambiguous_external_dispatch_stale_and_missing_evidence_never_guess() {
    for mode in [
        "unresolved",
        "ambiguous",
        "external",
        "dispatch",
        "stale",
        "missing",
        "failed",
        "omitted",
    ] {
        let (state, root) = workspace();
        let mut publisher = Publisher::open(state.path(), root.path());
        let mut spec = Spec::new(&[("a.rs", "fn a() {\n b();\n}\nfn b() {}\n")]);
        let mut binding = bind("a", 0, "b");
        match mode {
            "unresolved" => binding.resolution = Resolution::Unresolved,
            "ambiguous" => binding.resolution = Resolution::Ambiguous,
            "external" => binding.resolution = Resolution::External,
            "dispatch" => binding.dispatch = Dispatch::Virtual,
            "stale" => spec.requested_version = "v2".into(),
            "failed" => {
                spec.states.insert("a.rs".into(), CoverageState::Failed);
            }
            "omitted" => {
                spec.states.insert("a.rs".into(), CoverageState::Omitted);
            }
            _ => {}
        }
        if mode != "missing" {
            spec.bindings.push(binding);
        }
        let pin = publisher.publish(&spec);
        let result = navigate(&publisher.store, pin, 2, Some(PRODUCER));
        assert!(callees(&result).is_empty(), "{mode}");
        assert!(
            result
                .targets
                .iter()
                .any(|target| target.reason == "enclosing" && target.symbol.name == "a"),
            "{mode}"
        );
    }
}

#[test]
fn retained_pin_uses_its_bindings_and_targets_not_the_new_head() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[("a.rs", "fn a() { b(); }\nfn b() {}\nfn c() {}\n")]);
    spec.bindings = vec![bind("a", 0, "b")];
    let old = publisher.publish(&spec);
    spec.files[0].1 = "fn a() { c(); }\nfn b() {}\nfn c() {}\n".into();
    spec.bindings = vec![bind("a", 0, "c")];
    let new = publisher.publish(&spec);
    assert_eq!(
        callees(&navigate(&publisher.store, old, 1, Some(PRODUCER))),
        vec!["b"]
    );
    assert_eq!(
        callees(&navigate(&publisher.store, new, 1, Some(PRODUCER))),
        vec!["c"]
    );
}

#[test]
fn published_producer_outside_document_language_does_not_guess() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let spec = Spec::new(&[
        ("a.rs", "fn a() {}\n"),
        ("b.js", "function b() { a(); }\nfunction a() {}\n"),
    ]);
    let pin = publisher.publish(&spec);
    let result = publisher
        .store
        .navigation_at(&request(pin, "b.js", 1, Some(PRODUCER)))
        .unwrap();
    assert!(callees(&result).is_empty());
    assert!(
        result
            .targets
            .iter()
            .any(|target| target.symbol.name == "b")
    );
}

#[test]
fn navigation_row_and_target_limits_remain_bounded() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let source = graph_fixtures::fanout_source(80);
    let mut spec = Spec::new(&[("a.rs", &source)]);
    spec.bindings = (0..80)
        .map(|nth| bind("root", nth, &format!("f{nth}")))
        .collect();
    let pin = publisher.publish(&spec);
    let result = navigate(&publisher.store, pin, 1, Some(PRODUCER));
    assert!(result.truncated);
    assert!(!callees(&result).is_empty());
    assert!(result.targets.len() <= 64);
    assert!(serde_json::to_vec(&result).unwrap().len() <= trellis::navigation::MAX_RESPONSE_BYTES);
}

#[test]
fn internal_non_callable_target_and_lines_outside_call_range_are_not_callees() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[("a.rs", "fn a() {\n b();\n}\nstruct b {}\n")]);
    spec.bindings = vec![bind("a", 0, "b")];
    let pin = publisher.publish(&spec);
    assert!(callees(&navigate(&publisher.store, pin, 2, Some(PRODUCER))).is_empty());
    assert!(callees(&navigate(&publisher.store, pin, 3, Some(PRODUCER))).is_empty());
}

#[test]
fn missing_in_scope_coverage_is_corrupt_not_partial_navigation() {
    let (state, root) = workspace();
    let pin = {
        let mut publisher = Publisher::open(state.path(), root.path());
        let mut spec = Spec::new(&[("a.rs", "fn a() { b(); }\nfn b() {}\n")]);
        spec.bindings = vec![bind("a", 0, "b")];
        publisher.publish(&spec)
    };
    let index = std::fs::read_dir(state.path().join("cache/indexes"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
        .join("index.db");
    {
        let db = rusqlite::Connection::open(index).unwrap();
        db.execute_batch(
            "PRAGMA foreign_keys=OFF; DELETE FROM semantic_coverage WHERE path='a.rs'",
        )
        .unwrap();
    }
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let error = store
        .navigation_at(&request(pin, "a.rs", 1, Some(PRODUCER)))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("missing producer coverage tuple"),
        "{error:#}"
    );
}

fn sole_request(pin: IndexPin) -> NavigationRequest {
    serde_json::from_value(json!({
        "expectedRevision":pin, "path":"a.rs", "line":2,
        "semanticProducerPolicy":"solePublished"
    }))
    .unwrap()
}

#[test]
fn sole_published_policy_zero_one_multiple_and_retained_cardinality() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let files = [("a.rs", "fn a() {\n b();\n}\nfn b() {}\n")];
    let zero = publisher.publish(&Spec::syntax_only(&files));
    let mut spec = Spec::new(&files);
    spec.bindings = vec![bind("a", 0, "b")];
    let one = publisher.publish(&spec);
    let multiple = publisher.publish_with_semantic(&spec, add_second_producer);
    for pin in [zero, one, multiple] {
        let result = publisher.store.navigation_at(&sole_request(pin)).unwrap();
        assert_eq!(result.revision, pin);
        assert_eq!(
            callees(&result),
            if pin == one { vec!["b"] } else { vec![] }
        );
        assert_eq!(
            result
                .warnings
                .iter()
                .any(|warning| warning == "multiple semantic producers; showing syntax"),
            pin == multiple
        );
        assert!(
            result
                .targets
                .iter()
                .any(|target| target.reason == "enclosing" && target.symbol.name == "a")
        );
        assert!(!result.truncated);
    }
    // A non-policy request never inherits automatic selection, even on a sole-producer pin.
    assert!(callees(&navigate(&publisher.store, one, 2, None)).is_empty());
    // An explicit producer remains usable even when cardinality is multiple.
    assert_eq!(
        callees(&navigate(&publisher.store, multiple, 2, Some(PRODUCER))),
        vec!["b"]
    );
    assert!(
        callees(&navigate(
            &publisher.store,
            multiple,
            2,
            Some("semantic.second")
        ))
        .is_empty()
    );
}

#[test]
fn sole_policy_unresolved_call_keeps_measured_enclosing_fallback() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[("a.rs", "fn a() {\n b();\n}\nfn b() {}\n")]);
    spec.bindings = vec![Bind {
        resolution: Resolution::Unresolved,
        ..bind("a", 0, "b")
    }];
    let pin = publisher.publish(&spec);
    let result = publisher.store.navigation_at(&sole_request(pin)).unwrap();
    assert!(callees(&result).is_empty());
    assert!(
        result
            .targets
            .iter()
            .any(|target| target.reason == "enclosing" && target.symbol.name == "a")
    );
}

#[test]
fn policy_validation_rejects_unknown_and_combined_selectors_and_null_stays_syntax() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let pin = publisher.publish(&Spec::new(&[("a.rs", "fn a() {}\n")]));
    for policy in [json!("first"), json!(""), json!({}), json!(true)] {
        assert!(
            serde_json::from_value::<NavigationRequest>(json!({
                "expectedRevision":pin,"path":"a.rs","line":1,"semanticProducerPolicy":policy
            }))
            .is_err()
        );
    }
    let combined: NavigationRequest = serde_json::from_value(json!({
        "expectedRevision":pin,"path":"a.rs","line":1,
        "semanticProducerId":PRODUCER,"semanticProducerPolicy":"solePublished"
    }))
    .unwrap();
    assert!(
        combined
            .validate()
            .unwrap_err()
            .downcast_ref::<InvalidRequest>()
            .is_some()
    );
    for error in [
        publisher.store.navigation_at(&combined).unwrap_err(),
        publisher
            .store
            .evidence_response()
            .unwrap()
            .navigation_at(&combined)
            .unwrap_err(),
    ] {
        assert!(
            error.downcast_ref::<InvalidRequest>().is_some(),
            "{error:#}"
        );
    }
    let null: NavigationRequest = serde_json::from_value(json!({
        "expectedRevision":pin,"path":"a.rs","line":1,"semanticProducerPolicy":null
    }))
    .unwrap();
    match null {
        NavigationRequest::Source(selector) => {
            assert!(selector.semantic_producer_policy.is_none());
            assert!(selector.semantic_producer_id.is_none());
        }
        _ => panic!("source selector expected"),
    }
}
