//! Rust structural references across Cargo workspace member crates (#772) and inline
//! `crate::`/`self::`/`super::` paths without a `use` item (#828).
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

const ROOT: &str = "[workspace]\nmembers = [\"crates/*\"]\n";
const CORE: &str = "[package]\nname = \"acme-core\"\nversion = \"0.1.0\"\n[dependencies]\nacme-app = { path = \"../app\" }\n";
const APP: &str = "[package]\nname = \"acme-app\"\nversion = \"0.1.0\"\n[dependencies]\nacme-core = { path = \"../core\" }\n";
// The app is ordered above core: core must never import it.
const RULES: &str = r#"version = 1
[layers.app]
order = 1
paths = ["crates/app/**"]
[layers.core]
order = 2
paths = ["crates/core/**"]
"#;

fn fixture(files: &[(&str, &str)]) -> (tempfile::TempDir, tempfile::TempDir, Store, IndexPin) {
    let base: Vec<(&str, &str)> = vec![
        ("Cargo.toml", ROOT),
        ("crates/core/Cargo.toml", CORE),
        ("crates/core/src/lib.rs", "pub mod model;\n"),
        (
            "crates/core/src/model.rs",
            "use acme_app::view::show;\npub struct Model;\npub fn render() { show(); }\n",
        ),
        ("crates/app/Cargo.toml", APP),
        ("crates/app/src/lib.rs", "pub mod view;\n"),
        (
            "crates/app/src/view.rs",
            "use acme_core::model::Model;\npub fn show() {}\npub fn keep(_: &Model) {}\n",
        ),
        (".trellis/rules.toml", RULES),
    ];
    indexed(base, files)
}

