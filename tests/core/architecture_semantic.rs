#![cfg(feature = "test-support")]
//! Architecture references and rules use fresh selected semantic bindings (#523).
use crate::graph_fixtures::{Bind, PRODUCER, Publisher, Spec, bind};
use serde_json::{Value, json};
use std::fs;
use trellis::{
    model::IndexPin,
    semantic_evidence::{Dispatch, Resolution},
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

fn publish(spec: &Spec) -> (tempfile::TempDir, tempfile::TempDir, Store, IndexPin) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut publisher = Publisher::open(state.path(), root.path());
    let pin = publisher.publish(spec);
    fs::create_dir_all(root.path().join(".trellis")).unwrap();
    fs::write(root.path().join(".trellis/rules.toml"), RULES).unwrap();
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
        // The unbound method call is an explicit unresolved semantic site.
        let reason = if resolution == Resolution::Unresolved {
            "call_binding_unresolved"
        } else {
            "call_binding_ambiguous"
        };
        assert!(references["unresolved"].as_array().unwrap().iter().any(
            |row| row["source"]["path"] == "src/low.rs"
                && row["reason"] == reason
                && row["site"]["relationKind"] == "call"
        ));
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
    for arrow in summary["edges"].as_array().unwrap() {
        for pair in arrow["pairs"].as_array().unwrap() {
            assert!(pair["sites"].as_array().unwrap().len() <= 5);
        }
    }
    // The summary carries no per-site arrays beyond the pair samples.
    assert!(summary["references"].get("unresolved").is_none());
    assert!(serde_json::to_vec(&summary).unwrap().len() < 256 * 1024);
}
