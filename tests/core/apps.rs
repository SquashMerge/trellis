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
    let apps: Vec<(&str, &str)> = data["apps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|app| {
            (
                app["name"].as_str().unwrap(),
                app["manifest"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        apps,
        [
            ("trellis", "Cargo.toml"),
            ("trellis-web", "web/package.json")
        ]
    );
    // The stubs carry no references, so the inventory has no rule instances to count. The
    // expected storage -> query deviations from src/store/architecture_core.rs to src/apps.rs and
    // src/interfaces.rs are not asserted: inline `crate::` paths are unresolved (#828).
    assert_eq!(data["counts"]["comparable"], 0);
    assert_eq!(data["noApp"]["counts"], data["counts"]);
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
    assert_eq!(diagnostic["line"], 6);
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

#[test]
fn the_first_bad_layer_in_name_order_wins() {
    let rules_file = "version = 1\n[layers.zeta]\napp = \"nope\"\norder = 1\npaths = [\"b/**\"]\n[layers.alpha]\napp = \"nada\"\norder = 2\npaths = [\"a/**\"]\n";
    let diagnostic = invalid(&with(
        rules_file,
        &[A_PACKAGE, B_PACKAGE, A_SOURCE, B_SOURCE],
    ));
    assert_eq!(diagnostic["kind"], "reference");
    assert_eq!(diagnostic["line"], 7);
}

/// A root manifest that declares the denied package. It is hidden, so it detects no app.
const EXTERNAL_MANIFEST: (&str, &str) = (
    "package.json",
    "{\"name\":\"scratch\",\"dependencies\":{\"widget\":\"^1.0.0\"}}\n",
);

const COUNTING_RULES: &str = r#"version = 1
[external_policy]
deny = [{ language = "javascript", package = "widget", stdlib = false }]
[roles.decl]
select = { name = "Decl*" }
[roles.any]
select = { paths = ["**"] }
[[rules]]
id = "decl-calls"
for_each = "decl"
require = [{ calls = "any" }]
[layers.root-manifest]
app = "alpha"
display = "hidden"
order = 9
paths = ["package.json"]
[layers.alpha-low]
app = "alpha"
order = 2
paths = ["a/low.js", "a/cross.js"]
[layers.alpha-high]
app = "alpha"
order = 1
paths = ["a/high.js"]
[layers.alpha-ext]
app = "alpha"
order = 3
paths = ["a/ext.js"]
[layers.alpha-only]
shared = true
apps = ["alpha"]
paths = ["a/shared/**"]
[layers.beta-top]
app = "beta"
order = 1
paths = ["b/top.js"]
[layers.beta-scripts]
app = "beta"
order = 2
paths = ["scripts/b_owned.js"]
[layers.common]
shared = true
paths = ["common/**"]
"#;

const COUNTING_FILES: [(&str, &str); 13] = [
    EXTERNAL_MANIFEST,
    A_PACKAGE,
    B_PACKAGE,
    (
        "a/low.js",
        "import { hi } from './high.js';\nexport function lo() { hi(); }\n",
    ),
    ("a/high.js", "export function hi() {}\n"),
    (
        "a/cross.js",
        "import { top } from '../b/top.js';\nexport function cross() { top(); }\n",
    ),
    ("b/top.js", "export function top() {}\n"),
    (
        "a/shared/s.js",
        "import { hi } from '../high.js';\nexport function s() { hi(); }\n",
    ),
    (
        "common/s.js",
        "import { top } from '../b/top.js';\nexport function c() { top(); }\n",
    ),
    ("a/ext.js", "import 'widget';\nexport class DeclA {}\n"),
    (
        "a/loose.js",
        "import 'widget';\nexport class DeclLoose {}\n",
    ),
    (
        "scripts/b_owned.js",
        "import 'widget';\nexport class DeclB {}\n",
    ),
    ("scripts/x.js", "import 'widget';\nexport class DeclX {}\n"),
];

/// Where each production rule row must count, keyed by rule and the file the row targets.
fn expected_app(row: &Value) -> &'static str {
    match (
        row["rule"].as_str().unwrap(),
        row["target"]["path"].as_str().unwrap(),
    ) {
        ("$layers", "a/low.js") => "alpha",
        ("$layers", "a/cross.js") => "noApp",
        ("$layers", "a/shared/s.js") => "alpha",
        ("$layers", "common/s.js") => "beta",
        (_, "a/ext.js" | "a/loose.js") => "alpha",
        (_, "scripts/b_owned.js") => "beta",
        (_, "scripts/x.js") => "noApp",
        other => panic!("unexpected rule row {other:?}"),
    }
}

fn tally(rows: &[&Value]) -> Value {
    let mut tally = [0u64; 5];
    for row in rows {
        tally[0] += 1;
        if row["excepted"] == true {
            tally[4] += 1;
        } else {
            match row["status"].as_str().unwrap() {
                "conforms" => tally[1] += 1,
                "deviates" => tally[2] += 1,
                _ => tally[3] += 1,
            }
        }
    }
    serde_json::json!({"comparable":tally[0],"conforms":tally[1],"deviates":tally[2],"unknown":tally[3],"excepted":tally[4]})
}

#[test]
fn apps_plus_no_app_equal_rules_counts() {
    let data = check(&with(COUNTING_RULES, &COUNTING_FILES)).unwrap();
    let rows: Vec<&Value> = data["instances"].as_array().unwrap().iter().collect();
    let by_app = |name: &str| -> Vec<&Value> {
        rows.iter()
            .copied()
            .filter(|row| expected_app(row) == name)
            .collect()
    };
    let apps = data["apps"].as_array().unwrap();
    assert_eq!(apps.len(), 2);
    for app in apps {
        let name = app["name"].as_str().unwrap();
        assert_eq!(app["counts"], tally(&by_app(name)), "{name}");
    }
    assert_eq!(data["noApp"]["counts"], tally(&by_app("noApp")));
    // Every row of the table counts where it should: 8 under alpha, 4 under beta, 4 under none.
    assert_eq!(apps[0]["counts"]["comparable"], 8);
    assert_eq!(apps[1]["counts"]["comparable"], 4);
    assert_eq!(data["noApp"]["counts"]["comparable"], 4);
    assert_eq!(data["noApp"]["counts"]["unknown"], 3);
    assert_eq!(data["noApp"]["counts"]["deviates"], 1);
    for field in ["comparable", "conforms", "deviates", "unknown", "excepted"] {
        let sum: u64 = apps
            .iter()
            .map(|app| &app["counts"])
            .chain([&data["noApp"]["counts"]])
            .map(|counts| counts[field].as_u64().unwrap())
            .sum();
        assert_eq!(sum, data["counts"][field].as_u64().unwrap(), "{field}");
    }
}

#[test]
fn with_no_detected_app_no_app_holds_every_count() {
    let rules_file = "version = 1\n[layers.top]\norder = 1\npaths = [\"b/**\"]\n[layers.bottom]\norder = 2\npaths = [\"a/**\"]\n";
    let data = check(&with(rules_file, &[A_SOURCE, B_SOURCE])).unwrap();
    assert_eq!(data["apps"], serde_json::json!([]));
    assert_eq!(data["counts"]["deviates"], 1);
    assert_eq!(data["noApp"]["counts"], data["counts"]);
}

#[test]
fn a_single_app_counts_unassigned_files_outside_its_root_under_no_app() {
    let rules_file = "version = 1\n[layers.top]\norder = 1\npaths = [\"b/**\"]\n[layers.bottom]\norder = 2\npaths = [\"a/**\"]\n";
    let data = check(&with(
        rules_file,
        &[
            A_PACKAGE,
            A_SOURCE,
            B_SOURCE,
            ("scripts/x.js", "import { g } from '../b/lib.js';\n"),
        ],
    ))
    .unwrap();
    let apps = data["apps"].as_array().unwrap();
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0]["name"], "alpha");
    assert_eq!(apps[0]["manifest"], "a/package.json");
    let row = data["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["target"]["path"] == "scripts/x.js")
        .expect("the unassigned file's row");
    assert_eq!(row["status"], "unknown");
    assert_eq!(apps[0]["counts"]["comparable"], 1);
    assert_eq!(apps[0]["counts"]["deviates"], 1);
    assert_eq!(data["noApp"]["counts"]["comparable"], 1);
    assert_eq!(data["counts"]["comparable"], 2);
}

