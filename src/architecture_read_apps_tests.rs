use super::*;
use crate::apps::Outcome;
use crate::indexer::{IndexOptions, index_workspace_bundle};
use crate::store::{SelectedIntegrity, Store};
use rusqlite::Connection;
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
    write(dir, "crates/alpha/src/main.rs", SERVER);
    write(dir, "tools/loose/Cargo.toml", "this is not toml [\n");
    write(dir, "tools/loose/src/lib.rs", "pub fn f() {}\n");
    write(dir, "web/package.json", "{\"name\":\"web-app\"}\n");
    write(dir, "web/app.js", CLIENT);
    if root_package_json {
        write(dir, "package.json", "{\"name\":\"root\"}\n");
    }
    write(dir, "a.js", "function a() {}\n");
    let store = publish(&state, dir);
    (state, root, store)
}

fn publish(state: &tempfile::TempDir, dir: &std::path::Path) -> Store {
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
    store
}

const SERVER: &str = "async fn h() {}\nfn router() -> Router {\n    Router::new().route(\"/health\", get(h))\n}\nfn main() {}\n";
const CLIENT: &str =
    "async function app(api) {\n  await api(\"/health\");\n  await api(\"/missing\");\n}\n";

/// A checkout with one app only, so that interfaces have nothing to relate.
fn published_single_app() -> (tempfile::TempDir, tempfile::TempDir, Store) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "web/package.json", "{\"name\":\"web-app\"}\n");
    write(root.path(), "web/app.js", CLIENT);
    write(root.path(), "src/lib.rs", SERVER);
    let store = publish(&state, root.path());
    (state, root, store)
}

fn core_with_rules(store: &Store) -> Result<ArchitectureCore> {
    let config = crate::rules::parse("version = 1\n").unwrap();
    let response = store.evidence_response()?;
    let pin = response.selected_pin(None)?;
    response.architecture_core_at(
        pin,
        Some(&config),
        false,
        &mut || Ok(()),
        &|_: ArchitectureProgress| {},
    )
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
        (member.name.as_str(), member.roots[0].as_str()),
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
    let db = Connection::open(store.index_db_path_for_tests()).unwrap();
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
    let db = Connection::open(store.index_db_path_for_tests()).unwrap();
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

#[test]
fn two_apps_extract_routes_and_match_their_client_calls() {
    let (_state, _root, store) = published(false);
    let core = core_with_rules(&store).unwrap();
    let found = core.interfaces().unwrap();
    let routes: Vec<&str> = found
        .routes
        .iter()
        .map(|route| route.path_template.as_str())
        .collect();
    assert_eq!(routes, ["/health"]);
    let paths: Vec<_> = found
        .calls
        .iter()
        .map(|call| call.path_template.as_deref())
        .collect();
    assert_eq!(paths, [Some("/health"), Some("/missing")]);
    assert_eq!(
        found.matched[0].map(|m| m.confidence),
        Some(crate::interfaces::Confidence::Exact)
    );
    assert!(found.matched[1].is_none());
}

#[test]
fn one_app_has_empty_interfaces_without_extracting() {
    let (_state, _root, store) = published_single_app();
    let core = core_with_rules(&store).unwrap();
    let found = core.interfaces().unwrap();
    assert!(found.routes.is_empty() && found.calls.is_empty() && found.matched.is_empty());
}

/// #851: without a rules file every detected manifest is an app, so two of them extract the
/// same interfaces a rules file's core does, for `trellis_endpoints`.
#[test]
fn a_core_without_rules_extracts_the_interfaces_of_two_detected_apps() {
    let (_state, _root, store) = published(false);
    let unruled = core(&store).unwrap();
    let ruled = core_with_rules(&store).unwrap();
    let (found, expected) = (unruled.interfaces().unwrap(), ruled.interfaces().unwrap());
    let routes = |found: &crate::interfaces::Extraction| -> Vec<String> {
        found
            .routes
            .iter()
            .map(|route| route.path_template.clone())
            .collect()
    };
    assert_eq!(routes(found), ["/health"]);
    assert_eq!(routes(found), routes(expected));
    assert_eq!(found.matched, expected.matched);
}

/// App detection that exhausts its work budget fails the interfaces part with the same
/// typed failure; it is never a successful, empty extraction (#843 review).
#[test]
fn failed_app_detection_fails_the_interfaces_part() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path();
    write(dir, "Cargo.toml", "[package]\nname = \"srv\"\n");
    write(dir, "src/main.rs", SERVER);
    let manifest = format!(
        "{{\"name\":\"web-app\",\"padding\":\"{}\"}}",
        "x".repeat(8_000_000)
    );
    write(dir, "web/package.json", &manifest);
    write(dir, "web/app.js", CLIENT);
    let store = publish(&state, dir);
    let core = core_with_rules(&store).unwrap();
    let candidates = core.app_candidates().expect_err("detection fails");
    assert_eq!(
        candidates.to_string(),
        "bounded_work: app manifest parsing",
        "{candidates:#}"
    );
    let interfaces = core.interfaces().err().expect("interfaces part fails");
    assert_eq!(
        interfaces.to_string(),
        "bounded_work: app manifest parsing",
        "{interfaces:#}"
    );
}

