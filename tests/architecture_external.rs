#![cfg(feature = "test-support")]
use serde_json::{Value, json};
use std::{
    fs,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    indexer::{IndexOptions, index_workspace_bundle},
    model::{CancelFlag, IndexPin},
    store::{MetricsOptions, Store},
};

const MANIFEST: &str = r#"[package]
name = "fixture"
version = "0.1.0"
edition = "2021"
[dependencies]
wire = { package = "upstream-wire", version = "^2" }
local = { path = "vendor/local", version = "1" }
[dev-dependencies]
test_wire = { package = "upstream-wire", version = "^3" }
"#;
const RULES: &str = r#"version = 1
[external_policy]
deny = [{ language = "rust", package = "upstream-wire", stdlib = false }]
[layers.source]
order = 2
paths = ["src/**"]
[layers.packages]
order = 1
external = [{ language = "rust", package = "upstream-wire", stdlib = false }]
[layers.standard]
shared = true
external = [{ language = "rust", package = "std", stdlib = true }]
"#;
fn fixture(manifest: &str) -> (tempfile::TempDir, tempfile::TempDir, Store, IndexPin) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for (path, text) in [
        ("Cargo.toml", manifest),
        (
            "src/lib.rs",
            "mod child; pub fn root() { wire::send(); std::mem::drop(1); local::run(); absent::run(); } #[cfg(test)] mod tests { fn test() { test_wire::send(); } }",
        ),
        ("src/child.rs", "pub fn child() { wire::send(); }"),
        ("src/orphan.rs", "pub fn orphan() { wire::send(); }"),
        (".trellis/rules.toml", RULES),
    ] {
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
/// The Architecture summary adds each package's display `layer` (#448); the
/// descriptor itself is the public one.
fn without_layer(packages: &Value) -> Value {
    let mut packages = packages.clone();
    for package in packages.as_array_mut().unwrap() {
        let layer = package.as_object_mut().unwrap().remove("layer").unwrap();
        assert!(layer.is_null() || layer.is_string());
    }
    packages
}
fn read(store: &Store, kind: &str, pin: IndexPin) -> Value {
    let response = store.evidence_response().unwrap();
    let value = response
        .architecture_response(kind, &json!({"expectedBasis":pin}), &mut || Ok(()))
        .unwrap();
    response.finish(value).unwrap()
}
fn package<'a>(data: &'a Value, name: &str) -> &'a Value {
    data["externalPackages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|value| value["identity"]["package"] == name)
        .unwrap()
}

#[test]
fn selected_cargo_alias_identity_survives_live_manifest_change_and_delete() {
    let (_state, root, store, pin) = fixture(MANIFEST);
    let before = read(&store, "references", pin);
    let descriptor = package(&before, "upstream-wire");
    assert_eq!(descriptor["identity"]["kind"], "externalPackage");
    assert_eq!(descriptor["identity"]["standardLibrary"], false);
    assert_eq!(descriptor["ecosystem"], "cargo");
    let evidence = descriptor["evidence"].as_array().unwrap();
    assert!(evidence.iter().any(|entry| entry["importRoot"] == "wire"
        && entry["key"] == "wire"
        && entry["section"] == "dependencies"
        && entry["versionConstraint"] == "^2"));
    assert!(
        evidence
            .iter()
            .any(|entry| entry["importRoot"] == "test_wire"
                && entry["key"] == "test_wire"
                && entry["section"] == "dev-dependencies"
                && entry["versionConstraint"] == "^3")
    );
    assert!(
        evidence
            .iter()
            .all(|entry| entry["nativeRevisionId"].is_string()
                && entry["dependencyHash"].is_string()
                && entry["contentHash"].is_string()
                && entry["path"] == "Cargo.toml")
    );
    for role in ["production", "tests"] {
        assert!(
            before[role]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| edge["target"] == descriptor["identity"])
        );
    }
    assert_eq!(
        before["externalPackages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["identity"]["package"] == "upstream-wire")
            .count(),
        1
    );
    for source in ["src/lib.rs", "src/child.rs"] {
        assert!(
            before["production"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| edge["source"]["path"] == source
                    && edge["target"] == descriptor["identity"])
        );
    }
    assert!(
        !before["production"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["source"]["path"] == "src/orphan.rs"
                && edge["target"]["kind"] == "externalPackage")
    );
    assert!(before["unresolved"].as_array().unwrap().iter().any(|row| {
        row["site"]["spelling"]
            .as_str()
            .is_some_and(|text| text.contains("local::"))
    }));
    assert!(
        !before["externalPackages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| matches!(
                entry["identity"]["package"].as_str(),
                Some("local" | "absent")
            ))
    );
    let standard = package(&before, "std");
    assert_eq!(standard["identity"]["standardLibrary"], true);
    assert_eq!(
        standard["evidence"],
        json!([{"kind":"standardLibraryCatalog","catalog":"rust-standard-library:v1","entry":"std"}])
    );
    fs::write(
        root.path().join("Cargo.toml"),
        "invalid current manifest = [",
    )
    .unwrap();
    assert_eq!(read(&store, "references", pin), before);
    fs::remove_file(root.path().join("Cargo.toml")).unwrap();
    assert_eq!(read(&store, "references", pin), before);
}

