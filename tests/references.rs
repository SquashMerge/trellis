use serde_json::Value;
use sha2::{Digest, Sha256};
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
        manifests: Vec::new(),
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

fn project_rust_source(snapshot: &Snapshot, limit: usize) -> Value {
    references::project(snapshot, &["src/lib.rs".into()], limit, &mut || Ok(()))
        .unwrap()
        .data
}
fn has_edge(data: &Value, source: &str, target: &str) -> bool {
    data["production"]
        .as_array()
        .unwrap()
        .iter()
        .any(|pair| pair["source"]["path"] == source && pair["target"]["path"] == target)
}
fn module_paths(data: &Value) -> Vec<&str> {
    data["modules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|module| module["identity"]["path"].as_str().unwrap())
        .collect()
}

#[test]
fn references_java_direct_package_wildcard_expands_public_type_owner() {
    let input = snapshot(&[
        ("app/Use.java", "package app; import pkg.*; class Use {}"),
        ("types/Thing.java", "package pkg; public class Thing {}"),
    ]);
    let data = references::project(&input, &["app/Use.java".into()], 20, &mut || Ok(()))
        .unwrap()
        .data;
    assert!(
        has_edge(&data, "app/Use.java", "types/Thing.java"),
        "{data}"
    );
    assert_eq!(
        edge(&data, "production", "app/Use.java", "types/Thing.java")["count"],
        1
    );
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
}

#[test]
fn references_rust_direct_wildcard_expands_public_declaration_owner() {
    let input = snapshot(&[
        ("src/lib.rs", "mod m; use crate::m::*;"),
        ("src/m.rs", "pub fn a() {}"),
    ]);
    let data = project(&input);
    assert_eq!(
        edge(&data, "production", "src/lib.rs", "src/m.rs")["count"],
        1
    );
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert!(data["unresolved"].as_array().unwrap().is_empty());
}

#[test]
fn references_count_maximal_paths_and_use_stored_roles() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            r#"
mod store;
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
            "mod store; fn f() { crate::store::one(); crate::store::two(); }",
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
fn references_filtered_closure_keeps_selected_sources_and_both_role_destinations() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            "mod store; mod helper; fn f() { crate::store::run(); } #[cfg(test)] mod tests { fn t() { crate::helper::run(); } }",
        ),
        ("src/store.rs", "pub fn run() {}"),
        ("src/helper.rs", "pub fn run() {}"),
        ("src/unrelated.rs", "pub fn unused() {}"),
    ]);
    let data = references::project(&input, &["src/lib.rs".into()], 1, &mut || Ok(()))
        .unwrap()
        .data;
    let paths: Vec<_> = data["modules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["identity"]["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["src/helper.rs", "src/lib.rs", "src/store.rs"]);
    assert_eq!(
        edge(&data, "production", "src/lib.rs", "src/store.rs")["count"],
        1
    );
    assert_eq!(
        edge(&data, "tests", "src/lib.rs", "src/helper.rs")["count"],
        1
    );
    for module in data["modules"].as_array().unwrap() {
        let path = module["identity"]["path"].as_str().unwrap();
        let doc = input.documents.iter().find(|d| d.key.path == path).unwrap();
        let root = doc
            .declarations
            .iter()
            .find(|d| d.kind == "module" && d.ancestors.is_empty())
            .unwrap();
        assert_eq!(module["revisionId"], root.revision_id);
        assert_eq!(module["contentHash"], doc.file.hash);
        assert_eq!(module["provenanceId"], root.provenance_id);
        assert_eq!(module["identity"]["sid"], root.syntax_id);
    }
}

#[test]
fn references_filtered_resolution_keeps_unselected_ambiguity_and_rust_layout() {
    let input = snapshot(&[
        ("web/a.js", "import './b';"),
        ("web/b.js", ""),
        ("web/b.mjs", ""),
        ("src/lib.rs", "mod store; fn f() { crate::store::run(); }"),
        ("src/store.rs", "pub fn run() {}"),
        ("src/layout.rs", r#"#[path = "other.rs"] mod store;"#),
    ]);
    let data = references::project(
        &input,
        &["web/a.js".into(), "src/lib.rs".into()],
        1,
        &mut || Ok(()),
    )
    .unwrap()
    .data;
    assert_eq!(
        edge(&data, "production", "src/lib.rs", "src/store.rs")["count"],
        1
    );
    let mut reasons: Vec<_> = data["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["reason"].as_str().unwrap())
        .collect();
    reasons.sort_unstable();
    assert_eq!(reasons, ["module_path_ambiguous"]);
    assert_eq!(data["modules"].as_array().unwrap().len(), 3);
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
    let java_unknowns: Vec<_> = data["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["source"]["path"] == "A.java")
        .map(|row| row["site"]["spelling"].as_str().unwrap())
        .collect();
    assert_eq!(java_unknowns, ["import example.B;", "example.B"]);
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
fn references_rust_relevant_custom_layout_stays_unresolved() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            r#"#[path = "elsewhere.rs"] mod store; fn f() { crate::store::run(); }"#,
        ),
        ("src/store.rs", "pub fn run() {}"),
        ("src/elsewhere.rs", "pub fn run() {}"),
    ]);
    let data = project(&input);
    assert!(data["production"].as_array().unwrap().is_empty());
    assert!(!data["unresolved"].as_array().unwrap().is_empty());
}

