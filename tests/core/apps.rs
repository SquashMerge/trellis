#![cfg(feature = "test-support")]
//! App detection and per-app layer rules (#784), through the published store.
use serde_json::Value;
use std::{
    fs,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    indexer::{IndexOptions, index_workspace_bundle},
    model::{CancelFlag, IndexPin},
    rules,
    store::Store,
};

fn publish(files: &[(&str, &str)]) -> (tempfile::TempDir, tempfile::TempDir, Store, IndexPin) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for (path, text) in files {
        let path = root.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let store =
        trellis::testing::retry_admission(|| Store::open_for_tests(state.path(), root.path()));
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

fn check(files: &[(&str, &str)]) -> anyhow::Result<Value> {
    let (_state, _root, store, pin) = publish(files);
    let response = store.evidence_response().unwrap();
    let value = response.architecture_response(
        "check",
        &serde_json::json!({"expectedBasis": pin}),
        &mut || Ok(()),
    )?;
    response.finish(value)
}

fn invalid(files: &[(&str, &str)]) -> Value {
    let error = check(files).expect_err("expected invalid rules");
    let diagnostic = rules::diagnostic(&error).expect("a located rules error");
    assert_eq!(diagnostic["code"], "invalid_rules");
    diagnostic
}

fn limitation_count(data: &Value) -> usize {
    data["limitations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|text| {
            text.as_str()
                .unwrap()
                .contains("could not be read for app detection")
        })
        .count()
}

const A_PACKAGE: (&str, &str) = ("a/package.json", "{\"name\":\"alpha\"}\n");
const B_PACKAGE: (&str, &str) = ("b/package.json", "{\"name\":\"beta\"}\n");
const A_SOURCE: (&str, &str) = (
    "a/main.js",
    "import { g } from '../b/lib.js';\nexport function f() { g(); }\n",
);
const B_SOURCE: (&str, &str) = ("b/lib.js", "export function g() {}\n");

fn with<'a>(rules_file: &'a str, files: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut all = vec![(".trellis/rules.toml", rules_file)];
    all.extend_from_slice(files);
    all
}

#[test]
fn trellis_layout_detects_daemon_and_web_apps() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let read = |path: &str| fs::read_to_string(format!("{manifest}/{path}")).unwrap();
    let rules_file = read(".trellis/rules.toml");
    let cargo = read("Cargo.toml");
    let web = read("web/package.json");
    let files = [
        (".trellis/rules.toml", rules_file.as_str()),
        ("Cargo.toml", cargo.as_str()),
        ("web/package.json", web.as_str()),
        ("src/main.rs", "fn main() {}\n"),
        ("src/bin/trellis-producer-stub.rs", "fn main() {}\n"),
        ("web/app.js", "export function app() {}\n"),
        ("tests/browser/package.json", "{\"name\":\"browser\"}\n"),
        ("tests/browser/a.test.cjs", "module.exports = 1;\n"),
        ("tests/mcp/package.json", "{\"name\":\"mcp\"}\n"),
        ("tests/mcp/a.mjs", "export const a = 1;\n"),
        (
            "tools/extraction/package.json",
            "{\"name\":\"extraction\"}\n",
        ),
        ("tools/extraction/a.mjs", "export const a = 1;\n"),
        (
            "tests/fixtures/typescript-mixed/package.json",
            "{\"name\":\"mixed\"}\n",
        ),
        (
            "tests/fixtures/typescript-mixed/a.ts",
            "export const a = 1;\n",
        ),
        (
            "tools/metrics-oracle/Cargo.toml",
            "[package]\nname = \"oracle\"\n",
        ),
        ("tools/metrics-oracle/src/main.rs", "fn main() {}\n"),
        (
            "tests/fixtures/scip-rust-analyzer-packages/workspace/Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\n",
        ),
        (
            "tests/fixtures/scip-rust-analyzer-packages/workspace/crates/alpha/Cargo.toml",
            "[package]\nname = \"alpha\"\n",
        ),
        (
            "tests/fixtures/scip-rust-analyzer-packages/workspace/crates/alpha/src/lib.rs",
            "pub fn a() {}\n",
        ),
    ];
    let data = check(&files).expect("the Trellis layout validates");
    assert_eq!(limitation_count(&data), 0);
}

