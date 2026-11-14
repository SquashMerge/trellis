#![cfg(feature = "test-support")]
//! App detection and per-app layer rules (#784), through the published store.
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    indexer::{IndexOptions, index_workspace_bundle},
    model::{CancelFlag, IndexPin},
    rules,
    store::{MetricsOptions, Store},
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

/// Every build manifest the capture walk would admit in this checkout, with its real text. The
/// walk skips hidden entries and the directories capture skips, so a manifest that main adds
/// later is found here without editing the test.
fn real_manifests(root: &Path) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let walk = ignore::WalkBuilder::new(root)
        .require_git(false)
        .follow_links(false)
        .hidden(true)
        .parents(false)
        .git_global(false)
        .git_exclude(false)
        .filter_entry(|entry| {
            !matches!(
                entry.file_name().to_str(),
                Some(".git" | "node_modules" | ".venv" | "target" | "dist" | "build")
            )
        })
        .build();
    for entry in walk {
        let entry = entry.unwrap();
        let name = entry.file_name().to_str().unwrap_or_default();
        if entry.file_type().is_some_and(|kind| kind.is_file())
            && matches!(name, "Cargo.toml" | "package.json" | "pyproject.toml")
        {
            let relative = entry.path().strip_prefix(root).unwrap();
            found.push((
                relative.to_str().unwrap().to_owned(),
                fs::read_to_string(entry.path()).unwrap(),
            ));
        }
    }
    found.sort();
    found
}

#[test]
fn trellis_layout_detects_daemon_herdr_mod_and_web_apps() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let rules_file = fs::read_to_string(manifest.join(".trellis/rules.toml")).unwrap();
    let mut owned = real_manifests(manifest);
    assert!(
        owned.iter().any(|(path, _)| path == "web/package.json"),
        "the walk finds the checkout's manifests"
    );
    let stubs: Vec<(String, String)> = owned
        .iter()
        .map(|(path, text)| match path.strip_suffix("package.json") {
            Some(dir) => (format!("{dir}index.js"), "export const a = 1;\n".to_owned()),
            // A Python source below a `pyproject.toml` makes capture retain it (#807).
            None if path.ends_with("pyproject.toml") => (
                format!("{}stub.py", path.trim_end_matches("pyproject.toml")),
                "A = 1\n".to_owned(),
            ),
            None if text.contains("[package]") => (
                format!("{}src/main.rs", path.trim_end_matches("Cargo.toml")),
                "fn main() {}\n".to_owned(),
            ),
            None => (String::new(), String::new()),
        })
        .filter(|(path, _)| !path.is_empty())
        .collect();
    owned.extend(stubs);
    owned.push((
        "src/bin/trellis-producer-stub.rs".into(),
        "fn main() {}\n".into(),
    ));
    owned.dedup_by(|a, b| a.0 == b.0);
    let mut files: Vec<(&str, &str)> = vec![(".trellis/rules.toml", rules_file.as_str())];
    files.extend(
        owned
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str())),
    );
    // The Python producer fixture declares `[project]`; the hidden `fixtures` layer drops it.
    assert!(
        owned
            .iter()
            .any(|(path, _)| path == "tests/fixtures/producers/python/pyproject.toml"),
        "the walk finds the checkout's pyproject.toml"
    );
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
            ("trellis-herdr-mod", "herdr-mod/package.json"),
            ("trellis-web", "web/package.json")
        ]
    );
    // The stubs carry no references, so the inventory has no rule instances to count.
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

const PY_PROJECT: (&str, &str) = (
    "svc/pyproject.toml",
    "[project]\nname = \"svc\"\n[project.scripts]\nsvc = \"svc.main:run\"\n",
);
const PY_SOURCE: (&str, &str) = (
    "svc/svc/main.py",
    "from ui import helper\n\ndef run():\n    return helper()\n",
);
const PY_UI_PROJECT: (&str, &str) = ("ui/pyproject.toml", "[project]\nname = \"ui\"\n");
const PY_UI_SOURCE: (&str, &str) = ("ui/ui/__init__.py", "def helper():\n    return 1\n");

