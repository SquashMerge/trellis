//! JavaScript and TypeScript workspace packages of the captured root `package.json`
//! `workspaces` and `pnpm-workspace.yaml` `packages` (#773).
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

const ROOT: &str = r#"{"name":"acme","private":true,"workspaces":["apps/*","packages/*"]}"#;
const ROOT_PNPM: &str = r#"{"name":"acme","private":true}"#;
const PNPM: &str =
    "# members\npackages:\n  - 'apps/*'\n  - \"packages/*\"\ncatalog:\n  react: ^18.0.0\n";
const UI: &str = r#"{"name":"@acme/ui","main":"./src/index.js"}"#;
// The web app is ordered above the UI package: the UI package must never import it. Every
// manifest is an npm app (#783), so the layers name the root app `acme`, whose root `.`
// contains both paths.
const RULES: &str = r#"version = 1
[layers.web]
app = "acme"
order = 1
paths = ["apps/web/**"]
[layers.ui]
app = "acme"
order = 2
paths = ["packages/**"]
"#;

fn fixture(files: &[(&str, &str)]) -> (tempfile::TempDir, tempfile::TempDir, Store, IndexPin) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut all: Vec<(&str, &str)> = vec![
        ("apps/web/package.json", r#"{"name":"web"}"#),
        (
            "apps/web/src/main.js",
            "import { render } from \"@acme/ui\";\nimport { button } from \"@acme/ui/src/button\";\nrender(button);\n",
        ),
        ("apps/web/src/boot.js", "export function boot() {}\n"),
        ("packages/ui/package.json", UI),
        (
            "packages/ui/src/index.js",
            "import { boot } from \"web/src/boot.js\";\nexport function render() { boot(); }\n",
        ),
        ("packages/ui/src/button.js", "export const button = 1;\n"),
        (".trellis/rules.toml", RULES),
    ];
    for file in files {
        all.retain(|(path, _)| *path != file.0);
        all.push(*file);
    }
    for (path, text) in all {
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
fn reason(data: &Value, source: &str, spelling: &str) -> Option<Value> {
    data["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| {
            row["source"]["path"] == source && row["site"]["spelling"] == format!("\"{spelling}\"")
        })
        .map(|row| row["reason"].clone())
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

fn assert_members_resolve(store: &Store, pin: IndexPin) {
    let data = read(store, "references", pin);
    assert!(
        has_edge(&data, "apps/web/src/main.js", "packages/ui/src/index.js"),
        "{data}"
    );
    assert!(has_edge(
        &data,
        "apps/web/src/main.js",
        "packages/ui/src/button.js"
    ));
    assert!(has_edge(
        &data,
        "packages/ui/src/index.js",
        "apps/web/src/boot.js"
    ));
    assert_eq!(reason(&data, "apps/web/src/main.js", "@acme/ui"), None);
    let checked = read(store, "check", pin);
    assert_eq!(
        layer_status(&checked, "packages/ui/src/index.js", "apps/web/src/boot.js"),
        Some(json!("deviates")),
        "{checked}"
    );
    assert_eq!(
        layer_status(&checked, "apps/web/src/main.js", "packages/ui/src/index.js"),
        Some(json!("conforms")),
        "{checked}"
    );
}

#[test]
fn workspace_members_resolve_and_an_upward_edge_deviates() {
    let (_state, _root, store, pin) = fixture(&[("package.json", ROOT)]);
    assert_members_resolve(&store, pin);
}

#[test]
fn pnpm_workspace_members_resolve_and_an_upward_edge_deviates() {
    let (_state, _root, store, pin) =
        fixture(&[("package.json", ROOT_PNPM), ("pnpm-workspace.yaml", PNPM)]);
    assert_members_resolve(&store, pin);
}

#[test]
fn without_a_workspace_member_names_stay_unknown() {
    let (_state, _root, store, pin) = fixture(&[("package.json", ROOT_PNPM)]);
    let data = read(&store, "references", pin);
    assert!(!has_edge(
        &data,
        "apps/web/src/main.js",
        "packages/ui/src/index.js"
    ));
    assert_eq!(
        reason(&data, "apps/web/src/main.js", "@acme/ui"),
        Some(json!("absolute_or_bare_import")),
        "{data}"
    );
}

#[test]
fn a_typescript_member_resolves_from_typescript_and_javascript() {
    let (_state, _root, store, pin) = fixture(&[
        ("package.json", ROOT),
        (
            "packages/kit/package.json",
            r#"{"name":"@acme/kit","main":"src/index.ts"}"#,
        ),
        (
            "packages/kit/src/index.ts",
            "export const kit: number = 1;\n",
        ),
        (
            "packages/kit/src/extra.ts",
            "export const extra: number = 2;\n",
        ),
        (
            "apps/web/src/app.ts",
            "import { kit } from \"@acme/kit\";\nimport { extra } from \"@acme/kit/src/extra\";\nimport { render } from \"@acme/ui\";\nexport const all = [kit, extra, render];\n",
        ),
    ]);
    let data = read(&store, "references", pin);
    assert!(
        has_edge(&data, "apps/web/src/app.ts", "packages/kit/src/index.ts"),
        "{data}"
    );
    assert!(has_edge(
        &data,
        "apps/web/src/app.ts",
        "packages/kit/src/extra.ts"
    ));
    assert!(has_edge(
        &data,
        "apps/web/src/app.ts",
        "packages/ui/src/index.js"
    ));
}

#[test]
fn two_members_with_one_name_stay_unknown() {
    let (_state, _root, store, pin) = fixture(&[
        ("package.json", ROOT),
        ("packages/ui2/package.json", UI),
        ("packages/ui2/src/index.js", "export function render() {}\n"),
    ]);
    let data = read(&store, "references", pin);
    assert!(!has_edge(
        &data,
        "apps/web/src/main.js",
        "packages/ui/src/index.js"
    ));
    assert!(!has_edge(
        &data,
        "apps/web/src/main.js",
        "packages/ui2/src/index.js"
    ));
    assert_eq!(
        reason(&data, "apps/web/src/main.js", "@acme/ui"),
        Some(json!("javascript_workspace_package_ambiguous")),
        "{data}"
    );
}

#[test]
fn a_member_with_an_exports_map_stays_unknown() {
    let (_state, _root, store, pin) = fixture(&[
        ("package.json", ROOT),
        (
            "packages/ui/package.json",
            r#"{"name":"@acme/ui","main":"./src/index.js","exports":{".":{"import":"./src/index.js"}}}"#,
        ),
    ]);
    let data = read(&store, "references", pin);
    assert!(!has_edge(
        &data,
        "apps/web/src/main.js",
        "packages/ui/src/index.js"
    ));
    for spelling in ["@acme/ui", "@acme/ui/src/button"] {
        assert_eq!(
            reason(&data, "apps/web/src/main.js", spelling),
            Some(json!("javascript_workspace_exports_unsupported")),
            "{spelling}: {data}"
        );
    }
}

#[test]
fn a_single_string_exports_names_the_entry_and_no_subpath() {
    let (_state, _root, store, pin) = fixture(&[
        ("package.json", ROOT),
        (
            "packages/ui/package.json",
            r#"{"name":"@acme/ui","exports":"./src/index.js"}"#,
        ),
    ]);
    let data = read(&store, "references", pin);
    assert!(has_edge(
        &data,
        "apps/web/src/main.js",
        "packages/ui/src/index.js"
    ));
    assert_eq!(
        reason(&data, "apps/web/src/main.js", "@acme/ui/src/button"),
        Some(json!("javascript_workspace_exports_unsupported")),
        "{data}"
    );
}

#[test]
fn malformed_workspace_config_is_a_typed_unknown() {
    for files in [
        vec![
            ("package.json", ROOT_PNPM),
            ("pnpm-workspace.yaml", "packages:\n\t- 'apps/*'\n"),
        ],
        vec![
            ("package.json", ROOT_PNPM),
            ("pnpm-workspace.yaml", "packages: [unterminated\n"),
        ],
        vec![(
            "package.json",
            r#"{"name":"acme","workspaces":"packages/*"}"#,
        )],
        vec![
            ("package.json", ROOT),
            ("packages/ui/package.json", "{\"name\": \"@acme/ui\",,}"),
        ],
    ] {
        let (_state, _root, store, pin) = fixture(&files);
        let data = read(&store, "references", pin);
        assert!(!has_edge(
            &data,
            "apps/web/src/main.js",
            "packages/ui/src/index.js"
        ));
        assert_eq!(
            reason(&data, "apps/web/src/main.js", "@acme/ui"),
            Some(json!("javascript_workspace_unreadable")),
            "{files:?}: {data}"
        );
    }
}
