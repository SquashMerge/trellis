use serde_json::Value;
use std::sync::{Arc, atomic::AtomicBool};
use trellis::{
    indexer::{IndexOptions, index_workspace_with_native},
    model::IndexPin,
    references::{self, DocumentInput, Projection, Snapshot},
};

fn snapshot(files: &[(&str, &str)]) -> Snapshot {
    let root = tempfile::tempdir().unwrap();
    for (path, text) in files {
        let path = root.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let (graph, native) = index_workspace_with_native(
        &IndexOptions::new(root.path().to_owned()),
        &identity.record_id,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let documents = graph
        .files
        .into_iter()
        .map(|file| {
            let key = native
                .revision
                .documents
                .iter()
                .find(|d| d.key.path == file.path)
                .unwrap()
                .key
                .clone();
            let declarations = native
                .declarations
                .iter()
                .filter(|d| d.document == key)
                .cloned()
                .collect();
            let calls = native
                .calls
                .iter()
                .filter(|c| c.document == key)
                .cloned()
                .collect();
            DocumentInput {
                key,
                file,
                declarations,
                calls,
            }
        })
        .collect();
    Snapshot {
        pin: IndexPin {
            index_generation: uuid::Uuid::new_v4(),
            index_revision: 7,
        },
        documents,
    }
}
fn project(snapshot: &Snapshot) -> Value {
    references::project(snapshot, &[], 20, &mut || Ok(()))
        .unwrap()
        .data
}
fn edge<'a>(data: &'a Value, role: &str, source: &str, target: &str) -> &'a Value {
    data[role]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["source"]["path"] == source && e["target"]["path"] == target)
        .unwrap()
}

#[test]
fn references_count_maximal_paths_and_use_stored_roles() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            r#"
fn production() { crate::store::run(); crate::store::run(); }
#[cfg(test)] mod tests { fn helper() { crate::store::run(); } }
#[cfg(feature = "test-support")] fn seam() { crate::store::run(); }
// crate::store::comment();
const TEXT: &str = "crate::store::string()";
fn opaque() { some_macro!(crate::store::inside()); }
"#,
        ),
        ("src/store.rs", "pub fn run() {}"),
    ]);
    let data = project(&input);
    let production = edge(&data, "production", "src/lib.rs", "src/store.rs");
    assert_eq!(production["count"], 3);
    assert_eq!(
        edge(&data, "tests", "src/lib.rs", "src/store.rs")["count"],
        1
    );
    assert_eq!(production["mapping"], "structural");
    assert_eq!(data["semanticComplete"], false);
    for site in production["sites"].as_array().unwrap() {
        assert_eq!(site["role"], "production");
        assert!(site["callId"].as_str().unwrap().starts_with("occ:v2:"));
        assert!(site["lines"]["start"].as_u64().unwrap() > 0);
        assert!(site.get("provenanceId").is_none());
    }
}

#[test]
fn references_sample_limit_does_not_change_exact_counts_and_source_filter_keeps_targets() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            "fn f() { crate::store::one(); crate::store::two(); }",
        ),
        ("src/store.rs", "pub fn one() {} pub fn two() {}"),
    ]);
    let data = references::project(&input, &["src/lib.rs".into()], 1, &mut || Ok(()))
        .unwrap()
        .data;
    let pair = edge(&data, "production", "src/lib.rs", "src/store.rs");
    assert_eq!(pair["count"], 2);
    assert_eq!(pair["sites"].as_array().unwrap().len(), 1);
    assert_eq!(pair["omittedSites"], 1);
    assert_eq!(data["modules"].as_array().unwrap().len(), 2);
    assert_eq!(data["counts"]["scannedDocuments"], 1);
    assert_eq!(project(&input), project(&input));
}

