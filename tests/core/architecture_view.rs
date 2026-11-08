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
            "mod store; mod peer; mod shared; mod entry; mod other; mod unknown; mod empty; pub fn entry() {crate::store::run();crate::peer::run();crate::shared::run();crate::entry::run();}\n",
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
// One absolute deadline covers snapshot admission, references, rules, metrics,
// final publication fence and serialization; indexing stays outside it.
#[test]
fn architecture_full_view_shares_one_deadline_across_parallel_preparation() {
    let (_state, root, store, pin) = fixture();
    rules(
        root.path(),
        &format!(
            "{RULES}\n[roles.inherited]\nselect={{extends='Base'}}\n[roles.any]\nselect={{paths=['**']}}\n[[rules]]\nid='inheritance'\nfor_each='inherited'\nrequire=[{{calls='any'}}]"
        ),
    );
    let deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
    let mut check = || {
        anyhow::ensure!(std::time::Instant::now() < deadline, "deadline_exceeded");
        Ok(())
    };
    let response = store.evidence_response().unwrap();
    let data = response
        .architecture_view(&options(pin, "max"), &mut check)
        .unwrap();
    let data = response.finish(data).unwrap();
    let bytes = serde_json::to_vec(&data).unwrap();
    check().unwrap();
    assert!(!bytes.is_empty());
    assert_eq!(data["state"], "ready");
    assert_eq!(data["references"]["counts"]["scannedDocuments"], 8);
    assert_eq!(data["modules"].as_array().unwrap().len(), 8);
    assert!(data["rules"]["rulesFile"]["contentHash"].is_string());
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
    // The summary keeps the public projections' exact counts (#448).
    let references = native(&store, pin, "references");
    let checked = native(&store, pin, "check");
    assert_eq!(data["references"]["counts"], references["counts"]);
    assert_eq!(data["references"]["limitations"], references["limitations"]);
    for field in ["counts", "testCounts", "rulesFile", "limitations"] {
        assert_eq!(data["rules"][field], checked[field], "{field}");
    }
    assert_eq!(
        data["rules"]["unassignedCount"],
        checked["unassigned"].as_array().unwrap().len()
    );
}

/// The public `references` or `check` projection, as the CLI prints it.
fn native(store: &Store, pin: IndexPin, kind: &str) -> Value {
    let response = store.evidence_response().unwrap();
    let value = response
        .architecture_response(kind, &json!({"expectedBasis":pin}), &mut || Ok(()))
        .unwrap();
    response.finish(value).unwrap()
}