#[test]
fn a_layer_naming_an_undetected_app_is_a_located_reference_error() {
    let rules_file = "version = 1\n[layers.one]\napp = \"ghost\"\norder = 1\npaths = [\"a/**\"]\n[layers.two]\napp = \"beta\"\norder = 2\npaths = [\"b/**\"]\n";
    let diagnostic = invalid(&with(
        rules_file,
        &[A_PACKAGE, B_PACKAGE, A_SOURCE, B_SOURCE],
    ));
    assert_eq!(diagnostic["kind"], "reference");
    assert_eq!(diagnostic["line"], 3);
    assert!(diagnostic["message"].as_str().unwrap().contains("ghost"));
}

#[test]
fn an_ordered_layer_without_an_app_is_a_schema_error_with_two_apps() {
    let rules_file = "version = 1\n[layers.one]\napp = \"alpha\"\norder = 1\npaths = [\"a/**\"]\n[layers.two]\norder = 2\npaths = [\"b/**\"]\n";
    let diagnostic = invalid(&with(
        rules_file,
        &[A_PACKAGE, B_PACKAGE, A_SOURCE, B_SOURCE],
    ));
    assert_eq!(diagnostic["kind"], "schema");
    assert_eq!(diagnostic["line"], 6);
}

#[test]
fn an_app_name_detected_twice_is_a_located_ambiguity() {
    let rules_file =
        "version = 1\n[layers.one]\napp = \"alpha\"\norder = 1\npaths = [\"a/**\", \"b/**\"]\n";
    let twin = ("b/package.json", "{\"name\":\"alpha\"}\n");
    let diagnostic = invalid(&with(rules_file, &[A_PACKAGE, twin, A_SOURCE, B_SOURCE]));
    assert_eq!(diagnostic["kind"], "ambiguity");
    assert_eq!(diagnostic["line"], 3);
    let message = diagnostic["message"].as_str().unwrap();
    assert!(message.contains("a/package.json") && message.contains("b/package.json"));
}

#[test]
fn a_layer_app_with_no_detected_app_is_a_reference_error() {
    let rules_file = "version = 1\n[layers.one]\napp = \"alpha\"\norder = 1\npaths = [\"a/**\"]\n";
    let diagnostic = invalid(&with(rules_file, &[A_SOURCE, B_SOURCE]));
    assert_eq!(diagnostic["kind"], "reference");
    assert_eq!(diagnostic["line"], 3);
}

#[test]
fn one_app_accepts_layers_without_an_app_and_rejects_a_wrong_name() {
    let plain = "version = 1\n[layers.one]\norder = 1\npaths = [\"a/**\"]\n[layers.two]\norder = 2\npaths = [\"b/**\"]\n";
    check(&with(plain, &[A_PACKAGE, A_SOURCE, B_SOURCE])).expect("one app needs no names");
    let named = plain.replace("order = 2", "app = \"beta\"\norder = 2");
    let diagnostic = invalid(&with(&named, &[A_PACKAGE, A_SOURCE, B_SOURCE]));
    assert_eq!(diagnostic["kind"], "reference");
}

#[test]
fn single_app_rules_unchanged() {
    let layers = "[layers.top]\norder = 1\npaths = [\"b/**\"]\n[layers.bottom]\norder = 2\npaths = [\"a/**\"]\n";
    let rules_file = format!("version = 1\n{layers}");
    let without = check(&with(&rules_file, &[A_SOURCE, B_SOURCE])).unwrap();
    let one_app = check(&with(&rules_file, &[A_PACKAGE, A_SOURCE, B_SOURCE])).unwrap();
    assert_eq!(without["counts"], one_app["counts"]);
    let statuses = |data: &Value| -> Vec<Value> {
        data["instances"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["status"].clone())
            .collect()
    };
    assert_eq!(statuses(&without), statuses(&one_app));
    assert_eq!(one_app["counts"]["deviates"], 1);
}