#[test]
fn references_grouped_imports_are_leaf_sites_not_nested_prefixes() {
    let input = snapshot(&[
        ("src/lib.rs", "mod store; use crate::store::{A, B};"),
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
        ("src/lib.rs", "mod store; fn f() { crate::store::run(); }"),
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
}

#[test]
fn references_rust_imported_inline_module_maps_owner_not_decoy() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            "mod actual; use crate::actual::store; fn f() { crate::store::run(); }",
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
    assert_eq!(
        edge(&data, "production", "src/lib.rs", "src/actual.rs")["count"],
        2
    );
    assert!(data["unresolved"].as_array().unwrap().is_empty());
}

#[test]
fn references_rust_spaced_path_attributes_cannot_prove_decoy_file_edges() {
    for source in [
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
    assert!(data["production"].as_array().unwrap().iter().any(|pair| {
        pair["source"]["path"] == "src/lib.rs" && pair["target"]["path"] == "src/actual/store.rs"
    }));
    assert!(data["unresolved"].as_array().unwrap().is_empty());
}

#[test]
fn references_modest_file_with_many_calls_keeps_exact_counts() {
    let source = "mod target;\n".to_owned()
        + &(0..600)
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
        ("src/lib.rs", "mod target; fn f() { crate::target::run(); }"),
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

#[test]
fn references_rust_conventional_directory_and_inline_wildcards_expand() {
    for files in [
        vec![
            ("src/lib.rs", "mod m; use crate::m::*;"),
            ("src/m/mod.rs", "pub fn a() {} pub fn b() {}"),
        ],
        vec![("src/lib.rs", "mod m { pub fn a() {} } use crate::m::*;")],
    ] {
        let input = snapshot(&files);
        let data = project_rust_source(&input, 20);
        let target = if files.len() == 1 {
            "src/lib.rs"
        } else {
            "src/m/mod.rs"
        };
        assert_eq!(edge(&data, "production", "src/lib.rs", target)["count"], 1);
        assert_eq!(data["counts"]["mappedSites"], 1);
        assert!(data["unresolved"].as_array().unwrap().is_empty());
    }
}

#[test]
fn references_rust_nested_self_super_and_inline_file_ancestry_are_proved() {
    let input = snapshot(&[
        ("src/lib.rs", "mod outer;"),
        (
            "src/outer.rs",
            "pub fn a() {} mod inner { mod leaf; use self::leaf::*; use super::a; }",
        ),
        ("src/outer/inner/leaf.rs", "pub fn b() {}"),
        ("src/inner/leaf.rs", "pub fn decoy() {}"),
    ]);
    let data = references::project(&input, &["src/outer.rs".into()], 20, &mut || Ok(()))
        .unwrap()
        .data;
    assert_eq!(
        edge(
            &data,
            "production",
            "src/outer.rs",
            "src/outer/inner/leaf.rs"
        )["count"],
        1
    );
    assert_eq!(
        edge(&data, "production", "src/outer.rs", "src/outer.rs")["count"],
        1
    );
    assert!(!has_edge(&data, "src/outer.rs", "src/inner/leaf.rs"));
    assert!(data["unresolved"].as_array().unwrap().is_empty());
}

#[test]
fn references_rust_missing_duplicate_and_ambiguous_module_proof_stay_unknown() {
    for files in [
        vec![
            ("src/lib.rs", "use crate::m::*;"),
            ("src/m.rs", "pub fn a() {}"),
        ],
        vec![
            ("src/lib.rs", "mod m; mod m; use crate::m::*;"),
            ("src/m.rs", "pub fn a() {}"),
        ],
        vec![
            ("src/lib.rs", "mod m; use crate::m::*;"),
            ("src/m.rs", "pub fn a() {}"),
            ("src/m/mod.rs", "pub fn b() {}"),
        ],
    ] {
        let input = snapshot(&files);
        let data = project_rust_source(&input, 20);
        assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
        assert_eq!(data["counts"]["unresolvedSites"], 1);
        assert_eq!(data["unresolved"].as_array().unwrap().len(), 1);
    }
}

#[test]
fn references_rust_private_methods_and_function_locals_are_not_exports() {
    let input = snapshot(&[
        ("src/lib.rs", "mod m; use crate::m::*; fn visible() {}"),
        (
            "src/m.rs",
            "pub fn visible() { fn nested() {} } fn hidden() {} struct Private; impl Private { pub fn method() {} }",
        ),
    ]);
    let data = project_rust_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty());
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
}

#[test]
fn references_rust_explicit_import_and_proved_alias_shadow_only_matching_name() {
    for binding in ["use crate::local::a;", "use crate::local::original as a;"] {
        let source = format!("mod local; mod m; {binding} use crate::m::*; fn f() {{ a(); }}");
        let input = snapshot(&[
            ("src/lib.rs", &source),
            ("src/local.rs", "pub fn a() {} pub fn original() {}"),
            ("src/m.rs", "pub fn a() {}"),
        ]);
        let data = project_rust_source(&input, 20);
        assert_eq!(
            edge(&data, "production", "src/lib.rs", "src/local.rs")["count"],
            1
        );
        assert!(!has_edge(&data, "src/lib.rs", "src/m.rs"));
        assert_eq!(data["counts"]["mappedSites"], 1);
        assert_eq!(data["counts"]["unresolvedSites"], 1);
        let site = &edge(&data, "production", "src/lib.rs", "src/local.rs")["sites"][0];
        assert!(site["callId"].is_null());
        assert_eq!(site["relationKind"], "import");
    }
}

#[test]
fn references_rust_unproved_explicit_alias_blocks_fallback_but_underscore_does_not() {
    for (binding, expected_target) in [
        ("use crate::missing::a as a;", false),
        ("use crate::local::a as _;", true),
    ] {
        let source = format!("mod local; mod m; {binding} use crate::m::*;");
        let input = snapshot(&[
            ("src/lib.rs", &source),
            ("src/local.rs", "pub fn a() {}"),
            ("src/m.rs", "pub fn a() {}"),
        ]);
        let data = project_rust_source(&input, 20);
        assert_eq!(
            has_edge(&data, "src/lib.rs", "src/m.rs"),
            expected_target,
            "{data}"
        );
    }
}

#[test]
fn references_rust_shadowing_obeys_lexical_scope_and_constructor_namespace() {
    for (declaration, survives) in [
        ("fn a() {}", false),
        ("struct a {}", true),
        ("struct a;", false),
        ("struct a(u8);", false),
        ("mod sibling { fn a() {} }", true),
        ("fn f() { fn a() {} }", true),
    ] {
        let source = format!("mod m; use crate::m::*; {declaration}");
        let input = snapshot(&[("src/lib.rs", &source), ("src/m.rs", "pub fn a() {}")]);
        let data = project_rust_source(&input, 20);
        assert_eq!(
            has_edge(&data, "src/lib.rs", "src/m.rs"),
            survives,
            "{declaration}: {data}"
        );
        assert_eq!(
            data["counts"]["unresolvedSites"],
            if survives { 0 } else { 1 }
        );
    }
    let input = snapshot(&[
        (
            "src/lib.rs",
            "mod m; fn f() { use crate::m::*; fn a() {} } fn g() { use crate::m::*; }",
        ),
        ("src/m.rs", "pub fn a() {}"),
    ]);
    let data = project_rust_source(&input, 20);
    assert_eq!(
        edge(&data, "production", "src/lib.rs", "src/m.rs")["count"],
        1
    );
    assert_eq!(data["counts"]["unresolvedSites"], 1);
}

#[test]
fn references_rust_named_module_export_uses_declaration_owner_not_members() {
    let input = snapshot(&[
        ("src/lib.rs", "mod m; use crate::m::*;"),
        ("src/m.rs", "pub mod child;"),
        ("src/m/child.rs", "pub fn a() {}"),
    ]);
    let data = project_rust_source(&input, 20);
    assert_eq!(
        edge(&data, "production", "src/lib.rs", "src/m.rs")["count"],
        1
    );
    assert!(!has_edge(&data, "src/lib.rs", "src/m/child.rs"));
}

#[test]
fn references_rust_named_and_renamed_public_forwarding_keep_final_owner() {
    for forwarding in [
        "pub use crate::origin::a;",
        "pub use crate::origin::a as renamed;",
    ] {
        let input = snapshot(&[
            (
                "src/lib.rs",
                "mod origin; mod facade; use crate::facade::*;",
            ),
            ("src/facade.rs", forwarding),
            ("src/origin.rs", "pub fn a() {}"),
        ]);
        let data = project_rust_source(&input, 20);
        assert_eq!(
            edge(&data, "production", "src/lib.rs", "src/origin.rs")["count"],
            1
        );
        assert!(!has_edge(&data, "src/lib.rs", "src/facade.rs"));
        assert_eq!(
            module_paths(&data),
            ["src/facade.rs", "src/lib.rs", "src/origin.rs"]
        );
        assert!(data["unresolved"].as_array().unwrap().is_empty());
    }
}

#[test]
fn references_rust_forwarding_depth_four_resolves_but_fifth_edge_is_unknown() {
    for (last_forward, resolves) in [("pub fn a() {}", true), ("pub use crate::e::*;", false)] {
        let input = snapshot(&[
            (
                "src/lib.rs",
                "mod a; mod b; mod c; mod d; mod e; use crate::a::*;",
            ),
            ("src/a.rs", "pub use crate::b::*;"),
            ("src/b.rs", "pub use crate::c::*;"),
            ("src/c.rs", "pub use crate::d::*;"),
            ("src/d.rs", last_forward),
            ("src/e.rs", "pub fn a() {}"),
        ]);
        let data = project_rust_source(&input, 20);
        assert_eq!(
            has_edge(&data, "src/lib.rs", "src/d.rs"),
            resolves,
            "{data}"
        );
        assert!(!has_edge(&data, "src/lib.rs", "src/e.rs"));
        assert_eq!(
            data["counts"]["unresolvedSites"],
            if resolves { 0 } else { 1 }
        );
        if resolves {
            assert_eq!(
                module_paths(&data),
                ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs", "src/lib.rs"]
            );
        }
    }
}

#[test]
fn references_rust_cycle_keeps_independent_direct_export_and_residual_unknown() {
    let input = snapshot(&[
        ("src/lib.rs", "mod a; mod b; use crate::a::*;"),
        ("src/a.rs", "pub fn good() {} pub use crate::b::*;"),
        ("src/b.rs", "pub use crate::a::*;"),
    ]);
    let data = project_rust_source(&input, 20);
    assert_eq!(
        edge(&data, "production", "src/lib.rs", "src/a.rs")["count"],
        1
    );
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
    assert_eq!(data["unresolved"].as_array().unwrap().len(), 1);
}

#[test]
fn references_rust_diamond_deduplicates_original_identity_not_spelling() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            "mod facade; mod left; mod right; mod origin; use crate::facade::*;",
        ),
        (
            "src/facade.rs",
            "pub use crate::left::*; pub use crate::right::*;",
        ),
        ("src/left.rs", "pub use crate::origin::a;"),
        ("src/right.rs", "pub use crate::origin::a;"),
        ("src/origin.rs", "pub fn a() {}"),
    ]);
    let data = project_rust_source(&input, 20);
    assert_eq!(data["production"].as_array().unwrap().len(), 1);
    assert_eq!(
        edge(&data, "production", "src/lib.rs", "src/origin.rs")["count"],
        1
    );
    assert!(data["unresolved"].as_array().unwrap().is_empty());
    assert_eq!(
        module_paths(&data),
        [
            "src/facade.rs",
            "src/left.rs",
            "src/lib.rs",
            "src/origin.rs",
            "src/right.rs"
        ]
    );
}

#[test]
fn references_rust_filtered_catalog_retains_distinct_wildcard_collision() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            "mod facade; mod left; mod right; use crate::facade::*;",
        ),
        (
            "src/facade.rs",
            "pub use crate::left::*; pub use crate::right::*;",
        ),
        ("src/left.rs", "pub fn a() {}"),
        ("src/right.rs", "pub fn a() {}"),
    ]);
    let data = project_rust_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty());
    assert_eq!(data["counts"]["scannedDocuments"], 1);
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
    assert_eq!(data["unresolved"].as_array().unwrap().len(), 1);
}

#[test]
fn references_rust_conditional_known_name_keeps_other_positive_and_stored_role() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            "mod m; use crate::m::*; #[cfg(test)] mod tests { use crate::m::*; }",
        ),
        (
            "src/m.rs",
            "pub fn good() {} #[cfg(feature = \"optional\")] pub fn bad() {}",
        ),
    ]);
    let data = project_rust_source(&input, 20);
    assert_eq!(
        edge(&data, "production", "src/lib.rs", "src/m.rs")["count"],
        1
    );
    assert_eq!(edge(&data, "tests", "src/lib.rs", "src/m.rs")["count"], 1);
    assert_eq!(data["counts"]["mappedSites"], 2);
    assert_eq!(data["counts"]["unresolvedSites"], 2);
    let roles: Vec<_> = data["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .map(|unknown| unknown["site"]["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["production", "test"]);
}

#[test]
fn references_rust_opaque_exports_block_wildcard_uniqueness_not_explicit_names() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            "mod m; mod opaque; mod facade; use crate::facade::*;",
        ),
        ("src/m.rs", "pub fn good() {}"),
        ("src/opaque.rs", "make_items!();"),
        (
            "src/facade.rs",
            "pub use crate::m::good; pub use crate::opaque::*;",
        ),
    ]);
    let data = project_rust_source(&input, 20);
    assert_eq!(
        edge(&data, "production", "src/lib.rs", "src/m.rs")["count"],
        1
    );
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
    assert_eq!(data["unresolved"].as_array().unwrap().len(), 1);

    let blocked = snapshot(&[
        (
            "src/lib.rs",
            "mod m; mod opaque; use crate::m::*; use crate::opaque::*;",
        ),
        ("src/m.rs", "pub fn good() {}"),
        ("src/opaque.rs", "make_items!();"),
    ]);
    let data = project_rust_source(&blocked, 20);
    assert!(data["production"].as_array().unwrap().is_empty());
    assert_eq!(data["counts"]["unresolvedSites"], 2);
}

#[test]
fn references_rust_fanout_counts_once_per_site_and_once_per_final_owner() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            "mod facade; mod left; mod right; use crate::facade::*;",
        ),
        (
            "src/facade.rs",
            "pub use crate::left::*; pub use crate::right::*; #[cfg(test)] pub fn conditional() {}",
        ),
        ("src/left.rs", "pub fn a() {} pub fn b() {}"),
        ("src/right.rs", "pub fn c() {}"),
    ]);
    let data = project_rust_source(&input, 20);
    assert_eq!(data["production"].as_array().unwrap().len(), 2);
    assert_eq!(
        edge(&data, "production", "src/lib.rs", "src/left.rs")["count"],
        1
    );
    assert_eq!(
        edge(&data, "production", "src/lib.rs", "src/right.rs")["count"],
        1
    );
    assert!(!has_edge(&data, "src/lib.rs", "src/facade.rs"));
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
    let counts: u64 = data["production"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pair| pair["count"].as_u64().unwrap())
        .sum();
    assert_eq!(counts, 2);
}

#[test]
fn references_rust_empty_and_fully_shadowed_expansions_have_distinct_coverage() {
    let mut reasons = Vec::new();
    for (source, target) in [
        ("mod m; use crate::m::*;", "fn private() {}"),
        ("mod m; use crate::m::*; fn a() {}", "pub fn a() {}"),
    ] {
        let input = snapshot(&[("src/lib.rs", source), ("src/m.rs", target)]);
        let data = project_rust_source(&input, 20);
        assert!(data["production"].as_array().unwrap().is_empty());
        assert_eq!(data["counts"]["mappedSites"], 0);
        assert_eq!(data["counts"]["unresolvedSites"], 1);
        assert_eq!(data["unresolved"].as_array().unwrap().len(), 1);
        reasons.push(data["unresolved"][0]["reason"].as_str().unwrap().to_owned());
    }
    assert_ne!(reasons[0], reasons[1]);
}