/// All detail items of one kind, paged `limit` at a time.
fn details(
    store: &Store,
    data: &Value,
    kind: &str,
    filters: &[(&str, &str)],
    limit: usize,
) -> Vec<Value> {
    let response = store.evidence_response().unwrap();
    let (mut offset, mut items) = (0usize, Vec::new());
    loop {
        let mut fields: std::collections::BTreeMap<String, String> = [
            ("kind", kind.to_owned()),
            (
                "indexGeneration",
                data["indexGeneration"].as_str().unwrap().to_owned(),
            ),
            ("indexRevision", data["indexRevision"].to_string()),
            (
                "rulesHash",
                data["rulesFile"]["contentHash"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            ),
            ("offset", offset.to_string()),
            ("limit", limit.to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        fields.extend(
            filters
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
        );
        let query = trellis::architecture_details::Query::from_fields(fields).unwrap();
        let page = response
            .architecture_details(&query, &mut || Ok(()))
            .unwrap();
        items.extend(page["items"].as_array().unwrap().iter().cloned());
        match page["nextOffset"].as_u64() {
            Some(next) => offset = next as usize,
            None => {
                assert_eq!(page["total"], items.len());
                response.finish(()).unwrap();
                return items;
            }
        }
    }
}

/// #448: the default view is bounded by what it draws. Unresolved sites, pairs
/// and rule instances grow without growing the summary; details page them.
#[test]
fn architecture_summary_is_bounded_and_details_page_the_rest() {
    let (_state, root, store, pin) = fixture();
    rules(root.path(), RULES);
    let small = view(&store, pin);
    let (_state, root, store, pin) = {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("src")).unwrap();
        // Same files, each with 400 extra unresolved sites.
        let noise = "absent::call();".repeat(400);
        for (path, source) in [
            (
                "src/lib.rs",
                format!(
                    "mod store; mod peer; mod shared; mod entry; mod other; mod unknown; mod empty; pub fn entry() {{crate::store::run();crate::peer::run();crate::shared::run();crate::entry::run();{noise}}}\n"
                ),
            ),
            (
                "src/store.rs",
                format!(
                    "pub fn run(){{crate::entry::run();crate::other::run();crate::other::run();crate::other::run();crate::other::run();crate::other::run();crate::other::run();crate::other::run();{noise}}}\n#[cfg(test)] mod tests{{fn f(){{crate::entry::run();}}}}\n#[cfg(feature=\"test-support\")] fn seam(){{crate::entry::run();}}\n"
                ),
            ),
            ("src/entry.rs", format!("pub fn run(){{{noise}}}\n")),
            ("src/other.rs", format!("pub fn run(){{{noise}}}\n")),
            ("src/peer.rs", format!("pub fn run(){{{noise}}}\n")),
            (
                "src/shared.rs",
                format!("pub fn run(){{crate::entry::run();{noise}}}\n"),
            ),
            (
                "src/unknown.rs",
                format!("pub fn unknown(){{crate::entry::run();{noise}}}\n"),
            ),
            ("src/empty.rs", "// no callable\n".into()),
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
    };
    rules(root.path(), RULES);
    let large = view(&store, pin);
    let references = native(&store, pin, "references");
    let unresolved = references["unresolved"].as_array().unwrap();
    assert!(unresolved.len() >= 2400, "{}", unresolved.len());
    let full = serde_json::to_vec(&references).unwrap().len();
    let summary = |data: &Value| {
        let mut data = data.clone();
        // Per-module metric values and pins legitimately differ between fixtures.
        for field in ["modules", "indexGeneration", "references", "rules"] {
            data[field] = Value::Null;
        }
        serde_json::to_vec(&data).unwrap().len()
    };
    let bytes = serde_json::to_vec(&large).unwrap().len();
    assert!(
        bytes * 4 < full,
        "summary {bytes} bytes vs {full} bytes of references alone"
    );
    // Only sampled site offsets gain digits; 2,800 extra sites add no rows.
    assert!(
        summary(&small).abs_diff(summary(&large)) < 64,
        "edges do not grow with sites: {} vs {}",
        summary(&small),
        summary(&large)
    );
    assert_eq!(
        serde_json::to_vec(&large["references"]["externalPackages"])
            .unwrap()
            .len(),
        serde_json::to_vec(&small["references"]["externalPackages"])
            .unwrap()
            .len()
    );
    assert_eq!(
        large["references"]["counts"]["unresolvedSites"],
        unresolved.len()
    );
    // Paged details return every unresolved site, per file on request.
    let paged = details(&store, &large, "unresolved", &[], 500);
    assert_eq!(paged, *unresolved);
    let file = details(&store, &large, "unresolved", &[("path", "src/peer.rs")], 50);
    assert_eq!(file.len(), 400);
    assert!(
        file.iter()
            .all(|item| item["source"]["path"] == "src/peer.rs")
    );
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
    let references = native(&store, pin, "references");
    let pair = references["production"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["source"]["path"] == "src/store.rs" && p["target"]["path"] == "src/entry.rs")
        .unwrap();
    let target = |v: &Value| {
        format!(
            "{{language='{}',path='{}'}}",
            v["language"].as_str().unwrap(),
            v["path"].as_str().unwrap()
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
        native(&store, pin, "references")["production"]
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
        native(&store, pin, "check")["instances"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["target"]["path"] == "src/unknown.rs" && e["status"] == "unknown")
    );
    for (field, role) in [("production", "production"), ("tests", "test")] {
        let assigned: u64 = native(&store, pin, "references")[field]
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

#[test]
fn architecture_specificity_assignments_match_evaluated_layer_verdicts() {
    let (_state, root, store, pin) = fixture();
    rules(
        root.path(),
        "version=1\n[layers.broad]\norder=9\npaths=['src/**']\n[layers.narrow]\norder=3\npaths=['src/s*.rs']\n[layers.exact]\norder=1\npaths=['src/entry.rs']",
    );
    let data = view(&store, pin);
    let assignments: std::collections::BTreeMap<_, _> = data["modules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["identity"]["path"].as_str().unwrap(), &m["layer"]))
        .collect();
    assert_eq!(*assignments["src/store.rs"], "narrow");
    assert_eq!(*assignments["src/entry.rs"], "exact");
    assert_eq!(*assignments["src/lib.rs"], "broad");
    for field in ["instances", "testOnly"] {
        for row in native(&store, pin, "check")[field].as_array().unwrap() {
            assert_eq!(
                &row["sourceLayer"],
                assignments[row["target"]["path"].as_str().unwrap()]
            );
            assert_eq!(
                &row["destinationLayer"],
                assignments[row["destination"]["path"].as_str().unwrap()]
            );
        }
    }
}

#[test]
fn workbench_architecture_forwards_population_without_changing_references_or_policy() {
    let (_state, root, store, pin) = fixture();
    rules(
        root.path(),
        &format!("{RULES}\n[layers.quiet]\norder=5\npaths=['src/unknown.rs']\ndisplay='hidden'"),
    );
    let response = store.evidence_response().unwrap();
    let ordinary = response
        .architecture_view(&options(pin, "max"), &mut || Ok(()))
        .unwrap();
    assert_eq!(ordinary["workbench"], false);
    for role in ["production", "all", "test"] {
        let selected = MetricsOptions {
            workbench: true,
            role: role.into(),
            ..options(pin, "max")
        };
        let data = response
            .architecture_view(&selected, &mut || Ok(()))
            .unwrap();
        let measured = response.metrics_at(&selected, &mut || Ok(())).unwrap();
        assert_eq!(data["workbench"], true);
        for field in ["references", "edges", "rules", "layers"] {
            assert_eq!(data[field], ordinary[field], "{field}");
        }
        assert_eq!(
            data["modules"].as_array().unwrap().len(),
            ordinary["modules"].as_array().unwrap().len()
        );
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
            assert_eq!(module["roleCounts"], json!(file.role_counts));
            assert_eq!(
                module.get("excludedReason"),
                serde_json::to_value(file).unwrap().get("excludedReason")
            );
        }
        let hidden = data["modules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|module| module["identity"]["path"] == "src/unknown.rs")
            .unwrap();
        assert_eq!(hidden["declarationCount"], usize::from(role == "all"));
        if role == "production" {
            assert_eq!(hidden["excludedReason"], "hidden_layer");
        }
    }
    response.finish(()).unwrap();
}

#[test]
fn boundary_sites_publish_exact_callers_without_guessing_noncall_references() {
    let (_state, root, store, pin) = fixture();
    rules(root.path(), RULES);
    let checked = native(&store, pin, "check");
    let edge = checked["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| {
            row["target"]["path"] == "src/store.rs" && row["destination"]["path"] == "src/entry.rs"
        })
        .unwrap();
    assert_eq!(edge["status"], "deviates");
    let site = &edge["evidence"][0];
    assert_eq!(
        site["caller"]["stableLookupKey"], "src/store.rs::run",
        "{site}"
    );
    assert_eq!(site["caller"]["syntaxId"], site["syntaxId"]);
    assert_eq!(site["caller"]["document"], site["document"]);
    assert!(site["callerReason"].is_null());
    let references = native(&store, pin, "references");
    let imports: Vec<_> = references["production"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|edge| edge["source"]["path"] == "src/lib.rs")
        .flat_map(|edge| edge["sites"].as_array().unwrap())
        .filter(|site| site["callId"].is_null())
        .collect();
    for site in imports {
        assert!(site["caller"].is_null());
        assert_eq!(site["callerReason"], "syntax_only");
    }
}

#[test]
fn noncall_argument_references_do_not_borrow_the_surrounding_call_owner() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("src")).unwrap();
    fs::write(
        root.path().join("src/lib.rs"),
        "mod dest; pub fn run() { consume(crate::dest::VALUE); } fn consume(_: i32) {}\n",
    )
    .unwrap();
    fs::write(
        root.path().join("src/dest.rs"),
        "pub const VALUE: i32 = 1;\n",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let baseline = store.index_baseline().unwrap();
    let leader = store.publication_for_tests().unwrap();
    let (graph, native_rows, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let pin = store
        .publish_native(&graph, &capture, &native_rows, &leader, baseline, &cancel)
        .unwrap();
    drop(leader);
    let references = native(&store, pin, "references");
    let site = references["production"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|edge| edge["sites"].as_array().unwrap())
        .find(|site| site["spelling"] == "crate::dest::VALUE")
        .unwrap();
    assert!(site["caller"].is_null());
    assert_eq!(site["callerReason"], "syntax_only");
}

#[test]
fn nested_boundary_sites_use_the_exact_inner_measured_call() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("src")).unwrap();
    fs::write(
        root.path().join("src/lib.rs"),
        "mod top; pub fn run() { consume(crate::top::target()); } fn consume(_: ()) {}\n",
    )
    .unwrap();
    fs::write(root.path().join("src/top.rs"), "pub fn target() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let baseline = store.index_baseline().unwrap();
    let leader = store.publication_for_tests().unwrap();
    let (graph, native_rows, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let inner = native_rows
        .calls
        .iter()
        .find(|call| call.spelling.as_deref() == Some("target"))
        .unwrap();
    let owner = native_rows
        .declarations
        .iter()
        .find(|d| d.syntax_id == inner.owner_syntax_id)
        .unwrap();
    let pin = store
        .publish_native(&graph, &capture, &native_rows, &leader, baseline, &cancel)
        .unwrap();
    drop(leader);
    rules(
        root.path(),
        "version=1\n[layers.low]\norder=2\npaths=['src/lib.rs']\n[layers.top]\norder=1\npaths=['src/top.rs']\n",
    );
    let references = native(&store, pin, "references");
    let site = references["production"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|edge| edge["sites"].as_array().unwrap())
        .find(|site| site["spelling"] == "crate::top::target")
        .unwrap();
    assert_eq!(site["callId"], inner.id, "{site}");
    assert_eq!(site["caller"]["syntaxId"], owner.syntax_id);
    assert_eq!(site["caller"]["stableLookupKey"], "src/lib.rs::run");
    assert!(site["callerReason"].is_null());
    let checked = native(&store, pin, "check");
    let evidence = &checked["instances"][0]["evidence"][0];
    assert_eq!(evidence["callId"], inner.id);
    assert_eq!(evidence["caller"], site["caller"]);
    assert!(evidence["callerReason"].is_null());
}

/// Publishes `files` (relative path, text) as native evidence under `rules_text`.
fn publish_files(
    files: &[(&str, &str)],
    rules_text: &str,
) -> (tempfile::TempDir, tempfile::TempDir, Store, IndexPin) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for (path, text) in files {
        let path = root.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let baseline = store.index_baseline().unwrap();
    let leader = store.publication_for_tests().unwrap();
    let (graph, native_rows, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let pin = store
        .publish_native(&graph, &capture, &native_rows, &leader, baseline, &cancel)
        .unwrap();
    drop(leader);
    rules(root.path(), rules_text);
    (state, root, store, pin)
}

fn kind_total(counts: &Value) -> u64 {
    ["call", "type", "import", "reference"]
        .iter()
        .map(|kind| counts[*kind].as_u64().unwrap())
        .sum()
}

#[test]
fn rule_evidence_carries_relation_kind_and_spelling() {
    let (_state, _root, store, pin) = publish_files(
        &[
            (
                "src/lib.rs",
                "mod top; use crate::top::Thing; pub fn run(_: &crate::top::Thing) { crate::top::target(); }\n",
            ),
            ("src/top.rs", "pub struct Thing;\npub fn target() {}\n"),
        ],
        "version=1\n[layers.low]\norder=2\npaths=['src/lib.rs']\n[layers.top]\norder=1\npaths=['src/top.rs']\n",
    );
    let checked = native(&store, pin, "check");
    let row = &checked["instances"][0];
    assert_eq!(row["rule"], "$layers");
    let by = |spelling: &str| {
        row["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["spelling"] == spelling)
            .unwrap_or_else(|| panic!("no {spelling}: {row}"))
    };
    let call = by("crate::top::target");
    assert_eq!(call["relationKind"], "call", "{call}");
    assert!(call["lines"]["start"].as_u64().unwrap() >= 1);
    assert!(
        row["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["spelling"] == "crate::top::Thing" && e["relationKind"] == "reference")
    );
    assert!(
        row["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["relationKind"] == "import")
    );
    assert_eq!(row["evidence"][0]["relationKind"], "call");
}

#[test]
fn rule_evidence_names_its_owner() {
    let (_state, _root, store, pin) = publish_files(
        &[
            (
                "src/lib.rs",
                "mod top; use crate::top::target; pub fn run() { crate::top::target(); }\n",
            ),
            ("src/top.rs", "pub fn target() {}\n"),
        ],
        "version=1\n[layers.low]\norder=2\npaths=['src/lib.rs']\n[layers.top]\norder=1\npaths=['src/top.rs']\n",
    );
    let checked = native(&store, pin, "check");
    let evidence = checked["instances"][0]["evidence"].as_array().unwrap();
    let call = evidence
        .iter()
        .find(|e| e["relationKind"] == "call")
        .unwrap();
    assert_eq!(call["owner"]["name"], "run");
    assert_eq!(call["owner"]["syntaxId"], call["syntaxId"]);
    assert_eq!(call["owner"]["stableLookupKey"], "src/lib.rs::run");
    let import = evidence
        .iter()
        .find(|e| e["relationKind"] == "import")
        .unwrap();
    assert!(import["owner"]["name"].is_null(), "{import}");
    assert!(import["owner"]["stableLookupKey"].is_null(), "{import}");
}

#[test]
fn structural_rule_evidence_has_no_target_and_says_why() {
    let (_state, _root, store, pin) = publish_files(
        &[
            (
                "src/lib.rs",
                "mod top; pub fn run() { crate::top::target(); }\n",
            ),
            ("src/top.rs", "pub fn target() {}\n"),
        ],
        "version=1\n[layers.low]\norder=2\npaths=['src/lib.rs']\n[layers.top]\norder=1\npaths=['src/top.rs']\n",
    );
    let checked = native(&store, pin, "check");
    for item in checked["instances"][0]["evidence"].as_array().unwrap() {
        assert_eq!(item["kind"], "structuralReference");
        assert!(item["target"].is_null(), "{item}");
        assert_eq!(item["targetReason"], "structural_only");
    }
}

#[test]
fn typescript_imports_are_classified_evidence() {
    let (_state, _root, store, pin) = publish_files(
        &[
            (
                "web/a.ts",
                "import { b } from './b';\nexport const a = b;\n",
            ),
            ("web/b.ts", "export const b = 1;\n"),
            ("lib/c.ts", "export const c = 1;\n"),
        ],
        "version=1\n[layers.web]\norder=1\npaths=['web/a.ts']\n[layers.lib]\norder=2\npaths=['web/b.ts','lib/**']\n",
    );
    let checked = native(&store, pin, "check");
    let row = checked["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["rule"] == "$layers" && row["target"]["path"] == "web/a.ts")
        .unwrap_or_else(|| panic!("no instance: {checked}"));
    let evidence = row["evidence"].as_array().unwrap();
    assert!(!evidence.is_empty());
    for item in evidence {
        assert_eq!(item["relationKind"], "import", "{item}");
        assert_eq!(item["targetReason"], "structural_only");
    }
    assert_eq!(row["kindCounts"]["import"], row["referenceCount"]);
}

#[test]
fn javascript_require_inside_a_function_keeps_its_caller() {
    let (_state, _root, store, pin) = publish_files(
        &[
            (
                "web/a.js",
                "export function load() {\n  return require(\"./b.js\");\n}\n",
            ),
            ("web/b.js", "export const b = 1;\n"),
        ],
        "version=1\n[layers.web]\norder=1\npaths=['web/**']\n",
    );
    let references = native(&store, pin, "references");
    let entry = references["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["source"]["path"] == "web/a.js")
        .unwrap_or_else(|| panic!("require resolved: {references}"));
    assert_eq!(entry["site"]["relationKind"], "import");
    let checked = native(&store, pin, "check");
    let row = checked["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["rule"] == "$layers" && row["status"] == "unknown")
        .unwrap_or_else(|| panic!("no unknown row: {checked}"));
    let item = &row["evidence"][0];
    assert_eq!(item["relationKind"], "import");
    assert!(item["callId"].is_string());
    assert_eq!(item["caller"]["name"], "load");
    assert!(item["callerReason"].is_null());
    assert_eq!(item["owner"]["name"], "load");
    assert_eq!(item["targetReason"], "structural_only");
    assert_eq!(row["kindCounts"]["import"], row["referenceCount"]);
}

#[test]
fn kind_counts_are_exact_and_sum_to_reference_count() {
    let (_state, _root, store, pin) = publish_files(
        &[
            (
                "src/lib.rs",
                "mod top; pub fn run(_: &crate::top::Thing) { crate::top::target(); }\nmod orphan_user;\n",
            ),
            ("src/top.rs", "pub struct Thing;\npub fn target() {}\n"),
            (
                "src/loose.rs",
                "pub fn loose() { crate::top::target(); crate::nowhere::gone(); }\n",
            ),
        ],
        "version=1\n[layers.low]\norder=2\npaths=['src/lib.rs','src/loose.rs']\n[layers.top]\norder=1\npaths=['src/top.rs']\n",
    );
    let references = native(&store, pin, "references");
    for edge in references["production"].as_array().unwrap() {
        assert_eq!(
            kind_total(&edge["kindCounts"]),
            edge["count"].as_u64().unwrap()
        );
    }
    let checked = native(&store, pin, "check");
    let rows = checked["instances"].as_array().unwrap();
    assert!(rows.iter().any(|row| row["status"] == "unknown"));
    for row in rows.iter().filter(|row| row["rule"] == "$layers") {
        assert_eq!(
            kind_total(&row["kindCounts"]),
            row["referenceCount"].as_u64().unwrap(),
            "{row}"
        );
    }
}

#[test]
fn calls_lead_the_rules_sample_past_the_sample_size() {
    let mut types = String::new();
    for index in 0..8 {
        types.push_str(&format!("pub fn t{index}(_: &crate::high::T) {{}}\n"));
    }
    types.push_str("pub fn last() { crate::high::f(); }\n");
    let (_state, _root, store, pin) = publish_files(
        &[
            ("src/lib.rs", "mod high; mod low;\n"),
            ("src/high.rs", "pub struct T;\npub fn f() {}\n"),
            ("src/low.rs", &types),
        ],
        "version=1\n[layers.high]\norder=1\npaths=['src/high.rs','src/lib.rs']\n[layers.low]\norder=2\npaths=['src/low.rs']\n",
    );
    let checked = native(&store, pin, "check");
    let row = checked["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["rule"] == "$layers" && row["target"]["path"] == "src/low.rs")
        .unwrap();
    assert_eq!(row["kindCounts"]["call"], 1);
    assert_eq!(row["kindCounts"]["reference"], 8);
    assert_eq!(row["evidence"][0]["relationKind"], "call", "{row}");
}