#[test]
fn references_relative_imports_and_java_unresolved_are_honest() {
    let input = snapshot(&[
        (
            "web/a.js",
            "import './b.js'; export { x } from './b.js'; import('dynamic');",
        ),
        ("web/b.js", "export const x = 1;"),
        (
            "pkg/a.py",
            "from .b import x\nfrom . import b\nimport external\n",
        ),
        ("pkg/b.py", "x = 1\n"),
        ("A.java", "import example.B; class A { example.B value; }"),
    ]);
    let data = project(&input);
    assert_eq!(
        edge(&data, "production", "web/a.js", "web/b.js")["count"],
        2
    );
    assert_eq!(
        edge(&data, "production", "pkg/a.py", "pkg/b.py")["count"],
        1
    );
    let reasons: Vec<_> = data["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["reason"].as_str().unwrap())
        .collect();
    assert!(reasons.contains(&"module_or_attribute_ambiguous"));
    assert!(reasons.contains(&"java_package_mapping_unavailable"));
    assert!(reasons.contains(&"dynamic_import_unsupported"));
}

#[test]
fn references_ambiguous_import_files_never_choose_arbitrary_target() {
    let input = snapshot(&[
        ("web/a.js", "import './b';"),
        ("web/b.js", ""),
        ("web/b.mjs", ""),
        ("pkg/a.py", "from .b import x\n"),
        ("pkg/b.py", ""),
        ("pkg/b/__init__.py", ""),
    ]);
    let data = project(&input);
    assert!(data["production"].as_array().unwrap().is_empty());
    assert_eq!(data["unresolved"].as_array().unwrap().len(), 2);
    assert!(
        data["unresolved"]
            .as_array()
            .unwrap()
            .iter()
            .all(|u| u["reason"] == "module_path_ambiguous")
    );
}

#[test]
fn references_rust_alias_reexport_inline_and_custom_layouts_stay_unresolved() {
    for source in [
        "use crate::store::run as alias; fn f() { alias(); }",
        "pub use crate::store::run;",
        "mod store { pub fn run() {} } fn f() { crate::store::run(); }",
        "#[path = \"elsewhere.rs\"] mod store; fn f() { crate::store::run(); }",
    ] {
        let input = snapshot(&[("src/lib.rs", source), ("src/store.rs", "pub fn run() {}")]);
        let data = project(&input);
        assert!(
            data["production"].as_array().unwrap().is_empty(),
            "{source}: {data}"
        );
        assert!(
            !data["unresolved"].as_array().unwrap().is_empty(),
            "{source}"
        );
    }
}

#[test]
fn references_grouped_imports_are_leaf_sites_not_nested_prefixes() {
    let input = snapshot(&[
        ("src/lib.rs", "use crate::store::{A, B};"),
        ("src/store.rs", "pub struct A; pub struct B;"),
    ]);
    let data = project(&input);
    assert_eq!(
        edge(&data, "production", "src/lib.rs", "src/store.rs")["count"],
        2
    );
    assert_eq!(data["counts"]["mappedSites"], 2);
}

#[test]
fn references_bad_syntax_work_bounds_and_cancellation_fail_atomically() {
    let mut input = snapshot(&[("src/lib.rs", "fn f() {}")]);
    input.documents[0].file.text = "fn f( {".into();
    assert!(
        references::project(&input, &[], 20, &mut || Ok(()))
            .unwrap_err()
            .to_string()
            .contains("invalid_source_syntax")
    );
    input.documents[0].file.text = " ".repeat(2 * 1024 * 1024 + 1);
    assert!(
        references::project(&input, &[], 20, &mut || Ok(()))
            .unwrap_err()
            .to_string()
            .contains("bounded_work")
    );
    let error =
        references::project(&input, &[], 20, &mut || anyhow::bail!("cancelled")).unwrap_err();
    assert!(error.to_string().contains("cancelled"));
}

#[test]
fn references_missing_or_ambiguous_owner_never_guesses_production() {
    let mut input = snapshot(&[
        ("src/lib.rs", "fn f() { crate::store::run(); }"),
        ("src/store.rs", "pub fn run() {}"),
    ]);
    let source = input
        .documents
        .iter_mut()
        .find(|d| d.key.path == "src/lib.rs")
        .unwrap();
    let mut duplicate = source
        .declarations
        .iter()
        .find(|d| d.name.as_deref() == Some("f"))
        .unwrap()
        .clone();
    duplicate.syntax_id = "sid:v1:duplicate".into();
    source.declarations.push(duplicate);
    let data = project(&input);
    assert!(data["production"].as_array().unwrap().is_empty());
    let unknown = &data["unresolved"][0];
    assert_eq!(unknown["reason"], "reference_owner_ambiguous");
    assert!(unknown["site"]["role"].is_null());
    assert!(unknown["site"]["ownerSyntaxId"].is_null());
    let _: Projection = references::project(&input, &[], 20, &mut || Ok(())).unwrap();
}

#[test]
fn references_request_path_globs_use_the_closed_config_grammar() {
    for path in ["src/store.rs", "src/**", "src/*/a?.rs", "**", "a*b"] {
        assert!(references::valid_path_glob(path), "{path:?}");
    }
    for path in [
        "", "/src/a", "src\\a", "a:b", "a\0b", "[ab]", "{a,b}", "!a", "a//b", "a/./b", "a/../b",
        "a**", "**a", "a/***/b",
    ] {
        assert!(!references::valid_path_glob(path), "{path:?}");
    }
    let input = snapshot(&[("src/lib.rs", "fn f() {}")]);
    let error = references::project(&input, &["a**".into()], 20, &mut || Ok(())).unwrap_err();
    assert!(error.to_string().starts_with("invalid_request:"));
}

#[test]
fn references_rust_binary_crate_paths_do_not_map_to_root_modules() {
    let input = snapshot(&[
        ("src/bin/tool.rs", "fn main() { crate::shared::run(); }"),
        ("src/lib.rs", "fn f() { crate::bin::tool::run(); }"),
        ("src/shared.rs", "pub fn run() {}"),
    ]);
    let data = project(&input);
    assert!(data["production"].as_array().unwrap().is_empty());
    assert_eq!(data["unresolved"].as_array().unwrap().len(), 2);
    assert!(
        data["unresolved"]
            .as_array()
            .unwrap()
            .iter()
            .all(|site| site["reason"] == "rust_custom_root_unsupported")
    );
}

#[test]
fn references_private_imported_module_names_do_not_become_file_edges() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            "use crate::actual::store; fn f() { crate::store::run(); }",
        ),
        ("src/actual.rs", "pub mod store { pub fn run() {} }"),
        ("src/store.rs", "pub fn run() {}"),
    ]);
    let data = project(&input);
    assert!(
        !data["production"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["source"]["path"] == "src/lib.rs"
                && edge["target"]["path"] == "src/store.rs")
    );
    assert!(
        data["unresolved"]
            .as_array()
            .unwrap()
            .iter()
            .any(|site| site["site"]["spelling"] == "crate::store::run"
                && site["reason"] == "rust_inline_or_alias_ambiguous")
    );
}