#[test]
fn python_projects_are_apps_that_layers_can_name() {
    let rules_file = "version = 1\n[layers.service]\napp = \"svc\"\norder = 1\npaths = [\"svc/**\"]\n[layers.screen]\napp = \"ui\"\norder = 1\npaths = [\"ui/**\"]\n";
    let data = check(&with(
        rules_file,
        &[PY_PROJECT, PY_SOURCE, PY_UI_PROJECT, PY_UI_SOURCE],
    ))
    .unwrap();
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
        [("svc", "svc/pyproject.toml"), ("ui", "ui/pyproject.toml")]
    );
    assert_eq!(limitation_count(&data), 0);
    // A layer naming an undetected Python app is still a reference error.
    let diagnostic = invalid(&with(
        "version = 1\n[layers.service]\napp = \"nope\"\norder = 1\npaths = [\"svc/**\"]\n",
        &[PY_PROJECT, PY_SOURCE],
    ));
    assert!(diagnostic.to_string().contains("nope"), "{diagnostic}");
}

#[test]
fn a_malformed_pyproject_adds_one_limitation_unless_hidden() {
    let bad = ("bad/pyproject.toml", "[project\nname = \"bad\"\n");
    let bad_source = ("bad/bad.py", "X = 1\n");
    let data = check(&with(
        "version = 1\n",
        &[PY_PROJECT, PY_SOURCE, bad, bad_source],
    ))
    .unwrap();
    assert_eq!(limitation_count(&data), 1);
    assert_eq!(data["apps"].as_array().unwrap().len(), 1);
    let hidden =
        "version = 1\n[layers.fixtures]\ndisplay = \"hidden\"\norder = 1\npaths = [\"bad/**\"]\n";
    let data = check(&with(hidden, &[PY_PROJECT, PY_SOURCE, bad, bad_source])).unwrap();
    assert_eq!(limitation_count(&data), 0);
    assert_eq!(data["apps"].as_array().unwrap().len(), 1);
}

#[test]
fn a_tool_only_pyproject_is_not_an_app() {
    let data = check(&with(
        "version = 1\n",
        &[
            ("tools/pyproject.toml", "[tool.ruff]\nline-length = 100\n"),
            ("tools/lint.py", "X = 1\n"),
        ],
    ))
    .unwrap();
    assert_eq!(data["apps"], json!([]));
    assert_eq!(limitation_count(&data), 0);
}

#[test]
fn the_architecture_summary_lists_a_python_app_with_its_kind_and_entries() {
    // The summary is computed only when the rules file has layers.
    let rules_file = "version = 1\n[layers.service]\napp = \"svc\"\norder = 1\npaths = [\"svc/**\"]\n[layers.screen]\napp = \"ui\"\norder = 1\npaths = [\"ui/**\"]\n";
    let (_state, _root, store, pin) = publish(&with(
        rules_file,
        &[PY_PROJECT, PY_SOURCE, PY_UI_PROJECT, PY_UI_SOURCE],
    ));
    let response = store.evidence_response().unwrap();
    let summary = response
        .architecture_view(
            &MetricsOptions {
                pin: Some(pin),
                aggregate: "max".into(),
                ..Default::default()
            },
            &mut || Ok(()),
        )
        .unwrap();
    let summary = response.finish(summary).unwrap();
    let svc = summary["apps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|app| app["name"] == "svc")
        .expect("the svc app");
    assert_eq!(svc["kind"], "python");
    assert_eq!(svc["manifest"], "svc/pyproject.toml");
    assert_eq!(svc["roots"], json!(["svc"]));
    assert_eq!(svc["entries"], json!(["svc.main"]));
    assert_eq!(svc["languages"], json!(["python"]));
}

const MAVEN_ROOT: (&str, &str) = (
    "pom.xml",
    "<project><artifactId>shop</artifactId><packaging>pom</packaging><modules><module>api</module></modules></project>\n",
);
const MAVEN_API: (&str, &str) = (
    "api/pom.xml",
    "<project><artifactId>shop-api</artifactId><build><plugins><plugin><artifactId>maven-jar-plugin</artifactId><configuration><archive><manifest><mainClass>shop.Api</mainClass></manifest></archive></configuration></plugin></plugins></build></project>\n",
);
const MAVEN_SOURCE: (&str, &str) = (
    "api/src/main/java/shop/Api.java",
    "package shop;\npublic class Api { public static void main(String[] a) {} }\n",
);
const GRADLE_SETTINGS: (&str, &str) = ("settings.gradle", "include 'cli'\n");
const GRADLE_CLI: (&str, &str) = (
    "cli/build.gradle",
    "plugins { id 'application' }\napplication { mainClass = 'tools.Cli' }\n",
);
const GRADLE_SOURCE: (&str, &str) = (
    "cli/src/main/java/tools/Cli.java",
    "package tools;\npublic class Cli { public static void main(String[] a) {} }\n",
);

#[test]
fn maven_and_gradle_modules_are_apps_that_layers_can_name() {
    let rules_file = "version = 1\n[layers.server]\napp = \"shop-api\"\norder = 1\npaths = [\"api/**\"]\n[layers.tool]\napp = \"cli\"\norder = 1\npaths = [\"cli/**\"]\n";
    let files = [
        MAVEN_ROOT,
        MAVEN_API,
        MAVEN_SOURCE,
        GRADLE_SETTINGS,
        GRADLE_CLI,
        GRADLE_SOURCE,
    ];
    let data = check(&with(rules_file, &files)).unwrap();
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
        [("shop-api", "api/pom.xml"), ("cli", "cli/build.gradle")]
    );
    assert_eq!(limitation_count(&data), 0);
    let summary = summary(&with(rules_file, &files));
    let api = app_named(&summary["apps"], "shop-api");
    assert_eq!(api["kind"], "maven");
    assert_eq!(api["roots"], json!(["api"]));
    assert_eq!(api["entries"], json!(["shop.Api"]));
    assert_eq!(api["languages"], json!(["java"]));
    let cli = app_named(&summary["apps"], "cli");
    assert_eq!(cli["kind"], "gradle");
    assert_eq!(cli["entries"], json!(["tools.Cli"]));
    // A library module is no app, so naming it is a reference error.
    let diagnostic = invalid(&with(
        "version = 1\n[layers.server]\napp = \"shop\"\norder = 1\npaths = [\"api/**\"]\n",
        &files,
    ));
    assert!(diagnostic.to_string().contains("shop"), "{diagnostic}");
}

