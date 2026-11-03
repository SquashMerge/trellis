#![cfg(feature = "test-support")]
//! Architecture references and rules use fresh selected semantic bindings (#523).
use crate::graph_fixtures::{Bind, PRODUCER, Publisher, Spec, add_second_producer, bind};
use serde_json::{Value, json};
use std::fs;
use trellis::{
    model::IndexPin,
    semantic_evidence::{self as s, CoverageState, Dispatch, Resolution},
    store::{MetricsOptions, Store},
};

// `orphan.rs` is not declared by `lib.rs`, so its structural sites are unresolved; `t.go()`
// is a method call no structural site spells.
const FILES: &[(&str, &str)] = &[
    ("src/lib.rs", "pub mod high;\npub mod low;\n"),
    (
        "src/high.rs",
        "pub struct Thing;\nimpl Thing {\n    pub fn go(&self) {}\n}\npub fn upper() {}\n",
    ),
    (
        "src/low.rs",
        "pub fn lower(t: &crate::high::Thing) {\n    t.go();\n}\n",
    ),
    (
        "src/orphan.rs",
        "pub fn orphan() {\n    crate::high::upper();\n}\n",
    ),
];

const RULES: &str = r#"version = 1
[layers.high]
order = 1
paths = ["src/high.rs", "src/lib.rs"]
[layers.low]
order = 2
paths = ["src/low.rs", "src/orphan.rs"]
[roles.orphan]
select = { name = "orphan" }
[roles.lower]
select = { name = "lower" }
[roles.upper]
select = { name = "upper" }
[roles.go]
select = { name = "go" }
[[rules]]
id = "orphan-calls-upper"
for_each = "orphan"
require = [{ calls = "upper" }]
[[rules]]
id = "lower-never-goes"
for_each = "lower"
forbid = [{ calls = "go" }]
[[rules]]
id = "orphan-never-goes"
for_each = "orphan"
forbid = [{ calls = "go" }]
"#;

const NO_BINDINGS: &str =
    "Calls and type relationships have no admitted bindings and remain unknown.";

fn bindings() -> Vec<Bind> {
    vec![
        bind("orphan", 0, "upper"),
        Bind {
            // A method call: a direct binding all the same.
            dispatch: Dispatch::Direct,
            ..bind("lower", 0, "go")
        },
    ]
}

type Published = (tempfile::TempDir, tempfile::TempDir, Store, IndexPin);

fn publish(spec: &Spec) -> Published {
    publish_with(spec, RULES, |_, _| {})
}

/// Publishes `spec`, adjusting its evidence before the validator admits it.
fn publish_with(
    spec: &Spec,
    rules: &str,
    transform: impl FnOnce(&mut s::SemanticPublication, &mut s::CapturedComponents),
) -> Published {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut publisher = Publisher::open(state.path(), root.path());
    let pin = publisher.publish_with_semantic(spec, transform);
    fs::create_dir_all(root.path().join(".trellis")).unwrap();
    fs::write(root.path().join(".trellis/rules.toml"), rules).unwrap();
    (state, root, publisher.store, pin)
}

fn semantic() -> Spec {
    let mut spec = Spec::new(FILES);
    spec.bindings = bindings();
    spec
}

fn read(store: &Store, kind: &str, args: Value) -> Value {
    let response = store.evidence_response().unwrap();
    let payload = response
        .architecture_response(kind, &args, &mut || Ok(()))
        .unwrap();
    response.finish(payload).unwrap()
}

fn view(store: &Store, pin: IndexPin) -> Value {
    let response = store.evidence_response().unwrap();
    let value = response
        .architecture_view(
            &MetricsOptions {
                pin: Some(pin),
                aggregate: "max".into(),
                ..Default::default()
            },
            &mut || Ok(()),
        )
        .unwrap();
    response.finish(value).unwrap()
}

fn edge<'a>(data: &'a Value, source: &str, target: &str) -> Option<&'a Value> {
    data["production"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["source"]["path"] == source && e["target"]["path"] == target)
}

