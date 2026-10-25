#![cfg(feature = "test-support")]
use serde_json::{Value, json};
use std::{
    fs,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    indexer::{IndexOptions, index_workspace_bundle},
    model::{CancelFlag, IndexPin},
    store::Store,
};

fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Store, IndexPin) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for (path, source) in [
        ("src/lib.rs", "pub mod high; pub mod low;\n"),
        ("src/high.rs", "pub fn upper() {}\n"),
        (
            "src/low.rs",
            "pub fn lower() { crate::high::upper(); }\n#[cfg(test)] mod tests { fn test_ref() { crate::high::upper(); } }\n#[cfg(feature = \"test-support\")] pub fn support() { crate::high::upper(); }\n",
        ),
    ] {
        let path = root.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let baseline = store.index_baseline().unwrap();
    let leader = store.publication_for_tests().unwrap();
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().into()),
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
fn config(root: &std::path::Path, text: &str) {
    fs::create_dir_all(root.join(".trellis")).unwrap();
    fs::write(root.join(".trellis/rules.toml"), text).unwrap();
}
const RULES: &str = r#"version = 1
[layers.high]
order = 1
paths = ["src/high.rs"]
[layers.low]
order = 2
paths = ["src/low.rs"]
[roles.lower]
select = { name = "lower" }
[roles.upper]
select = { name = "upper" }
[[rules]]
id = "must-call-upper"
for_each = "lower"
require = [{ calls = "upper" }]
"#;
fn read(store: &Store, kind: &str, args: Value) -> Value {
    let response = store.evidence_response().unwrap();
    let payload = response
        .architecture_response(kind, &args, &mut || Ok(()))
        .unwrap();
    response.finish(payload).unwrap()
}
fn db_metadata(state: &std::path::Path) -> Vec<(String, u64, std::time::SystemTime)> {
    let index = fs::read_dir(state.join("cache/indexes"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.is_dir())
        .unwrap();
    let mut result: Vec<_> = fs::read_dir(index)
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.file_name().to_string_lossy().starts_with("index.db"))
        .map(|e| {
            let metadata = e.metadata().unwrap();
            (
                e.file_name().to_string_lossy().into_owned(),
                metadata.len(),
                metadata.modified().unwrap(),
            )
        })
        .collect();
    result.sort();
    result
}
#[test]
fn captured_references_keep_stored_roles_and_call_rules_unknown_without_writes() {
    let (state, root, store, pin) = fixture();
    config(root.path(), RULES);
    let metadata = db_metadata(state.path());
    let args = json!({"expectedBasis":pin});
    let refs = read(&store, "references", args.clone());
    let prod = refs["production"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["source"]["path"] == "src/low.rs" && e["target"]["path"] == "src/high.rs")
        .unwrap();
    assert_eq!(prod["count"], 2, "feature support remains production");
    let tests = refs["tests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["source"]["path"] == "src/low.rs" && e["target"]["path"] == "src/high.rs")
        .unwrap();
    assert_eq!(tests["count"], 1);
    let check = read(&store, "check", args);
    assert_eq!(check["indexGeneration"], pin.index_generation.to_string());
    assert_eq!(check["indexRevision"], pin.index_revision);
    assert!(check["counts"]["deviates"].as_u64().unwrap() > 0);
    let call = check["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["rule"] == "must-call-upper")
        .unwrap();
    assert_eq!(call["status"], "unknown");
    assert!(
        !call["constraints"][0]["evidence"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db_metadata(state.path()),
        metadata,
        "evaluation must not write index or WAL"
    );
    fs::write(
        root.path().join("src/low.rs"),
        "this is not the captured Rust",
    )
    .unwrap();
    assert_eq!(
        read(&store, "references", json!({"expectedBasis":pin})),
        refs,
        "never reread source from disk"
    );
}
#[test]
fn live_config_has_separate_digest_and_exact_pair_exception_round_trips() {
    let (_state, root, store, pin) = fixture();
    config(root.path(), RULES);
    let args = json!({"expectedBasis":pin});
    let before = read(&store, "check", args.clone());
    let edge = before["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["rule"] == "$layers" && i["status"] == "deviates")
        .unwrap();
    let identity = |value: &Value| {
        format!(
            "{{ source_set = {:?}, language = {:?}, path = {:?}, sid = {:?} }}",
            value["sourceSetId"].as_str().unwrap(),
            value["language"].as_str().unwrap(),
            value["path"].as_str().unwrap(),
            value["sid"].as_str().unwrap()
        )
    };
    config(
        root.path(),
        &format!(
            "{RULES}\n[[exceptions]]\nrule = \"$layers\"\ntarget = {}\ndestination = {}\nreason = \"Reviewed existing upward pair\"\n",
            identity(&edge["target"]),
            identity(&edge["destination"])
        ),
    );
    let after = read(&store, "check", args.clone());
    assert_eq!(before["indexRevision"], after["indexRevision"]);
    assert_ne!(
        before["rulesFile"]["contentHash"],
        after["rulesFile"]["contentHash"]
    );
    let accepted = after["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| {
            i["rule"] == "$layers"
                && i["target"] == edge["target"]
                && i["destination"] == edge["destination"]
        })
        .unwrap();
    assert_eq!(accepted["status"], "deviates");
    assert_eq!(accepted["excepted"], true);
    assert_eq!(accepted["exceptionReason"], "Reviewed existing upward pair");
    let explanation = read(
        &store,
        "explain",
        json!({"expectedBasis":pin,"rule":"$layers","target":edge["target"],"destination":edge["destination"]}),
    );
    assert_eq!(
        explanation["instances"].as_array().unwrap(),
        &vec![accepted.clone()]
    );
    assert_eq!(explanation["counts"]["deviates"], 0);
    let prompt = read(&store, "prompt", args);
    assert_eq!(prompt["rulesFile"], after["rulesFile"]);
    assert!(
        prompt["rulesText"]
            .as_str()
            .unwrap()
            .contains("Reviewed existing upward pair")
    );
}
#[test]
fn config_and_pin_failures_are_atomic_and_cancelled_reads_do_not_publish_results() {
    let (_state, root, store, pin) = fixture();
    assert!(read(&store, "prompt", json!({"expectedBasis":pin}))["rulesText"].is_null());
    let response = store.evidence_response().unwrap();
    assert!(
        response
            .architecture_response("check", &json!({}), &mut || Ok(()))
            .unwrap_err()
            .to_string()
            .contains("rules_not_found")
    );
    config(
        root.path(),
        "version = 1\n[roles.bad]\nselect = { mystery = \"x\" }\n",
    );
    let error = response
        .architecture_response("check", &json!({}), &mut || Ok(()))
        .unwrap_err();
    let diagnostic = trellis::rules::diagnostic(&error).unwrap();
    assert_eq!(diagnostic["code"], "invalid_rules");
    assert!(diagnostic["line"].as_u64().unwrap() > 0);
    config(root.path(), RULES);
    let wrong = IndexPin {
        index_generation: uuid::Uuid::new_v4(),
        index_revision: pin.index_revision,
    };
    assert!(
        response
            .architecture_response("check", &json!({"expectedBasis":wrong}), &mut || Ok(()))
            .is_err()
    );
    let mut checks = 0;
    assert!(
        response
            .architecture_response("check", &json!({}), &mut || {
                checks += 1;
                anyhow::ensure!(checks < 3, "cancelled");
                Ok(())
            })
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
}