#[test]
fn a_malformed_pom_adds_one_limitation_unless_hidden() {
    let bad = ("bad/pom.xml", "<project><artifactId>bad</artifactId>\n");
    let bad_source = ("bad/src/Bad.java", "class Bad {}\n");
    // No root POM, so each module is judged alone.
    let files = [MAVEN_API, MAVEN_SOURCE, bad, bad_source];
    let data = check(&with("version = 1\n", &files)).unwrap();
    assert_eq!(limitation_count(&data), 1, "{data}");
    assert_eq!(data["apps"].as_array().unwrap().len(), 1);
    let hidden =
        "version = 1\n[layers.fixtures]\ndisplay = \"hidden\"\norder = 1\npaths = [\"bad/**\"]\n";
    let data = check(&with(hidden, &files)).unwrap();
    assert_eq!(limitation_count(&data), 0);
    assert_eq!(data["apps"].as_array().unwrap().len(), 1);
}

fn summary(files: &[(&str, &str)]) -> Value {
    let (_state, _root, store, pin) = publish(files);
    let response = store.evidence_response().unwrap();
    let summary = response
        .architecture_view(
            &MetricsOptions {
                pin: Some(pin),
                aggregate: "max".into(),
                ..Default::default()
            },
            &mut || Ok(()),
        )
        .unwrap();
    response.finish(summary).unwrap()
}

fn app_named<'a>(apps: &'a Value, name: &str) -> &'a Value {
    apps.as_array()
        .unwrap()
        .iter()
        .find(|app| app["name"] == name)
        .unwrap_or_else(|| panic!("no app {name} in {apps}"))
}

fn module_app<'a>(summary: &'a Value, path: &str) -> &'a Value {
    &summary["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|module| module["identity"]["path"] == path)
        .unwrap_or_else(|| panic!("no module {path}"))["app"]
}

/// U1 (#794): a Node backend and a JavaScript front end in one root `package.json`.
const MONOLITH_FILES: [(&str, &str); 6] = [
    ("package.json", "{\"name\":\"monolith\"}\n"),
    ("server/api.js", "export function handle() {}\n"),
    (
        "client/ui.js",
        "import { core } from './core.js';\nexport function ui() { core(); }\n",
    ),
    (
        "client/core.js",
        "import { ui } from './ui.js';\nexport function core() { ui(); }\n",
    ),
    (
        "client/net.js",
        "import { handle } from '../server/api.js';\nexport function net() { handle(); }\n",
    ),
    ("client/extra.js", "export const extra = 1;\n"),
];

const MONOLITH_LAYERS: &str = "[layers.server]\napp = \"api\"\norder = 1\npaths = [\"server/**\"]\n[layers.ui]\napp = \"web\"\norder = 1\npaths = [\"client/ui.js\", \"client/net.js\"]\n[layers.core]\napp = \"web\"\norder = 2\npaths = [\"client/core.js\"]\n";

fn monolith_rules() -> String {
    format!(
        "version = 1\n[apps.api]\nroots = [\".\"]\n[apps.web]\nroots = [\"client\"]\n{MONOLITH_LAYERS}"
    )
}