fn instance<'a>(data: &'a Value, rule: &str, path: &str, destination: Option<&str>) -> &'a Value {
    data["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| {
            row["rule"] == rule
                && row["target"]["path"] == path
                && match destination {
                    Some(d) => row["destination"]["path"] == d,
                    None => true,
                }
        })
        .unwrap_or_else(|| panic!("no {rule} instance for {path}"))
}

/// What a syntax-only evaluation of the fixture shows: the orphan's site is unknown and
/// call constraints have no bindings.
fn assert_syntax_only(store: &Store, pin: IndexPin) {
    let references = read(store, "references", json!({"expectedBasis":pin}));
    assert_eq!(references["evidenceTier"], "syntax");
    assert_eq!(references["counts"]["semanticSites"], 0);
    assert!(edge(&references, "src/orphan.rs", "src/high.rs").is_none());
    assert!(
        references["unresolved"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["source"]["path"] == "src/orphan.rs")
    );
    let structural = edge(&references, "src/low.rs", "src/high.rs").unwrap();
    assert_eq!(structural["evidenceTier"], "syntax");
    assert_eq!(structural["count"], 1);
    for site in structural["sites"].as_array().unwrap() {
        assert_eq!(site["mapping"], "structural");
        assert_eq!(site["provenanceId"], Value::Null);
    }
    let check = read(store, "check", json!({"expectedBasis":pin}));
    let layers = instance(&check, "$layers", "src/orphan.rs", None);
    assert_eq!(layers["destination"], Value::Null);
    assert_eq!(layers["status"], "unknown");
    for rule in ["orphan-calls-upper", "lower-never-goes"] {
        let path = if rule.starts_with("orphan") {
            "src/orphan.rs"
        } else {
            "src/low.rs"
        };
        let row = instance(&check, rule, path, None);
        assert_eq!(row["status"], "unknown", "{rule}");
        assert_eq!(row["constraints"][0]["status"], "unknown");
        assert_eq!(
            row["constraints"][0]["unresolved"],
            json!(["call_binding_unavailable"])
        );
    }
    assert!(
        check["limitations"]
            .as_array()
            .unwrap()
            .contains(&json!(NO_BINDINGS))
    );
}

