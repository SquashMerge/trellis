#![cfg(feature = "test-support")]
//! Architecture references and rules use selected semantic bindings that are not stale
//! (#523): fresh ones, and possibly stale ones labelled as such (#936).
use crate::graph_fixtures::{
    Bind, PRODUCER, Publisher, Spec, add_second_producer, bind, syntax_id,
};
use serde_json::{Value, json};
use std::fs;
use trellis::{
    metrics_read::MetricsOptions,
    model::IndexPin,
    semantic_evidence::{self as s, CoverageState, Dispatch, Resolution},
    store::Store,
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
        assert_eq!(site["target"], Value::Null);
        assert_eq!(site["targetReason"], "structural_only");
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
    assert_eq!(site["relationKind"], "call");
    assert_eq!(site["spelling"], "crate::high::upper");
    assert_eq!(site["provenanceId"], "call-orphan-0");
    assert!(site["target"]["syntaxId"].is_string());
    assert!(site["target"]["displayKey"].is_string());
    assert_eq!(site["target"]["document"]["path"], "src/high.rs");
    assert!(site["target"]["lines"]["start"].as_u64().unwrap() >= 1);
    assert_eq!(site["targetReason"], Value::Null);
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
                .starts_with("Semantic sites use selected producer bindings"))
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

/// #936 (owner ruling 2026-11-12): possibly stale evidence, here from producer drift, keeps
/// mapping sites and settling rules until a producer re-run replaces it, and every site and
/// rule row it supports says `possiblyStale`.
#[test]
fn possibly_stale_evidence_maps_sites_labelled_possibly_stale() {
    let mut drift = semantic();
    drift.requested_version = "v2".into();
    let (_state, _root, store, pin) = publish(&drift);
    let references = read(&store, "references", json!({"expectedBasis":pin}));
    assert_eq!(references["evidenceTier"], "mixed");
    assert_eq!(references["counts"]["semanticSites"], 2);
    let orphan = edge(&references, "src/orphan.rs", "src/high.rs").unwrap();
    assert_eq!(orphan["evidenceTier"], "semantic");
    let low = edge(&references, "src/low.rs", "src/high.rs").unwrap();
    assert_eq!(low["semanticSites"], 1);
    let semantic_sites: Vec<&Value> = [orphan, low]
        .into_iter()
        .flat_map(|edge| edge["sites"].as_array().unwrap())
        .filter(|site| site["mapping"] == "semantic")
        .collect();
    assert_eq!(semantic_sites.len(), 2, "{references}");
    for site in semantic_sites {
        assert_eq!(site["freshness"], "possiblyStale", "{site}");
        assert_eq!(site["historicalFreshness"], "possiblyStale", "{site}");
        assert!(site.get("projection").is_none(), "{site}");
    }
    let check = read(&store, "check", json!({"expectedBasis":pin}));
    let required = instance(&check, "orphan-calls-upper", "src/orphan.rs", None);
    assert_eq!(required["status"], "conforms");
    let binding = required["constraints"][0]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "semanticBinding" && e["provenanceId"] == "call-orphan-0")
        .unwrap();
    assert_eq!(binding["freshness"], "possiblyStale", "{binding}");
    assert_eq!(
        instance(&check, "lower-never-goes", "src/low.rs", None)["status"],
        "deviates"
    );
}