#[test]
fn references_rust_clipped_samples_keep_proof_closure_ranges_and_exact_counts() {
    let source = "mod first; mod second; mod origin; fn f() { use crate::first::*; } fn g() { use crate::second::*; }";
    let input = snapshot(&[
        ("src/lib.rs", source),
        ("src/first.rs", "pub use crate::origin::a;"),
        ("src/second.rs", "pub use crate::origin::a;"),
        ("src/origin.rs", "pub fn a() {}"),
        ("src/unrelated.rs", "pub fn unused() {}"),
    ]);
    let data = project_rust_source(&input, 1);
    let pair = edge(&data, "production", "src/lib.rs", "src/origin.rs");
    assert_eq!(pair["count"], 2);
    assert_eq!(pair["omittedSites"], 1);
    assert_eq!(pair["sites"].as_array().unwrap().len(), 1);
    assert_eq!(data["counts"]["mappedSites"], 2);
    assert_eq!(
        module_paths(&data),
        [
            "src/first.rs",
            "src/lib.rs",
            "src/origin.rs",
            "src/second.rs"
        ]
    );
    let site = &pair["sites"][0];
    let start = site["range"]["start"].as_u64().unwrap() as usize;
    let end = site["range"]["end"].as_u64().unwrap() as usize;
    assert_eq!(&source[start..end], site["spelling"].as_str().unwrap());
    assert!(site["callId"].is_null());
    let owner = input
        .documents
        .iter()
        .find(|doc| doc.key.path == "src/lib.rs")
        .unwrap()
        .declarations
        .iter()
        .find(|declaration| declaration.name.as_deref() == Some("f"))
        .unwrap();
    assert_eq!(site["ownerSyntaxId"], owner.syntax_id);
    assert_eq!(site["role"], "production");
    assert_eq!(project_rust_source(&input, 20)["modules"], data["modules"]);
    for module in data["modules"].as_array().unwrap() {
        let document = input
            .documents
            .iter()
            .find(|document| document.key.path == module["identity"]["path"].as_str().unwrap())
            .unwrap();
        let native = document
            .declarations
            .iter()
            .find(|declaration| declaration.kind == "module" && declaration.ancestors.is_empty())
            .unwrap();
        assert_eq!(module["identity"]["sid"], native.syntax_id);
        assert_eq!(module["revisionId"], native.revision_id);
        assert_eq!(module["provenanceId"], native.provenance_id);
        assert_eq!(module["contentHash"], document.file.hash);
    }
}

#[test]
fn references_rust_expansion_order_is_stable_across_document_and_declaration_order() {
    let mut input = snapshot(&[
        (
            "src/lib.rs",
            "mod facade; mod left; mod right; use crate::facade::*;",
        ),
        (
            "src/facade.rs",
            "pub use crate::right::*; pub use crate::left::*;",
        ),
        ("src/left.rs", "pub fn a() {}"),
        ("src/right.rs", "pub fn b() {} #[cfg(test)] pub fn c() {}"),
    ]);
    let expected = project_rust_source(&input, 1);
    input.documents.reverse();
    for doc in &mut input.documents {
        doc.declarations.reverse();
    }
    assert_eq!(project_rust_source(&input, 1), expected);
    assert_eq!(project_rust_source(&input, 1), expected);
}

#[test]
fn references_rust_competing_consumer_wildcards_do_not_select_by_source_order() {
    for imports in [
        "use crate::left::*; use crate::right::*;",
        "use crate::right::*; use crate::left::*;",
    ] {
        let source = format!("mod left; mod right; {imports}");
        let input = snapshot(&[
            ("src/lib.rs", &source),
            ("src/left.rs", "pub fn a() {}"),
            ("src/right.rs", "pub fn a() {}"),
        ]);
        let data = project_rust_source(&input, 20);
        assert!(data["production"].as_array().unwrap().is_empty());
        assert_eq!(data["counts"]["mappedSites"], 0);
        assert_eq!(data["counts"]["unresolvedSites"], 2);
        assert_eq!(data["unresolved"].as_array().unwrap().len(), 2);
    }
}

#[test]
fn references_rust_restricted_visibility_is_not_promoted_outside_its_scope() {
    let input = snapshot(&[
        ("src/lib.rs", "mod m; use crate::m::inner::*;"),
        ("src/m.rs", "pub mod inner;"),
        (
            "src/m/inner.rs",
            "pub fn good() {} pub(in crate::m) fn restricted() {} fn private() {}",
        ),
    ]);
    let data = project_rust_source(&input, 20);
    assert_eq!(
        edge(&data, "production", "src/lib.rs", "src/m/inner.rs")["count"],
        1
    );
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
}

#[test]
fn references_rust_conditional_module_edge_does_not_certify_unconditional_export() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            "#[cfg(feature = \"optional\")] mod m; use crate::m::*;",
        ),
        ("src/m.rs", "pub fn a() {}"),
    ]);
    let data = project_rust_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty());
    assert_eq!(data["counts"]["unresolvedSites"], 1);
}

#[test]
fn references_rust_shared_file_with_unproved_crate_membership_stays_unknown() {
    let input = snapshot(&[
        ("src/lib.rs", "mod shared;"),
        ("src/main.rs", "mod shared; fn main() {}"),
        ("src/shared.rs", "mod child; use crate::shared::child::*;"),
        ("src/shared/child.rs", "pub fn a() {}"),
    ]);
    let data = references::project(&input, &["src/shared.rs".into()], 20, &mut || Ok(()))
        .unwrap()
        .data;
    assert!(data["production"].as_array().unwrap().is_empty());
    assert_eq!(data["counts"]["unresolvedSites"], 1);
}

#[test]
fn references_rust_external_wildcard_does_not_erase_proved_local_name_or_bind_bare_call() {
    let source = "mod local; use crate::local::*; use dep::*; fn f() { local_name(); }";
    let mut input = snapshot(&[
        ("src/lib.rs", source),
        ("src/local.rs", "pub fn local_name() {}"),
    ]);
    let source_set_id = input
        .documents
        .iter()
        .find(|doc| doc.key.path == "src/lib.rs")
        .unwrap()
        .key
        .source_set_id
        .clone();
    let manifest = b"[package]\nname = 'fixture'\nversion = '0.1.0'\n[dependencies]\ndep = '1'\n";
    input.manifests.push(references::external::ManifestInput {
        source_set_id,
        native_revision_id: "selected-native-revision".into(),
        dependency_hash: "captured-dependencies".into(),
        path: "Cargo.toml".into(),
        content_hash: Some(hex::encode(Sha256::digest(manifest))),
        bytes: Some(manifest.to_vec()),
    });
    let data = project_rust_source(&input, 20);
    let local = edge(&data, "production", "src/lib.rs", "src/local.rs");
    assert_eq!(local["count"], 1);
    assert_eq!(local["sites"][0]["relationKind"], "import");
    assert!(local["sites"][0]["callId"].is_null());
    let package = data["production"]
        .as_array()
        .unwrap()
        .iter()
        .find(|pair| pair["target"]["kind"] == "externalPackage")
        .unwrap();
    assert_eq!(package["count"], 1);
    assert_eq!(package["target"]["package"], "dep");
    assert_eq!(package["target"]["standardLibrary"], false);
    assert_eq!(data["counts"]["mappedSites"], 2);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
    assert_eq!(data["production"].as_array().unwrap().len(), 2);
    assert_eq!(data["externalPackages"].as_array().unwrap().len(), 1);
    assert_eq!(module_paths(&data), ["src/lib.rs", "src/local.rs"]);
    assert!(
        data["production"].as_array().unwrap().iter().all(|pair| {
            pair["sites"]
                .as_array()
                .unwrap()
                .iter()
                .all(|site| site["relationKind"] == "import")
        }),
        "the bare call is not a new structural site or semantic call binding"
    );
}

#[test]
fn references_rust_opaque_binding_in_importing_scope_blocks_wildcard_fallback() {
    let input = snapshot(&[
        ("src/lib.rs", "mod m; make_bindings!(); use crate::m::*;"),
        ("src/m.rs", "pub fn a() {}"),
    ]);
    let data = project_rust_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty());
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
    assert_eq!(data["unresolved"].as_array().unwrap().len(), 1);
}

#[test]
fn references_rust_test_only_inline_export_cannot_prove_production_availability() {
    let input = snapshot(&[(
        "src/lib.rs",
        "mod m { #[test] pub fn a() {} } use crate::m::*;",
    )]);
    let data = project_rust_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty());
    assert!(data["tests"].as_array().unwrap().is_empty());
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
    assert_eq!(data["unresolved"][0]["site"]["role"], "production");
}

#[test]
fn references_rust_forwarded_module_alias_keeps_final_owner_and_intermediate_proof() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            "mod alias; mod bridge; use crate::alias::renamed::*;",
        ),
        ("src/alias.rs", "pub use crate::bridge::m as renamed;"),
        ("src/bridge.rs", "pub mod m;"),
        ("src/bridge/m.rs", "pub fn a() {}"),
    ]);
    let data = project_rust_source(&input, 20);
    assert_eq!(
        edge(&data, "production", "src/lib.rs", "src/bridge/m.rs")["count"],
        1
    );
    assert!(!has_edge(&data, "src/lib.rs", "src/alias.rs"));
    assert!(!has_edge(&data, "src/lib.rs", "src/bridge.rs"));
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
    assert_eq!(
        module_paths(&data),
        [
            "src/alias.rs",
            "src/bridge.rs",
            "src/bridge/m.rs",
            "src/lib.rs"
        ]
    );
}

#[test]
fn references_rust_file_module_super_uses_lexical_parent_not_file_itself() {
    let input = snapshot(&[
        ("src/lib.rs", "mod m; mod n;"),
        ("src/m.rs", "use super::n::*;"),
        ("src/n.rs", "pub fn a() {}"),
        ("src/m/n.rs", "pub fn decoy() {}"),
    ]);
    let data = references::project(&input, &["src/m.rs".into()], 20, &mut || Ok(()))
        .unwrap()
        .data;
    assert_eq!(
        edge(&data, "production", "src/m.rs", "src/n.rs")["count"],
        1
    );
    assert!(!has_edge(&data, "src/m.rs", "src/m/n.rs"));
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
}