#[test]
fn cross_app_pair_is_unknown_not_deviation() {
    let rules_file = "version = 1\n[layers.low]\napp = \"alpha\"\norder = 2\npaths = [\"a/**\"]\n[layers.high]\napp = \"beta\"\norder = 1\npaths = [\"b/**\"]\n";
    let data = check(&with(
        rules_file,
        &[A_PACKAGE, B_PACKAGE, A_SOURCE, B_SOURCE],
    ))
    .unwrap();
    let rows = data["instances"].as_array().unwrap();
    let row = rows
        .iter()
        .find(|row| row["rule"] == "$layers" && row["target"]["path"] == "a/main.js")
        .expect("the cross-app edge");
    assert_eq!(row["status"], "unknown");
    assert!(
        row["unresolved"]
            .as_array()
            .unwrap()
            .contains(&"cross_app".into())
    );
    assert_eq!(data["counts"]["deviates"], 0);
}

#[test]
fn equal_orders_in_two_apps_are_not_peers() {
    let rules_file = "version = 1\n[layer_policy]\npeers = \"forbid\"\n[layers.low]\napp = \"alpha\"\norder = 1\npaths = [\"a/**\"]\n[layers.high]\napp = \"beta\"\norder = 1\npaths = [\"b/**\"]\n";
    let data = check(&with(
        rules_file,
        &[A_PACKAGE, B_PACKAGE, A_SOURCE, B_SOURCE],
    ))
    .unwrap();
    assert_eq!(data["counts"]["deviates"], 0);
    assert_eq!(data["counts"]["unknown"], 1);
}

#[test]
fn a_shared_layer_serves_only_its_listed_apps() {
    let rules_file = "version = 1\n[layers.low]\napp = \"alpha\"\norder = 1\npaths = [\"a/**\"]\n[layers.base]\nshared = true\napps = [\"beta\"]\npaths = [\"b/**\"]\n";
    let data = check(&with(
        rules_file,
        &[A_PACKAGE, B_PACKAGE, A_SOURCE, B_SOURCE],
    ))
    .unwrap();
    assert_eq!(data["counts"]["conforms"], 0);
    assert_eq!(data["counts"]["unknown"], 1);
}

#[test]
fn hidden_manifest_detects_no_app_and_no_limitation() {
    let rules_file = "version = 1\n[layers.fixtures]\ndisplay = \"hidden\"\norder = 1\npaths = [\"fixture/**\"]\n";
    let data = check(&with(
        rules_file,
        &[
            A_PACKAGE,
            ("fixture/package.json", "not json {"),
            ("fixture/x.js", "export const x = 1;\n"),
            A_SOURCE,
            B_SOURCE,
        ],
    ))
    .unwrap();
    assert_eq!(limitation_count(&data), 0);
}

#[test]
fn a_malformed_manifest_no_hidden_layer_selects_adds_one_limitation() {
    let data = check(&with(
        "version = 1\n",
        &[
            A_PACKAGE,
            ("bad/package.json", "not json {"),
            ("bad/x.js", "export const x = 1;\n"),
            A_SOURCE,
            B_SOURCE,
        ],
    ))
    .unwrap();
    assert_eq!(limitation_count(&data), 1);
}

#[test]
fn a_manifest_with_no_sources_is_not_captured() {
    let data = check(&with(
        "version = 1\n",
        &[("lonely/package.json", "not json {"), A_SOURCE, B_SOURCE],
    ))
    .unwrap();
    assert_eq!(limitation_count(&data), 0);
}

#[test]
fn a_nested_manifest_over_the_source_cap_still_reads() {
    let mut big = String::from("{\"name\":\"big\",\"padding\":\"");
    big.push_str(&"x".repeat(3 * 1024 * 1024));
    big.push_str("\"}\n");
    let data = check(&with(
        "version = 1\n",
        &[
            ("big/package.json", &big),
            ("big/x.js", "export const x = 1;\n"),
        ],
    ));
    assert!(data.is_ok(), "{:?}", data.err());
}
