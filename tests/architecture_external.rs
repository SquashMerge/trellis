#![cfg(feature = "test-support")]
use serde_json::{Value, json};
use std::{
    fs,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    indexer::{IndexOptions, index_workspace_bundle},
    model::{CancelFlag, IndexPin},
    references::external::{ManifestInput, RustPackages},
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
        view["references"]["externalPackages"],
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

fn manifest(text: &str) -> ManifestInput {
    ManifestInput {
        source_set_id: "source-set:test".into(),
        native_revision_id: "revision:test".into(),
        dependency_hash: "dependency-hash".into(),
        path: "Cargo.toml".into(),
        content_hash: Some("content-hash".into()),
        bytes: Some(text.as_bytes().to_vec()),
    }
}
#[test]
fn normalized_import_collisions_and_unproved_ownership_never_guess_packages() {
    let input = manifest(
        "[package]
name='fixture'
version='1.0.0'
[dependencies]
wire-core='1'
wire_core='1'
plain={package='literal-upstream',version='2'}
local={path='vendor/local',version='1'}
[build-dependencies]
build_wire={package='build-upstream',version='3'}
[target.'cfg(unix)'.dependencies]
target_wire={package='target-upstream',version='4'}",
    );
    let packages = RustPackages::new(&[input], &mut 0, &mut || Ok(())).unwrap();
    assert!(
        packages
            .resolve("source-set:test", "wire_core", true)
            .is_none()
    );
    assert!(
        packages
            .resolve("source-set:test", "plain", false)
            .is_none()
    );
    assert!(packages.resolve("source-set:test", "local", true).is_none());
    for (root, name) in [
        ("plain", "literal-upstream"),
        ("build_wire", "build-upstream"),
        ("target_wire", "target-upstream"),
    ] {
        let package = packages.resolve("source-set:test", root, true).unwrap();
        assert_eq!(package.identity.package, name);
        assert!(!package.identity.standard_library);
    }
    let error = RustPackages::new(&[manifest(MANIFEST)], &mut 0, &mut || {
        anyhow::bail!("cancelled")
    })
    .err()
    .unwrap();
    assert_eq!(error.to_string(), "cancelled");
}

#[test]
fn repeated_identical_binding_merges_evidence_and_cargo_roots_are_checked_separately() {
    let input = manifest(
        "[package]\nname='fixture'\nversion='1.0.0'\nautobins=false\n[dependencies]\nwire={package='upstream-wire',version='1'}\n[dev-dependencies]\nwire={package='upstream-wire',version='2'}\n[target.'cfg(unix)'.dependencies]\nwire={package='upstream-wire',version='3'}",
    );
    let packages = RustPackages::new(&[input], &mut 0, &mut || Ok(())).unwrap();
    assert!(packages.root_allowed("source-set:test", "src/lib.rs"));
    assert!(!packages.root_allowed("source-set:test", "src/main.rs"));
    let package = packages.resolve("source-set:test", "wire", true).unwrap();
    assert_eq!(package.evidence.len(), 3);
    assert_eq!(package.identity.package, "upstream-wire");
    let input = manifest(
        "[package]\nname='fixture'\nversion='1.0.0'\n[dependencies]\nwire={package='one',version='1'}\n[dev-dependencies]\nwire={package='two',version='1'}",
    );
    let packages = RustPackages::new(&[input], &mut 0, &mut || Ok(())).unwrap();
    assert!(packages.resolve("source-set:test", "wire", true).is_none());
}