#[test]
fn references_rust_inner_cfg_applies_to_whole_module_export_scope() {
    for files in [
        vec![
            ("src/lib.rs", "mod m; use crate::m::*;"),
            (
                "src/m.rs",
                r##"#![cfg(feature = "x")] pub fn a() {} pub fn b() {}"##,
            ),
        ],
        vec![(
            "src/lib.rs",
            r##"mod m { #![cfg(feature = "x")] pub fn a() {} pub fn b() {} } use crate::m::*;"##,
        )],
    ] {
        let input = snapshot(&files);
        let data = project_rust_source(&input, 20);
        assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
        assert!(data["tests"].as_array().unwrap().is_empty());
        assert_eq!(data["counts"]["mappedSites"], 0);
        assert_eq!(data["counts"]["unresolvedSites"], 1);
        assert_eq!(data["unresolved"].as_array().unwrap().len(), 1);
        assert_eq!(data["unresolved"][0]["site"]["role"], "production");
    }
}

#[test]
fn references_rust_inner_cfg_prevents_explicit_later_item_availability() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            "mod m; use crate::m::b; fn f() { crate::m::b(); }",
        ),
        (
            "src/m.rs",
            r##"#![cfg(feature = "x")] pub fn a() {} pub fn b() {}"##,
        ),
    ]);
    let data = project_rust_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty());
    assert!(data["tests"].as_array().unwrap().is_empty());
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert_eq!(data["counts"]["unresolvedSites"], 2);
    assert_eq!(data["unresolved"].as_array().unwrap().len(), 2);
    assert!(
        data["unresolved"]
            .as_array()
            .unwrap()
            .iter()
            .all(|unknown| { unknown["site"]["role"] == "production" })
    );
}

#[test]
fn references_rust_qualified_type_context_keeps_only_type_namespace_owner() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            "mod types; mod values; mod x; fn f(_: crate::x::A) {}",
        ),
        ("src/types.rs", "pub type A = u8;"),
        ("src/values.rs", "pub fn A() {}"),
        (
            "src/x.rs",
            "pub use crate::types::A; pub use crate::values::A;",
        ),
    ]);
    let data = project_rust_source(&input, 20);
    assert_eq!(data["production"].as_array().unwrap().len(), 1);
    assert_eq!(
        edge(&data, "production", "src/lib.rs", "src/types.rs")["count"],
        1
    );
    assert!(!has_edge(&data, "src/lib.rs", "src/values.rs"));
    assert!(!has_edge(&data, "src/lib.rs", "src/x.rs"));
    assert_eq!(data["counts"]["scannedDocuments"], 1);
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
}

#[test]
fn references_rust_generic_type_binder_blocks_same_spelled_module_path() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            "mod T; trait Tr { fn a(); } fn f<T: Tr>() { T::a(); }",
        ),
        ("src/T.rs", "pub fn a() {}"),
    ]);
    let data = project_rust_source(&input, 20);
    assert!(!has_edge(&data, "src/lib.rs", "src/T.rs"));
    assert!(data["production"].as_array().unwrap().is_empty());
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
    assert_eq!(data["unresolved"].as_array().unwrap().len(), 1);
    assert_eq!(data["unresolved"][0]["site"]["spelling"], "T::a");
    assert_eq!(data["unresolved"][0]["site"]["role"], "production");
}

#[test]
fn references_rust_unqualified_module_lookup_stops_at_inline_module_boundary() {
    let input = snapshot(&[("src/lib.rs", "mod m { pub fn a() {} } mod n { use m::a; }")]);
    let data = project_rust_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty());
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
    assert_eq!(data["unresolved"].as_array().unwrap().len(), 1);
    assert_eq!(data["unresolved"][0]["site"]["spelling"], "m::a");
    assert_eq!(data["unresolved"][0]["site"]["role"], "production");
}

#[test]
fn references_rust_private_local_binding_shadows_public_star_forwarding() {
    let input = snapshot(&[
        ("src/lib.rs", "mod m; mod n; use crate::n::*;"),
        ("src/m.rs", "pub fn a() {}"),
        ("src/n.rs", "fn a() {} pub use crate::m::*;"),
    ]);
    let data = project_rust_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty());
    assert!(!has_edge(&data, "src/lib.rs", "src/m.rs"));
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
    assert_eq!(data["unresolved"].as_array().unwrap().len(), 1);
}

#[test]
fn references_rust_wildcard_imported_local_std_module_precedes_standard_library() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            "mod local; use crate::local::*; use std::Item;",
        ),
        ("src/local.rs", "pub mod std { pub struct Item; }"),
    ]);
    let data = project_rust_source(&input, 20);
    assert_eq!(
        edge(&data, "production", "src/lib.rs", "src/local.rs")["count"],
        2
    );
    assert_eq!(data["production"].as_array().unwrap().len(), 1);
    assert_eq!(data["counts"]["mappedSites"], 2);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
    assert!(data["externalPackages"].as_array().unwrap().is_empty());
}

#[test]
fn references_rust_extern_crate_alias_blocks_standard_library_spelling_fallback() {
    let input = snapshot(&[("src/lib.rs", "extern crate serde as std; use std::Item;")]);
    let data = project_rust_source(&input, 20);
    assert!(!data["production"].as_array().unwrap().iter().any(|pair| {
        pair["target"]["kind"] == "externalPackage" && pair["target"]["standardLibrary"] == true
    }));
    assert!(
        !data["externalPackages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|package| { package["identity"]["standardLibrary"] == true })
    );
    assert!(
        data["unresolved"]
            .as_array()
            .unwrap()
            .iter()
            .any(|unknown| { unknown["site"]["spelling"] == "std::Item" })
    );
}

#[test]
fn references_rust_follow_depth_is_consumed_across_multiple_module_alias_segments() {
    for (path, middle, resolves) in [
        ("crate::A::B::C::*", "pub fn good() {}", true),
        ("crate::A::B::C::D::*", "pub use crate::d as D;", false),
    ] {
        let consumer = format!("use {path};");
        let input = snapshot(&[
            (
                "src/lib.rs",
                "pub mod a; pub mod b; pub mod c; pub mod d; mod consumer; pub use crate::a as A;",
            ),
            ("src/a.rs", "pub use crate::b as B;"),
            ("src/b.rs", "pub use crate::c as C;"),
            ("src/c.rs", middle),
            ("src/d.rs", "pub fn good() {}"),
            ("src/consumer.rs", &consumer),
        ]);
        let data = references::project(&input, &["src/consumer.rs".into()], 20, &mut || Ok(()))
            .unwrap()
            .data;
        assert_eq!(
            has_edge(&data, "src/consumer.rs", "src/c.rs"),
            resolves,
            "{data}"
        );
        assert!(!has_edge(&data, "src/consumer.rs", "src/d.rs"));
        assert_eq!(data["counts"]["mappedSites"], if resolves { 1 } else { 0 });
        assert_eq!(
            data["counts"]["unresolvedSites"],
            if resolves { 0 } else { 1 }
        );
        if resolves {
            assert_eq!(
                module_paths(&data),
                [
                    "src/a.rs",
                    "src/b.rs",
                    "src/c.rs",
                    "src/consumer.rs",
                    "src/lib.rs"
                ]
            );
        }
    }
}

#[test]
fn references_rust_unsupported_attribute_item_cannot_prove_explicit_availability() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            "mod m; use crate::m::a; fn f() { crate::m::a(); }",
        ),
        ("src/m.rs", "#[custom_attr] pub fn a() {}"),
    ]);
    let data = project_rust_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty());
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert_eq!(data["counts"]["unresolvedSites"], 2);
    assert_eq!(data["unresolved"].as_array().unwrap().len(), 2);
}

#[test]
fn references_rust_root_only_standard_library_import_does_not_shadow_itself() {
    let input = snapshot(&[("src/lib.rs", "use std;")]);
    let data = project_rust_source(&input, 20);
    assert_eq!(data["production"].as_array().unwrap().len(), 1);
    let pair = &data["production"][0];
    assert_eq!(pair["source"]["path"], "src/lib.rs");
    assert_eq!(pair["target"]["kind"], "externalPackage");
    assert_eq!(pair["target"]["package"], "std");
    assert_eq!(pair["target"]["standardLibrary"], true);
    assert_eq!(pair["count"], 1);
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
    assert_eq!(data["externalPackages"].as_array().unwrap().len(), 1);
    assert_eq!(module_paths(&data), ["src/lib.rs"]);
}

#[test]
fn references_rust_private_named_import_shadows_public_star_forwarding() {
    let input = snapshot(&[
        ("src/lib.rs", "mod m; mod n; mod other; use crate::n::*;"),
        ("src/m.rs", "pub fn a() {}"),
        ("src/n.rs", "use crate::other::a; pub use crate::m::*;"),
        ("src/other.rs", "pub fn a() {}"),
    ]);
    let data = project_rust_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty());
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
}

#[test]
fn references_rust_private_item_cannot_be_imported_from_parent_scope() {
    let input = snapshot(&[
        ("src/lib.rs", "mod n; use crate::n::private;"),
        ("src/n.rs", "fn private() {}"),
    ]);
    let data = project_rust_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty());
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
}

#[test]
fn references_rust_root_only_dependency_import_does_not_shadow_itself() {
    let mut input = snapshot(&[("src/lib.rs", "use serde;")]);
    let source_set_id = input.documents[0].key.source_set_id.clone();
    let manifest = b"[package]\nname = 'fixture'\nversion = '0.1.0'\n[dependencies]\nserde = '1'\n";
    input.manifests.push(references::external::ManifestInput {
        source_set_id,
        native_revision_id: "selected-native-revision".into(),
        dependency_hash: "captured-dependencies".into(),
        path: "Cargo.toml".into(),
        content_hash: Some(hex::encode(Sha256::digest(manifest))),
        bytes: Some(manifest.to_vec()),
    });
    let data = project_rust_source(&input, 20);
    assert_eq!(data["production"].as_array().unwrap().len(), 1);
    let pair = &data["production"][0];
    assert_eq!(pair["target"]["kind"], "externalPackage");
    assert_eq!(pair["target"]["package"], "serde");
    assert_eq!(pair["target"]["standardLibrary"], false);
    assert_eq!(pair["count"], 1);
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
    assert_eq!(data["externalPackages"].as_array().unwrap().len(), 1);
}