#[test]
fn references_rust_wildcards_and_spaced_path_attributes_cannot_prove_file_edges() {
    for source in [
        "use crate::actual::*; fn f() { crate::store::run(); }",
        "pub use crate::actual::*; fn f() { crate::store::run(); }",
        "#[path\t=\t\"actual.rs\"] mod store; fn f() { crate::store::run(); }",
        "#[path /* chosen module */ = \"actual.rs\"] mod store; fn f() { crate::store::run(); }",
    ] {
        let input = snapshot(&[
            ("src/lib.rs", source),
            ("src/actual.rs", "pub fn run() {}"),
            ("src/store.rs", "pub fn run() {}"),
        ]);
        let data = project(&input);
        assert!(
            data["production"].as_array().unwrap().is_empty(),
            "{source}: {data}"
        );
        assert!(!data["unresolved"].as_array().unwrap().is_empty());
    }
}

#[test]
fn references_grouped_self_import_does_not_map_a_decoy_file() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            "mod actual; use crate::actual::store::{self}; fn f() { crate::store::run(); }",
        ),
        ("src/actual.rs", "pub mod store;"),
        ("src/actual/store.rs", "pub fn run() {}"),
        ("src/store.rs", "pub fn run() {}"),
    ]);
    let data = project(&input);
    assert!(
        !data["production"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["source"]["path"] == "src/lib.rs"
                && edge["target"]["path"] == "src/store.rs")
    );
    assert!(
        data["unresolved"]
            .as_array()
            .unwrap()
            .iter()
            .any(|site| site["site"]["spelling"] == "crate::store::run"
                && site["reason"] == "rust_inline_or_alias_ambiguous")
    );
}

#[test]
fn references_modest_file_with_many_calls_keeps_exact_counts() {
    let source = (0..600)
        .map(|i| format!("fn f{i}() {{ {} }}\n", "crate::target::run(); ".repeat(4)))
        .collect::<String>();
    let input = snapshot(&[
        ("src/lib.rs", &source),
        ("src/target.rs", "pub fn run() {}"),
    ]);
    let data = references::project(&input, &[], 1, &mut || Ok(()))
        .unwrap()
        .data;
    let pair = edge(&data, "production", "src/lib.rs", "src/target.rs");
    assert_eq!(pair["count"], 2400);
    assert_eq!(pair["omittedSites"], 2399);
    assert_eq!(data["counts"]["mappedSites"], 2400);
    assert!(data["unresolved"].as_array().unwrap().is_empty());
}

#[test]
fn references_indexed_owners_preserve_linear_selection_and_call_order() {
    let mut input = snapshot(&[
        ("src/lib.rs", "fn f() { crate::target::run(); }"),
        ("src/target.rs", "pub fn run() {}"),
    ]);
    let source_index = input
        .documents
        .iter()
        .position(|d| d.key.path == "src/lib.rs")
        .unwrap();
    let original = input.documents[source_index].declarations.clone();
    let f = original
        .iter()
        .find(|d| d.name.as_deref() == Some("f"))
        .unwrap()
        .clone();
    for (kind, depth, ambiguous) in [
        ("function", 0, true),
        ("method", 0, false),
        ("module", 0, false),
        ("function", 1, false),
    ] {
        let source = &mut input.documents[source_index];
        source.declarations = original.clone();
        let mut duplicate = f.clone();
        duplicate.syntax_id = "sid:v1:duplicate".into();
        duplicate.kind = kind.into();
        if depth != 0 || kind == "module" {
            duplicate.ancestors.push(f.key.clone());
        }
        source.declarations.push(duplicate);
        let expected = if depth != 0 {
            "sid:v1:duplicate"
        } else {
            &f.syntax_id
        };
        let data = project(&input);
        if ambiguous {
            assert_eq!(data["unresolved"][0]["reason"], "reference_owner_ambiguous");
        } else {
            assert_eq!(
                edge(&data, "production", "src/lib.rs", "src/target.rs")["sites"][0]["ownerSyntaxId"],
                expected
            );
        }
    }
    let source = &mut input.documents[source_index];
    source.declarations = original;
    let mut outer = source.calls[0].clone();
    outer.id = "occ:v2:first-stored".into();
    outer.range = f.range;
    source.calls.insert(0, outer);
    let data = project(&input);
    assert_eq!(
        edge(&data, "production", "src/lib.rs", "src/target.rs")["sites"][0]["callId"],
        "occ:v2:first-stored"
    );
}
