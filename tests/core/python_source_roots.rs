//! Python packages under declared source roots of the captured root `pyproject.toml` (#771).
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

const WORKSPACE: &str = "[project]\nname = 'core'\n[tool.uv.workspace]\nmembers = ['plugins/x']\n";
// The plugin is ordered above core: core must never import it.
const RULES: &str = r#"version = 1
[layers.plugin]
order = 1
paths = ["plugins/x/**"]
[layers.core]
order = 2
paths = ["core/**"]
"#;

fn fixture(files: &[(&str, &str)]) -> (tempfile::TempDir, tempfile::TempDir, Store, IndexPin) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for (path, text) in [
        ("core/__init__.py", ""),
        ("core/app.py", "from plugin_x.feature import run\n"),
        ("plugins/x/pyproject.toml", "[project]\nname = 'plugin-x'\n"),
        ("plugins/x/plugin_x/__init__.py", ""),
        (
            "plugins/x/plugin_x/feature.py",
            "from core.app import run as core_run\nfrom .helper import help\ndef run():\n    pass\n",
        ),
        ("plugins/x/plugin_x/helper.py", "def help():\n    pass\n"),
        (".trellis/rules.toml", RULES),
    ]
    .iter()
    .chain(files)
    {
        let path = root.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
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
fn read(store: &Store, kind: &str, pin: IndexPin) -> Value {
    let response = store.evidence_response().unwrap();
    let value = response
        .architecture_response(kind, &json!({"expectedBasis":pin}), &mut || Ok(()))
        .unwrap();
    response.finish(value).unwrap()
}
fn has_edge(data: &Value, source: &str, target: &str) -> bool {
    data["production"]
        .as_array()
        .unwrap()
        .iter()
        .any(|edge| edge["source"]["path"] == source && edge["target"]["path"] == target)
}
fn unresolved<'a>(data: &'a Value, source: &str, spelling: &str) -> Option<&'a Value> {
    data["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["source"]["path"] == source && row["site"]["spelling"] == spelling)
}
fn layer_status(checked: &Value, source: &str, target: &str) -> Option<Value> {
    checked["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| {
            row["rule"] == "$layers"
                && row["target"]["path"] == source
                && row["destination"]["path"] == target
        })
        .map(|row| row["status"].clone())
}

#[test]
fn declared_workspace_member_root_resolves_nested_package_and_upward_edge_deviates() {
    let (_state, _root, store, pin) = fixture(&[("pyproject.toml", WORKSPACE)]);
    let data = read(&store, "references", pin);
    assert!(
        has_edge(&data, "core/app.py", "plugins/x/plugin_x/feature.py"),
        "{data}"
    );
    assert!(unresolved(&data, "core/app.py", "plugin_x.feature").is_none());
    // The member's own absolute and relative imports keep resolving.
    assert!(has_edge(
        &data,
        "plugins/x/plugin_x/feature.py",
        "core/app.py"
    ));
    assert!(has_edge(
        &data,
        "plugins/x/plugin_x/feature.py",
        "plugins/x/plugin_x/helper.py"
    ));
    let checked = read(&store, "check", pin);
    assert_eq!(
        layer_status(&checked, "core/app.py", "plugins/x/plugin_x/feature.py"),
        Some(json!("deviates")),
        "{checked}"
    );
    assert_eq!(
        layer_status(&checked, "plugins/x/plugin_x/feature.py", "core/app.py"),
        Some(json!("conforms")),
        "{checked}"
    );
}

#[test]
fn same_module_under_two_roots_stays_unknown() {
    let (_state, _root, store, pin) = fixture(&[
        ("pyproject.toml", WORKSPACE),
        ("plugin_x/__init__.py", ""),
        ("plugin_x/feature.py", "def run():\n    pass\n"),
    ]);
    let data = read(&store, "references", pin);
    assert!(!has_edge(
        &data,
        "core/app.py",
        "plugins/x/plugin_x/feature.py"
    ));
    assert!(!has_edge(&data, "core/app.py", "plugin_x/feature.py"));
    let row = unresolved(&data, "core/app.py", "plugin_x.feature").expect("unknown site");
    assert_eq!(row["reason"], "module_path_ambiguous", "{row}");
}

#[test]
fn nested_file_without_a_proved_package_under_its_root_does_not_compete() {
    // `plugins/x/shared/` has no `__init__.py`, so `shared.util` there is not a module.
    let (_state, _root, store, pin) = fixture(&[
        ("pyproject.toml", WORKSPACE),
        ("core/app.py", "from shared.util import run\n"),
        ("shared/__init__.py", ""),
        ("shared/util.py", "def run():\n    pass\n"),
        ("plugins/x/shared/util.py", "def run():\n    pass\n"),
    ]);
    let data = read(&store, "references", pin);
    assert!(has_edge(&data, "core/app.py", "shared/util.py"), "{data}");
    assert!(unresolved(&data, "core/app.py", "shared.util").is_none());
}

#[test]
fn nested_pyproject_without_a_declaring_root_stays_unknown() {
    let (_state, _root, store, pin) = fixture(&[("pyproject.toml", "[project]\nname = 'core'\n")]);
    let data = read(&store, "references", pin);
    assert!(!has_edge(
        &data,
        "core/app.py",
        "plugins/x/plugin_x/feature.py"
    ));
    let row = unresolved(&data, "core/app.py", "plugin_x.feature").expect("unknown site");
    assert_eq!(row["reason"], "module_path_not_indexed", "{row}");
    let checked = read(&store, "check", pin);
    assert_eq!(
        layer_status(&checked, "core/app.py", "plugins/x/plugin_x/feature.py"),
        None
    );
}