#[test]
fn external_rules_explain_closure_and_native_metrics_do_not_create_package_modules() {
    let (_state, _root, store, pin) = fixture(MANIFEST);
    let refs = read(&store, "references", pin);
    let check = read(&store, "check", pin);
    let ban = check["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["rule"] == "$external")
        .unwrap();
    assert_eq!(ban["status"], "deviates");
    assert!(
        check["testOnly"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["rule"] == "$external")
    );
    assert_eq!(
        package(&check, "upstream-wire")["identity"],
        ban["destination"]
    );
    let response = store.evidence_response().unwrap();
    let explanation = response.architecture_response("explain", &json!({"expectedBasis":pin,"rule":"$external","target":ban["target"],"destination":ban["destination"]}), &mut || Ok(())).unwrap();
    assert_eq!(
        package(&explanation, "upstream-wire")["identity"],
        ban["destination"]
    );
    let options = MetricsOptions {
        pin: Some(pin),
        ..Default::default()
    };
    let view = response
        .architecture_view(&options, &mut || Ok(()))
        .unwrap();
    let metrics = response.metrics_at(&options, &mut || Ok(())).unwrap();
    for module in view["modules"].as_array().unwrap() {
        let identity = &module["identity"];
        assert!(identity.get("package").is_none());
        let metric = metrics
            .files
            .iter()
            .find(|file| {
                identity["sourceSetId"] == file.source_set_id
                    && identity["language"] == file.language
                    && identity["path"] == file.path
            })
            .unwrap();
        assert_eq!(module["metrics"], metric.metrics);
    }
    assert_eq!(
        without_layer(&view["references"]["externalPackages"]),
        refs["externalPackages"]
    );
    assert!(
        view["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["destinationLayer"] == "packages")
    );
    response.finish(()).unwrap();
}

#[test]
fn malformed_captured_cargo_does_not_prove_declared_packages() {
    let (_state, _root, store, pin) = fixture(
        "[dependencies
wire = '2'",
    );
    let refs = read(&store, "references", pin);
    assert!(
        !refs["externalPackages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["identity"]["package"] == "upstream-wire")
    );
    assert!(refs["unresolved"].as_array().unwrap().iter().any(|row| {
        row["site"]["spelling"]
            .as_str()
            .is_some_and(|text| text.contains("wire::"))
    }));
}

fn java_fixture() -> (tempfile::TempDir, tempfile::TempDir, Store, IndexPin) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for (path, text) in [
        (
            "pom.xml",
            "<project><modelVersion>4.0.0</modelVersion><groupId>fixture</groupId><artifactId>fixture</artifactId><version>1</version><dependencies><dependency><groupId>org.example</groupId><artifactId>widget</artifactId><version>1</version></dependency></dependencies></project>",
        ),
        (
            "build.gradle",
            "dependencies { implementation 'org.example:widget:1' }",
        ),
        (
            "src/main/java/app/Use.java",
            "package app; import pkg.*; import java.util.*; import javax.swing.*; import org.example.*; import javaish.util.*; class Use {}",
        ),
        (
            "src/main/java/unassigned/Unlayered.java",
            "package unassigned; import java.util.List; class Unlayered {}",
        ),
        (
            "src/test/java/app/UseTest.java",
            "package app; import java.util.*; class UseTest {}",
        ),
        (
            "src/main/java/types/Local.java",
            "package pkg; public class Local {}",
        ),
        (
            ".trellis/rules.toml",
            r#"version = 1
[external_policy]
deny = [
  { language = "java", package = "java", stdlib = true },
  { language = "java", package = "org.example:widget", stdlib = false },
]
[layers.source]
order = 2
paths = ["src/main/java/app/**", "src/test/java/app/**"]
[layers.standard]
order = 1
external = [{ language = "java", package = "java", stdlib = true }]
[layers.swing]
shared = true
external = [{ language = "java", package = "javax", stdlib = true }]
"#,
        ),
    ] {
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

#[test]
fn selected_java_package_families_and_native_types_survive_live_edits() {
    let (_state, root, store, pin) = java_fixture();
    let before = read(&store, "references", pin);
    assert_eq!(before["referencesVersion"], "references:v2");
    assert_eq!(before["externalPackages"].as_array().unwrap().len(), 2);
    for name in ["java", "javax"] {
        let descriptor = package(&before, name);
        assert_eq!(descriptor["ecosystem"], "java");
        assert_eq!(descriptor["identity"]["kind"], "externalPackage");
        assert_eq!(descriptor["identity"]["language"], "java");
        assert_eq!(descriptor["identity"]["standardLibrary"], true);
        assert_eq!(
            descriptor["evidence"],
            json!([{"kind":"standardLibraryCatalog","catalog":"java-standard-library:v2","entry":name}])
        );
        assert!(before["production"].as_array().unwrap().iter().any(|edge| {
            edge["source"]["path"] == "src/main/java/app/Use.java"
                && edge["target"] == descriptor["identity"]
                && edge["target"]["sourceSetId"] == edge["source"]["sourceSetId"]
        }));
    }
    assert!(before["production"].as_array().unwrap().iter().any(|edge| {
        edge["source"]["path"] == "src/main/java/app/Use.java"
            && edge["target"]["path"] == "src/main/java/types/Local.java"
    }));
    assert!(before["tests"].as_array().unwrap().iter().any(|edge| {
        edge["source"]["path"] == "src/test/java/app/UseTest.java"
            && edge["target"] == package(&before, "java")["identity"]
    }));
    for spelling in ["org.example", "javaish"] {
        assert!(
            before["unresolved"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| { row["site"]["spelling"].as_str().unwrap().contains(spelling) })
        );
    }
    for path in ["src/main/java/app/Use.java", "pom.xml", "build.gradle"] {
        fs::write(root.path().join(path), "invalid live contents = [").unwrap();
    }
    fs::write(
        root.path().join("src/main/java/types/New.java"),
        "package pkg; public class New {}",
    )
    .unwrap();
    assert_eq!(read(&store, "references", pin), before);
    for path in ["src/main/java/types/Local.java", "pom.xml", "build.gradle"] {
        fs::remove_file(root.path().join(path)).unwrap();
    }
    assert_eq!(read(&store, "references", pin), before);
}

#[test]
fn java_external_rules_explain_and_browser_keep_package_and_native_closure_separate() {
    let (_state, _root, store, pin) = java_fixture();
    let refs = read(&store, "references", pin);
    let checked = read(&store, "check", pin);
    assert_eq!(checked["rulesVersion"], "rules:v2");
    let rows = checked["instances"].as_array().unwrap();
    let ban = rows
        .iter()
        .find(|row| {
            row["rule"] == "$external"
                && row["target"]["path"] == "src/main/java/unassigned/Unlayered.java"
        })
        .unwrap();
    assert_eq!(ban["status"], "deviates");
    assert!(ban["sourceLayer"].is_null());
    assert_eq!(ban["destination"], package(&checked, "java")["identity"]);
    assert!(rows.iter().any(|row| {
        row["rule"] == "$layers"
            && row["target"]["path"] == "src/main/java/app/Use.java"
            && row["destination"] == package(&checked, "java")["identity"]
            && row["destinationLayer"] == "standard"
            && row["status"] == "deviates"
    }));
    assert!(checked["testOnly"].as_array().unwrap().iter().any(|row| {
        row["rule"] == "$external"
            && row["target"]["path"] == "src/test/java/app/UseTest.java"
            && row["destination"] == package(&checked, "java")["identity"]
    }));
    assert!(!rows.iter().any(|row| {
        row["target"]["path"] == "src/test/java/app/UseTest.java"
            || row["destination"]["package"] == "org.example:widget"
    }));
    let response = store.evidence_response().unwrap();
    let explanation = response
        .architecture_response(
            "explain",
            &json!({"expectedBasis":pin,"rule":"$external","target":ban["target"],"destination":ban["destination"]}),
            &mut || Ok(()),
        )
        .unwrap();
    assert_eq!(
        package(&explanation, "java")["identity"],
        ban["destination"]
    );
    let options = MetricsOptions {
        pin: Some(pin),
        ..Default::default()
    };
    let view = response
        .architecture_view(&options, &mut || Ok(()))
        .unwrap();
    let metrics = response.metrics_at(&options, &mut || Ok(())).unwrap();
    for module in view["modules"].as_array().unwrap() {
        let identity = &module["identity"];
        assert!(identity.get("package").is_none());
        let metric = metrics
            .files
            .iter()
            .find(|file| {
                identity["sourceSetId"] == file.source_set_id
                    && identity["language"] == file.language
                    && identity["path"] == file.path
            })
            .unwrap();
        assert_eq!(module["metrics"], metric.metrics);
    }
    assert_eq!(
        without_layer(&view["references"]["externalPackages"]),
        refs["externalPackages"]
    );
    assert!(refs["modules"].as_array().unwrap().iter().all(|module| {
        module["identity"].get("package").is_none() && module["identity"].get("sid").is_some()
    }));
    response.finish(()).unwrap();
}

#[test]
fn selected_javax_jdk_maven_coordinates_do_not_prove_non_jdk_namespaces() {
    let (_state, root, store, _pin) = java_fixture();
    fs::write(
        root.path().join("pom.xml"),
        "<project><modelVersion>4.0.0</modelVersion><groupId>fixture</groupId><artifactId>fixture</artifactId><version>1</version><dependencies><dependency><groupId>javax.inject</groupId><artifactId>javax.inject</artifactId><version>1</version></dependency></dependencies></project>",
    )
    .unwrap();
    fs::write(
        root.path().join("src/main/java/app/Use.java"),
        "package app; import javax.inject.*; import javax.inject.Inject; class Use { javax.inject.Inject field; }",
    )
    .unwrap();
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
    let data = read(&store, "references", pin);
    let source = "src/main/java/app/Use.java";
    assert!(
        !data["production"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["source"]["path"] == source),
        "Maven coordinates alone must not prove javax.inject: {data}"
    );
    let unknowns: Vec<_> = data["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["source"]["path"] == source)
        .map(|row| row["site"]["spelling"].as_str().unwrap())
        .collect();
    assert_eq!(
        unknowns,
        [
            "import javax.inject.*;",
            "import javax.inject.Inject;",
            "javax.inject.Inject"
        ]
    );
    assert!(
        !data["externalPackages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|descriptor| descriptor["identity"]["standardLibrary"] == false)
    );
}

fn python_fixture() -> (tempfile::TempDir, tempfile::TempDir, Store, IndexPin) {
    python_fixture_with_files(&[])
}

fn python_fixture_with_files(
    files: &[(&str, &str)],
) -> (tempfile::TempDir, tempfile::TempDir, Store, IndexPin) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for (path, text) in [
        (
            "pyproject.toml",
            "[project]\nname = 'fixture'\nversion = '1'\ndependencies = ['requests>=2']\n",
        ),
        ("requirements.txt", "requests>=2\n"),
        (
            "app.py",
            "import json as j\nfrom pathlib import Path\nimport requests\nimport flask\n",
        ),
        ("other.py", "import json\n"),
        (
            "tests/test_external.py",
            "def test_catalog():\n    import sys\n",
        ),
        (
            ".trellis/rules.toml",
            r#"version = 1
[external_policy]
deny = [
  { language = "python", package = "json", stdlib = true },
  { language = "python", package = "sys", stdlib = true },
  { language = "python", package = "requests", stdlib = false },
  { language = "python", package = "flask", stdlib = true },
]
[layers.source]
order = 2
paths = ["app.py", "tests/**"]
[layers.standard]
order = 1
external = [
  { language = "python", package = "json", stdlib = true },
  { language = "python", package = "pathlib", stdlib = true },
  { language = "python", package = "sys", stdlib = true },
]
"#,
        ),
    ] {
        let path = root.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    for (path, text) in files {
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

#[test]
fn selected_python_catalog_roots_are_cross_platform_labels_not_installation_proof() {
    let (_state, _root, store, pin) = python_fixture_with_files(&[(
        "app.py",
        "import os\nimport collections\nimport http.client\nimport winreg\nimport flask\n",
    )]);
    let data = read(&store, "references", pin);
    for name in ["os", "collections", "http", "winreg"] {
        let descriptor = package(&data, name);
        assert_eq!(descriptor["identity"]["standardLibrary"], true);
        assert_eq!(
            descriptor["evidence"],
            json!([{
                "kind":"standardLibraryCatalog", "catalog":"python-standard-library:v2", "entry":name
            }])
        );
        assert!(data["production"].as_array().unwrap().iter().any(|edge| {
            edge["source"]["path"] == "app.py" && edge["target"] == descriptor["identity"]
        }));
    }
    assert!(
        data["unresolved"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| { row["source"]["path"] == "app.py" && row["site"]["spelling"] == "flask" })
    );
    assert!(
        !data["externalPackages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|descriptor| descriptor["identity"]["package"] == "flask")
    );
}

#[test]
fn selected_python_local_module_keeps_precedence_over_catalog_root() {
    let (_state, _root, store, pin) = python_fixture_with_files(&[
        ("app.py", "import os\n"),
        ("os.py", "def local_only(): pass\n"),
    ]);
    let data = read(&store, "references", pin);
    assert!(
        data["production"].as_array().unwrap().iter().any(|edge| {
            edge["source"]["path"] == "app.py" && edge["target"]["path"] == "os.py"
        })
    );
    assert!(
        !data["externalPackages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|descriptor| descriptor["identity"]["package"] == "os")
    );
    assert!(
        !data["unresolved"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| { row["source"]["path"] == "app.py" && row["site"]["spelling"] == "os" })
    );
}

#[test]
fn python_catalog_labels_are_exact_and_selectors_do_not_prove_membership() {
    use trellis::references::external::{python_standard_library, valid_package};
    for name in [
        "json",
        "pathlib",
        "sys",
        "email",
        "os",
        "collections",
        "http",
        "winreg",
    ] {
        let descriptor =
            serde_json::to_value(python_standard_library("fixture", name).unwrap()).unwrap();
        assert_eq!(
            descriptor["identity"],
            json!({
                "kind":"externalPackage", "sourceSetId":"fixture", "language":"python",
                "package":name, "standardLibrary":true
            })
        );
        assert_eq!(descriptor["ecosystem"], "python");
        assert_eq!(
            descriptor["evidence"],
            json!([{
                "kind":"standardLibraryCatalog", "catalog":"python-standard-library:v2", "entry":name
            }])
        );
    }
    for name in ["jsonish", "JSON", "json.tool", "requests", "flask"] {
        assert!(python_standard_library("fixture", name).is_none());
    }
    assert!(valid_package("python", "email", true));
    assert!(valid_package("python", "requests", false));
}

#[test]
fn selected_python_catalog_edges_and_unknowns_survive_live_source_and_manifest_edits() {
    let (_state, root, store, pin) = python_fixture();
    let before = read(&store, "references", pin);
    assert_eq!(before["externalPackages"].as_array().unwrap().len(), 3);
    for name in ["json", "pathlib", "sys"] {
        let descriptor = package(&before, name);
        assert_eq!(descriptor["identity"]["language"], "python");
        assert_eq!(descriptor["identity"]["standardLibrary"], true);
        assert_eq!(descriptor["ecosystem"], "python");
        assert_eq!(
            descriptor["evidence"],
            json!([{
                "kind":"standardLibraryCatalog", "catalog":"python-standard-library:v2", "entry":name
            }])
        );
    }
    for spelling in ["requests", "flask"] {
        assert!(
            before["unresolved"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| { row["site"]["spelling"].as_str().unwrap().contains(spelling) })
        );
        assert!(
            !before["externalPackages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| { item["identity"]["package"] == spelling })
        );
    }
    assert!(before["tests"].as_array().unwrap().iter().any(|edge| {
        edge["source"]["path"] == "tests/test_external.py"
            && edge["target"] == package(&before, "sys")["identity"]
            && edge["sites"][0]["role"] == "test"
    }));
    for field in ["production", "tests"] {
        for edge in before[field].as_array().unwrap() {
            assert_eq!(edge["source"]["sourceSetId"], edge["target"]["sourceSetId"]);
            assert!(
                before["modules"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|module| { module["identity"] == edge["source"] })
            );
            let source =
                fs::read_to_string(root.path().join(edge["source"]["path"].as_str().unwrap()))
                    .unwrap();
            for site in edge["sites"].as_array().unwrap() {
                let start = site["range"]["start"].as_u64().unwrap() as usize;
                let end = site["range"]["end"].as_u64().unwrap() as usize;
                assert_eq!(&source[start..end], site["spelling"].as_str().unwrap());
            }
        }
    }
    let checked = read(&store, "check", pin);
    let response = store.evidence_response().unwrap();
    let ban = checked["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["rule"] == "$external" && row["target"]["path"] == "other.py")
        .unwrap();
    let arguments = json!({"expectedBasis":pin,"rule":"$external",
        "target":ban["target"],"destination":ban["destination"]});
    let explained = response
        .architecture_response("explain", &arguments, &mut || Ok(()))
        .unwrap();
    response.finish(()).unwrap();
    for path in [
        "app.py",
        "tests/test_external.py",
        "pyproject.toml",
        "requirements.txt",
    ] {
        fs::write(root.path().join(path), "invalid live contents = [").unwrap();
    }
    fs::remove_file(root.path().join("other.py")).unwrap();
    assert_eq!(read(&store, "references", pin), before);
    assert_eq!(read(&store, "check", pin), checked);
    let response = store.evidence_response().unwrap();
    assert_eq!(
        response
            .architecture_response("explain", &arguments, &mut || Ok(()))
            .unwrap(),
        explained
    );
    response.finish(()).unwrap();
}

#[test]
fn python_external_rules_explain_and_browser_keep_native_metrics_separate() {
    let (_state, _root, store, pin) = python_fixture();
    let refs = read(&store, "references", pin);
    let checked = read(&store, "check", pin);
    let rows = checked["instances"].as_array().unwrap();
    let ban = rows
        .iter()
        .find(|row| row["rule"] == "$external" && row["target"]["path"] == "other.py")
        .unwrap();
    assert_eq!(ban["status"], "deviates");
    assert!(ban["sourceLayer"].is_null());
    assert_eq!(ban["destination"], package(&checked, "json")["identity"]);
    assert!(rows.iter().any(|row| {
        row["rule"] == "$layers"
            && row["target"]["path"] == "app.py"
            && row["destination"] == package(&checked, "json")["identity"]
            && row["destinationLayer"] == "standard"
            && row["status"] == "deviates"
    }));
    assert!(!rows.iter().any(|row| {
        row["target"]["path"] == "tests/test_external.py"
            || matches!(
                row["destination"]["package"].as_str(),
                Some("requests" | "flask")
            )
    }));
    assert!(checked["testOnly"].as_array().unwrap().iter().any(|row| {
        row["rule"] == "$external"
            && row["target"]["path"] == "tests/test_external.py"
            && row["destination"] == package(&checked, "sys")["identity"]
    }));
    let response = store.evidence_response().unwrap();
    let explanation = response.architecture_response("explain", &json!({
        "expectedBasis":pin,"rule":"$external","target":ban["target"],"destination":ban["destination"]
    }), &mut || Ok(())).unwrap();
    assert_eq!(
        package(&explanation, "json")["identity"],
        ban["destination"]
    );
    let options = MetricsOptions {
        pin: Some(pin),
        ..Default::default()
    };
    let view = response
        .architecture_view(&options, &mut || Ok(()))
        .unwrap();
    let metrics = response.metrics_at(&options, &mut || Ok(())).unwrap();
    for module in view["modules"].as_array().unwrap() {
        let identity = &module["identity"];
        assert!(identity.get("package").is_none());
        assert!(identity.get("sid").is_some());
        let metric = metrics
            .files
            .iter()
            .find(|file| {
                identity["sourceSetId"] == file.source_set_id
                    && identity["language"] == file.language
                    && identity["path"] == file.path
            })
            .unwrap();
        assert_eq!(module["metrics"], metric.metrics);
    }
    assert_eq!(
        without_layer(&view["references"]["externalPackages"]),
        refs["externalPackages"]
    );
    response.finish(()).unwrap();
}

const JS_MANIFEST: &str = r#"{
  "name":"fixture",
  "dependencies":{"widget":"^1.2.3","@scope/tool":"2.3.4","http":"1.2.3","blocked":"1.2.3","alias":"npm:elsewhere@1.2.3"},
  "devDependencies":{"widget":"~2.3.4","blocked":"workspace:*"}
}"#;
const JS_RULES: &str = r#"version = 1
[external_policy]
deny = [
  { language = "javascript", package = "widget", stdlib = false },
  { language = "javascript", package = "fs", stdlib = true },
  { language = "javascript", package = "absent", stdlib = false },
]
[layers.source]
order = 2
paths = ["app.js", "tests/**"]
[layers.packages]
order = 1
external = [{ language = "javascript", package = "widget", stdlib = false }]
[layers.standard]
shared = true
external = [{ language = "javascript", package = "fs", stdlib = true }]
"#;
fn javascript_fixture(manifest: &str) -> (tempfile::TempDir, tempfile::TempDir, Store, IndexPin) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for (path, text) in [
        ("package.json", manifest),
        (
            "app.js",
            "import { send } from 'widget/sub/path';\nexport { tool } from '@scope/tool/sub';\nimport 'fs';\nimport 'node:fs';\nimport 'fs/promises';\nimport 'node:fs/promises';\nimport 'path';\nimport 'node:path';\nimport 'http';\nimport 'http/fake';\nimport 'node:invented';\nimport 'blocked';\nimport 'alias';\nimport 'absent';\n",
        ),
        ("other.js", "import 'widget';\n"),
        ("tests/external.test.js", "import 'widget';\nimport 'fs';\n"),
        (".trellis/rules.toml", JS_RULES),
    ] {
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

#[test]
fn selected_javascript_packages_and_builtin_aliases_keep_captured_evidence_after_live_edits() {
    let (_state, root, store, pin) = javascript_fixture(JS_MANIFEST);
    let before = read(&store, "references", pin);
    assert_eq!(before["externalPackages"].as_array().unwrap().len(), 4);
    let widget = package(&before, "widget");
    assert_eq!(widget["identity"]["language"], "javascript");
    assert_eq!(widget["ecosystem"], "npm");
    let evidence = widget["evidence"].as_array().unwrap();
    assert_eq!(evidence.len(), 2);
    use sha2::{Digest, Sha256};
    let manifest_hash = hex::encode(Sha256::digest(JS_MANIFEST.as_bytes()));
    for entry in evidence {
        assert_eq!(entry["path"], "package.json");
        assert_eq!(entry["importRoot"], "widget");
        assert_eq!(entry["key"], "widget");
        assert_eq!(entry["sourceSetId"], widget["identity"]["sourceSetId"]);
        assert!(entry["nativeRevisionId"].is_string());
        assert_eq!(entry["dependencyHash"].as_str().unwrap().len(), 64);
        assert_eq!(entry["contentHash"], manifest_hash);
    }
    assert!(
        evidence
            .iter()
            .any(|entry| entry["section"] == "dependencies"
                && entry["versionConstraint"] == "^1.2.3")
    );
    assert!(evidence.iter().any(
        |entry| entry["section"] == "devDependencies" && entry["versionConstraint"] == "~2.3.4"
    ));
    let scoped = package(&before, "@scope/tool");
    assert_eq!(scoped["evidence"][0]["importRoot"], "@scope/tool");
    for name in ["fs", "path"] {
        let builtin = package(&before, name);
        assert_eq!(builtin["ecosystem"], "node");
        assert_eq!(builtin["identity"]["standardLibrary"], true);
        assert_eq!(
            builtin["evidence"],
            json!([{"kind":"standardLibraryCatalog", "catalog":"node-standard-library:v1", "entry":name}])
        );
    }
    let fs_edge = before["production"]
        .as_array()
        .unwrap()
        .iter()
        .find(|edge| {
            edge["source"]["path"] == "app.js"
                && edge["target"] == package(&before, "fs")["identity"]
        })
        .unwrap();
    assert_eq!(fs_edge["count"], 4);
    for unknown in ["http", "blocked", "alias", "absent", "node:invented"] {
        assert!(
            before["unresolved"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["site"]["spelling"].as_str().unwrap().contains(unknown))
        );
        assert!(
            !before["externalPackages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["identity"]["package"] == unknown)
        );
    }
    assert!(
        before["tests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["source"]["path"] == "tests/external.test.js"
                && edge["target"] == widget["identity"])
    );
    let checked = read(&store, "check", pin);
    let ban = checked["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["rule"] == "$external" && row["target"]["path"] == "other.js")
        .unwrap();
    let arguments = json!({"expectedBasis":pin,"rule":"$external","target":ban["target"],"destination":ban["destination"]});
    let response = store.evidence_response().unwrap();
    let explained = response
        .architecture_response("explain", &arguments, &mut || Ok(()))
        .unwrap();
    response.finish(()).unwrap();
    fs::write(root.path().join("package.json"), "invalid live manifest").unwrap();
    fs::write(root.path().join("app.js"), "invalid live source").unwrap();
    fs::remove_file(root.path().join("other.js")).unwrap();
    assert_eq!(read(&store, "references", pin), before);
    assert_eq!(read(&store, "check", pin), checked);
    fs::remove_file(root.path().join("package.json")).unwrap();
    assert_eq!(read(&store, "references", pin), before);
    assert_eq!(read(&store, "check", pin), checked);
    let response = store.evidence_response().unwrap();
    assert_eq!(
        response
            .architecture_response("explain", &arguments, &mut || Ok(()))
            .unwrap(),
        explained
    );
    response.finish(()).unwrap();
}

#[test]
fn javascript_external_policy_layers_explain_and_metrics_retain_only_native_modules() {
    let (_state, _root, store, pin) = javascript_fixture(JS_MANIFEST);
    let refs = read(&store, "references", pin);
    let checked = read(&store, "check", pin);
    let rows = checked["instances"].as_array().unwrap();
    let ban = rows
        .iter()
        .find(|row| row["rule"] == "$external" && row["target"]["path"] == "other.js")
        .unwrap();
    assert_eq!(ban["status"], "deviates");
    assert!(ban["sourceLayer"].is_null());
    assert_eq!(ban["destination"], package(&checked, "widget")["identity"]);
    assert!(rows.iter().any(|row| row["rule"] == "$layers"
        && row["target"]["path"] == "app.js"
        && row["destination"] == package(&checked, "widget")["identity"]
        && row["status"] == "deviates"));
    assert!(
        !rows
            .iter()
            .any(|row| row["target"]["path"] == "tests/external.test.js"
                || row["destination"]["package"] == "absent")
    );
    assert!(
        checked["testOnly"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["rule"] == "$external"
                && row["destination"] == package(&checked, "fs")["identity"])
    );
    let response = store.evidence_response().unwrap();
    let explanation = response.architecture_response("explain", &json!({"expectedBasis":pin,"rule":"$external","target":ban["target"],"destination":ban["destination"]}), &mut || Ok(())).unwrap();
    assert_eq!(
        package(&explanation, "widget")["identity"],
        ban["destination"]
    );
    let options = MetricsOptions {
        pin: Some(pin),
        ..Default::default()
    };
    let view = response
        .architecture_view(&options, &mut || Ok(()))
        .unwrap();
    let metrics = response.metrics_at(&options, &mut || Ok(())).unwrap();
    for module in view["modules"].as_array().unwrap() {
        let identity = &module["identity"];
        assert!(identity.get("package").is_none());
        let metric = metrics
            .files
            .iter()
            .find(|file| {
                identity["sourceSetId"] == file.source_set_id
                    && identity["language"] == file.language
                    && identity["path"] == file.path
            })
            .unwrap();
        assert_eq!(module["metrics"], metric.metrics);
    }
    assert_eq!(
        without_layer(&view["references"]["externalPackages"]),
        refs["externalPackages"]
    );
    response.finish(()).unwrap();
}

#[test]
fn malformed_captured_package_json_is_unknown_not_empty_dependency_proof() {
    let (_state, _root, store, pin) = javascript_fixture("{ invalid json");
    let refs = read(&store, "references", pin);
    assert!(
        !refs["externalPackages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["ecosystem"] == "npm")
    );
    assert!(
        refs["unresolved"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["site"]["spelling"].as_str().unwrap().contains("widget"))
    );
    assert_eq!(package(&refs, "fs")["ecosystem"], "node");
}