#[test]
fn bindings_map_unresolved_and_unspelled_sites_and_settle_rules() {
    let (_state, _root, store, pin) = publish(&semantic());
    let references = read(&store, "references", json!({"expectedBasis":pin}));
    assert_eq!(references["evidenceTier"], "mixed");
    assert_eq!(references["semanticComplete"], false);
    assert_eq!(
        references["semanticProducers"],
        json!([{"language":"rust","producerId":PRODUCER}])
    );
    assert_eq!(references["counts"]["semanticSites"], 2);
    // The orphan's unresolved structural site is bound by its call binding.
    let orphan = edge(&references, "src/orphan.rs", "src/high.rs").unwrap();
    assert_eq!(orphan["evidenceTier"], "semantic");
    assert_eq!(orphan["semanticSites"], 1);
    let site = &orphan["sites"][0];
    assert_eq!(site["mapping"], "semantic");
    assert_eq!(site["relationKind"], "reference");
    assert_eq!(site["spelling"], "crate::high::upper");
    assert_eq!(site["provenanceId"], "call-orphan-0");
    assert!(
        !references["unresolved"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["source"]["path"] == "src/orphan.rs")
    );
    // The method call becomes a semantic call site beside the structural type path.
    let low = edge(&references, "src/low.rs", "src/high.rs").unwrap();
    assert_eq!(low["count"], 2);
    assert_eq!(low["semanticSites"], 1);
    assert_eq!(low["evidenceTier"], "mixed");
    let call = low["sites"]
        .as_array()
        .unwrap()
        .iter()
        .find(|site| site["relationKind"] == "call")
        .unwrap();
    assert_eq!(call["spelling"], "go");
    assert_eq!(call["mapping"], "semantic");
    assert!(call["callId"].as_str().unwrap().starts_with("occ:v2:"));
    assert!(
        references["limitations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|text| text
                .as_str()
                .unwrap()
                .starts_with("Semantic sites use fresh bindings"))
    );

    let check = read(&store, "check", json!({"expectedBasis":pin}));
    // Upward from low to high: the formerly unknown orphan site now deviates.
    let layers = instance(&check, "$layers", "src/orphan.rs", Some("src/high.rs"));
    assert_eq!(layers["status"], "deviates");
    assert_eq!(layers["evidence"][0]["kind"], "semanticBinding");
    assert_eq!(layers["evidence"][0]["provenanceId"], "call-orphan-0");
    assert!(
        !check["instances"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["rule"] == "$layers"
                && row["target"]["path"] == "src/orphan.rs"
                && row["destination"].is_null())
    );
    let required = instance(&check, "orphan-calls-upper", "src/orphan.rs", None);
    assert_eq!(required["status"], "conforms");
    assert_eq!(required["constraints"][0]["status"], "conforms");
    assert!(
        required["constraints"][0]["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "semanticBinding" && e["provenanceId"] == "call-orphan-0")
    );
    assert_eq!(
        instance(&check, "lower-never-goes", "src/low.rs", None)["status"],
        "deviates"
    );
    // Every measured call of `orphan` is bound and none reaches `go`.
    assert_eq!(
        instance(&check, "orphan-never-goes", "src/orphan.rs", None)["status"],
        "conforms"
    );
    let limitations = check["limitations"].as_array().unwrap();
    assert!(!limitations.contains(&json!(NO_BINDINGS)));
    assert_eq!(check["counts"]["unknown"], 0);

    // The summary arrow from low to high is exact: no low site stayed unresolved.
    let summary = view(&store, pin);
    assert_eq!(summary["evidenceTier"], "mixed");
    let arrow = summary["edges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| {
            e["sourceLayer"] == "low"
                && e["destinationLayer"] == "high"
                && e["role"] == "production"
        })
        .unwrap();
    assert_eq!(arrow["count"], 3);
    assert_eq!(arrow["semanticSites"], 2);
    assert_eq!(arrow["evidenceTier"], "mixed");
    assert_eq!(arrow["coverage"]["complete"], true);
    assert_eq!(arrow["coverage"]["unresolvedSites"], 0);
    let low_row = summary["references"]["coverage"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["layer"] == "low" && row["role"] == "production")
        .unwrap();
    assert_eq!(low_row["semanticSites"], 2);
    assert_eq!(low_row["unresolvedSites"], 0);
}

