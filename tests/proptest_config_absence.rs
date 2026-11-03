//! Captured config histories against a small declarative reference model.
//! Run with `--features test-support` and optionally `PROPTEST_CASES`.
#![cfg(feature = "test-support")]

use proptest::prelude::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::Path,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    indexer::{IndexOptions, index_workspace_bundle},
    store::Store,
};

const CAP: usize = 4096;

fn write(root: &Path, path: &str, bytes: &[u8]) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn observe(store: &Store) -> Value {
    store
        .evidence_response()
        .unwrap()
        .architecture_response("references", &json!({"limit": 20}), &mut || Ok(()))
        .unwrap()
}

// The oracle describes only the deliberately generated single bare import, not
// Cargo/npm parsing. Unknown includes a genuinely absent or unsupported binding;
// it must never be represented as a proved dependency absence.
fn check(
    data: &Value,
    source: &str,
    spelling: &str,
    external: Option<(&str, &str, &str, &str, &str)>,
) {
    assert!(
        data["limitations"].as_array().unwrap().iter().any(|v| v
            .as_str()
            .unwrap()
            .contains("missing sites do not prove absence")),
        "{data}"
    );
    let packages = data["externalPackages"].as_array().unwrap();
    let edges = data["production"].as_array().unwrap();
    for edge in edges {
        assert_eq!(edge["omittedSites"], 0, "{data}");
    }
    assert!(data["tests"].as_array().unwrap().is_empty(), "{data}");
    if let Some((name, path, hash, section, version)) = external {
        assert_eq!(packages.len(), 1, "{data}");
        assert_eq!(edges.len(), 1, "{data}");
        assert_eq!(data["counts"]["mappedSites"], 1, "{data}");
        assert_eq!(data["counts"]["unresolvedSites"], 0, "{data}");
        assert_eq!(edges[0]["source"]["path"], source);
        assert_eq!(edges[0]["count"], 1);
        assert_eq!(edges[0]["sites"].as_array().unwrap().len(), 1);
        assert_eq!(edges[0]["sites"][0]["spelling"], spelling);
        assert_eq!(edges[0]["target"], packages[0]["identity"]);
        let language = if path == "Cargo.toml" {
            "rust"
        } else {
            "javascript"
        };
        assert_eq!(packages[0]["identity"]["language"], language);
        assert_eq!(packages[0]["identity"]["kind"], "externalPackage");
        assert_eq!(
            packages[0]["ecosystem"],
            if language == "rust" { "cargo" } else { "npm" }
        );
        assert_eq!(packages[0]["identity"]["package"], name);
        assert_eq!(packages[0]["identity"]["standardLibrary"], false);
        let evidence = packages[0]["evidence"].as_array().unwrap();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0]["kind"], "manifest");
        assert_eq!(evidence[0]["path"], path);
        assert_eq!(evidence[0]["contentHash"], hash);
        assert_eq!(evidence[0]["section"], section);
        assert_eq!(evidence[0]["key"], name);
        assert_eq!(evidence[0]["importRoot"], name);
        assert_eq!(evidence[0]["versionConstraint"], version);
        assert_eq!(
            evidence[0]["sourceSetId"],
            packages[0]["identity"]["sourceSetId"]
        );
    } else {
        assert!(packages.is_empty(), "{data}");
        assert!(edges.is_empty(), "{data}");
        assert_eq!(data["counts"]["mappedSites"], 0, "{data}");
        assert_eq!(data["counts"]["unresolvedSites"], 1, "{data}");
        assert_eq!(data["unresolved"].as_array().unwrap().len(), 1, "{data}");
        assert_eq!(
            data["unresolved"][0]["site"]["spelling"], spelling,
            "{data}"
        );
    }
}

