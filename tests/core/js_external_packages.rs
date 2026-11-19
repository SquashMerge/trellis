//! Node builtins and the nearest captured `package.json` label bare JavaScript and
//! TypeScript imports (owner ruling 2026-11-18, #1088).
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

fn fixture(files: &[(&str, &str)]) -> (tempfile::TempDir, tempfile::TempDir, Store, IndexPin) {
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
fn references(store: &Store, pin: IndexPin) -> Value {
    let response = store.evidence_response().unwrap();
    let value = response
        .architecture_response("references", &json!({"expectedBasis":pin}), &mut || Ok(()))
        .unwrap();
    response.finish(value).unwrap()
}
/// The external destination of `source`'s import of `spelling`, as
/// `(ecosystem, package, standardLibrary)`, or `None` when it stays unresolved.
fn external(data: &Value, source: &str, spelling: &str) -> Option<(String, String, bool)> {
    let quoted = format!("'{spelling}'");
    let unresolved = data["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["source"]["path"] == source && row["site"]["spelling"] == quoted);
    let edges: Vec<_> = ["production", "tests"]
        .iter()
        .flat_map(|role| data[role].as_array().unwrap())
        .filter(|edge| {
            edge["source"]["path"] == source
                && edge["target"]["kind"] == "externalPackage"
                && edge["sites"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|site| site["spelling"] == quoted)
        })
        .collect();
    match (unresolved, &edges[..]) {
        (true, []) => None,
        (false, [edge]) => {
            let identity = &edge["target"];
            let descriptor = data["externalPackages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["identity"] == *identity)
                .unwrap();
            Some((
                descriptor["ecosystem"].as_str().unwrap().to_owned(),
                identity["package"].as_str().unwrap().to_owned(),
                identity["standardLibrary"].as_bool().unwrap(),
            ))
        }
        other => panic!("{source} {spelling}: {other:?}\n{data}"),
    }
}
fn node(package: &str) -> Option<(String, String, bool)> {
    Some(("node".into(), package.into(), true))
}
fn npm(package: &str) -> Option<(String, String, bool)> {
    Some(("npm".into(), package.into(), false))
}
fn imports(specifiers: &[&str]) -> String {
    specifiers
        .iter()
        .map(|specifier| format!("import '{specifier}';\n"))
        .collect()
}

#[test]
fn every_builtin_spelling_labels_its_node_root() {
    let source = imports(&[
        "crypto",
        "node:crypto",
        "child_process",
        "node:worker_threads",
        "path/posix",
        "node:util/types",
        "stream/web",
        "_http_agent",
        "node:test",
        "node:test/reporters",
        "node:sqlite",
        "node:sea",
        "test",
        "sqlite",
        "http/fake",
        "node:invented",
        "node:fs/fake",
    ]);
    let (_state, _root, store, pin) = fixture(&[
        ("package.json", r#"{"name":"fixture"}"#),
        ("app.mjs", &source),
        ("typed.ts", "import 'node:crypto';\nimport 'os';\n"),
    ]);
    let data = references(&store, pin);
    for (spelling, root) in [
        ("crypto", "crypto"),
        ("node:crypto", "crypto"),
        ("child_process", "child_process"),
        ("node:worker_threads", "worker_threads"),
        ("path/posix", "path"),
        ("node:util/types", "util"),
        ("stream/web", "stream"),
        ("_http_agent", "_http_agent"),
        ("node:test", "test"),
        ("node:test/reporters", "test"),
        ("node:sqlite", "sqlite"),
        ("node:sea", "sea"),
    ] {
        assert_eq!(
            external(&data, "app.mjs", spelling),
            node(root),
            "{spelling}"
        );
    }
    // Scheme-only builtins name npm packages bare; nothing here declares them.
    for spelling in [
        "test",
        "sqlite",
        "http/fake",
        "node:invented",
        "node:fs/fake",
    ] {
        assert_eq!(external(&data, "app.mjs", spelling), None, "{spelling}");
    }
    let crypto = data["externalPackages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| {
            item["identity"]["package"] == "crypto" && item["identity"]["language"] == "javascript"
        })
        .unwrap();
    assert_eq!(
        crypto["evidence"],
        json!([{"kind":"standardLibraryCatalog","catalog":"node-standard-library:v2","entry":"crypto"}])
    );
    assert_eq!(external(&data, "typed.ts", "os"), node("os"));
    let typed = data["production"]
        .as_array()
        .unwrap()
        .iter()
        .find(|edge| edge["source"]["path"] == "typed.ts" && edge["target"]["package"] == "crypto")
        .unwrap();
    assert_eq!(typed["target"]["language"], "typescript");
}

#[test]
fn bare_imports_resolve_against_the_nearest_package_json_never_node_modules() {
    let (_state, _root, store, pin) = fixture(&[
        (
            "package.json",
            r#"{"name":"root","dependencies":{"root-only":"1.0.0","shared":"1.0.0","sqlite":"5.1.1"}}"#,
        ),
        (
            "app.js",
            &imports(&[
                "shared",
                "root-only",
                "esbuild",
                "sqlite",
                "unknown-package",
            ]),
        ),
        (
            "tools/bundle/package.json",
            r#"{"name":"bundle","devDependencies":{"esbuild":"^0.20.2","shared":"2.0.0"}}"#,
        ),
        (
            "tools/bundle/build.mjs",
            &imports(&["esbuild", "shared", "root-only", "node:fs"]),
        ),
        ("tools/bundle/lib/deep.js", &imports(&["esbuild"])),
        ("tools/loose.js", &imports(&["shared", "esbuild"])),
        ("tools/broken/package.json", "{ invalid"),
        ("tools/broken/x.js", &imports(&["shared"])),
        // An installed copy is never read: it neither declares nor is resolved.
        (
            "node_modules/esbuild/package.json",
            r#"{"name":"esbuild","version":"0.20.2"}"#,
        ),
        ("node_modules/esbuild/index.js", "export const build = 1;\n"),
    ]);
    let data = references(&store, pin);
    assert_eq!(external(&data, "app.js", "shared"), npm("shared"));
    assert_eq!(external(&data, "app.js", "root-only"), npm("root-only"));
    assert_eq!(external(&data, "app.js", "sqlite"), npm("sqlite"));
    assert_eq!(external(&data, "app.js", "esbuild"), None);
    assert_eq!(external(&data, "app.js", "unknown-package"), None);
    for source in ["tools/bundle/build.mjs", "tools/bundle/lib/deep.js"] {
        assert_eq!(
            external(&data, source, "esbuild"),
            npm("esbuild"),
            "{source}"
        );
    }
    assert_eq!(
        external(&data, "tools/bundle/build.mjs", "shared"),
        npm("shared")
    );
    // The nearest manifest decides alone, never deferring to the root's declarations.
    assert_eq!(external(&data, "tools/bundle/build.mjs", "root-only"), None);
    assert_eq!(
        external(&data, "tools/bundle/build.mjs", "node:fs"),
        node("fs")
    );
    assert_eq!(external(&data, "tools/loose.js", "shared"), npm("shared"));
    assert_eq!(external(&data, "tools/loose.js", "esbuild"), None);
    assert_eq!(external(&data, "tools/broken/x.js", "shared"), None);
    let esbuild = data["externalPackages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["identity"]["package"] == "esbuild")
        .unwrap();
    assert_eq!(esbuild["evidence"].as_array().unwrap().len(), 1);
    assert_eq!(esbuild["evidence"][0]["path"], "tools/bundle/package.json");
    assert_eq!(esbuild["evidence"][0]["section"], "devDependencies");
    assert_eq!(esbuild["evidence"][0]["versionConstraint"], "^0.20.2");
    // One identity across two manifests carries the evidence of both.
    let shared = data["externalPackages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["identity"]["package"] == "shared")
        .unwrap();
    let mut paths: Vec<_> = shared["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|evidence| {
            format!(
                "{}@{}",
                evidence["path"].as_str().unwrap(),
                evidence["versionConstraint"].as_str().unwrap()
            )
        })
        .collect();
    paths.sort();
    assert_eq!(
        paths,
        ["package.json@1.0.0", "tools/bundle/package.json@2.0.0"]
    );
    assert!(!data["modules"].as_array().unwrap().iter().any(|module| {
        module["identity"]["path"]
            .as_str()
            .is_some_and(|path| path.starts_with("node_modules/"))
    }));
}

#[test]
fn workspace_roots_and_members_keep_their_773_rules() {
    const DEP: &str = r#""dependencies":{"widget":"1.0.0"}"#;
    let root = format!(r#"{{"name":"acme","private":true,"workspaces":["packages/*"],{DEP}}}"#);
    let ui = format!(r#"{{"name":"@acme/ui",{DEP}}}"#);
    let tool = format!(r#"{{"name":"tool",{DEP}}}"#);
    let (_state, _root, store, pin) = fixture(&[
        ("package.json", &root),
        ("index.js", &imports(&["widget"])),
        ("packages/ui/package.json", &ui),
        ("packages/ui/index.js", &imports(&["widget", "node:fs"])),
        ("packages/web/package.json", r#"{"name":"web"}"#),
        ("packages/web/main.js", &imports(&["@acme/ui"])),
        ("tools/package.json", &tool),
        ("tools/x.js", &imports(&["widget"])),
    ]);
    let data = references(&store, pin);
    assert_eq!(external(&data, "index.js", "widget"), None);
    assert_eq!(external(&data, "packages/ui/index.js", "widget"), None);
    assert_eq!(
        external(&data, "packages/ui/index.js", "node:fs"),
        node("fs")
    );
    // A nested package the member patterns do not name resolves against its manifest.
    assert_eq!(external(&data, "tools/x.js", "widget"), npm("widget"));
    // A member name still resolves in-repo, never to a package.
    assert!(data["production"].as_array().unwrap().iter().any(|edge| {
        edge["source"]["path"] == "packages/web/main.js"
            && edge["target"]["path"] == "packages/ui/index.js"
    }));
    assert!(
        !data["externalPackages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["identity"]["package"] == "@acme/ui")
    );
}
