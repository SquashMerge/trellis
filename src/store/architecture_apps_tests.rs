use super::*;
use crate::apps::Outcome;
use crate::indexer::{IndexOptions, index_workspace_bundle};
use std::{fs, sync::atomic::AtomicBool};

fn write(root: &std::path::Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// A checkout with a root workspace, a member, and nested and non-member manifests.
fn published(root_package_json: bool) -> (tempfile::TempDir, tempfile::TempDir, Store) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path();
    write(dir, "Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
    write(
        dir,
        "crates/alpha/Cargo.toml",
        "[package]\nname = \"alpha\"\n",
    );
    write(dir, "crates/alpha/src/main.rs", "fn main() {}\n");
    write(dir, "tools/loose/Cargo.toml", "this is not toml [\n");
    write(dir, "tools/loose/src/lib.rs", "pub fn f() {}\n");
    write(dir, "web/package.json", "{\"name\":\"web-app\"}\n");
    write(dir, "web/app.js", "function app() {}\n");
    if root_package_json {
        write(dir, "package.json", "{\"name\":\"root\"}\n");
    }
    write(dir, "a.js", "function a() {}\n");
    let store = crate::testing::retry_admission(|| Store::open_for_tests(state.path(), dir));
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(dir.into()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            &store.writer().unwrap(),
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    (state, root, store)
}

fn core(store: &Store) -> Result<ArchitectureCore> {
    let response = store.evidence_response()?;
    let pin = response.selected_pin(None)?;
    response.architecture_core_at(
        pin,
        None,
        false,
        &mut || Ok(()),
        &|_: ArchitectureProgress| {},
    )
}

#[test]
fn nested_manifests_reach_core_app_candidates_with_their_outcomes() {
    let (_state, _root, store) = published(true);
    let core = core(&store).unwrap();
    let found = core.app_candidates().unwrap();
    let outcomes: BTreeMap<&str, &Outcome> = found
        .manifests
        .iter()
        .map(|(path, outcome)| (path.as_str(), outcome))
        .collect();
    let Outcome::App(member) = outcomes["crates/alpha/Cargo.toml"] else {
        panic!("{outcomes:?}");
    };
    assert_eq!(
        (member.name.as_str(), member.root.as_str()),
        ("alpha", "crates/alpha")
    );
    let Outcome::App(web) = outcomes["web/package.json"] else {
        panic!("{outcomes:?}");
    };
    assert_eq!(web.name, "web-app");
    assert!(matches!(outcomes["package.json"], Outcome::App(_)));
    assert_eq!(outcomes["Cargo.toml"], &Outcome::NotApp);
    // Not a workspace member: never parsed, so its malformed bytes are not `Unread`.
    assert_eq!(outcomes["tools/loose/Cargo.toml"], &Outcome::NotApp);
}

#[test]
fn an_absent_root_package_json_is_not_an_app() {
    let (_state, _root, store) = published(false);
    let core = core(&store).unwrap();
    let found = core.app_candidates().unwrap();
    let root = found
        .manifests
        .iter()
        .find(|(path, _)| path == "package.json")
        .unwrap();
    assert_eq!(root.1, Outcome::NotApp);
}

#[test]
fn a_corrupt_retained_nested_entry_is_a_typed_selected_integrity_failure() {
    let (_state, _root, store) = published(true);
    let db = Connection::open(store.roots.index_db(&store.identity)).unwrap();
    let retained: String = db
        .query_row("SELECT retained_config FROM native_revisions", [], |row| {
            row.get(0)
        })
        .unwrap();
    let mut entries: Vec<serde_json::Value> = serde_json::from_str(&retained).unwrap();
    let entry = entries
        .iter_mut()
        .find(|entry| entry["path"] == "web/package.json")
        .expect("nested manifest is retained");
    entry["byteLength"] = serde_json::json!(1);
    db.execute(
        "UPDATE native_revisions SET retained_config=?1",
        [serde_json::to_string(&entries).unwrap()],
    )
    .unwrap();
    drop(db);
    let error = core(&store).err().expect("corrupt entry fails the read");
    assert!(
        error
            .chain()
            .any(|cause| cause.downcast_ref::<SelectedIntegrity>().is_some()),
        "{error:#}"
    );
    assert!(
        error
            .to_string()
            .contains("corrupt retained web/package.json length"),
        "{error:#}"
    );
}

#[test]
fn a_duplicated_retained_nested_manifest_is_a_typed_selected_integrity_failure() {
    let (_state, _root, store) = published(true);
    let db = Connection::open(store.roots.index_db(&store.identity)).unwrap();
    let retained: String = db
        .query_row("SELECT retained_config FROM native_revisions", [], |row| {
            row.get(0)
        })
        .unwrap();
    let mut entries: Vec<serde_json::Value> = serde_json::from_str(&retained).unwrap();
    let entry = entries
        .iter()
        .find(|entry| entry["path"] == "web/package.json")
        .expect("nested manifest is retained")
        .clone();
    entries.push(entry);
    db.execute(
        "UPDATE native_revisions SET retained_config=?1",
        [serde_json::to_string(&entries).unwrap()],
    )
    .unwrap();
    drop(db);
    let error = core(&store).err().expect("duplicate entry fails the read");
    assert!(
        error
            .chain()
            .any(|cause| cause.downcast_ref::<SelectedIntegrity>().is_some()),
        "{error:#}"
    );
    assert!(
        error
            .to_string()
            .contains("incompatible_index: duplicate retained nested manifest"),
        "{error:#}"
    );
}