#[test]
fn stale_unusable_and_unbound_evidence_reverts_to_structural() {
    // Stale evidence keeps the producer listed but is left out, as before #936.
    let (state, root, store, pin) = publish(&semantic());
    drop(store);
    mark_all_stale(&state);
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    assert_syntax_only_with_producer(&store, pin);

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
        // A call without a known target is not a site of its own, like a call without a
        // binding; the orphan's structural site stays unresolved, is still a call by its
        // callee path, and says why it has no target.
        let unresolved = references["unresolved"].as_array().unwrap();
        assert!(
            unresolved
                .iter()
                .all(|row| row["site"]["mapping"] == "structural")
        );
        let orphan = unresolved
            .iter()
            .find(|row| row["source"]["path"] == "src/orphan.rs")
            .unwrap();
        assert_eq!(orphan["site"]["relationKind"], "call");
        assert_eq!(orphan["site"]["target"], Value::Null);
        assert_eq!(
            orphan["site"]["targetReason"],
            if matches!(resolution, Resolution::Unresolved) {
                "unresolved"
            } else {
                "ambiguous"
            }
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
fn assert_syntax_only_with_producer(store: &Store, pin: IndexPin) {
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
        json!(["call_binding_incomplete"])
    );
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
    // Edges carry no pairs or site samples; the details route's edge kind pages them (#917).
    for arrow in summary["edges"].as_array().unwrap() {
        assert!(arrow.get("pairs").is_none(), "{arrow}");
    }
    // The summary carries no per-site arrays.
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
                    "UPDATE semantic_call_bindings SET {column}={value} WHERE (payload_id,provenance_ordinal) IN \
                     (SELECT payload_id,ordinal FROM semantic_provenance WHERE provenance_id='call-lower-0')"
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

#[test]
fn bound_method_call_evidence_names_caller_and_target() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut spec = Spec::new(&[
        ("src/lib.rs", "pub mod http;\npub mod producer;\n"),
        ("src/http.rs", "pub mod worker;\n"),
        (
            "src/http/worker.rs",
            "pub struct DaemonState;\nimpl DaemonState {\n    pub fn enqueue_generated_semantic_import(&self) {}\n}\npub enum RuntimePhase {\n    Ready,\n}\n",
        ),
        ("src/producer.rs", "pub mod import;\n"),
        (
            "src/producer/import.rs",
            "use crate::http::worker::RuntimePhase;\npub fn publish(scheduler: &crate::http::worker::DaemonState, _phase: RuntimePhase) {\n    scheduler.enqueue_generated_semantic_import();\n}\n",
        ),
    ]);
    spec.bindings = vec![bind("publish", 0, "enqueue_generated_semantic_import")];
    let mut publisher = Publisher::open(state.path(), root.path());
    let pin = publisher.publish(&spec);
    fs::create_dir_all(root.path().join(".trellis")).unwrap();
    fs::write(
        root.path().join(".trellis/rules.toml"),
        "version = 1\n[layers.entry]\norder = 1\npaths = [\"src/lib.rs\", \"src/http.rs\", \"src/http/**\"]\n[layers.indexing]\norder = 2\npaths = [\"src/producer.rs\", \"src/producer/**\"]\n",
    )
    .unwrap();
    let method = syntax_id(publisher.native(), "enqueue_generated_semantic_import");
    let check = read(&publisher.store, "check", json!({"expectedBasis":pin}));
    let row = instance(
        &check,
        "$layers",
        "src/producer/import.rs",
        Some("src/http/worker.rs"),
    );
    assert_eq!(row["status"], "deviates");
    assert_eq!(
        row["kindCounts"],
        json!({"call":1,"import":1,"reference":1,"type":0})
    );
    let evidence = row["evidence"].as_array().unwrap();
    let call = evidence
        .iter()
        .find(|e| e["relationKind"] == "call")
        .unwrap_or_else(|| panic!("no call evidence: {row}"));
    assert_eq!(call["kind"], "semanticBinding");
    assert_eq!(call["spelling"], "enqueue_generated_semantic_import");
    assert_eq!(call["lines"], json!({"start":3,"end":3}));
    assert_eq!(call["caller"]["name"], "publish");
    assert_eq!(
        call["caller"]["stableLookupKey"],
        "src/producer/import.rs::publish"
    );
    assert_eq!(call["owner"]["name"], "publish");
    assert_eq!(call["target"]["syntaxId"], method);
    assert_eq!(call["target"]["document"]["path"], "src/http/worker.rs");
    assert_eq!(call["target"]["lines"], json!({"start":3,"end":3}));
    let display = call["target"]["displayKey"].as_str().unwrap();
    assert!(display.starts_with("src/http/worker.rs#"), "{display}");
    assert!(
        display.ends_with(".enqueue_generated_semantic_import"),
        "{display}"
    );
    assert_eq!(call["targetReason"], Value::Null);
    for other in evidence.iter().filter(|e| e["relationKind"] != "call") {
        assert_eq!(other["target"], Value::Null, "{other}");
        assert_eq!(other["targetReason"], "unresolved", "{other}");
    }
    assert_eq!(evidence.len(), 3);
}

/// Marks every published semantic freshness row `stale`, as for a document whose bytes changed.
fn mark_all_stale(state: &tempfile::TempDir) {
    let index = fs::read_dir(state.path().join("cache/indexes"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
        .join("index.db");
    let db = rusqlite::Connection::open(index).unwrap();
    let changed = db
        .execute(
            "UPDATE semantic_coverage_freshness SET freshness='stale'",
            [],
        )
        .unwrap();
    assert!(changed > 0);
}

/// Deletes only the per-revision freshness association of the basis behind `provenance`.
/// Payload, provenance, basis and manifest rows stay valid, and SQLite's foreign keys pass.
fn delete_freshness(state: &tempfile::TempDir, provenance: &str) {
    let index = fs::read_dir(state.path().join("cache/indexes"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
        .join("index.db");
    let db = rusqlite::Connection::open(index).unwrap();
    db.pragma_update(None, "foreign_keys", "ON").unwrap();
    let changed = db
        .execute(
            "DELETE FROM semantic_coverage_freshness WHERE basis_id IN \
             (SELECT basis_id FROM semantic_provenance WHERE provenance_id=?1)",
            [provenance],
        )
        .unwrap();
    assert!(changed > 0, "{provenance}");
    let violations: i64 = db
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0, "the missing required row passes SQLite FKs");
}

/// AC10, #261: after reopening, the read fails typed instead of dropping the fact.
fn assert_missing_freshness_fails(published: Published, kind: &str) {
    let (state, root, store, pin) = published;
    drop(store);
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let response = store.evidence_response().unwrap();
    let error = response
        .architecture_response(kind, &json!({"expectedBasis":pin}), &mut || Ok(()))
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("incompatible_index: semantic freshness missing"),
        "{kind}: {text}"
    );
}

fn semantic_sites<'a>(data: &'a Value, provenance: &str) -> Vec<&'a Value> {
    data["production"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|edge| edge["sites"].as_array().unwrap())
        .filter(|site| site["provenanceId"] == provenance)
        .collect()
}

/// Control: a healthy index answers the same before and after reopening, with both calls.
#[test]
fn healthy_semantic_references_answer_identically_after_reopen() {
    let (state, root, store, pin) = publish(&semantic());
    let before = read(&store, "references", json!({"expectedBasis":pin}));
    assert_eq!(before["counts"]["semanticSites"], 2, "{before}");
    assert_eq!(before["partial"], false);
    assert_eq!(semantic_sites(&before, "call-lower-0").len(), 1, "{before}");
    drop(store);
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    assert_eq!(
        read(&store, "references", json!({"expectedBasis":pin})),
        before
    );
}

/// Review of #873: a call whose basis lost its freshness row is corruption, not a dropped
/// lower->go binding with `semanticSites: 1` and `partial: false`.
#[test]
fn missing_call_freshness_fails_typed_after_reopen() {
    let published = publish(&semantic());
    delete_freshness(&published.0, "call-lower-0");
    assert_missing_freshness_fails(published, "references");
}

/// Shapes with `Square implements Shape` and, optionally, a semantic use of `Shape` in
/// `src/low.rs`, which holds no calls, so the references and type-relationship reads are
/// the ones that meet the fact.
fn shapes(reference: bool, relationship: bool) -> Published {
    let mut spec = Spec::new(SHAPES);
    spec.declaration_bindings = vec!["Square".into(), "Shape".into()];
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut publisher = Publisher::open(state.path(), root.path());
    publisher.publish(&spec);
    let use_site = publisher
        .native()
        .references
        .iter()
        .find(|r| r.document.path == "src/low.rs" && r.spelling.ends_with("Shape"))
        .unwrap()
        .clone();
    let pin = publisher.publish_with_semantic(&spec, |semantic, _| {
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
        let base = semantic
            .provenance
            .iter()
            .find(|proof| proof.id == "decl-Square")
            .unwrap()
            .clone();
        let document = base.document.clone();
        let mut roles = vec![];
        if relationship {
            let mut proof = base.clone();
            proof.id = "rel-square".into();
            proof.evidence_kind = s::EvidenceKind::TypeRelationship;
            semantic.provenance.push(proof);
            semantic.type_relationships.push(s::TypeRelationship {
                kind: s::RelationshipKind::Implements,
                source: square,
                target: shape.clone(),
                provenance_id: "rel-square".into(),
            });
            roles.push(s::Role::Type);
        }
        if reference {
            let mut proof = base.clone();
            proof.id = "ref-shape".into();
            proof.evidence_kind = s::EvidenceKind::SemanticReference;
            semantic.references.push(s::Reference {
                id: use_site.id.clone(),
                owner_syntax_id: use_site.owner_syntax_id.clone(),
                ordinal: use_site.ordinal,
                document: use_site.document.clone(),
                revision_id: proof.revision_id.clone(),
                range: use_site.range.clone(),
                spelling: use_site.spelling.clone(),
                lookup_key: use_site.lookup_key.clone(),
                site: s::ReferenceSite::Use,
                roles: vec![s::Role::Read],
                resolution: Resolution::Resolved,
                declared_target: Some(shape),
                candidates: vec![],
                provenance_id: "ref-shape".into(),
            });
            semantic.provenance.push(proof);
            roles.push(s::Role::Read);
        }
        semantic.provenance.sort_by(|a, b| a.id.cmp(&b.id));
        for coverage in &mut semantic.coverage {
            coverage.supported_roles.extend(roles.iter().copied());
            coverage.supported_roles.sort();
            if coverage.document_path == document.path {
                coverage.observed_roles.extend(roles.iter().copied());
                coverage.observed_roles.sort();
            }
        }
    });
    fs::create_dir_all(root.path().join(".trellis")).unwrap();
    fs::write(root.path().join(".trellis/rules.toml"), SHAPE_RULES).unwrap();
    (state, root, publisher.store, pin)
}

#[test]
fn missing_reference_freshness_fails_typed_after_reopen() {
    // Control: the healthy reference is a semantic site the read returns.
    let (_state, _root, store, pin) = shapes(true, false);
    let data = read(&store, "references", json!({"expectedBasis":pin}));
    assert_eq!(semantic_sites(&data, "ref-shape").len(), 1, "{data}");

    let published = shapes(true, false);
    delete_freshness(&published.0, "ref-shape");
    assert_missing_freshness_fails(published, "references");
}

#[test]
fn missing_type_relationship_freshness_fails_typed_after_reopen() {
    // Control: the healthy relationship settles the rule.
    let (_state, _root, store, pin) = shapes(false, true);
    let check = read(&store, "check", json!({"expectedBasis":pin}));
    assert_eq!(
        instance(&check, "squares-are-shapes", "src/low.rs", None)["status"],
        "conforms"
    );

    let published = shapes(false, true);
    delete_freshness(&published.0, "rel-square");
    assert_missing_freshness_fails(published, "check");
}

/// The language tier reads one proof's freshness per tuple; a missing row there is corruption
/// too, never a healthy `fresh`.
#[test]
fn missing_freshness_fails_the_language_tier_typed() {
    let (_state, _root, store, _pin) = publish(&semantic());
    let rust = |store: &Store| {
        store
            .evidence_response()
            .unwrap()
            .mcp_language_tiers()
            .map(|tiers| {
                let rust = s::LANGUAGE_ORDER.iter().position(|l| *l == "rust").unwrap();
                tiers.into_iter().nth(rust).unwrap()
            })
    };
    assert_eq!(rust(&store).unwrap().freshness, "fresh");

    let (state, root, store, _pin) = publish(&semantic());
    drop(store);
    delete_freshness(&state, "call-lower-0");
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let text = format!("{:#}", rust(&store).unwrap_err());
    assert!(
        text.contains("invalid selected semantic proof freshness"),
        "{text}"
    );
}