/// mimir's layout (#807): a root Python service and a root Vite frontend, plus a Python
/// plugin with its own `pyproject.toml`.
const MIMIR: &[(&str, &str)] = &[
    (
        "pyproject.toml",
        "[project]\nname = \"mimir-agent\"\n[project.scripts]\nmimir = \"mimir.main:main\"\n",
    ),
    (
        "package.json",
        "{\"name\":\"mimir-frontend\",\"private\":true}\n",
    ),
    (
        "plugins/cultivate/pyproject.toml",
        "[project]\nname = \"mimir-cultivate\"\n",
    ),
    ("mimir/__init__.py", ""),
    ("mimir/main.py", "def main():\n    return 1\n"),
    (
        "plugins/cultivate/cultivate/__init__.py",
        "def grow():\n    return 2\n",
    ),
    (
        "src/main.ts",
        "export function start(): number { return 1; }\n",
    ),
    // Not above any Python source, so capture does not retain it.
    ("docs/pyproject.toml", "[project]\nname = \"docs\"\n"),
    ("docs/guide.md", "# guide\n"),
];

#[test]
fn a_captured_mimir_layout_reaches_core_app_candidates_as_three_apps() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for (path, text) in MIMIR {
        write(root.path(), path, text);
    }
    let store = publish(&state, root.path());
    let found = core(&store).unwrap().app_candidates().unwrap().clone();
    let outcomes: BTreeMap<&str, &Outcome> = found
        .manifests
        .iter()
        .map(|(path, outcome)| (path.as_str(), outcome))
        .collect();
    let Outcome::App(agent) = outcomes["pyproject.toml"] else {
        panic!("{outcomes:?}");
    };
    assert_eq!(
        (agent.kind, agent.entries.as_slice()),
        (
            Some(crate::apps::AppKind::Python),
            &["mimir.main".to_owned()][..]
        )
    );
    assert!(
        !outcomes.contains_key("docs/pyproject.toml"),
        "{outcomes:?}"
    );
    let detected = crate::apps::detect(&found, &Default::default());
    let apps: Vec<(&str, crate::apps::AppKind, &str)> = detected
        .apps
        .iter()
        .map(|app| (app.name.as_str(), app.kind.unwrap(), app.roots[0].as_str()))
        .collect();
    use crate::apps::AppKind::{Npm, Python};
    assert_eq!(
        apps,
        [
            ("mimir-frontend", Npm, "."),
            ("mimir-cultivate", Python, "plugins/cultivate"),
            ("mimir-agent", Python, "."),
        ]
    );
    assert_eq!(detected.unread, 0);
}

/// #854: a Maven multi-module build and a Gradle multi-module build in one checkout. Capture
/// retains the nested module manifests above Java sources, and each runnable module is an app.
const JAVA_BUILDS: &[(&str, &str)] = &[
    (
        "shop/pom.xml",
        "<project><artifactId>shop</artifactId><packaging>pom</packaging><modules><module>api</module><module>core</module></modules></project>\n",
    ),
    (
        "shop/api/pom.xml",
        "<project><artifactId>shop-api</artifactId><packaging>war</packaging></project>\n",
    ),
    (
        "shop/core/pom.xml",
        "<project><artifactId>shop-core</artifactId></project>\n",
    ),
    (
        "shop/api/src/main/java/shop/Api.java",
        "package shop; public class Api {}\n",
    ),
    (
        "shop/core/src/main/java/shop/Core.java",
        "package shop; public class Core {}\n",
    ),
    (
        "settings.gradle",
        "rootProject.name = 'tools'\ninclude 'cli', 'lib'\n",
    ),
    (
        "cli/build.gradle.kts",
        "plugins { application }\napplication { mainClass.set(\"tools.Cli\") }\n",
    ),
    ("lib/build.gradle.kts", "plugins { `java-library` }\n"),
    (
        "cli/src/main/java/tools/Cli.java",
        "package tools; public class Cli {}\n",
    ),
    (
        "lib/src/main/java/tools/Lib.java",
        "package tools; public class Lib {}\n",
    ),
    // Not above any Java source, so capture does not retain it.
    (
        "docs/pom.xml",
        "<project><artifactId>docs</artifactId><packaging>war</packaging></project>\n",
    ),
    ("docs/guide.md", "# guide\n"),
];