fn history(rust: bool, section_choice: usize, name_choice: usize, nested: bool, reverse: bool) {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let name = ["widget", "gizmo", "quux"][name_choice];
    let (source, spelling, manifest, section, version, good, nested_path) = if rust {
        let section =
            ["dependencies", "dev-dependencies", "build-dependencies"][section_choice % 3];
        (
            "src/lib.rs",
            format!("use {name}::Thing;"),
            "Cargo.toml",
            section,
            "1.2.3",
            format!(
                "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n[{section}]\n{name}='1.2.3'\n"
            ),
            "nested/Cargo.toml",
        )
    } else {
        let section = [
            "dependencies",
            "devDependencies",
            "optionalDependencies",
            "peerDependencies",
        ][section_choice];
        (
            if nested {
                "nested/app.js"
            } else {
                "src/app.js"
            },
            format!("import '{name}';"),
            "package.json",
            section,
            "^1.2.3",
            format!("{{\"name\":\"app\",\"{section}\":{{\"{name}\":\"^1.2.3\"}}}}\n"),
            "nested/package.json",
        )
    };
    let prefix = if reverse {
        "// generated fixture\n"
    } else {
        ""
    };
    write(
        root.path(),
        source,
        format!("{prefix}{spelling}\n").as_bytes(),
    );
    let site_spelling = if rust {
        format!("{name}::Thing")
    } else {
        format!("'{name}'")
    };
    if rust {
        write(root.path(), "nested/src/lib.rs", b"pub fn other() {}\n");
    }
    if nested {
        let text = if rust {
            "[package]\nname='other'\nversion='0.1.0'\n"
        } else {
            "{\"name\":\"other\"}"
        };
        write(root.path(), nested_path, text.as_bytes());
    }
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let leader = store.publication_for_tests().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let mut options = IndexOptions::new(root.path().to_owned());
    options.max_config_bytes = CAP as u64;
    let publish = || {
        let (graph, native, capture) =
            index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
        store
            .publish_native(
                &graph,
                &capture,
                &native,
                &leader,
                store.index_baseline().unwrap(),
                &cancel,
            )
            .unwrap();
        let diagnostics: Vec<_> = store
            .status()
            .unwrap()
            .diagnostics
            .into_iter()
            .filter(|d| d.code == "config-input-oversized")
            .map(|d| d.path.unwrap())
            .collect();
        (observe(&store), diagnostics)
    };
    let hash = hex::encode(Sha256::digest(good.as_bytes()));
    let expected = Some((name, manifest, hash.as_str(), section, version));
    write(root.path(), manifest, good.as_bytes());
    let (initial, d) = publish();
    assert!(d.is_empty(), "{d:?}");
    check(&initial, source, &site_spelling, expected);

    // Every generated case exercises all states; order varies and restoration is observed.
    let modes = if reverse { [2, 1, 0] } else { [0, 1, 2] };
    for mode in modes {
        match mode {
            0 => {
                let mut large = good.as_bytes().to_vec();
                large.resize(CAP + 1, b' ');
                write(root.path(), manifest, &large);
            }
            1 => {
                let broken = if rust {
                    "[package]\nname='app'\n[dependencies\nwidget='1.2.3'\n"
                } else if name_choice == 0 {
                    "{\"dependencies\":{\"widget\":\"1.2.3\""
                } else {
                    "{\"dependencies\":{},\"dependencies\":{}}"
                };
                write(root.path(), manifest, broken.as_bytes());
            }
            _ => fs::remove_file(root.path().join(manifest)).unwrap(),
        }
        let (data, diagnostics) = publish();
        assert_eq!(
            diagnostics,
            if mode == 0 {
                vec![manifest.to_owned()]
            } else {
                vec![]
            },
            "mode={mode}"
        );
        check(&data, source, &site_spelling, None);
        write(root.path(), manifest, good.as_bytes());
        let (restored, diagnostics) = publish();
        assert!(diagnostics.is_empty());
        check(&restored, source, &site_spelling, expected);
    }
    // A valid different declaration and an unsupported declaration are unknown,
    // not positive evidence about this import. Both are followed by recovery.
    let changed = good.replace(name, "otherdep");
    let unproved = if rust {
        good.replace("'1.2.3'", "{ path = 'vendor/local', version = '1.2.3' }")
    } else {
        good.replace("^1.2.3", "file:../local")
    };
    for text in [&changed, &unproved] {
        write(root.path(), manifest, text.as_bytes());
        let (data, diagnostics) = publish();
        assert!(diagnostics.is_empty());
        check(&data, source, &site_spelling, None);
    }
    write(root.path(), manifest, good.as_bytes());
    let (restored, diagnostics) = publish();
    assert!(diagnostics.is_empty());
    check(&restored, source, &site_spelling, expected);
    // Exactly at the test cap is still read, with the hash of the full bytes.
    let mut at_cap = good.as_bytes().to_vec();
    at_cap.resize(CAP, b' ');
    write(root.path(), manifest, &at_cap);
    let cap_hash = hex::encode(Sha256::digest(&at_cap));
    let (data, diagnostics) = publish();
    assert!(diagnostics.is_empty());
    check(
        &data,
        source,
        &site_spelling,
        Some((name, manifest, &cap_hash, section, version)),
    );
    write(root.path(), manifest, good.as_bytes());
    if nested {
        let nested_good = if rust {
            "[package]\nname='other'\nversion='0.1.0'\n"
        } else {
            "{\"name\":\"other\"}"
        };
        for mode in 0..3 {
            match mode {
                0 => {
                    let mut text = nested_good.as_bytes().to_vec();
                    text.resize(CAP + 1, b' ');
                    write(root.path(), nested_path, &text);
                }
                1 => write(
                    root.path(),
                    nested_path,
                    if rust { b"[package\n" } else { b"{\"name\":" },
                ),
                _ => fs::remove_file(root.path().join(nested_path)).unwrap(),
            }
            let (data, diagnostics) = publish();
            assert_eq!(
                diagnostics,
                if mode == 0 {
                    vec![nested_path.to_owned()]
                } else {
                    vec![]
                }
            );
            // References use only root dependency declarations; nested manifests
            // are captured for other consumers and do not become npm/Cargo proof.
            check(&data, source, &site_spelling, expected);
            write(root.path(), nested_path, nested_good.as_bytes());
        }
    }
}