#[test]
fn references_rust_conditional_local_wildcard_blocks_standard_library_fallback() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            "mod local; use crate::local::*; use std::Item;",
        ),
        (
            "src/local.rs",
            "#[cfg(feature = \"optional\")] pub mod std { pub struct Item; }",
        ),
    ]);
    let data = project_rust_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty());
    assert!(data["externalPackages"].as_array().unwrap().is_empty());
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert!(
        data["unresolved"]
            .as_array()
            .unwrap()
            .iter()
            .any(|unknown| { unknown["site"]["spelling"] == "std::Item" })
    );
}

#[test]
fn references_rust_opaque_local_bindings_block_standard_library_fallback() {
    let input = snapshot(&[("src/lib.rs", "make_bindings!(); use std::Item;")]);
    let data = project_rust_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty());
    assert!(data["externalPackages"].as_array().unwrap().is_empty());
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
    assert_eq!(data["unresolved"][0]["site"]["spelling"], "std::Item");
}

fn retain_rust_manifest(input: &mut Snapshot, bytes: &[u8]) {
    let source_set_id = input
        .documents
        .iter()
        .find(|doc| doc.key.path == "src/lib.rs")
        .unwrap()
        .key
        .source_set_id
        .clone();
    input.manifests.push(references::external::ManifestInput {
        source_set_id,
        native_revision_id: "selected-native-revision".into(),
        dependency_hash: "captured-dependencies".into(),
        path: "Cargo.toml".into(),
        content_hash: Some(hex::encode(Sha256::digest(bytes))),
        bytes: Some(bytes.to_vec()),
    });
}

#[test]
fn references_rust_comments_preserve_custom_path_module_uncertainty() {
    for comment in ["// custom module", "/* custom module */"] {
        let source = format!(
            "#[path = \"actual.rs\"] {comment}\nmod store; fn f() {{ crate::store::run(); }}"
        );
        let input = snapshot(&[
            ("src/lib.rs", &source),
            ("src/store.rs", "pub fn run() {}"),
            ("src/actual.rs", "pub fn run() {}"),
        ]);
        let data = project_rust_source(&input, 20);
        assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
        assert_eq!(data["counts"]["mappedSites"], 0, "{data}");
        assert_eq!(data["counts"]["unresolvedSites"], 1, "{data}");
    }
}

#[test]
fn references_rust_comments_preserve_cfg_module_uncertainty() {
    for comment in ["// optional module", "/* optional module */"] {
        let source = format!(
            "#[cfg(feature = \"optional\")] {comment}\nmod store; fn f() {{ crate::store::run(); }}"
        );
        let input = snapshot(&[("src/lib.rs", &source), ("src/store.rs", "pub fn run() {}")]);
        let data = project_rust_source(&input, 20);
        assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
        assert_eq!(data["counts"]["mappedSites"], 0, "{data}");
        assert_eq!(data["counts"]["unresolvedSites"], 1, "{data}");
    }
}

#[test]
fn references_rust_qualified_local_star_path_survives_standard_library_star() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            "mod outer; use crate::outer::*; use std::collections::*; fn f() { inner::g(); }",
        ),
        ("src/outer.rs", "pub mod inner;"),
        ("src/outer/inner.rs", "pub fn g() {}"),
    ]);
    let data = project_rust_source(&input, 20);
    assert!(
        has_edge(&data, "src/lib.rs", "src/outer/inner.rs"),
        "{data}"
    );
    assert_eq!(
        edge(&data, "production", "src/lib.rs", "src/outer/inner.rs")["count"],
        1,
        "{data}"
    );
    assert_eq!(
        edge(&data, "production", "src/lib.rs", "src/outer/inner.rs")["sites"][0]["spelling"],
        "inner::g"
    );
    assert_eq!(data["counts"]["mappedSites"], 3, "{data}");
    assert_eq!(data["counts"]["unresolvedSites"], 0, "{data}");
}

#[test]
fn references_rust_public_local_star_forwarding_survives_dependency_star() {
    let mut input = snapshot(&[
        (
            "src/lib.rs",
            "mod origin; mod facade; use crate::facade::*;",
        ),
        ("src/facade.rs", "pub use crate::origin::*; pub use dep::*;"),
        ("src/origin.rs", "pub fn a() {}"),
    ]);
    retain_rust_manifest(
        &mut input,
        b"[package]\nname = 'fixture'\nversion = '0.1.0'\n[dependencies]\ndep = '1'\n",
    );
    let data = project_rust_source(&input, 20);
    assert!(has_edge(&data, "src/lib.rs", "src/origin.rs"), "{data}");
    assert_eq!(
        edge(&data, "production", "src/lib.rs", "src/origin.rs")["count"],
        1,
        "{data}"
    );
    assert!(!has_edge(&data, "src/lib.rs", "src/facade.rs"));
    assert_eq!(data["counts"]["mappedSites"], 1, "{data}");
    assert_eq!(data["counts"]["unresolvedSites"], 0, "{data}");
}

#[test]
fn references_rust_repository_src_projects_within_existing_work_budget() {
    // Use this build's checkout, not a runtime Cargo environment variable or the
    // caller's working directory. Index a disposable copy through the same
    // native-backed snapshot helper as the small fixtures.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut pending = vec![root.join("src")];
    let mut sources = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            let path = entry.path();
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .replace('\\', "/");
                sources.push((relative, std::fs::read_to_string(path).unwrap()));
            }
        }
    }
    sources.sort_by(|a, b| a.0.cmp(&b.0));
    let files: Vec<_> = sources
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    let mut input = snapshot(&files);
    retain_rust_manifest(&mut input, &std::fs::read(root.join("Cargo.toml")).unwrap());
    assert_eq!(input.documents.len(), sources.len());
    for path in ["src/store.rs", "src/model.rs"] {
        assert!(input.documents.iter().any(|doc| doc.key.path == path));
    }
    for paths in [
        vec!["src/store.rs".into(), "src/model.rs".into()],
        vec!["src/**".into()],
    ] {
        let result = references::project(&input, &paths, 1, &mut || Ok(()));
        assert!(
            result.is_ok(),
            "repository src projection {paths:?} must fit the existing budget: {result:?}"
        );
        let data = result.unwrap().data;
        assert_eq!(
            data["counts"]["scannedDocuments"],
            if paths.len() == 2 { 2 } else { sources.len() },
            "{paths:?}"
        );
        assert!(data["counts"]["mappedSites"].as_u64().unwrap() > 0);
        let modules = module_paths(&data);
        for path in ["src/store.rs", "src/model.rs"] {
            assert!(modules.contains(&path), "{paths:?}: {path}");
        }
    }
}

fn project_java_source(input: &Snapshot, limit: usize) -> Value {
    references::project(input, &["app/Use.java".into()], limit, &mut || Ok(()))
        .unwrap()
        .data
}

#[test]
fn references_java_public_top_level_inventory_uses_declared_package_and_final_owners() {
    let input = snapshot(&[
        ("app/Use.java", "package app; import pkg.*; class Use {}"),
        (
            "odd/Class.java",
            "package pkg; public class C { public class Nested {} }",
        ),
        ("odd/Interface.java", "package pkg; public interface I {}"),
        ("odd/Enum.java", "package pkg; public enum E { VALUE }"),
        (
            "odd/Record.java",
            "package pkg; public record R(int value) {}",
        ),
        ("odd/Hidden.java", "package pkg; class Hidden {}"),
        ("odd/Child.java", "package pkg.child; public class Child {}"),
        ("pkg/Decoy.java", "package elsewhere; public class Decoy {}"),
        ("pkg/Default.java", "public class Default {}"),
    ]);
    let data = project_java_source(&input, 20);
    assert_eq!(data["production"].as_array().unwrap().len(), 4, "{data}");
    for target in [
        "odd/Class.java",
        "odd/Enum.java",
        "odd/Interface.java",
        "odd/Record.java",
    ] {
        assert_eq!(
            edge(&data, "production", "app/Use.java", target)["count"],
            1
        );
    }
    assert_eq!(
        module_paths(&data),
        [
            "app/Use.java",
            "odd/Class.java",
            "odd/Enum.java",
            "odd/Interface.java",
            "odd/Record.java"
        ]
    );
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
}

#[test]
fn references_java_single_import_and_top_level_type_shadow_only_the_matching_name() {
    for (source, explicit_target, unknowns) in [
        (
            "package app; import other.T; import pkg.*; class Use {}",
            Some("other/T.java"),
            0,
        ),
        (
            "package app; import pkg.*; import other.T; class Use {}",
            Some("other/T.java"),
            0,
        ),
        (
            "package app; import missing.T; import pkg.*; class Use {}",
            None,
            1,
        ),
        (
            "package app; import pkg.*; class T {} class Use {}",
            None,
            0,
        ),
    ] {
        let input = snapshot(&[
            ("app/Use.java", source),
            ("types/T.java", "package pkg; public class T {}"),
            ("types/Keep.java", "package pkg; public class Keep {}"),
            ("other/T.java", "package other; public class T {}"),
        ]);
        let data = project_java_source(&input, 20);
        assert!(
            !has_edge(&data, "app/Use.java", "types/T.java"),
            "{source}: {data}"
        );
        assert_eq!(
            edge(&data, "production", "app/Use.java", "types/Keep.java")["count"],
            1
        );
        assert_eq!(
            has_edge(&data, "app/Use.java", "other/T.java"),
            explicit_target.is_some()
        );
        assert_eq!(
            data["production"].as_array().unwrap().len(),
            1 + usize::from(explicit_target.is_some())
        );
        assert_eq!(
            data["counts"]["mappedSites"],
            1 + usize::from(explicit_target.is_some())
        );
        assert_eq!(data["counts"]["unresolvedSites"], unknowns);
    }
}

#[test]
fn references_java_inner_and_unrelated_types_do_not_shadow_compilation_unit_imports() {
    for source in [
        "package app; import pkg.*; class Use { void f() { class T {} } }",
        "package app; import pkg.*; class Use { class T {} }",
        "package app; import pkg.*; class Use<T> { <Keep> void f() {} }",
    ] {
        let input = snapshot(&[
            ("app/Use.java", source),
            ("app/Unrelated.java", "package elsewhere; class T {}"),
            ("types/T.java", "package pkg; public class T {}"),
            ("types/Keep.java", "package pkg; public class Keep {}"),
        ]);
        let data = project_java_source(&input, 20);
        assert_eq!(
            data["production"].as_array().unwrap().len(),
            2,
            "{source}: {data}"
        );
        for target in ["types/T.java", "types/Keep.java"] {
            assert_eq!(
                edge(&data, "production", "app/Use.java", target)["count"],
                1
            );
        }
        assert_eq!(data["counts"]["mappedSites"], 1);
        assert_eq!(data["counts"]["unresolvedSites"], 0);
    }
}