#[test]
fn captured_maven_and_gradle_multi_module_builds_reach_core_as_runnable_modules() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for (path, text) in JAVA_BUILDS {
        write(root.path(), path, text);
    }
    let store = publish(&state, root.path());
    let found = core(&store).unwrap().app_candidates().unwrap().clone();
    let outcomes: BTreeMap<&str, &Outcome> = found
        .manifests
        .iter()
        .map(|(path, outcome)| (path.as_str(), outcome))
        .collect();
    assert!(!outcomes.contains_key("docs/pom.xml"), "{outcomes:?}");
    for not_app in [
        "shop/pom.xml",
        "shop/core/pom.xml",
        "lib/build.gradle.kts",
        "settings.gradle",
    ] {
        assert_eq!(
            outcomes.get(not_app),
            Some(&&Outcome::NotApp),
            "{not_app}: {outcomes:?}"
        );
    }
    let detected = crate::apps::detect(&found, &Default::default());
    let apps: Vec<(&str, crate::apps::AppKind, &str, &[String])> = detected
        .apps
        .iter()
        .map(|app| {
            (
                app.name.as_str(),
                app.kind.unwrap(),
                app.roots[0].as_str(),
                app.entries.as_slice(),
            )
        })
        .collect();
    use crate::apps::AppKind::{Gradle, Maven};
    assert_eq!(
        apps,
        [
            ("cli", Gradle, "cli", &["tools.Cli".to_owned()][..]),
            ("shop-api", Maven, "shop/api", &[][..]),
        ]
    );
    assert_eq!(detected.unread, 0);
}

/// #851 review (Prime, PR #868): one detected app and one authored app make a ruled core
/// extract interfaces. Once the rules file is removed, that core's extraction must not answer
/// an endpoints read: the owner hands such a read only a core built without rules
/// (`CoreRules::Exactly(None)`), and the endpoint stage refuses a core of another rules file
/// rather than projecting it. The fresh unruled core answers no rows.
#[test]
fn review_removed_rules_must_not_reuse_old_interface_extraction() {
    let (_state, _root, store) = published_single_app();
    let config = crate::rules::parse("version = 1\n[apps.server]\nroots = [\"src\"]\n").unwrap();
    let response = store.evidence_response().unwrap();
    let pin = response.selected_pin(None).unwrap();
    let old = response
        .architecture_core_at(pin, Some(&config), false, &mut || Ok(()), &|_| {})
        .unwrap();
    let fresh = response
        .architecture_core_at(pin, None, false, &mut || Ok(()), &|_| {})
        .unwrap();
    let arguments = serde_json::json!({});
    let parsed = EvidenceResponse::architecture_arguments("endpoints", &arguments).unwrap();
    let stale = response.architecture_response_at(
        "endpoints",
        &arguments,
        &parsed,
        None,
        &old,
        &mut || Ok(()),
    );
    let fresh_answer = response
        .architecture_response_at("endpoints", &arguments, &parsed, None, &fresh, &mut || {
            Ok(())
        })
        .unwrap();
    // Controls: the core of the file answers that file, and the fresh unruled core is empty.
    let ruled = response
        .architecture_response_at(
            "endpoints",
            &arguments,
            &parsed,
            Some(&config),
            &old,
            &mut || Ok(()),
        )
        .unwrap();
    assert_eq!(ruled["extracted"], true);
    assert_eq!(ruled["endpoints"].as_array().unwrap().len(), 2);
    assert_eq!(fresh_answer["extracted"], false);
    assert_eq!(fresh_answer["endpoints"], serde_json::json!([]));
    // #851 requires no endpoints when fewer than two detected plus authored apps remain.
    match stale {
        Ok(stale_answer) => {
            assert_eq!(stale_answer["extracted"], false);
            assert_eq!(
                stale_answer["endpoints"],
                serde_json::json!([]),
                "#851 requires no endpoints when fewer than two detected plus authored apps remain"
            );
        }
        Err(error) => assert!(
            error
                .to_string()
                .starts_with("store_unavailable: architecture core of another rules file"),
            "{error:#}"
        ),
    }
}