#[test]
fn without_a_semantic_import_rules_stay_unknown_and_arrows_bounded() {
    let (_state, _root, store, pin) = publish(&Spec::syntax_only(FILES));
    assert_syntax_only(&store, pin);
    let references = read(&store, "references", json!({"expectedBasis":pin}));
    assert_eq!(references["semanticProducers"], json!([]));
    let summary = view(&store, pin);
    assert_eq!(summary["evidenceTier"], "syntax");
    let arrow = summary["edges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["sourceLayer"] == "low" && e["destinationLayer"] == "high")
        .unwrap();
    assert_eq!(arrow["evidenceTier"], "syntax");
    assert_eq!(arrow["coverage"]["complete"], false);
}

#[test]
fn stale_unusable_and_unbound_evidence_reverts_to_structural() {
    // Producer drift makes every binding possibly stale.
    let mut stale = semantic();
    stale.requested_version = "v2".into();
    let (_state, _root, store, pin) = publish(&stale);
    assert_syntax_only_with_producer(&store, pin, "call_binding_incomplete");

    // Unresolved and ambiguous bindings never guess a target, and an unresolved binding
    // at a structural site keeps the structural result.
    for resolution in [Resolution::Unresolved, Resolution::Ambiguous] {
        let mut spec = semantic();
        for binding in &mut spec.bindings {
            binding.resolution = resolution;
        }
        let (_state, _root, store, pin) = publish(&spec);
        let references = read(&store, "references", json!({"expectedBasis":pin}));
        assert!(edge(&references, "src/orphan.rs", "src/high.rs").is_none());
        let low = edge(&references, "src/low.rs", "src/high.rs").unwrap();
        assert_eq!(low["count"], 1);
        assert_eq!(low["evidenceTier"], "syntax");
        // A call without a known target is not a site, like a call without a binding; the
        // orphan's structural site stays unresolved.
        let unresolved = references["unresolved"].as_array().unwrap();
        assert!(
            !unresolved
                .iter()
                .any(|row| row["site"]["relationKind"] == "call")
        );
        assert!(
            unresolved
                .iter()
                .any(|row| row["source"]["path"] == "src/orphan.rs")
        );
        let check = read(&store, "check", json!({"expectedBasis":pin}));
        let row = instance(&check, "orphan-calls-upper", "src/orphan.rs", None);
        assert_eq!(row["status"], "unknown");
        assert_eq!(
            row["constraints"][0]["unresolved"],
            json!(["call_binding_incomplete"])
        );
        assert_eq!(
            instance(&check, "lower-never-goes", "src/low.rs", None)["status"],
            "unknown"
        );
    }
}

/// Stale evidence keeps the producer listed but uses none of its bindings.
fn assert_syntax_only_with_producer(store: &Store, pin: IndexPin, expected_reason: &str) {
    let references = read(store, "references", json!({"expectedBasis":pin}));
    assert_eq!(references["evidenceTier"], "syntax");
    assert_eq!(references["counts"]["semanticSites"], 0);
    assert!(edge(&references, "src/orphan.rs", "src/high.rs").is_none());
    let low = edge(&references, "src/low.rs", "src/high.rs").unwrap();
    assert_eq!(low["count"], 1);
    assert_eq!(low["evidenceTier"], "syntax");
    let check = read(store, "check", json!({"expectedBasis":pin}));
    assert_eq!(
        instance(&check, "$layers", "src/orphan.rs", None)["status"],
        "unknown"
    );
    let row = instance(&check, "orphan-calls-upper", "src/orphan.rs", None);
    assert_eq!(row["status"], "unknown");
    assert_eq!(
        row["constraints"][0]["unresolved"],
        json!([expected_reason])
    );
}

#[test]
fn unrelated_native_edit_keeps_origin_proofs_but_never_traverses_them() {
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::{
        graph_query::{BoundaryReason, GraphQuery, Visit},
        indexer::{IndexOptions, index_workspace_bundle},
        model::{SemanticAvailability, SemanticState, ViewQuery},
    };

    let mut spec = semantic();
    spec.files.push((
        "src/other.rs".into(),
        "pub fn unrelated() -> usize { 1 }\n".into(),
    ));
    let (_state, root, store, original_pin) = publish(&spec);
    let original = store.semantic_at(original_pin).unwrap();
    assert_eq!(original.call_bindings.len(), 2);
    assert_eq!(original.provenance.len(), 2);
    assert!(
        original
            .provenance
            .iter()
            .all(|proof| proof.freshness == s::Freshness::Fresh)
    );
    let (original_native, _) = store.retained_native_snapshot_at(original_pin).unwrap();
    let seed = crate::graph_fixtures::syntax_id(&original_native, "orphan");
    let query: ViewQuery = serde_json::from_value(json!({
        "seed":seed,"depth":3,"maxNodes":40
    }))
    .unwrap();
    let fresh = store.query_view(&query).unwrap().unwrap();
    assert_eq!(fresh.nodes.len(), 2);
    assert_eq!(
        fresh.semantic.bindings[0].boundary_reason,
        BoundaryReason::None
    );
    assert_eq!(
        read(&store, "references", json!({"expectedBasis":original_pin}))["counts"]["semanticSites"],
        2
    );

    // Only an unrelated document changes. Neither bound call nor its target changes.
    fs::write(
        root.path().join("src/other.rs"),
        "pub fn unrelated() -> usize { 2 }\n",
    )
    .unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(root.path().to_owned());
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    assert_ne!(native.revision.id, original_native.revision.id);
    let leader = store.publication_for_tests().unwrap();
    let pin = store
        .publish_native(&graph, &capture, &native, &leader, original_pin, &cancel)
        .unwrap();
    assert_ne!(pin, original_pin);
    let mut compared = original.clone();
    for proof in &mut compared.provenance {
        proof.freshness = s::Freshness::PossiblyStale;
    }
    let selected = store.semantic_at(pin).unwrap();
    assert_eq!(selected.call_bindings, original.call_bindings);
    assert_eq!(selected.provenance, compared.provenance);
    assert_eq!(store.semantic_at(original_pin).unwrap(), original);

    // Availability and proof freshness are distinct: the unchanged source is available
    // from its original semantic pin, but its proof is compared against this native head.
    let response = store.evidence_response().unwrap();
    let availability = response
        .document_semantic_status_at("src/orphan.rs", Some(pin))
        .unwrap();
    assert_eq!(availability.state, SemanticAvailability::Fresh);
    let origin = availability.semantic_origin.unwrap();
    assert_eq!(origin.pin, original_pin);
    assert_eq!(origin.revision_id, original_native.revision.id);
    let status = response.status().unwrap();
    let rust = status
        .semantic_languages
        .iter()
        .find(|row| row.language == "rust")
        .unwrap();
    assert_eq!(rust.semantic_state, SemanticState::Stale);
    let tiers = response.mcp_language_tiers().unwrap();
    assert_eq!(tiers[1].freshness, "possiblyStale");
    assert!(!tiers[1].semantic());
    let answer = response
        .graph_query(
            &GraphQuery {
                root_syntax_id: seed.clone(),
                semantic_producer_id: Some(PRODUCER.into()),
                depth: 3,
                max_nodes: 40,
                max_calls: 40,
                include_external: true,
                include_tests: true,
            },
            Some(pin),
            &mut || Ok(()),
        )
        .unwrap();
    assert_eq!(answer.basis, pin);
    let result = answer.outcome.unwrap();
    assert_eq!(result.nodes.len(), 1);
    assert_eq!(result.edges.len(), 1);
    assert_eq!(result.edges[0].visit, Visit::Boundary);
    assert_eq!(result.edges[0].boundary_reason, BoundaryReason::Stale);
    assert_eq!(result.edges[0].to, None);
    assert_eq!(
        result.edges[0].binding.as_ref().unwrap().provenance_id,
        "call-orphan-0"
    );
    let proof = result
        .provenance
        .iter()
        .find(|proof| proof.id == "call-orphan-0")
        .unwrap();
    assert_eq!(
        proof,
        compared
            .provenance
            .iter()
            .find(|proof| proof.id == "call-orphan-0")
            .unwrap()
    );
    response.finish(()).unwrap();

    let calls = store.query_view(&query).unwrap().unwrap();
    assert_eq!(calls.nodes.len(), 1);
    assert_eq!(calls.semantic.bindings.len(), 1);
    let binding = &calls.semantic.bindings[0];
    assert_eq!(binding.freshness, s::Freshness::PossiblyStale);
    assert_eq!(binding.boundary_reason, BoundaryReason::Stale);
    assert_eq!(binding.target_id, None);
    assert_eq!(binding.binding.provenance_id, "call-orphan-0");
    let sequence = store.sequence_at(&seed, pin, false).unwrap().unwrap();
    let step = sequence
        .steps
        .iter()
        .find(|step| step.call_id.is_some())
        .unwrap();
    assert_eq!(step.target, None);
    assert!(step.children.is_empty());
    assert!(step.call_evidence.is_none());
    assert_eq!(
        sequence.semantic.tier,
        trellis::behavior::EvidenceTier::Syntax
    );
    assert!(
        sequence
            .semantic
            .reason
            .as_deref()
            .unwrap()
            .contains("stale")
    );

    // Cross-revision origin tuples are excluded before Architecture admits a producer.
    // The constraint has unavailable bindings, not an incomplete admitted tuple.
    assert_syntax_only_with_producer(&store, pin, "call_binding_unavailable");
    let references = read(&store, "references", json!({"expectedBasis":pin}));
    let check = read(&store, "check", json!({"expectedBasis":pin}));
    assert_eq!(
        instance(&check, "orphan-never-goes", "src/orphan.rs", None)["status"],
        "unknown"
    );
    let original_ids: Vec<_> = original
        .provenance
        .iter()
        .map(|proof| proof.id.as_str())
        .collect();
    let mut pending = vec![&references, &check];
    while let Some(value) = pending.pop() {
        match value {
            Value::Object(fields) => {
                for key in ["id", "provenanceId"] {
                    if let Some(id) = fields.get(key).and_then(Value::as_str) {
                        assert!(!original_ids.contains(&id), "origin proof leaked: {id}");
                    }
                }
                pending.extend(fields.values());
            }
            Value::Array(values) => pending.extend(values),
            _ => {}
        }
    }
}

#[test]
fn semantic_sites_keep_sample_and_summary_bounds() {
    // 120 bound calls from one function into another module.
    let calls: String = (0..120).map(|i| format!("t.f{i}(); ")).collect();
    let methods: String = (0..120)
        .map(|i| format!("pub fn f{i}(&self) {{}}\n"))
        .collect();
    let files = [
        ("src/lib.rs", "pub mod high;\npub mod low;\n".to_owned()),
        (
            "src/high.rs",
            format!("pub struct Thing;\nimpl Thing {{\n{methods}}}\n"),
        ),
        (
            "src/low.rs",
            format!("pub fn lower(t: &crate::high::Thing) {{ {calls}}}\n"),
        ),
        ("src/orphan.rs", "pub fn orphan() {}\n".to_owned()),
    ];
    let borrowed: Vec<(&str, &str)> = files.iter().map(|(p, t)| (*p, t.as_str())).collect();
    let mut spec = Spec::new(&borrowed);
    spec.bindings = (0..120)
        .map(|i| bind("lower", i, &format!("f{i}")))
        .collect();
    let (_state, _root, store, pin) = publish(&spec);
    let references = read(&store, "references", json!({"expectedBasis":pin,"limit":3}));
    let low = edge(&references, "src/low.rs", "src/high.rs").unwrap();
    assert_eq!(low["count"], 121);
    assert_eq!(low["semanticSites"], 120);
    assert_eq!(low["sites"].as_array().unwrap().len(), 3);
    assert_eq!(low["omittedSites"], 118);
    let summary = view(&store, pin);
    for arrow in summary["edges"].as_array().unwrap() {
        for pair in arrow["pairs"].as_array().unwrap() {
            assert!(pair["sites"].as_array().unwrap().len() <= 5);
        }
    }
    // The summary carries no per-site arrays beyond the pair samples.
    assert!(summary["references"].get("unresolved").is_none());
    assert!(serde_json::to_vec(&summary).unwrap().len() < 256 * 1024);
}

#[test]
fn partial_tuples_and_unselected_producers_fall_back() {
    // A partial tuple still proves a required call, but never a forbidden call's absence.
    let mut partial = semantic();
    partial
        .states
        .insert("src/orphan.rs".into(), CoverageState::Partial);
    let (_state, _root, store, pin) = publish(&partial);
    let check = read(&store, "check", json!({"expectedBasis":pin}));
    assert_eq!(
        instance(&check, "orphan-calls-upper", "src/orphan.rs", None)["status"],
        "conforms"
    );
    let forbidden = instance(&check, "orphan-never-goes", "src/orphan.rs", None);
    assert_eq!(forbidden["status"], "unknown");
    assert_eq!(
        forbidden["constraints"][0]["unresolved"],
        json!(["call_binding_incomplete"])
    );

    // Two producers covering Rust select neither: syntax only, as without an import.
    let (_state, _root, store, pin) = publish_with(&semantic(), RULES, add_second_producer);
    assert_syntax_only(&store, pin);
    let references = read(&store, "references", json!({"expectedBasis":pin}));
    assert_eq!(references["semanticProducers"], json!([]));
}

#[test]
fn calls_within_one_module_add_no_self_edge() {
    let mut spec = Spec::new(&[
        ("src/lib.rs", "pub mod low;\n"),
        ("src/low.rs", "pub fn a() {\n    b();\n}\npub fn b() {}\n"),
    ]);
    spec.bindings = vec![bind("a", 0, "b")];
    let (_state, _root, store, pin) = publish(&spec);
    let references = read(&store, "references", json!({"expectedBasis":pin}));
    assert!(edge(&references, "src/low.rs", "src/low.rs").is_none());
}

const SHAPES: &[(&str, &str)] = &[
    ("src/lib.rs", "pub mod high;\npub mod low;\n"),
    ("src/high.rs", "pub trait Shape {}\n"),
    (
        "src/low.rs",
        "pub struct Square;\nimpl crate::high::Shape for Square {}\n",
    ),
];

const SHAPE_RULES: &str = r#"version = 1
[roles.square]
select = { name = "Square" }
[roles.shape]
select = { name = "Shape" }
[[rules]]
id = "squares-are-shapes"
for_each = "square"
require = [{ implements = "shape" }]
[[rules]]
id = "squares-are-not-shapes"
for_each = "square"
forbid = [{ implements = "shape" }]
"#;

#[test]
fn type_relationships_settle_implements_constraints() {
    let mut spec = Spec::new(SHAPES);
    spec.declaration_bindings = vec!["Square".into(), "Shape".into()];
    let (_state, _root, store, pin) = publish_with(&spec, SHAPE_RULES, |semantic, _| {
        let target = |name: &str| {
            semantic
                .symbols
                .iter()
                .find(|symbol| symbol.display_name.as_deref() == Some(name))
                .unwrap()
                .declarations[0]
                .clone()
        };
        let (square, shape) = (target("Square"), target("Shape"));
        let mut proof = semantic
            .provenance
            .iter()
            .find(|proof| proof.id == "decl-Square")
            .unwrap()
            .clone();
        proof.id = "rel-square".into();
        proof.evidence_kind = s::EvidenceKind::TypeRelationship;
        let document = proof.document.clone();
        semantic.provenance.push(proof);
        semantic.provenance.sort_by(|a, b| a.id.cmp(&b.id));
        semantic.type_relationships.push(s::TypeRelationship {
            kind: s::RelationshipKind::Implements,
            source: square,
            target: shape,
            provenance_id: "rel-square".into(),
        });
        for coverage in &mut semantic.coverage {
            coverage.supported_roles.push(s::Role::Type);
            if coverage.document_path == document.path {
                coverage.observed_roles.push(s::Role::Type);
            }
        }
    });
    let check = read(&store, "check", json!({"expectedBasis":pin}));
    let required = instance(&check, "squares-are-shapes", "src/low.rs", None);
    assert_eq!(required["status"], "conforms");
    assert!(
        required["constraints"][0]["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "typeRelationship" && e["provenanceId"] == "rel-square")
    );
    assert_eq!(
        instance(&check, "squares-are-not-shapes", "src/low.rs", None)["status"],
        "deviates"
    );

    // Without the relationship, neither constraint is settled.
    let mut spec = Spec::new(SHAPES);
    spec.declaration_bindings = vec!["Square".into(), "Shape".into()];
    let (_state, _root, store, pin) = publish_with(&spec, SHAPE_RULES, |_, _| {});
    let check = read(&store, "check", json!({"expectedBasis":pin}));
    let required = instance(&check, "squares-are-shapes", "src/low.rs", None);
    assert_eq!(required["status"], "unknown");
    assert_eq!(
        required["constraints"][0]["unresolved"],
        json!(["type_relationship_absent"])
    );
    let forbidden = instance(&check, "squares-are-not-shapes", "src/low.rs", None);
    assert_eq!(forbidden["status"], "unknown");
    assert_eq!(
        forbidden["constraints"][0]["unresolved"],
        json!(["type_relationship_absence_unproven"])
    );
}

#[test]
fn corrupt_semantic_rows_fail_typed_after_reopen() {
    for (column, value) in [("join_start", "-1"), ("dispatch", "'bogus'")] {
        let (state, root, store, pin) = publish(&semantic());
        drop(store);
        let index = fs::read_dir(state.path().join("cache/indexes"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.is_dir())
            .unwrap()
            .join("index.db");
        let db = rusqlite::Connection::open(index).unwrap();
        let changed = db
            .execute(
                &format!(
                    "UPDATE semantic_call_bindings SET {column}={value} WHERE provenance_id='call-lower-0'"
                ),
                [],
            )
            .unwrap();
        assert!(changed > 0, "{column}");
        drop(db);
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        let response = store.evidence_response().unwrap();
        let error = response
            .architecture_response("references", &json!({"expectedBasis":pin}), &mut || Ok(()))
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("corrupt semantic"), "{column}: {text}");
    }
}
