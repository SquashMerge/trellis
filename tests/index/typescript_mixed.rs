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