#[test]
fn a_denied_package_assigned_to_another_apps_layer_counts_under_the_importing_file() {
    let rules_file = r#"version = 1
[external_policy]
deny = [{ language = "javascript", package = "widget", stdlib = false }]
[layers.root-manifest]
app = "alpha"
display = "hidden"
order = 9
paths = ["package.json"]
[layers.alpha-ext]
app = "alpha"
order = 2
paths = ["a/ext.js"]
[layers.beta-packages]
app = "beta"
order = 1
external = [{ language = "javascript", package = "widget", stdlib = false }]
"#;
    let files = [
        EXTERNAL_MANIFEST,
        A_PACKAGE,
        B_PACKAGE,
        B_SOURCE,
        ("a/ext.js", "import 'widget';\n"),
    ];
    let data = check(&with(rules_file, &files)).unwrap();
    let rows = data["instances"].as_array().unwrap();
    let ban = rows
        .iter()
        .find(|row| row["rule"] == "$external")
        .expect("the ban row");
    assert_eq!(ban["status"], "deviates");
    let apps = data["apps"].as_array().unwrap();
    let alpha = &apps[0]["counts"];
    assert_eq!(apps[0]["name"], "alpha");
    assert_eq!(alpha["deviates"], 1, "{data}");
    assert_eq!(data["noApp"]["counts"]["deviates"], 0, "{data}");
    for field in ["comparable", "conforms", "deviates", "unknown", "excepted"] {
        let sum: u64 = apps
            .iter()
            .map(|app| &app["counts"])
            .chain([&data["noApp"]["counts"]])
            .map(|counts| counts[field].as_u64().unwrap())
            .sum();
        assert_eq!(sum, data["counts"][field].as_u64().unwrap(), "{field}");
    }
}