/// #794: authored apps split one manifest's checkout. Check, per-app counts, the summary's
/// inventory and file attribution all follow the authored apps, which win over detection.
#[test]
fn an_authored_override_splits_one_package_end_to_end() {
    let rules_file = monolith_rules();
    let data = check(&with(&rules_file, &MONOLITH_FILES)).unwrap();
    let apps = &data["apps"];
    assert_eq!(apps.as_array().unwrap().len(), 2, "{apps}");
    assert_eq!(
        apps[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["counts", "manifest", "name", "source"]
    );
    for (index, name) in ["api", "web"].into_iter().enumerate() {
        assert_eq!(apps[index]["name"], name);
        assert_eq!(apps[index]["source"], "override");
        assert_eq!(apps[index]["manifest"], Value::Null);
    }
    // core -> ui is upward inside web; net -> api crosses apps and is unknown, under noApp.
    assert_eq!(apps[1]["counts"]["comparable"], 2);
    assert_eq!(apps[1]["counts"]["deviates"], 1);
    assert_eq!(apps[0]["counts"]["comparable"], 0);
    assert_eq!(data["noApp"]["counts"]["comparable"], 1);
    assert_eq!(data["noApp"]["counts"]["unknown"], 1);
    let cross = data["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["target"]["path"] == "client/net.js")
        .unwrap();
    assert_eq!(cross["status"], "unknown");
    assert_eq!(cross["unresolved"], json!(["cross_app"]));

    let summary = summary(&with(&rules_file, &MONOLITH_FILES));
    let web = app_named(&summary["apps"], "web");
    assert_eq!(web["source"], "override");
    assert_eq!(web["kind"], Value::Null);
    assert_eq!(web["manifest"], Value::Null);
    assert_eq!(web["roots"], json!(["client"]));
    assert_eq!(web["entries"], json!([]));
    assert_eq!(web["fileCount"], 4);
    assert_eq!(web["unassignedFileCount"], 1);
    assert_eq!(web["counts"], apps[1]["counts"]);
    let api = app_named(&summary["apps"], "api");
    assert_eq!(api["roots"], json!(["."]));
    assert_eq!(api["fileCount"], 1);
    assert!(
        summary["apps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|app| app["name"] != "monolith"),
        "the shadowed manifest app is gone"
    );
    // The All apps view reads these: the unassigned file goes to the authored app whose root
    // holds it.
    assert_eq!(module_app(&summary, "client/extra.js"), "web");
    assert_eq!(module_app(&summary, "server/api.js"), "api");
    assert_eq!(summary["noApp"]["counts"], data["noApp"]["counts"]);
}

/// #794 negative control: the same checkout without `[apps]` is the one detected app of #783.
#[test]
fn without_authored_apps_the_package_stays_one_detected_app() {
    let rules_file = format!(
        "version = 1\n{}",
        MONOLITH_LAYERS
            .replace("\"api\"", "\"monolith\"")
            .replace("\"web\"", "\"monolith\"")
    );
    let data = check(&with(&rules_file, &MONOLITH_FILES)).unwrap();
    assert_eq!(
        data["apps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|app| (
                app["name"].clone(),
                app["source"].clone(),
                app["manifest"].clone()
            ))
            .collect::<Vec<_>>(),
        [(json!("monolith"), json!("manifest"), json!("package.json"))]
    );
    // One app: net -> api is a peer reference inside it, so it deviates instead of crossing.
    assert_eq!(data["noApp"]["counts"]["comparable"], 0);
    assert_eq!(data["apps"][0]["counts"]["deviates"], 2);
    // The layers of the override no longer validate: `api` and `web` are not apps.
    let diagnostic = invalid(&with(
        &format!("version = 1\n{MONOLITH_LAYERS}"),
        &MONOLITH_FILES,
    ));
    assert_eq!(diagnostic["kind"], "reference");
    assert!(
        diagnostic["message"]
            .as_str()
            .unwrap()
            .contains("apps: monolith")
    );
}

/// #794: the snapshot-dependent errors of authored apps, located, through the store.
#[test]
fn authored_app_snapshot_errors_are_located() {
    // D6: a root with no indexed document under it.
    let diagnostic = invalid(&with(
        "version = 1\n[apps.web]\nroots = [\"client\", \"missing\"]\n",
        &MONOLITH_FILES,
    ));
    assert_eq!(diagnostic["kind"], "reference");
    assert_eq!(diagnostic["line"], 3);
    assert!(diagnostic["message"].as_str().unwrap().contains("missing"));
    // D4: an authored name equal to a detected app that survives.
    let diagnostic = invalid(&with(
        "version = 1\n[apps.alpha]\nroots = [\"b\"]\n",
        &[
            A_PACKAGE,
            B_PACKAGE,
            A_SOURCE,
            B_SOURCE,
            ("b/x.js", "export const x = 1;\n"),
        ],
    ));
    assert_eq!(diagnostic["kind"], "ambiguity");
    assert_eq!(diagnostic["line"], 2);
    assert!(
        diagnostic["message"]
            .as_str()
            .unwrap()
            .contains("a/package.json")
    );
    // A root outside the checkout is a schema error before any snapshot read.
    let diagnostic = invalid(&with(
        "version = 1\n[apps.web]\nroots = [\"../elsewhere\"]\n",
        &MONOLITH_FILES,
    ));
    assert_eq!(diagnostic["kind"], "schema");
    assert_eq!(diagnostic["line"], 3);
}

/// mimir (#794 acceptance): a Python service and a Vite front end whose manifests both sit
/// at the checkout root, plus a Python plugin. Detection alone gives two apps at `.`; the
/// authored override expresses the real layout.
const MIMIR_FILES: [(&str, &str); 9] = [
    (
        "pyproject.toml",
        "[project]\nname = \"mimir-agent\"\n[project.scripts]\nmimir = \"mimir.main:main\"\n",
    ),
    (
        "package.json",
        "{\"name\":\"mimir-frontend\",\"private\":true,\"type\":\"module\"}\n",
    ),
    (
        "plugins/cultivate/pyproject.toml",
        "[project]\nname = \"mimir-cultivate\"\n",
    ),
    ("mimir/__init__.py", ""),
    ("mimir/main.py", "def main():\n    return 1\n"),
    ("scripts/run.py", "X = 1\n"),
    ("frontend/src/main.ts", "export const main = 1;\n"),
    ("frontend/src/App.ts", "export const app = 2;\n"),
    ("plugins/cultivate/cultivate/__init__.py", "Y = 1\n"),
];

const MIMIR_LAYERS: &str = "[layers.agent]\napp = \"mimir-agent\"\norder = 1\npaths = [\"mimir/**\"]\n[layers.ui]\napp = \"mimir-frontend\"\norder = 1\npaths = [\"frontend/**\"]\n[layers.plugin]\napp = \"mimir-cultivate\"\norder = 1\npaths = [\"plugins/cultivate/**\"]\n";

#[test]
fn mimir_layout_is_expressed_by_an_authored_override() {
    let rules_file = format!(
        "version = 1\n[apps.mimir-agent]\nroots = [\".\"]\n[apps.mimir-frontend]\nroots = [\"frontend\"]\n[apps.mimir-cultivate]\nroots = [\"plugins/cultivate\"]\n{MIMIR_LAYERS}"
    );
    let data = check(&with(&rules_file, &MIMIR_FILES)).unwrap();
    assert_eq!(
        data["apps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|app| (
                app["name"].as_str().unwrap(),
                app["source"].as_str().unwrap()
            ))
            .collect::<Vec<_>>(),
        [
            ("mimir-agent", "override"),
            ("mimir-cultivate", "override"),
            ("mimir-frontend", "override"),
        ]
    );
    let summary = summary(&with(&rules_file, &MIMIR_FILES));
    assert_eq!(
        app_named(&summary["apps"], "mimir-frontend")["roots"],
        json!(["frontend"])
    );
    assert_eq!(
        module_app(&summary, "frontend/src/App.ts"),
        "mimir-frontend"
    );
    assert_eq!(module_app(&summary, "mimir/main.py"), "mimir-agent");
    // An unassigned root file belongs to the agent, not to whichever root manifest sorts first.
    assert_eq!(module_app(&summary, "scripts/run.py"), "mimir-agent");
    assert_eq!(
        module_app(&summary, "plugins/cultivate/cultivate/__init__.py"),
        "mimir-cultivate"
    );

    // Negative control: detection alone puts both root apps at `.`, and the unassigned root
    // file goes to the first of them in manifest-path order, the front end.
    let detected = summary_detected();
    assert_eq!(module_app(&detected, "scripts/run.py"), "mimir-frontend");
    for name in ["mimir-agent", "mimir-frontend"] {
        let app = app_named(&detected["apps"], name);
        assert_eq!(app["source"], "manifest");
        assert_eq!(app["roots"], json!(["."]));
    }
}

fn summary_detected() -> Value {
    let rules_file = format!("version = 1\n{MIMIR_LAYERS}");
    summary(&with(&rules_file, &MIMIR_FILES))
}
