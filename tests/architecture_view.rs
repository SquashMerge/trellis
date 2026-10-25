#![cfg(feature = "test-support")]
use serde_json::{Value, json};
use std::{
    fs,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    indexer::{IndexOptions, index_workspace_bundle},
    model::{CancelFlag, IndexPin},
    store::{MetricsOptions, Store},
};

fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Store, IndexPin) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("src")).unwrap();
    for (path, source) in [
        (
            "src/lib.rs",
            "pub fn entry() {crate::store::run();crate::peer::run();crate::shared::run();crate::entry::run();}\n",
        ),
        (
            "src/store.rs",
            "pub fn run(){crate::entry::run();crate::other::run();crate::other::run();crate::other::run();crate::other::run();crate::other::run();crate::other::run();crate::other::run();}\n#[cfg(test)] mod tests{fn f(){crate::entry::run();}}\n#[cfg(feature=\"test-support\")] fn seam(){crate::entry::run();}\n",
        ),
        ("src/entry.rs", "pub fn run(){}\n"),
        ("src/other.rs", "pub fn run(){}\n"),
        ("src/peer.rs", "pub fn run(){}\n"),
        ("src/shared.rs", "pub fn run(){crate::entry::run();}\n"),
        ("src/unknown.rs", "pub fn unknown(){crate::entry::run();}\n"),
        ("src/empty.rs", "// no callable\n"),
    ] {
        fs::write(root.path().join(path), source).unwrap();
    }
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let baseline = store.index_baseline().unwrap();
    let leader = store.publication_for_tests().unwrap();
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let pin = store
        .publish_native(&graph, &capture, &native, &leader, baseline, &cancel)
        .unwrap();
    drop(leader);
    (state, root, store, pin)
}
const RULES: &str = "version=1\n[layers.entry]\norder=1\npaths=['src/lib.rs','src/entry.rs','src/other.rs']\n[layers.store]\norder=3\npaths=['src/store.rs']\n[layers.peer]\norder=1\npaths=['src/peer.rs']\n[layers.shared]\nshared=true\npaths=['src/shared.rs']\n[layers.empty]\norder=4\npaths=['absent/**']";
fn rules(root: &std::path::Path, text: &str) {
    fs::create_dir_all(root.join(".trellis")).unwrap();
    fs::write(root.join(".trellis/rules.toml"), text).unwrap();
}
fn options(pin: IndexPin, aggregate: &str) -> MetricsOptions {
    MetricsOptions {
        pin: Some(pin),
        aggregate: aggregate.into(),
        ..Default::default()
    }
}
fn view(store: &Store, pin: IndexPin) -> Value {
    let response = store.evidence_response().unwrap();
    let value = response
        .architecture_view(&options(pin, "max"), &mut || Ok(()))
        .unwrap();
    response.finish(value).unwrap()
}
#[test]
fn architecture_exact_counts_roles_policy_samples_and_full_identity_metrics() {
    let (_state, root, store, pin) = fixture();
    rules(root.path(), RULES);
    let data = view(&store, pin);
    assert_eq!(data["state"], "ready");
    assert_eq!(data["indexGeneration"], pin.index_generation.to_string());
    assert_eq!(data["indexRevision"], pin.index_revision);
    assert_eq!(data["role"], "all");
    assert!(data.get("metricsScope").is_none());
    assert_eq!(data["semanticComplete"], false);
    let edge = |source: &str, destination: &str, role: &str| {
        data["edges"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| {
                e["sourceLayer"] == source
                    && e["destinationLayer"] == destination
                    && e["role"] == role
            })
            .unwrap()
    };
    assert_eq!(edge("entry", "store", "production")["count"], 1);
    assert_eq!(edge("entry", "store", "production")["status"], "conforms");
    assert_eq!(edge("store", "entry", "production")["count"], 9);
    assert_eq!(edge("store", "entry", "test")["count"], 1);
    assert_eq!(edge("store", "entry", "production")["status"], "deviates");
    assert_eq!(edge("entry", "peer", "production")["status"], "deviates");
    assert_eq!(edge("shared", "entry", "production")["status"], "deviates");
    assert_eq!(edge("entry", "shared", "production")["status"], "conforms");
    let sampled = edge("store", "entry", "production")["pairs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["target"]["path"] == "src/other.rs")
        .unwrap();
    assert_eq!(sampled["count"], 7);
    assert_eq!(sampled["sites"].as_array().unwrap().len(), 5);
    assert_eq!(sampled["omittedSites"], 2);
    assert_eq!(data["layers"].as_array().unwrap().len(), 5);
    let response = store.evidence_response().unwrap();
    for aggregate in ["max", "sum", "mean"] {
        let projected = response
            .architecture_view(&options(pin, aggregate), &mut || Ok(()))
            .unwrap();
        let measured = response
            .metrics_at(&options(pin, aggregate), &mut || Ok(()))
            .unwrap();
        for module in projected["modules"].as_array().unwrap() {
            let identity = &module["identity"];
            let file = measured
                .files
                .iter()
                .find(|file| {
                    identity["sourceSetId"] == file.source_set_id
                        && identity["language"] == file.language
                        && identity["path"] == file.path
                })
                .unwrap();
            assert_eq!(module["metrics"], file.metrics);
            assert_eq!(module["declarationCount"], file.declaration_count);
            assert_eq!(
                module["roleCounts"],
                serde_json::to_value(&file.role_counts).unwrap()
            );
            assert!(identity["sid"].as_str().unwrap().starts_with("sid:v1:"));
        }
        let store_module = projected["modules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["identity"]["path"] == "src/store.rs")
            .unwrap();
        assert_eq!(store_module["physicalLines"], 3);
        assert_eq!(store_module["declarationCount"], 3);
        let unknown = projected["modules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["identity"]["path"] == "src/unknown.rs")
            .unwrap();
        assert!(unknown["layer"].is_null());
        let empty = projected["modules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["identity"]["path"] == "src/empty.rs")
            .unwrap();
        assert_eq!(empty["declarationCount"], 0);
        assert!(empty["metrics"]["cyclomatic"].is_null());
    }
    response.finish(()).unwrap();
    // Public references and rules shapes are not extended by the browser projection.
    let response = store.evidence_response().unwrap();
    for (kind, field) in [("references", "references"), ("check", "rules")] {
        let native = response
            .architecture_response(kind, &json!({"expectedBasis":pin}), &mut || Ok(()))
            .unwrap();
        assert_eq!(native, data[field]);
    }
}
#[test]
fn architecture_setup_diagnostics_rules_digest_and_no_live_source_fallback() {
    let (_state, root, store, pin) = fixture();
    let missing = view(&store, pin);
    assert_eq!(missing["state"], "missing_rules");
    assert!(missing["references"].is_null());
    assert_eq!(missing["modules"], json!([]));
    rules(root.path(), "version=1");
    let empty = view(&store, pin);
    assert_eq!(empty["state"], "no_layers");
    assert!(empty["rulesFile"]["contentHash"].is_string());
    rules(root.path(), RULES);
    let first = view(&store, pin);
    fs::write(root.path().join("src/store.rs"), "invalid live source (").unwrap();
    rules(
        root.path(),
        &format!("{RULES}\n[layer_policy]\npeers='allow'\nshared_outbound='allow'"),
    );
    let changed = view(&store, pin);
    assert_ne!(
        first["rulesFile"]["contentHash"],
        changed["rulesFile"]["contentHash"]
    );
    assert_eq!(first["modules"], changed["modules"]);
    assert!(
        changed["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["sourceLayer"] == "shared" && e["status"] == "conforms")
    );
    for text in [
        "version=[",
        "version=1\n[layers.a]\norder=1\npaths=['src/**']\n[layers.b]\norder=2\npaths=['src/**']",
    ] {
        rules(root.path(), text);
        let response = store.evidence_response().unwrap();
        let error = response
            .architecture_view(&options(pin, "max"), &mut || Ok(()))
            .unwrap_err();
        let diagnostic = trellis::rules::diagnostic(&error).unwrap();
        assert_eq!(diagnostic["code"], "invalid_rules");
        assert!(diagnostic["line"].as_u64().unwrap() > 0);
        assert!(diagnostic["range"]["start"].is_number());
        response.finish(()).unwrap();
    }
}
#[test]
fn architecture_cancellation_and_expired_pin_fail_atomically() {
    let (_state, root, store, pin) = fixture();
    rules(root.path(), RULES);
    let response = store.evidence_response().unwrap();
    let error = response
        .architecture_view(&options(pin, "max"), &mut || anyhow::bail!("cancelled"))
        .unwrap_err();
    assert_eq!(error.to_string(), "cancelled");
    let stale = IndexPin {
        index_revision: pin.index_revision + 1,
        ..pin
    };
    assert!(
        response
            .architecture_view(&options(stale, "max"), &mut || Ok(()))
            .is_err()
    );
}

#[test]
fn architecture_exceptions_preserve_unexcepted_deviations_and_unknown_evidence() {
    let (_state, root, store, pin) = fixture();
    rules(root.path(), RULES);
    let first = view(&store, pin);
    let pair = first["references"]["production"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["source"]["path"] == "src/store.rs" && p["target"]["path"] == "src/entry.rs")
        .unwrap();
    let target = |v: &Value| {
        format!(
            "{{source_set='{}',language='{}',path='{}',sid='{}'}}",
            v["sourceSetId"].as_str().unwrap(),
            v["language"].as_str().unwrap(),
            v["path"].as_str().unwrap(),
            v["sid"].as_str().unwrap()
        )
    };
    rules(
        root.path(),
        &format!(
            "{RULES}\n[[exceptions]]\nrule='$layers'\ntarget={}\ndestination={}\nreason='Reviewed debt'",
            target(&pair["source"]),
            target(&pair["target"])
        ),
    );
    let data = view(&store, pin);
    let contributions: Vec<_> = data["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| {
            e["sourceLayer"] == "store"
                && e["destinationLayer"] == "entry"
                && e["role"] == "production"
        })
        .collect();
    assert_eq!(contributions.len(), 2);
    let excepted = contributions
        .iter()
        .find(|e| e["excepted"] == true)
        .unwrap();
    assert_eq!(excepted["status"], "deviates");
    assert_eq!(excepted["exceptionReason"], "Reviewed debt");
    assert_eq!(excepted["count"], 2);
    assert_eq!(
        contributions
            .iter()
            .find(|e| e["excepted"] == false)
            .unwrap()["count"],
        7
    );
    assert!(
        data["references"]["production"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["source"]["path"] == "src/lib.rs" && p["target"]["path"] == "src/entry.rs")
    );
    assert!(
        data["edges"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["sourceLayer"] != e["destinationLayer"])
    );
    assert!(
        data["rules"]["instances"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["target"]["path"] == "src/unknown.rs" && e["status"] == "unknown")
    );
    for (field, role) in [("production", "production"), ("tests", "test")] {
        let assigned: u64 = data["references"][field]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| {
                let modules = data["modules"].as_array().unwrap();
                let layer = |target: &Value| {
                    modules.iter().find(|m| m["identity"] == *target).unwrap()["layer"].clone()
                };
                let from = layer(&p["source"]);
                let to = layer(&p["target"]);
                !from.is_null() && !to.is_null() && from != to
            })
            .map(|p| p["count"].as_u64().unwrap())
            .sum();
        let summarized: u64 = data["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["role"] == role)
            .map(|e| e["count"].as_u64().unwrap())
            .sum();
        assert_eq!(assigned, summarized);
    }
}

#[test]
fn architecture_metric_roles_leave_reference_roles_and_physical_lines_unchanged() {
    let (_state, root, store, pin) = fixture();
    rules(root.path(), RULES);
    let response = store.evidence_response().unwrap();
    let all = response
        .architecture_view(&options(pin, "sum"), &mut || Ok(()))
        .unwrap();
    for (role, count) in [("production", 2), ("all", 3), ("test", 1)] {
        let selected = MetricsOptions {
            role: role.into(),
            ..options(pin, "sum")
        };
        let data = response
            .architecture_view(&selected, &mut || Ok(()))
            .unwrap();
        let measured = response.metrics_at(&selected, &mut || Ok(())).unwrap();
        assert_eq!(data["role"], role);
        assert!(data.get("metricsScope").is_none());
        assert_eq!(data["references"], all["references"]);
        assert_eq!(data["edges"], all["edges"]);
        assert_eq!(data["rules"], all["rules"]);
        for module in data["modules"].as_array().unwrap() {
            let file = measured
                .files
                .iter()
                .find(|file| {
                    module["identity"]["sourceSetId"] == file.source_set_id
                        && module["identity"]["language"] == file.language
                        && module["identity"]["path"] == file.path
                })
                .unwrap();
            assert_eq!(module["metrics"], file.metrics);
            assert_eq!(module["declarationCount"], file.declaration_count);
            assert_eq!(
                module["roleCounts"],
                serde_json::to_value(&file.role_counts).unwrap()
            );
        }
        let mixed = data["modules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|module| module["identity"]["path"] == "src/store.rs")
            .unwrap();
        assert_eq!(mixed["declarationCount"], count);
        assert_eq!(mixed["roleCounts"], json!({"production": 2, "test": 1}));
        assert_eq!(mixed["metrics"]["cyclomatic"], count);
        assert_eq!(mixed["physicalLines"], 3);
        let production_only = data["modules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|module| module["identity"]["path"] == "src/entry.rs")
            .unwrap();
        if role == "test" {
            assert_eq!(production_only["declarationCount"], 0);
            assert!(production_only["metrics"]["cyclomatic"].is_null());
        }
    }
    response.finish(()).unwrap();
}