// Target ownership is a different consumer of the *selected* retained config.
// An oversized nested Cargo manifest may name a cross-directory target, so it
// makes even the root package's ownership unknown. Malformed is only local.
fn nested_ownership_history(reverse: bool, padding: usize) {
    use trellis::scip_raw::cargo_targets::CargoTargetsBuilder;
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let outer = "[package]\nname='outer'\nversion='1.0.0'\n";
    let inner = format!(
        "[package]\nname='inner'\nversion='2.0.0'\n#{}\n",
        "x".repeat(padding)
    );
    write(root.path(), "Cargo.toml", outer.as_bytes());
    write(root.path(), "src/lib.rs", b"pub fn outer() {}\n");
    write(root.path(), "nested/src/lib.rs", b"pub fn inner() {}\n");
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let leader = store.publication_for_tests().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let mut options = IndexOptions::new(root.path().to_owned());
    options.max_config_bytes = CAP as u64;
    let publish = |outer_expected: bool, inner_expected: bool, skipped: bool| {
        let (graph, native, capture) =
            index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
        let pin = store
            .publish_native(
                &graph,
                &capture,
                &native,
                &leader,
                store.index_baseline().unwrap(),
                &cancel,
            )
            .unwrap();
        let diagnostics: Vec<_> = store
            .status()
            .unwrap()
            .diagnostics
            .into_iter()
            .filter(|d| d.code == "config-input-oversized")
            .map(|d| d.path.unwrap())
            .collect();
        assert_eq!(
            diagnostics,
            if skipped {
                vec!["nested/Cargo.toml"]
            } else {
                vec![]
            }
        );
        let (_, config, _) = store.retained_native_components_at(pin).unwrap();
        let (artifact, sources) = store.retained_native_snapshot_at(pin).unwrap();
        if skipped {
            let nested = config
                .iter()
                .find(|v| v["path"] == "nested/Cargo.toml")
                .unwrap();
            assert_eq!(nested["type"], "present");
            assert_eq!(
                hex::decode(nested["bytes"].as_str().unwrap()).unwrap(),
                b"\x01 Trellis did not read this file: it is larger than the config input cap.\n"
            );
        }
        let mut builder = CargoTargetsBuilder::from_native_config(&config);
        for document in &artifact.revision.documents {
            if document.key.language == "rust" {
                builder.add_document(&document.key.path, &sources[&document.key]);
            }
        }
        let targets = builder.finish();
        let outer = targets.target_of("src/lib.rs");
        let inner = targets.target_of("nested/src/lib.rs");
        assert_eq!(
            outer.map(|t| t.package.as_str()),
            outer_expected.then_some("outer")
        );
        assert_eq!(
            inner.map(|t| t.package.as_str()),
            inner_expected.then_some("inner")
        );
    };
    write(root.path(), "nested/Cargo.toml", inner.as_bytes());
    publish(true, true, false);
    for mode in if reverse { [2, 1, 0] } else { [0, 1, 2] } {
        match mode {
            0 => {
                let mut text = inner.as_bytes().to_vec();
                text.resize(CAP + 1, b' ');
                write(root.path(), "nested/Cargo.toml", &text);
            }
            1 => write(
                root.path(),
                "nested/Cargo.toml",
                b"[package\nname='broken'\n",
            ),
            _ => fs::remove_file(root.path().join("nested/Cargo.toml")).unwrap(),
        }
        publish(mode != 0, false, mode == 0);
        write(root.path(), "nested/Cargo.toml", inner.as_bytes());
        publish(true, true, false);
    }
}

