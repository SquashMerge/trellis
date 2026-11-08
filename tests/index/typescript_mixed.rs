//! The mixed TypeScript/JavaScript fixture repository (`tests/fixtures/typescript-mixed`).
//! Each function covers one assertion group; later slices append their own.
#![cfg(feature = "test-support")]

use protobuf::EnumOrUnknown;
use std::{
    fs,
    path::Path,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    indexer::{IndexOptions, index_workspace_bundle, index_workspace_with_native},
    native_evidence::{Artifact, DeclarationRole},
    semantic_batch::testing,
    store::Store,
};

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Every file below `dir`, as a `/`-separated path relative to `root`.
fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk(root, &path, out);
        } else {
            out.push(
                path.strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned(),
            );
        }
    }
}

fn fixture_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/typescript-mixed"),
        root.path(),
    );
    root
}

fn native(root: &Path) -> Artifact {
    let identity = trellis::store::topology::WorkspaceIdentity::discover(Some(root), root).unwrap();
    index_workspace_with_native(
        &IndexOptions::new(root.to_owned()),
        &identity.record_id,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap()
    .1
}

/// A one-document SCIP index for `path`, which the scip-typescript pin covers as JavaScript.
fn scip_artifact(root: &Path, path: &str) -> Vec<u8> {
    use protobuf::Message;
    let mut index = scip::types::Index::new();
    let mut metadata = scip::types::Metadata::new();
    metadata.project_root = format!("file://{}", root.display());
    metadata.text_document_encoding = EnumOrUnknown::from_i32(1);
    let mut tool = scip::types::ToolInfo::new();
    tool.name = "scip-typescript".into();
    tool.version = "0.4.0".into();
    metadata.tool_info = Some(tool).into();
    index.metadata = Some(metadata).into();
    let mut document = scip::types::Document::new();
    document.relative_path = path.into();
    document.language = "javascript".into();
    let mut occurrence = scip::types::Occurrence::new();
    occurrence.range = vec![2, 16, 22];
    occurrence.symbol = "pkg d00000.".into();
    occurrence.symbol_roles = 1;
    document.occurrences.push(occurrence);
    let mut info = scip::types::SymbolInformation::new();
    info.symbol = "pkg d00000.".into();
    info.kind = EnumOrUnknown::from_i32(26);
    info.display_name = "helper".into();
    document.symbols.push(info);
    index.documents.push(document);
    index.write_to_bytes().unwrap()
}

#[test]
fn typescript_mixed_detection_and_capture() {
    let workspace = fixture_root();
    let root = fs::canonicalize(workspace.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let state = tempfile::tempdir().unwrap();
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(state.path(), &root));
    let options = IndexOptions::new(root.clone());
    let (graph, artifact, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();

    // Detection: TypeScript by extension (a declaration file included), JavaScript as before,
    // and `.jsx` is not a source.
    let language_of = |path: &str| {
        capture
            .files
            .iter()
            .find(|file| file.path == path)
            .map(|file| file.language.as_str())
    };
    let mut fixture_files = Vec::new();
    walk(&root, &root, &mut fixture_files);
    assert!(fixture_files.iter().any(|path| path.ends_with(".d.ts")));
    for path in &fixture_files {
        let expected = match path.rsplit('.').next().unwrap() {
            "ts" | "tsx" | "mts" | "cts" => Some("typescript"),
            "js" => Some("javascript"),
            _ => None,
        };
        assert_eq!(language_of(path), expected, "{path}");
    }
    assert_eq!(language_of("src/app/ignored.jsx"), None);
    assert!(
        artifact
            .revision
            .documents
            .iter()
            .all(|document| document.key.path != "src/app/ignored.jsx")
    );
    assert!(artifact.revision.documents.iter().all(|document| {
        capture
            .files
            .iter()
            .any(|file| file.path == document.key.path && file.language == document.key.language)
    }));

    // A nested package root of a TypeScript source is a captured input (D4): changing
    // pkg/package.json changes the revision's config hash.
    let other = fixture_root();
    let other_root = fs::canonicalize(other.path()).unwrap();
    let before = native(&other_root).revision.config_hash;
    fs::write(
        other_root.join("pkg/package.json"),
        "{\"name\":\"ts-mixed-pkg\",\"version\":\"2.0.0\"}\n",
    )
    .unwrap();
    assert_ne!(native(&other_root).revision.config_hash, before);

    // Roles follow the path convention.
    let functions = |path: &str| -> Vec<DeclarationRole> {
        artifact
            .declarations
            .iter()
            .filter(|d| d.document.path == path && matches!(d.kind.as_str(), "function" | "method"))
            .map(|d| d.role)
            .collect()
    };
    for path in [
        "src/app/model.test.ts",
        "__tests__/view.test.tsx",
        "tests/types.d.ts",
    ] {
        assert!(
            functions(path)
                .iter()
                .all(|role| *role == DeclarationRole::Test),
            "{path}"
        );
    }
    for path in ["src/app/model.test.ts", "__tests__/view.test.tsx"] {
        assert!(!functions(path).is_empty(), "{path} declares functions");
    }
    let model = functions("src/app/model.ts");
    assert!(!model.is_empty());
    assert!(
        model
            .iter()
            .all(|role| *role == DeclarationRole::Production)
    );

    // Publish, then reopen and read every document through the selected-read path.
    let leader = store.publication_for_tests().unwrap();
    let pin = store
        .publish_native(
            &graph,
            &capture,
            &artifact,
            &leader,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    drop(leader);
    drop(store);
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(state.path(), &root));
    for document in &artifact.revision.documents {
        let (stored, _) = store
            .native_source_at(pin, &document.key)
            .unwrap()
            .unwrap_or_else(|| panic!("{} is stored", document.key.path));
        assert_eq!(stored, *document);
        let (_, source) = store
            .source_at(&document.key.path, Some(pin))
            .unwrap()
            .unwrap();
        assert_eq!(
            source.text,
            capture
                .files
                .iter()
                .find(|file| file.path == document.key.path)
                .unwrap()
                .text
        );
    }

    // A scip-typescript batch whose coverage is the JavaScript document keeps TypeScript
    // source rows in contract order (javascript before typescript) and is admitted.
    let artifact_bytes = scip_artifact(&root, "src/app/legacy.js");
    let (descriptor, pages) =
        testing::single_artifact_batch(&capture, "scip-typescript", "javascript", &artifact_bytes)
            .unwrap();
    let pages: Vec<&[u8]> = pages.iter().map(Vec::as_slice).collect();
    let inventories = testing::admit_in_memory_from_capture(
        &capture,
        &descriptor,
        &pages,
        &[artifact_bytes.as_slice()],
        &cancel,
    )
    .unwrap();
    assert_eq!(inventories.len(), 1);
}

/// Declarations of `path` per kind, in kind order.
fn kind_counts(artifact: &Artifact, path: &str) -> Vec<(String, usize)> {
    let mut counts = std::collections::BTreeMap::new();
    for declaration in artifact
        .declarations
        .iter()
        .filter(|d| d.document.path == path)
    {
        *counts.entry(declaration.kind.clone()).or_insert(0) += 1;
    }
    counts.into_iter().collect()
}

fn counts(items: &[(&str, usize)]) -> Vec<(String, usize)> {
    items
        .iter()
        .map(|(kind, count)| ((*kind).to_owned(), *count))
        .collect()
}

#[test]
fn typescript_mixed_native_constructs() {
    let workspace = fixture_root();
    let root = fs::canonicalize(workspace.path()).unwrap();
    let artifact = native(&root);
    let calls = |path: &str| -> Vec<(String, String)> {
        artifact
            .calls
            .iter()
            .filter(|c| c.document.path == path)
            .map(|c| {
                let owner = artifact
                    .declarations
                    .iter()
                    .find(|d| d.syntax_id == c.owner_syntax_id)
                    .map(|d| d.kind.clone())
                    .unwrap();
                (c.spelling.clone().unwrap(), owner)
            })
            .collect()
    };
    let regions = |path: &str| {
        artifact
            .control_regions
            .iter()
            .filter(|r| r.document.path == path)
            .count()
    };

    // src/app/model.ts, per construct (§4):
    //   1 module (the file);
    //   type x5: interfaces Entity and Named (C4), enum Status (C5), abstract class Base
    //     (C3) and class Model (C1);
    //   alias x1: `type Id` (C6);
    //   field x5: Entity.id and Named.name (property_signature), Base.id (field with a type),
    //     Model.id and Model.name (parameter properties, C13); enum members are not
    //     declarations;
    //   method x4: Base.describe (abstract, C3), Model's constructor, describe and the
    //     decorated rename (C12);
    //   function x6: sealed, log, the three `parse` overloads (C10) and the ambient `make`
    //     (C18);
    //   anonymousFunction x1: the arrow returned by `sealed`;
    //   variable x1: `made`.
    assert_eq!(
        kind_counts(&artifact, "src/app/model.ts"),
        counts(&[
            ("alias", 1),
            ("anonymousFunction", 1),
            ("field", 5),
            ("function", 6),
            ("method", 4),
            ("module", 1),
            ("type", 5),
            ("variable", 1),
        ])
    );
    // Calls: `super()`, the `@sealed()` decorator (owned by the file module, since the
    // decorator belongs to the export statement), `new Model(...)`, `String(x)` and the
    // generic `make<Model>()` (C9, C12). `@log` is a reference only.
    let model_calls = calls("src/app/model.ts");
    let spellings: Vec<&str> = model_calls.iter().map(|(s, _)| s.as_str()).collect();
    assert_eq!(spellings.len(), 5, "{model_calls:?}");
    for expected in ["sealed", "super", "Model", "String", "make"] {
        assert!(spellings.contains(&expected), "{expected}: {model_calls:?}");
    }
    assert!(
        model_calls.contains(&("sealed".to_owned(), "module".to_owned())),
        "{model_calls:?}"
    );
    assert_eq!(regions("src/app/model.ts"), 0);

    // src/space.ts: module x1; namespace x2, `A.B.C` named by its rightmost segment `C` and
    // `module N` (C7, C8); type x1, `Inner`; variable x1, `v`.
    assert_eq!(
        kind_counts(&artifact, "src/space.ts"),
        counts(&[
            ("module", 1),
            ("namespace", 2),
            ("type", 1),
            ("variable", 1)
        ])
    );
    assert!(
        artifact
            .declarations
            .iter()
            .filter(|d| d.document.path == "src/space.ts" && d.kind == "namespace")
            .map(|d| d.name.as_deref())
            .eq([Some("C"), Some("N")])
    );

    // src/app/types.d.ts (C18, C19): module x1; namespace x1, "ambient-mod"; function x2, `f`
    // and `helperDecl`; type x3, `Window` (in `declare global`, which declares nothing),
    // `Shape` and `Config`; field x2, `Window.app` and `Config.x`; method x1, `Shape.area`.
    // A declaration-only document has no calls and no control regions.
    assert_eq!(
        kind_counts(&artifact, "src/app/types.d.ts"),
        counts(&[
            ("field", 2),
            ("function", 2),
            ("method", 1),
            ("module", 1),
            ("namespace", 1),
            ("type", 3),
        ])
    );
    assert!(calls("src/app/types.d.ts").is_empty());
    assert_eq!(regions("src/app/types.d.ts"), 0);

    // The JSX `render(...)` call in the TSX grammar (C21), owned by the onClick arrow.
    assert!(
        calls("src/app/view.tsx").contains(&("render".to_owned(), "anonymousFunction".to_owned())),
        "{:?}",
        calls("src/app/view.tsx")
    );
}

#[test]
fn typescript_mixed_class_catalog() {
    use trellis::{classes::Catalog, indexer::index_workspace};
    let workspace = fixture_root();
    let root = fs::canonicalize(workspace.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let graph = index_workspace(&IndexOptions::new(root), &cancel, |_| {}).unwrap();
    let catalog = Catalog::build(&graph.files, &graph.nodes, &cancel).unwrap();
    let class = |name: &str| {
        catalog
            .classes
            .iter()
            .find(|c| c.qualified_name == name)
            .unwrap_or_else(|| panic!("{name} is not catalogued"))
    };
    let names = |members: &[trellis::model::ClassMember]| -> Vec<String> {
        members.iter().map(|m| m.name.clone()).collect()
    };
    let relations = |owner: &str| -> Vec<(String, String)> {
        let id = &class(owner).symbol.id;
        let mut out: Vec<_> = catalog
            .relations
            .iter()
            .filter(|r| &r.owner == id)
            .map(|r| (r.kind.clone(), r.type_name.clone()))
            .collect();
        out.sort();
        out
    };
    let pairs = |items: &[(&str, &str)]| -> Vec<(String, String)> {
        items
            .iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    };

    for name in [
        "src.app.model.Entity",
        "src.app.model.Named",
        "src.app.model.Base",
        "src.app.model.Model",
        "src.app.model.Status",
        "src.app.view.View",
        "src.app.types.Shape",
    ] {
        assert_eq!(class(name).language, "typescript", "{name}");
    }
    assert_eq!(class("src.app.model.Entity").declaration_kind, "interface");
    assert_eq!(
        relations("src.app.model.Named"),
        pairs(&[("extends", "Entity")])
    );
    assert_eq!(
        class("src.app.model.Base").declaration_kind,
        "abstract_class"
    );
    assert_eq!(
        relations("src.app.model.Base"),
        pairs(&[("field", "Id"), ("implements", "Entity"), ("returns", "T")])
    );
    assert_eq!(
        relations("src.app.model.Model"),
        pairs(&[
            ("extends", "Base"),
            ("implements", "Named"),
            ("parameter", "Id"),
            ("returns", "Id"),
        ])
    );
    let model = class("src.app.model.Model");
    assert_eq!(names(&model.fields), ["id", "name"], "parameter properties");
    assert_eq!(names(&model.methods), ["constructor", "describe", "rename"]);
    assert_eq!(
        names(&class("src.app.model.Status").fields),
        ["Active", "Inactive"]
    );
    assert_eq!(class("src.app.view.View").declaration_kind, "class");
    assert_eq!(names(&class("src.app.types.Shape").methods), ["area"]);
    assert!(
        catalog
            .classes
            .iter()
            .all(|c| c.symbol.name != "Inner" && c.symbol.name != "Window"),
        "classes inside namespaces and `declare global` are not catalogued"
    );
}

#[test]
fn typescript_mixed_import_resolution() {
    use serde_json::json;
    let state = tempfile::tempdir().unwrap();
    let workspace = fixture_root();
    let root = fs::canonicalize(workspace.path()).unwrap();
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(state.path(), &root));
    let cancel: trellis::model::CancelFlag = Arc::new(AtomicBool::new(false));
    let baseline = store.index_baseline().unwrap();
    let leader = store.publication_for_tests().unwrap();
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let pin = store
        .publish_native(&graph, &capture, &native, &leader, baseline, &cancel)
        .unwrap();
    drop(leader);
    let response = store.evidence_response().unwrap();
    let references = response
        .architecture_response("references", &json!({"expectedBasis": pin}), &mut || Ok(()))
        .unwrap();
    response.finish(()).unwrap();
    let edges = |kind: &str| -> Vec<(String, String, u64)> {
        references[kind]
            .as_array()
            .unwrap()
            .iter()
            .map(|edge| {
                let target = &edge["target"];
                (
                    edge["source"]["path"].as_str().unwrap().to_owned(),
                    target["path"]
                        .as_str()
                        .or(target["package"].as_str())
                        .unwrap()
                        .to_owned(),
                    edge["count"].as_u64().unwrap(),
                )
            })
            .collect()
    };
    let edge = |from: &str, to: &str, count: u64| (from.to_owned(), to.to_owned(), count);
    let mut production = edges("production");
    production.sort();
    let mut expected = vec![
        // R11: JavaScript imports TypeScript.
        edge("src/app/legacy.js", "src/app/model.ts", 1),
        // R9 baseUrl, R13 `import m = require`, R8 paths.
        edge("src/app/alias.ts", "src/app/model.ts", 2),
        edge("src/app/alias.ts", "src/app/repo.ts", 1),
        // R11 and R6 implementation over declaration, R3, R6 declaration-only, R5.
        edge("src/app/bridge.ts", "src/app/legacy.js", 1),
        edge("src/app/bridge.ts", "pkg/index.ts", 1),
        edge("src/app/bridge.ts", "src/app/cjs.cts", 1),
        edge("src/app/bridge.ts", "src/app/esm.mts", 1),
        edge("src/app/bridge.ts", "src/app/types.d.ts", 1),
        // R7 type-only and R2 `.js` to `.ts`, then R12.
        edge("src/app/repo.ts", "src/app/model.ts", 2),
        edge("src/app/repo.ts", "lodash", 1),
        // R10 rootDirs.
        edge("src/app/uses-generated.ts", "generated/app/api.ts", 1),
        // R1 extensionless.
        edge("src/app/view.tsx", "src/app/model.ts", 1),
    ];
    expected.sort();
    assert_eq!(production, expected);
    assert_eq!(references["unresolved"], json!([]));
    let lodash = references["externalPackages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|package| package["identity"]["package"] == "lodash")
        .unwrap();
    assert_eq!(lodash["identity"]["language"], "typescript");
    assert_eq!(lodash["identity"]["standardLibrary"], false);
    assert_eq!(lodash["ecosystem"], "npm");
}

#[test]
fn typescript_interface_property_signatures_are_fields() {
    use trellis::{classes::Catalog, indexer::index_workspace};
    let workspace = fixture_root();
    let root = fs::canonicalize(workspace.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let graph = index_workspace(&IndexOptions::new(root), &cancel, |_| {}).unwrap();
    let catalog = Catalog::build(&graph.files, &graph.nodes, &cancel).unwrap();
    let entity = catalog
        .classes
        .iter()
        .find(|c| c.qualified_name == "src.app.model.Entity")
        .expect("Entity is catalogued");
    assert_eq!(entity.declaration_kind, "interface");
    assert!(
        entity.fields.iter().any(|field| field.name == "id"),
        "an interface property signature is a field"
    );
    assert!(
        catalog
            .relations
            .iter()
            .any(|relation| relation.owner == entity.symbol.id
                && relation.kind == "field"
                && relation.type_name == "Id"),
        "its declared type is a field relation"
    );
}

/// The `src/app/alias.ts` import edges after the root config files are rewritten by `setup`.
fn alias_edges_with_root_config(setup: impl FnOnce(&Path)) -> Vec<(String, u64)> {
    use serde_json::json;
    let state = tempfile::tempdir().unwrap();
    let workspace = fixture_root();
    let root = fs::canonicalize(workspace.path()).unwrap();
    setup(&root);
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(state.path(), &root));
    let cancel: trellis::model::CancelFlag = Arc::new(AtomicBool::new(false));
    let baseline = store.index_baseline().unwrap();
    let leader = store.publication_for_tests().unwrap();
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let pin = store
        .publish_native(&graph, &capture, &native, &leader, baseline, &cancel)
        .unwrap();
    drop(leader);
    let response = store.evidence_response().unwrap();
    let references = response
        .architecture_response("references", &json!({"expectedBasis": pin}), &mut || Ok(()))
        .unwrap();
    response.finish(()).unwrap();
    let mut edges: Vec<(String, u64)> = references["production"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|edge| edge["source"]["path"] == "src/app/alias.ts")
        .map(|edge| {
            let target = &edge["target"];
            (
                target["path"]
                    .as_str()
                    .or(target["package"].as_str())
                    .unwrap()
                    .to_owned(),
                edge["count"].as_u64().unwrap(),
            )
        })
        .collect();
    edges.sort();
    edges
}

#[test]
fn typescript_root_config_falls_back_to_jsconfig_and_prefers_tsconfig() {
    let tsconfig_edges = alias_edges_with_root_config(|_| {});
    assert!(
        tsconfig_edges
            .iter()
            .any(|(target, _)| target == "src/app/repo.ts"),
        "the fixture's tsconfig paths resolve the alias: {tsconfig_edges:?}"
    );

    // No `tsconfig.json`: the root `jsconfig.json` supplies the same mapping.
    let jsconfig_only = alias_edges_with_root_config(|root| {
        fs::rename(root.join("tsconfig.json"), root.join("jsconfig.json")).unwrap();
    });
    assert_eq!(jsconfig_only, tsconfig_edges);

    // Both present: a contradictory `jsconfig.json` is ignored.
    let both = alias_edges_with_root_config(|root| {
        fs::write(root.join("jsconfig.json"), "{}").unwrap();
    });
    assert_eq!(both, tsconfig_edges);

    // The jsconfig fallback is really what resolved it: an empty one resolves nothing.
    let empty = alias_edges_with_root_config(|root| {
        fs::remove_file(root.join("tsconfig.json")).unwrap();
        fs::write(root.join("jsconfig.json"), "{}").unwrap();
    });
    assert_ne!(empty, tsconfig_edges);
}
