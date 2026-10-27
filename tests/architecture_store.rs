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
            "{{ language = {:?}, path = {:?} }}",
            value["language"].as_str().unwrap(),
            value["path"].as_str().unwrap()
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

#[test]
fn portable_exception_matches_two_real_clones_without_replacing_native_identities() {
    let (_state_a, root_a, store_a, pin_a) = fixture();
    let (_state_b, root_b, store_b, pin_b) = fixture();
    let stores = [(&store_a, &root_a, pin_a), (&store_b, &root_b, pin_b)];
    let mut native_pairs = Vec::new();
    let portable = format!(
        "{RULES}\n[[exceptions]]\nrule='$layers'\ntarget={{language='rust',path='src/low.rs'}}\ndestination={{language='rust',path='src/high.rs'}}\nreason='Reviewed portable pair'\n"
    );
    for (store, root, pin) in stores {
        config(root.path(), RULES);
        let before = read(store, "check", json!({"expectedBasis":pin}));
        let edge = before["instances"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["rule"] == "$layers" && row["target"]["path"] == "src/low.rs")
            .unwrap();
        native_pairs.push((edge["target"].clone(), edge["destination"].clone()));
        config(root.path(), &portable);
    }
    for endpoint in 0..2 {
        let a = if endpoint == 0 {
            &native_pairs[0].0
        } else {
            &native_pairs[0].1
        };
        let b = if endpoint == 0 {
            &native_pairs[1].0
        } else {
            &native_pairs[1].1
        };
        assert_ne!(
            a["sourceSetId"], b["sourceSetId"],
            "fixture must be distinct clones"
        );
        assert_ne!(a["sid"], b["sid"], "native module SIDs must really differ");
        assert_eq!(a["language"], b["language"]);
        assert_eq!(a["path"], b["path"]);
    }
    assert_eq!(
        fs::read(root_a.path().join(trellis::rules::RULES_PATH)).unwrap(),
        fs::read(root_b.path().join(trellis::rules::RULES_PATH)).unwrap()
    );
    let mut hashes = Vec::new();
    for ((store, _root, pin), (target, destination)) in stores.into_iter().zip(native_pairs) {
        let checked = read(store, "check", json!({"expectedBasis":pin}));
        hashes.push(checked["rulesFile"]["contentHash"].clone());
        let row = checked["instances"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["rule"] == "$layers" && row["target"] == target)
            .unwrap();
        assert_eq!(row["destination"], destination);
        assert_eq!(row["status"], "deviates");
        assert_eq!(row["referenceCount"], 2);
        assert_eq!(row["excepted"], true);
        assert_eq!(row["exceptionReason"], "Reviewed portable pair");
        assert!(!row["evidence"].as_array().unwrap().is_empty());
        assert_eq!(checked["counts"]["deviates"], 0);
        let test = &checked["testOnly"][0];
        assert_eq!(test["target"], target);
        assert_eq!(test["destination"], destination);
        assert_eq!(test["excepted"], true);
        assert_eq!(test["referenceCount"], 1);
        assert_eq!(test["unresolved"], json!(["test_only"]));
        let explained = read(
            store,
            "explain",
            json!({"expectedBasis":pin,"rule":"$layers","target":target,"destination":destination}),
        );
        assert_eq!(explained["instances"], json!([row]));
        assert_eq!(explained["testOnly"], json!([test]));
        let mut wrong = target.clone();
        wrong["sid"] = json!("sid:v1:00000000000000000000000000000000");
        let response = store.evidence_response().unwrap();
        let error = response
            .architecture_response(
                "explain",
                &json!({"expectedBasis":pin,"rule":"$layers","target":wrong,"destination":destination}),
                &mut || Ok(()),
            )
            .unwrap_err();
        assert!(error.to_string().starts_with("not_found:"), "{error}");
        assert!(response.architecture_response(
            "explain",
            &json!({"expectedBasis":pin,"rule":"$layers","target":{"language":"rust","path":"src/low.rs"},"destination":{"language":"rust","path":"src/high.rs"}}),
            &mut || Ok(()),
        ).is_err(), "portable config does not make portable explain identities");
        response.finish(()).unwrap();
    }
    assert_eq!(hashes[0], hashes[1]);
}