#[test]
fn python_and_java_manifest_coordinates_do_not_prove_import_roots() {
    for (manifest, good, source, text) in [
        (
            "pyproject.toml",
            "[project]\nname='app'\nversion='1.0.0'\ndependencies=['widget==1.2.3']\n",
            "app.py",
            "import widget\n",
        ),
        (
            "pom.xml",
            "<project><dependencies><dependency><groupId>org.widget</groupId><artifactId>widget</artifactId><version>1.2.3</version></dependency></dependencies></project>",
            "app/Use.java",
            "package app; import org.widget.Thing; class Use {}\n",
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        write(root.path(), source, text.as_bytes());
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        let leader = store.publication_for_tests().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut options = IndexOptions::new(root.path().to_owned());
        options.max_config_bytes = CAP as u64;
        for mode in 0..4 {
            match mode {
                0 => write(root.path(), manifest, good.as_bytes()),
                1 => {
                    let mut large = good.as_bytes().to_vec();
                    large.resize(CAP + 1, b' ');
                    write(root.path(), manifest, &large);
                }
                2 => write(
                    root.path(),
                    manifest,
                    if manifest == "pom.xml" {
                        b"<project>"
                    } else {
                        b"[project\n"
                    },
                ),
                _ => fs::remove_file(root.path().join(manifest)).unwrap(),
            }
            let (graph, native, capture) =
                index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
            store
                .publish_native(
                    &graph,
                    &capture,
                    &native,
                    &leader,
                    store.index_baseline().unwrap(),
                    &cancel,
                )
                .unwrap();
            let diagnostics: Vec<_> = store
                .status()
                .unwrap()
                .diagnostics
                .into_iter()
                .filter(|d| d.code == "config-input-oversized")
                .map(|d| d.path.unwrap())
                .collect();
            assert_eq!(diagnostics, if mode == 1 { vec![manifest] } else { vec![] });
            let data = observe(&store);
            assert!(
                data["externalPackages"].as_array().unwrap().is_empty(),
                "{data}"
            );
            assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
            assert_eq!(data["counts"]["mappedSites"], 0, "{data}");
            assert_eq!(data["counts"]["unresolvedSites"], 1, "{data}");
            assert_eq!(data["unresolved"].as_array().unwrap().len(), 1, "{data}");
        }
    }
}

#[test]
fn local_reference_survives_skipped_root_manifest_without_inventing_external() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "src/app.js",
        b"import './local.js';\nimport 'widget';\n",
    );
    write(root.path(), "src/local.js", b"export const value = 1;\n");
    let manifest = b"{\"dependencies\":{\"widget\":\"1.2.3\"}}";
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let leader = store.publication_for_tests().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let mut options = IndexOptions::new(root.path().to_owned());
    options.max_config_bytes = CAP as u64;
    for skipped in [false, true, false] {
        let mut contents = manifest.to_vec();
        if skipped {
            contents.resize(CAP + 1, b' ');
        }
        write(root.path(), "package.json", &contents);
        let (graph, native, capture) =
            index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
        store
            .publish_native(
                &graph,
                &capture,
                &native,
                &leader,
                store.index_baseline().unwrap(),
                &cancel,
            )
            .unwrap();
        let diagnostics: Vec<_> = store
            .status()
            .unwrap()
            .diagnostics
            .into_iter()
            .filter(|d| d.code == "config-input-oversized")
            .map(|d| d.path.unwrap())
            .collect();
        assert_eq!(
            diagnostics,
            if skipped {
                vec!["package.json"]
            } else {
                vec![]
            }
        );
        let data = observe(&store);
        let edges = data["production"].as_array().unwrap();
        assert_eq!(edges.len(), if skipped { 1 } else { 2 }, "{data}");
        let local = edges
            .iter()
            .find(|e| e["target"]["path"] == "src/local.js")
            .unwrap();
        assert_eq!(local["source"]["path"], "src/app.js");
        assert_eq!(local["count"], 1);
        assert_eq!(local["omittedSites"], 0);
        assert_eq!(local["sites"].as_array().unwrap().len(), 1);
        assert_eq!(local["sites"][0]["spelling"], "'./local.js'");
        assert_eq!(
            data["externalPackages"].as_array().unwrap().len(),
            usize::from(!skipped)
        );
        assert_eq!(data["counts"]["mappedSites"], if skipped { 1 } else { 2 });
        assert_eq!(data["counts"]["unresolvedSites"], usize::from(skipped));
        if skipped {
            assert_eq!(data["unresolved"][0]["site"]["spelling"], "'widget'");
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(24),
        .. ProptestConfig::default()
    })]
    #[test]
    fn captured_manifest_history_never_invents_external(
        section in 0usize..4, name in 0usize..3, reverse in any::<bool>(),
    ) {
        // Deliberately cross both languages and directory scopes on every case.
        for rust in [false, true] {
            for nested in [false, true] {
                history(rust, section, name, nested, reverse);
            }
        }
    }

    #[test]
    fn retained_nested_cargo_ownership_is_unknown_only_as_required(
        reverse in any::<bool>(), padding in 0usize..40,
    ) {
        nested_ownership_history(reverse, padding);
    }
}