fn indexed<'a>(
    mut all: Vec<(&'a str, &'a str)>,
    files: &[(&'a str, &'a str)],
) -> (tempfile::TempDir, tempfile::TempDir, Store, IndexPin) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    // A later entry replaces an earlier one with the same path.
    for (path, text) in files {
        all.retain(|(p, _)| p != path);
        all.push((path, text));
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
fn member_crate_imports_resolve_and_an_upward_edge_deviates() {
    let (_state, _root, store, pin) = fixture(&[]);
    let data = read(&store, "references", pin);
    assert!(
        has_edge(&data, "crates/app/src/view.rs", "crates/core/src/model.rs"),
        "{data}"
    );
    assert!(has_edge(
        &data,
        "crates/core/src/model.rs",
        "crates/app/src/view.rs"
    ));
    assert!(unresolved(&data, "crates/app/src/view.rs", "acme_core::model::Model").is_none());
    let checked = read(&store, "check", pin);
    assert_eq!(
        layer_status(
            &checked,
            "crates/core/src/model.rs",
            "crates/app/src/view.rs"
        ),
        Some(json!("deviates")),
        "{checked}"
    );
    assert_eq!(
        layer_status(
            &checked,
            "crates/app/src/view.rs",
            "crates/core/src/model.rs"
        ),
        Some(json!("conforms")),
        "{checked}"
    );
}

#[test]
fn a_crate_name_two_members_claim_stays_unknown() {
    let (_state, _root, store, pin) = fixture(&[
        ("crates/dup/Cargo.toml", "[package]\nname = \"acme-core\"\n"),
        ("crates/dup/src/lib.rs", "pub mod model;\n"),
        ("crates/dup/src/model.rs", "pub struct Model;\n"),
    ]);
    let data = read(&store, "references", pin);
    assert!(!has_edge(
        &data,
        "crates/app/src/view.rs",
        "crates/core/src/model.rs"
    ));
    assert!(!has_edge(
        &data,
        "crates/app/src/view.rs",
        "crates/dup/src/model.rs"
    ));
    assert!(
        unresolved(&data, "crates/app/src/view.rs", "acme_core::model::Model").is_some(),
        "{data}"
    );
    // The other direction is unaffected.
    assert!(has_edge(
        &data,
        "crates/core/src/model.rs",
        "crates/app/src/view.rs"
    ));
}

#[test]
fn a_renamed_path_dependency_binds_its_key() {
    let (_state, _root, store, pin) = fixture(&[
        (
            "crates/app/Cargo.toml",
            "[package]\nname = \"acme-app\"\n[dependencies]\ncorelib = { package = \"acme-core\", path = \"../core\" }\n",
        ),
        (
            "crates/app/src/view.rs",
            "use corelib::model::Model;\nuse acme_core::model::Model as Old;\npub fn show() {}\npub fn keep(_: &Model, _: &Old) {}\n",
        ),
    ]);
    let data = read(&store, "references", pin);
    assert!(
        has_edge(&data, "crates/app/src/view.rs", "crates/core/src/model.rs"),
        "{data}"
    );
    assert!(unresolved(&data, "crates/app/src/view.rs", "corelib::model::Model").is_none());
    // The package name is not bound once the dependency is renamed.
    assert!(
        unresolved(
            &data,
            "crates/app/src/view.rs",
            "acme_core::model::Model as Old"
        )
        .is_some(),
        "{data}"
    );
}

#[test]
fn a_crate_outside_the_workspace_members_stays_unknown() {
    let (_state, _root, store, pin) =
        fixture(&[("Cargo.toml", "[workspace]\nmembers = [\"crates/app\"]\n")]);
    let data = read(&store, "references", pin);
    assert!(!has_edge(
        &data,
        "crates/app/src/view.rs",
        "crates/core/src/model.rs"
    ));
    assert!(unresolved(&data, "crates/app/src/view.rs", "acme_core::model::Model").is_some());
}

#[test]
fn a_crate_private_item_of_another_member_stays_unknown() {
    let (_state, _root, store, pin) = fixture(&[
        (
            "crates/core/src/model.rs",
            "use acme_app::view::show;\npub struct Model;\npub(crate) struct Hidden;\npub fn render() { show(); }\n",
        ),
        (
            "crates/app/src/view.rs",
            "use acme_core::model::Hidden;\npub fn show() {}\npub fn keep(_: &Hidden) {}\n",
        ),
    ]);
    let data = read(&store, "references", pin);
    assert!(!has_edge(
        &data,
        "crates/app/src/view.rs",
        "crates/core/src/model.rs"
    ));
    assert!(
        unresolved(&data, "crates/app/src/view.rs", "acme_core::model::Hidden").is_some(),
        "{data}"
    );
}

/// #828: inline `crate::`, `self::` and `super::` paths with no `use` item reach the module
/// they name, through `pub(crate)` modules and items as in `src/store/architecture_core.rs`.
#[test]
fn inline_qualified_paths_without_a_use_item_resolve() {
    let (_state, _root, store, pin) = indexed(
        vec![
            (
                "Cargo.toml",
                "[package]\nname = \"acme\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub(crate) mod apps;\npub mod store;\n"),
            // The binary binds its own package's library, whose `pub(crate)` items it cannot
            // see.
            (
                "src/main.rs",
                "use acme::store::shared_api;\nuse acme::apps::detect;\nfn main() { shared_api(); detect(); }\n",
            ),
            (
                "src/apps.rs",
                "pub(crate) struct Candidates;\npub(crate) fn detect() {}\n",
            ),
            (
                "src/store.rs",
                "mod core;\nmod helper;\npub(crate) fn shared() {}\npub fn shared_api() {}\n",
            ),
            ("src/store/helper.rs", "pub(crate) fn help() {}\n"),
            (
                "src/store/core.rs",
                "fn run(_: &crate::apps::Candidates) {\n    crate::apps::detect();\n    super::shared();\n    super::helper::help();\n    self::local();\n}\nfn local() {}\n",
            ),
        ],
        &[],
    );
    let data = read(&store, "references", pin);
    assert!(
        has_edge(&data, "src/store/core.rs", "src/apps.rs"),
        "{data}"
    );
    assert!(
        has_edge(&data, "src/store/core.rs", "src/store.rs"),
        "{data}"
    );
    assert!(has_edge(&data, "src/store/core.rs", "src/store/helper.rs"));
    assert!(has_edge(&data, "src/main.rs", "src/store.rs"), "{data}");
    assert!(!has_edge(&data, "src/main.rs", "src/apps.rs"), "{data}");
    assert!(unresolved(&data, "src/main.rs", "acme::apps::detect").is_some());
    for spelling in [
        "crate::apps::Candidates",
        "crate::apps::detect",
        "super::shared",
        "super::helper::help",
        "self::local",
    ] {
        assert!(
            unresolved(&data, "src/store/core.rs", spelling).is_none(),
            "{spelling}: {data}"
        );
    }
}

/// PR #883 review: a proved alias of a member's module keeps that member as its crate.
#[test]
fn review_883_workspace_foreign_module_alias() {
    let (_state, _root, store, pin) = indexed(
        vec![
            (
                "Cargo.toml",
                "[workspace]\nmembers = ['a', 'b']\nresolver = '2'\n",
            ),
            (
                "a/Cargo.toml",
                "[package]\nname = 'a'\nversion = '0.1.0'\nedition = '2021'\n[dependencies]\nb = { path = '../b' }\n",
            ),
            (
                "a/src/lib.rs",
                "use b::model as alias;\nuse alias::Model;\npub fn keep(_: Model) {}\n",
            ),
            (
                "b/Cargo.toml",
                "[package]\nname = 'b'\nversion = '0.1.0'\nedition = '2021'\n",
            ),
            ("b/src/lib.rs", "pub mod model;\n"),
            ("b/src/model.rs", "pub struct Model;\n"),
        ],
        &[],
    );
    let data = read(&store, "references", pin);
    assert!(
        unresolved(&data, "a/src/lib.rs", "b::model as alias").is_none(),
        "direct module import must resolve: {data}"
    );
    assert!(
        unresolved(&data, "a/src/lib.rs", "alias::Model").is_none(),
        "proved member module alias must resolve: {data}"
    );
    assert!(
        has_edge(&data, "a/src/lib.rs", "b/src/model.rs"),
        "aliased member item must produce the same edge as its direct path: {data}"
    );
}

/// PR #883 review: `[lib] path = "lib.rs"` at the checkout top level owns its child modules.
#[test]
fn review_883_checkout_top_level_declared_library() {
    let (_state, _root, store, pin) = indexed(
        vec![
            (
                "Cargo.toml",
                "[package]\nname = 'root_lib'\nversion = '0.1.0'\nedition = '2021'\n[lib]\npath = 'lib.rs'\n",
            ),
            (
                "lib.rs",
                "pub mod model;\nuse crate::model::Model;\npub fn keep(_: Model) {}\n",
            ),
            ("model.rs", "pub struct Model;\n"),
        ],
        &[],
    );
    let data = read(&store, "references", pin);
    assert!(
        unresolved(&data, "lib.rs", "crate::model::Model").is_none(),
        "declared library root must be owned: {data}"
    );
    assert!(
        has_edge(&data, "lib.rs", "model.rs"),
        "declared top-level library must reach its child module: {data}"
    );
}

/// Review probe: a public re-export resolves its defining-crate path internally,
/// even when the original caller entered the crate through the extern prelude.
#[test]
fn review_public_reexport_through_crate_private_modules_resolves() {
    let (_state, _root, store, pin) = fixture(&[
        (
            "crates/core/src/lib.rs",
            "pub(crate) mod outer { pub(crate) mod inner { pub fn public_api() {} } }\npub use crate::outer::inner::public_api;\npub fn own_call() { crate::outer::inner::public_api(); }\n",
        ),
        (
            "crates/core/src/model.rs",
            "// No module declaration reaches this old fixture file.\n",
        ),
        (
            "crates/app/src/view.rs",
            "use acme_core::public_api;\nuse acme_core::outer::inner::public_api as forbidden;\npub fn show() { public_api(); }\n",
        ),
    ]);
    let data = read(&store, "references", pin);
    // Its own crate can follow the crate-private modules.
    assert!(
        unresolved(
            &data,
            "crates/core/src/lib.rs",
            "crate::outer::inner::public_api"
        )
        .is_none(),
        "internal path must resolve: {data}"
    );
    // An external consumer cannot name those modules directly.
    assert!(
        unresolved(
            &data,
            "crates/app/src/view.rs",
            "acme_core::outer::inner::public_api as forbidden"
        )
        .is_some(),
        "crate-private module path must stay unknown: {data}"
    );
    // But it can name the legal public root re-export.
    assert!(
        unresolved(&data, "crates/app/src/view.rs", "acme_core::public_api").is_none(),
        "legal public re-export must resolve: {data}"
    );
    assert!(
        has_edge(&data, "crates/app/src/view.rs", "crates/core/src/lib.rs"),
        "public re-export must produce its structural edge: {data}"
    );
}