#[test]
fn references_java_duplicate_qualified_types_keep_partial_positives_and_full_catalog_collisions() {
    let mut input = snapshot(&[
        ("app/Use.java", "package app; import pkg.*; class Use {}"),
        ("types/Good.java", "package pkg; public class Good {}"),
        ("types/Also.java", "package pkg; public class Also {}"),
        ("one/T.java", "package pkg; public class T {}"),
        ("two/T.java", "package pkg; public class T {}"),
        (
            "unrelated/Other.java",
            "package other; import missing.*; public class Other {}",
        ),
    ]);
    let data = project_java_source(&input, 20);
    assert_eq!(data["production"].as_array().unwrap().len(), 2, "{data}");
    for target in ["types/Good.java", "types/Also.java"] {
        assert_eq!(
            edge(&data, "production", "app/Use.java", target)["count"],
            1
        );
    }
    for target in ["one/T.java", "two/T.java"] {
        assert!(!has_edge(&data, "app/Use.java", target));
    }
    assert_eq!(data["counts"]["scannedDocuments"], 1);
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
    assert_eq!(data["unresolved"].as_array().unwrap().len(), 1);
    assert_eq!(data["unresolved"][0]["site"]["spelling"], "import pkg.*;");
    assert_eq!(
        module_paths(&data),
        ["app/Use.java", "types/Also.java", "types/Good.java"]
    );
    let all = project(&input);
    let source_rows: Vec<_> = all["production"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["source"]["path"] == "app/Use.java")
        .cloned()
        .collect();
    assert_eq!(data["production"], serde_json::json!(source_rows));
    let source_unknowns: Vec<_> = all["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["source"]["path"] == "app/Use.java")
        .cloned()
        .collect();
    assert_eq!(data["unresolved"], serde_json::json!(source_unknowns));
    input.documents.reverse();
    for doc in &mut input.documents {
        doc.declarations.reverse();
    }
    assert_eq!(data, project_java_source(&input, 20));
}

#[test]
fn references_java_competing_package_wildcards_never_choose_distinct_same_named_types() {
    for source in [
        "package app; import left.*; import right.*; class Use {}",
        "package app; import right.*; import left.*; class Use {}",
    ] {
        let input = snapshot(&[
            ("app/Use.java", source),
            ("left/T.java", "package left; public class T {}"),
            ("right/T.java", "package right; public class T {}"),
            ("left/Good.java", "package left; public class Good {}"),
            ("right/Other.java", "package right; public class Other {}"),
        ]);
        let data = project_java_source(&input, 20);
        assert_eq!(data["production"].as_array().unwrap().len(), 2, "{data}");
        for target in ["left/Good.java", "right/Other.java"] {
            assert_eq!(
                edge(&data, "production", "app/Use.java", target)["count"],
                1
            );
        }
        for target in ["left/T.java", "right/T.java"] {
            assert!(!has_edge(&data, "app/Use.java", target));
        }
        assert_eq!(data["counts"]["mappedSites"], 2);
        assert_eq!(data["counts"]["unresolvedSites"], 2);
        let spellings: Vec<_> = data["unresolved"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["site"]["spelling"].as_str().unwrap())
            .collect();
        assert_eq!(spellings.len(), 2);
        assert!(spellings.contains(&"import left.*;"));
        assert!(spellings.contains(&"import right.*;"));
    }
}

#[test]
fn references_java_repeated_identical_imports_deduplicate_candidates_not_observed_sites() {
    let source = "package app;\nimport pkg.*;\nimport pkg.*;\nclass Use { T value; }";
    let input = snapshot(&[
        ("app/Use.java", source),
        ("types/T.java", "package pkg; public class T {}"),
    ]);
    let complete = project_java_source(&input, 20);
    let clipped = project_java_source(&input, 1);
    for data in [&complete, &clipped] {
        let pair = edge(data, "production", "app/Use.java", "types/T.java");
        assert_eq!(pair["count"], 2, "{data}");
        assert_eq!(data["counts"]["mappedSites"], 2);
        assert_eq!(data["counts"]["unresolvedSites"], 0);
        assert_eq!(data["production"].as_array().unwrap().len(), 1);
        assert_eq!(module_paths(data), ["app/Use.java", "types/T.java"]);
    }
    assert_eq!(complete["modules"], clipped["modules"]);
    assert_eq!(
        edge(&complete, "production", "app/Use.java", "types/T.java")["sites"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        edge(&clipped, "production", "app/Use.java", "types/T.java")["sites"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        edge(&clipped, "production", "app/Use.java", "types/T.java")["omittedSites"],
        1
    );

    let named = snapshot(&[
        (
            "app/Use.java",
            "package app; import pkg.T; import pkg.T; class Use { T value; }",
        ),
        ("types/T.java", "package pkg; public class T {}"),
    ]);
    let data = project_java_source(&named, 20);
    let pair = edge(&data, "production", "app/Use.java", "types/T.java");
    assert_eq!(pair["count"], 2, "{data}");
    assert_eq!(data["counts"]["mappedSites"], 2);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
    assert_eq!(data["production"].as_array().unwrap().len(), 1);
    let spellings: Vec<_> = pair["sites"]
        .as_array()
        .unwrap()
        .iter()
        .map(|site| site["spelling"].as_str().unwrap())
        .collect();
    assert_eq!(spellings, ["import pkg.T;", "import pkg.T;"]);
}

#[test]
fn references_java_empty_and_fully_shadowed_packages_keep_distinct_visible_coverage() {
    let mut reasons = Vec::new();
    for (source, provider) in [
        (
            "package app; import pkg.*; class Use {}",
            "package pkg; class Hidden {}",
        ),
        (
            "package app; import pkg.*; class T {} class Use {}",
            "package pkg; public class T {}",
        ),
    ] {
        let input = snapshot(&[("app/Use.java", source), ("types/Provider.java", provider)]);
        let data = project_java_source(&input, 20);
        assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
        assert_eq!(data["counts"]["mappedSites"], 0);
        assert_eq!(data["counts"]["unresolvedSites"], 1);
        assert_eq!(data["unresolved"].as_array().unwrap().len(), 1);
        reasons.push(data["unresolved"][0]["reason"].as_str().unwrap().to_owned());
    }
    assert_ne!(reasons[0], reasons[1]);
}

#[test]
fn references_java_single_type_and_maximal_qualified_type_are_independent_sites() {
    let input = snapshot(&[
        (
            "app/Use.java",
            "package app; import pkg.T; class Use { pkg.T value; }",
        ),
        ("types/T.java", "package pkg; public class T {}"),
    ]);
    let data = project_java_source(&input, 20);
    let pair = edge(&data, "production", "app/Use.java", "types/T.java");
    assert_eq!(pair["count"], 2, "{data}");
    assert_eq!(pair["sites"][0]["spelling"], "import pkg.T;");
    assert_eq!(pair["sites"][0]["relationKind"], "import");
    assert_eq!(pair["sites"][1]["spelling"], "pkg.T");
    assert_eq!(pair["sites"][1]["relationKind"], "reference");
    assert_eq!(data["counts"]["mappedSites"], 2);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
}

#[test]
fn references_java_static_and_nested_type_imports_remain_unsupported() {
    let input = snapshot(&[
        (
            "app/Use.java",
            "package app; import static pkg.Outer.*; import static pkg.Outer.member; import pkg.Outer.Inner; class Use {}",
        ),
        (
            "types/Outer.java",
            "package pkg; public class Outer { public static class Inner {} public static int member; }",
        ),
    ]);
    let data = project_java_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
    assert!(data["externalPackages"].as_array().unwrap().is_empty());
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert_eq!(data["counts"]["unresolvedSites"], 3);
    let spellings: Vec<_> = data["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["site"]["spelling"].as_str().unwrap())
        .collect();
    assert_eq!(
        spellings,
        [
            "import static pkg.Outer.*;",
            "import static pkg.Outer.member;",
            "import pkg.Outer.Inner;"
        ]
    );
}

#[test]
fn references_java_unsupported_known_export_and_missing_native_proof_keep_other_names() {
    for remove_native in [false, true] {
        let uncertain = if remove_native {
            "package pkg; public class Uncertain {}"
        } else {
            "package pkg; public @interface Uncertain {}"
        };
        let mut input = snapshot(&[
            ("app/Use.java", "package app; import pkg.*; class Use {}"),
            ("types/Good.java", "package pkg; public class Good {}"),
            ("types/Uncertain.java", uncertain),
        ]);
        if remove_native {
            let doc = input
                .documents
                .iter_mut()
                .find(|doc| doc.key.path == "types/Uncertain.java")
                .unwrap();
            doc.declarations
                .retain(|row| row.name.as_deref() != Some("Uncertain"));
        }
        let data = project_java_source(&input, 20);
        assert_eq!(data["production"].as_array().unwrap().len(), 1, "{data}");
        assert_eq!(
            edge(&data, "production", "app/Use.java", "types/Good.java")["count"],
            1
        );
        assert!(!has_edge(&data, "app/Use.java", "types/Uncertain.java"));
        assert_eq!(data["counts"]["mappedSites"], 1);
        assert_eq!(data["counts"]["unresolvedSites"], 1);
        assert_eq!(data["unresolved"].as_array().unwrap().len(), 1);
    }
}

#[test]
fn references_java_source_roles_ranges_and_native_proof_closure_survive_clipping() {
    let input = snapshot(&[
        (
            "src/main/java/app/Use.java",
            "package app;\nimport pkg.*;\nimport pkg.*;\nclass Use {}",
        ),
        (
            "src/test/java/app/UseTest.java",
            "package app;\nimport pkg.*;\nclass UseTest {}",
        ),
        ("providers/T.java", "package pkg; public class T {}"),
        (
            "providers/Unrelated.java",
            "package other; public class Unrelated {}",
        ),
    ]);
    let paths = [
        "src/main/java/app/Use.java".into(),
        "src/test/java/app/UseTest.java".into(),
    ];
    let complete = references::project(&input, &paths, 20, &mut || Ok(()))
        .unwrap()
        .data;
    let clipped = references::project(&input, &paths, 1, &mut || Ok(()))
        .unwrap()
        .data;
    for data in [&complete, &clipped] {
        assert_eq!(
            module_paths(data),
            [
                "providers/T.java",
                "src/main/java/app/Use.java",
                "src/test/java/app/UseTest.java"
            ]
        );
        assert_eq!(data["counts"]["mappedSites"], 3);
        assert_eq!(data["counts"]["unresolvedSites"], 0);
        for (section, role, path, count) in [
            ("production", "production", "src/main/java/app/Use.java", 2),
            ("tests", "test", "src/test/java/app/UseTest.java", 1),
        ] {
            let pair = edge(data, section, path, "providers/T.java");
            assert_eq!(pair["count"], count);
            let doc = input
                .documents
                .iter()
                .find(|doc| doc.key.path == path)
                .unwrap();
            let owner = doc
                .declarations
                .iter()
                .find(|row| row.kind == "module" && row.ancestors.is_empty())
                .unwrap();
            assert_eq!(serde_json::to_value(owner.role).unwrap(), role);
            for site in pair["sites"].as_array().unwrap() {
                let start = site["range"]["start"].as_u64().unwrap() as usize;
                let end = site["range"]["end"].as_u64().unwrap() as usize;
                assert_eq!(&doc.file.text[start..end], "import pkg.*;");
                assert_eq!(site["spelling"], "import pkg.*;");
                assert_eq!(site["ownerSyntaxId"], owner.syntax_id);
                assert_eq!(site["role"], role);
                assert_eq!(site["relationKind"], "import");
                assert!(site["callId"].is_null());
                assert_eq!(
                    site["lines"]["start"],
                    doc.file.text[..start]
                        .bytes()
                        .filter(|byte| *byte == b'\n')
                        .count()
                        + 1
                );
                assert_eq!(site["lines"]["end"], site["lines"]["start"]);
            }
        }
        for module in data["modules"].as_array().unwrap() {
            let path = module["identity"]["path"].as_str().unwrap();
            let doc = input
                .documents
                .iter()
                .find(|doc| doc.key.path == path)
                .unwrap();
            let native = doc
                .declarations
                .iter()
                .find(|row| row.kind == "module" && row.ancestors.is_empty())
                .unwrap();
            assert_eq!(module["identity"]["sid"], native.syntax_id);
            assert_eq!(module["revisionId"], native.revision_id);
            assert_eq!(module["provenanceId"], native.provenance_id);
            assert_eq!(module["contentHash"], doc.file.hash);
        }
    }
    assert_eq!(complete["modules"], clipped["modules"]);
    assert_eq!(
        edge(
            &clipped,
            "production",
            "src/main/java/app/Use.java",
            "providers/T.java"
        )["omittedSites"],
        1
    );
}

#[test]
fn references_java_standard_library_labels_are_package_only_and_share_descriptor_identity() {
    let input = snapshot(&[(
        "app/Use.java",
        "package app; import java.util.*; import java.util.List; import javax.swing.*; class Use { java.util.List value; }",
    )]);
    let data = project_java_source(&input, 20);
    assert_eq!(data["referencesVersion"], "references:v2");
    assert_eq!(data["production"].as_array().unwrap().len(), 2, "{data}");
    assert_eq!(data["externalPackages"].as_array().unwrap().len(), 2);
    assert_eq!(data["counts"]["mappedSites"], 4);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
    assert_eq!(module_paths(&data), ["app/Use.java"]);
    let source_set = &input.documents[0].key.source_set_id;
    for package in ["java", "javax"] {
        let pair = data["production"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["target"]["package"] == package)
            .unwrap();
        assert_eq!(
            pair["target"],
            serde_json::json!({"kind":"externalPackage", "sourceSetId":source_set, "language":"java", "package":package, "standardLibrary":true})
        );
        assert_eq!(pair["count"], if package == "java" { 3 } else { 1 });
        let descriptor = data["externalPackages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["identity"] == pair["target"])
            .unwrap();
        assert_eq!(descriptor["ecosystem"], "java");
        let evidence = descriptor["evidence"].as_array().unwrap();
        assert!(!evidence.is_empty());
        for item in evidence {
            assert_eq!(item["kind"], "standardLibraryCatalog");
            assert_eq!(item["entry"], package);
            assert!(!item["catalog"].as_str().unwrap().is_empty());
        }
    }
}

#[test]
fn references_java_external_wildcards_do_not_erase_proved_local_names_or_fix_local_collisions() {
    for (external, unknowns) in [("java.util", 0), ("thirdparty", 1)] {
        let source =
            format!("package app; import pkg.*; import {external}.*; class Use {{ Local value; }}");
        let input = snapshot(&[
            ("app/Use.java", &source),
            ("types/Local.java", "package pkg; public class Local {}"),
        ]);
        let data = project_java_source(&input, 20);
        assert_eq!(
            edge(&data, "production", "app/Use.java", "types/Local.java")["count"],
            1,
            "{data}"
        );
        assert_eq!(
            data["counts"]["mappedSites"],
            if unknowns == 0 { 2 } else { 1 }
        );
        assert_eq!(data["counts"]["unresolvedSites"], unknowns);
        assert_eq!(
            data["production"].as_array().unwrap().len(),
            if unknowns == 0 { 2 } else { 1 }
        );
    }
    let input = snapshot(&[
        (
            "app/Use.java",
            "package app; import pkg.*; import java.util.*; class Use {}",
        ),
        ("one/T.java", "package pkg; public class T {}"),
        ("two/T.java", "package pkg; public class T {}"),
    ]);
    let data = project_java_source(&input, 20);
    assert_eq!(data["production"].as_array().unwrap().len(), 1);
    assert_eq!(data["production"][0]["target"]["package"], "java");
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
    assert_eq!(module_paths(&data), ["app/Use.java"]);
}

#[test]
fn references_java_undeclared_and_near_standard_library_roots_stay_unknown() {
    let input = snapshot(&[(
        "app/Use.java",
        "package app; import org.example.*; import javaish.T; import javaxfoo.*; class Use {}",
    )]);
    let data = project_java_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
    assert!(data["externalPackages"].as_array().unwrap().is_empty());
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert_eq!(data["counts"]["unresolvedSites"], 3);
    assert_eq!(data["unresolved"].as_array().unwrap().len(), 3);
}

#[test]
fn references_java_cancellation_is_atomic_with_a_native_backed_inventory() {
    let input = snapshot(&[
        ("app/Use.java", "package app; import pkg.*; class Use {}"),
        ("types/T.java", "package pkg; public class T {}"),
    ]);
    let error =
        references::project(&input, &[], 20, &mut || anyhow::bail!("cancelled")).unwrap_err();
    assert!(error.to_string().contains("cancelled"));
}

#[test]
fn references_java_qualified_type_roots_respect_lexical_type_binders_and_block_boundaries() {
    for (body, positive_occurrences) in [
        (
            "class Use<pkg> { pkg.T blocked; } class After { pkg.T good; }",
            vec![1],
        ),
        (
            "class Use { <pkg> void f() { pkg.T blocked; } void g() { pkg.T good; } }",
            vec![1],
        ),
        (
            "class Use { <pkg> Use() { pkg.T blocked; } void g() { pkg.T good; } }",
            vec![1],
        ),
        (
            "class Use { void f() { pkg.T before; { class pkg {} pkg.T blocked; } pkg.T after; } }",
            vec![0, 2],
        ),
        (
            "class Use { void f(int x) { switch (x) { case 0: class pkg {} break; default: pkg.T blocked; } pkg.T after; } }",
            vec![1],
        ),
        (
            "class Use { pkg.T blocked; class pkg {} } class After { pkg.T good; }",
            vec![1],
        ),
        (
            "enum Use { A; class pkg {} pkg.T blocked; } class After { pkg.T good; }",
            vec![1],
        ),
        (
            "record Use<pkg>() { void f() { pkg.T blocked; } } class After { pkg.T good; }",
            vec![1],
        ),
    ] {
        let source = format!("package app; import pkg.*; {body}");
        let input = snapshot(&[
            ("app/Use.java", &source),
            ("types/T.java", "package pkg; public class T {}"),
        ]);
        let data = project_java_source(&input, 20);
        let pair = edge(&data, "production", "app/Use.java", "types/T.java");
        let qualified_starts: Vec<_> = source
            .match_indices("pkg.T")
            .map(|(start, _)| start)
            .collect();
        let mut expected_positive_starts = vec![source.find("import pkg.*;").unwrap()];
        expected_positive_starts.extend(
            positive_occurrences
                .iter()
                .map(|&index| qualified_starts[index]),
        );
        let actual_positive_starts: Vec<_> = pair["sites"]
            .as_array()
            .unwrap()
            .iter()
            .map(|site| site["range"]["start"].as_u64().unwrap() as usize)
            .collect();
        assert_eq!(
            actual_positive_starts, expected_positive_starts,
            "{body}: {data}"
        );
        assert_eq!(pair["count"], expected_positive_starts.len());
        let expected_unknown_starts: Vec<_> = qualified_starts
            .iter()
            .enumerate()
            .filter(|(index, _)| !positive_occurrences.contains(index))
            .map(|(_, &start)| start)
            .collect();
        let actual_unknown_starts: Vec<_> = data["unresolved"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["site"]["range"]["start"].as_u64().unwrap() as usize)
            .collect();
        assert_eq!(
            actual_unknown_starts, expected_unknown_starts,
            "{body}: {data}"
        );
        assert_eq!(
            data["counts"]["mappedSites"],
            expected_positive_starts.len()
        );
        assert_eq!(
            data["counts"]["unresolvedSites"],
            expected_unknown_starts.len()
        );
    }
}

#[test]
fn references_java_same_package_types_shadow_on_demand_but_not_single_type_imports() {
    for (source, explicit) in [
        ("package app; import pkg.*; class Use {}", false),
        (
            "package app; import other.T; import pkg.*; class Use {}",
            true,
        ),
    ] {
        let input = snapshot(&[
            ("app/Use.java", source),
            ("app/T.java", "package app; public class T {}"),
            ("types/T.java", "package pkg; public class T {}"),
            ("types/Keep.java", "package pkg; public class Keep {}"),
            ("other/T.java", "package other; public class T {}"),
        ]);
        let data = project_java_source(&input, 20);
        assert!(!has_edge(&data, "app/Use.java", "types/T.java"), "{data}");
        assert!(!has_edge(&data, "app/Use.java", "app/T.java"));
        assert_eq!(has_edge(&data, "app/Use.java", "other/T.java"), explicit);
        assert_eq!(
            edge(&data, "production", "app/Use.java", "types/Keep.java")["count"],
            1
        );
        assert_eq!(
            data["production"].as_array().unwrap().len(),
            1 + usize::from(explicit)
        );
        assert_eq!(data["counts"]["mappedSites"], 1 + usize::from(explicit));
        assert_eq!(data["counts"]["unresolvedSites"], 0);
    }
}

#[test]
fn references_java_test_only_type_cannot_prove_a_production_wildcard_export() {
    let input = snapshot(&[
        ("app/Use.java", "package app; import pkg.*; class Use {}"),
        ("types/Good.java", "package pkg; public class Good {}"),
        (
            "tests/TestOnly.java",
            "package pkg; public class TestOnly {}",
        ),
    ]);
    let test_doc = input
        .documents
        .iter()
        .find(|doc| doc.key.path == "tests/TestOnly.java")
        .unwrap();
    let test_type = test_doc
        .declarations
        .iter()
        .find(|row| row.name.as_deref() == Some("TestOnly"))
        .unwrap();
    assert_eq!(serde_json::to_value(test_type.role).unwrap(), "test");
    let data = project_java_source(&input, 20);
    assert_eq!(data["production"].as_array().unwrap().len(), 1, "{data}");
    assert_eq!(
        edge(&data, "production", "app/Use.java", "types/Good.java")["count"],
        1
    );
    assert!(!has_edge(&data, "app/Use.java", "tests/TestOnly.java"));
    assert!(data["tests"].as_array().unwrap().is_empty());
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
}

#[test]
fn references_java_conflicting_single_type_imports_block_wildcard_fallback() {
    let input = snapshot(&[
        (
            "app/Use.java",
            "package app; import left.T; import right.T; import pkg.*; class Use {}",
        ),
        ("left/T.java", "package left; public class T {}"),
        ("right/T.java", "package right; public class T {}"),
        ("types/T.java", "package pkg; public class T {}"),
        ("types/Keep.java", "package pkg; public class Keep {}"),
    ]);
    let data = project_java_source(&input, 20);
    for target in ["types/T.java", "left/T.java", "right/T.java"] {
        assert!(!has_edge(&data, "app/Use.java", target), "{data}");
    }
    assert_eq!(data["production"].as_array().unwrap().len(), 1);
    let pair = edge(&data, "production", "app/Use.java", "types/Keep.java");
    assert_eq!(pair["count"], 1);
    assert_eq!(pair["sites"][0]["spelling"], "import pkg.*;");
    assert!(
        data["unresolved"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["site"]["spelling"] == "import pkg.*;")
    );
}

#[test]
fn references_java_unknown_static_on_demand_type_bindings_block_qualified_package_roots() {
    let input = snapshot(&[
        (
            "app/Use.java",
            "package app; import static unknown.Outer.*; class Use { pkg.T field; }",
        ),
        ("pkg/T.java", "package pkg; public class T {}"),
    ]);
    let data = project_java_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
    assert!(data["externalPackages"].as_array().unwrap().is_empty());
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert_eq!(data["counts"]["unresolvedSites"], 2);
    let sites: Vec<_> = data["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["site"]["spelling"].as_str().unwrap(),
                row["site"]["relationKind"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        sites,
        [
            ("import static unknown.Outer.*;", "import"),
            ("pkg.T", "reference"),
        ]
    );
    assert_eq!(module_paths(&data), ["app/Use.java"]);
}

fn java_review_maven_snapshot() -> &'static Snapshot {
    static INPUT: std::sync::OnceLock<Snapshot> = std::sync::OnceLock::new();
    INPUT.get_or_init(|| {
        let sources: Vec<_> = (0..500)
            .map(|index| {
                let next: Vec<_> = (1..=3).map(|offset| (index + offset) % 500).collect();
                let mut text = format!("package com.example.p{index:03};\n");
                for target in &next {
                    text.push_str(&format!("import com.example.p{target:03}.*;\n"));
                }
                text.push_str(&format!("\npublic class Service{index:03} {{\n"));
                for (field, target) in next.iter().enumerate() {
                    text.push_str(&format!(
                        "    private Service{target:03} dependency{field};\n"
                    ));
                }
                text.push_str("    private int value;\n\n");
                for method in 0..20 {
                    text.push_str(&format!(
                        r#"    public int calculate{method}(int amount, int multiplier) {{
        int base = (value + amount) * (multiplier + 1);
        int weight = (amount + value) * (multiplier + 2) + (amount & 255);
        int result = (base + weight) * (multiplier + 3) - (value & 15);
        for (int step = 0; step < 4; step++) {{
            result = (result * 31 + amount * step) ^ (multiplier + step + weight);
        }}
        return (result + base) * (multiplier + value) + (weight & 1023);
    }}
"#
                    ));
                }
                text.push_str("}\n");
                assert_eq!(text.lines().count(), 192);
                (
                    format!("src/main/java/com/example/p{index:03}/Service{index:03}.java"),
                    text,
                )
            })
            .collect();
        let files: Vec<_> = sources
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect();
        let input = snapshot(&files);
        assert_eq!(input.documents.len(), 500);
        input
    })
}

fn assert_java_review_maven_projection(paths: &[String], all: bool) {
    let input = java_review_maven_snapshot();
    let result = references::project(input, paths, 1, &mut || Ok(()));
    assert!(
        result.is_ok(),
        "500-file Maven Java projection {paths:?} must fit the unchanged work budget: {result:?}"
    );
    let data = result.unwrap().data;
    let scanned = if all { 500 } else { 1 };
    assert_eq!(data["counts"]["scannedDocuments"], scanned);
    assert_eq!(data["counts"]["mappedSites"], scanned * 3);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
    assert!(data["unresolved"].as_array().unwrap().is_empty());
    assert!(data["tests"].as_array().unwrap().is_empty());
    assert!(data["externalPackages"].as_array().unwrap().is_empty());
    assert_eq!(data["production"].as_array().unwrap().len(), scanned * 3);
    assert_eq!(module_paths(&data).len(), if all { 500 } else { 4 });
    for index in 0..scanned {
        let source = format!("src/main/java/com/example/p{index:03}/Service{index:03}.java");
        let doc = input
            .documents
            .iter()
            .find(|doc| doc.key.path == source)
            .unwrap();
        let owner = doc
            .declarations
            .iter()
            .find(|row| row.kind == "module" && row.ancestors.is_empty())
            .unwrap();
        for offset in 1..=3 {
            let target = (index + offset) % 500;
            let target_path =
                format!("src/main/java/com/example/p{target:03}/Service{target:03}.java");
            let pair = edge(&data, "production", &source, &target_path);
            assert_eq!(pair["count"], 1);
            assert_eq!(pair["mapping"], "structural");
            assert_eq!(pair["omittedSites"], 0);
            assert_eq!(pair["sites"].as_array().unwrap().len(), 1);
            let site = &pair["sites"][0];
            let spelling = format!("import com.example.p{target:03}.*;");
            assert_eq!(site["spelling"], spelling);
            assert_eq!(site["relationKind"], "import");
            assert_eq!(site["role"], "production");
            assert_eq!(site["ownerSyntaxId"], owner.syntax_id);
            let start = site["range"]["start"].as_u64().unwrap() as usize;
            let end = site["range"]["end"].as_u64().unwrap() as usize;
            assert_eq!(&doc.file.text[start..end], spelling);
        }
    }
    for module in data["modules"].as_array().unwrap() {
        let path = module["identity"]["path"].as_str().unwrap();
        let doc = input
            .documents
            .iter()
            .find(|doc| doc.key.path == path)
            .unwrap();
        let native = doc
            .declarations
            .iter()
            .find(|row| row.kind == "module" && row.ancestors.is_empty())
            .unwrap();
        assert_eq!(module["identity"]["sid"], native.syntax_id);
        assert_eq!(module["revisionId"], native.revision_id);
        assert_eq!(module["provenanceId"], native.provenance_id);
        assert_eq!(module["contentHash"], doc.file.hash);
    }
}

#[test]
fn references_java_review_maven_one_file_fits_unchanged_work_budget() {
    assert_java_review_maven_projection(
        &["src/main/java/com/example/p000/Service000.java".into()],
        false,
    );
}

#[test]
fn references_java_review_maven_all_files_fit_unchanged_work_budget() {
    assert_java_review_maven_projection(&["**".into()], true);
}

#[test]
fn references_java_review_nested_on_demand_collision_keeps_explicit_proof() {
    for explicit_t in [false, true] {
        let source = format!(
            "package app; import pkg.*; import other.Outer.*; import known.Keep; {} class Use {{ T value; }}",
            if explicit_t { "import pkg.T;" } else { "" }
        );
        let input = snapshot(&[
            ("app/Use.java", &source),
            ("pkg/T.java", "package pkg; public class T {}"),
            (
                "other/Outer.java",
                "package other; public class Outer { public static class T {} }",
            ),
            ("known/Keep.java", "package known; public class Keep {}"),
        ]);
        let data = project_java_source(&input, 20);
        assert_eq!(
            has_edge(&data, "app/Use.java", "pkg/T.java"),
            explicit_t,
            "nested and package on-demand imports must not select wildcard-only T: {data}"
        );
        assert!(!has_edge(&data, "app/Use.java", "other/Outer.java"));
        let keep = edge(&data, "production", "app/Use.java", "known/Keep.java");
        assert_eq!(keep["count"], 1);
        assert_eq!(keep["sites"][0]["spelling"], "import known.Keep;");
        if explicit_t {
            let pair = edge(&data, "production", "app/Use.java", "pkg/T.java");
            assert_eq!(pair["count"], 1);
            assert_eq!(pair["sites"][0]["spelling"], "import pkg.T;");
        }
        assert_eq!(
            data["production"].as_array().unwrap().len(),
            1 + usize::from(explicit_t)
        );
        assert_eq!(data["counts"]["scannedDocuments"], 1);
        assert_eq!(data["counts"]["mappedSites"], 1 + usize::from(explicit_t));
        assert_eq!(data["counts"]["unresolvedSites"], 2);
        let unknowns: Vec<_> = data["unresolved"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["site"]["spelling"].as_str().unwrap())
            .collect();
        assert_eq!(unknowns, ["import pkg.*;", "import other.Outer.*;"]);
        assert!(data["tests"].as_array().unwrap().is_empty());
        assert!(data["externalPackages"].as_array().unwrap().is_empty());
        assert_eq!(
            module_paths(&data),
            if explicit_t {
                vec!["app/Use.java", "known/Keep.java", "pkg/T.java"]
            } else {
                vec!["app/Use.java", "known/Keep.java"]
            }
        );
    }
}
