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
                semantic: Default::default(),
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
// Snapshot construction is outside the deadline. Each run covers references and
// rules with one shared absolute deadline, including the final serialization.
// The deadline is a hang backstop only; check speed is measured separately (#377).
fn scaled_checkout_projection(input: &Snapshot, sample_limit: usize) -> Value {
    let config = trellis::rules::parse(
        r#"version=1
[layers.checkout]
order=0
paths=["**"]
[roles.inherited]
select={extends="Base"}
[roles.any]
select={paths=["**"]}
[[rules]]
id="inherited-calls"
for_each="inherited"
require=[{calls="any"}]
"#,
    )
    .unwrap();
    let run = |workers: usize| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let mut check = || {
            anyhow::ensure!(std::time::Instant::now() < deadline, "deadline_exceeded");
            Ok(())
        };
        #[cfg(feature = "test-support")]
        let projection =
            references::project_with_workers(input, &[], sample_limit, workers, &mut check)
                .unwrap();
        #[cfg(not(feature = "test-support"))]
        let projection = {
            let _ = workers;
            references::project(input, &[], sample_limit, &mut check).unwrap()
        };
        assert_eq!(projection.data["counts"]["scannedDocuments"], 500);
        #[cfg(feature = "test-support")]
        let rules = trellis::rules::evaluate_with_workers(
            &config,
            input,
            &projection,
            &[],
            workers,
            &mut check,
        )
        .unwrap();
        #[cfg(not(feature = "test-support"))]
        let rules = trellis::rules::evaluate(&config, input, &projection, &[], &mut check).unwrap();
        assert!(rules["unassigned"].as_array().unwrap().is_empty());
        assert!(!rules["instances"].as_array().unwrap().is_empty());
        let bytes = (
            serde_json::to_vec(&projection.data).unwrap(),
            serde_json::to_vec(&rules).unwrap(),
        );
        check().unwrap();
        (projection.data, bytes)
    };
    let (serial, serial_bytes) = run(1);
    #[cfg(feature = "test-support")]
    {
        let (_, parallel_bytes) = run(4);
        assert_eq!(
            serial_bytes.0, parallel_bytes.0,
            "reference output changed with workers"
        );
        assert_eq!(
            serial_bytes.1, parallel_bytes.1,
            "rule output changed with workers"
        );
    }
    #[cfg(not(feature = "test-support"))]
    let _ = serial_bytes;
    serial
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
fn references_javascript_export_star_forwards_to_native_declaration_owner() {
    let source = "export * from './facade.js';\n";
    let input = snapshot(&[
        ("use.js", source),
        ("facade.js", "export { value } from './provider.js';\n"),
        ("provider.js", "export function value() {}\n"),
    ]);
    let data = references::project(&input, &["use.js".into()], 20, &mut || Ok(()))
        .unwrap()
        .data;
    assert!(has_edge(&data, "use.js", "provider.js"), "{data}");
    assert!(!has_edge(&data, "use.js", "facade.js"), "{data}");
    let pair = edge(&data, "production", "use.js", "provider.js");
    assert_eq!(pair["count"], 1);
    assert_eq!(pair["sites"].as_array().unwrap().len(), 1);
    let site = &pair["sites"][0];
    assert_eq!(site["range"]["start"], 0);
    assert_eq!(site["range"]["end"], source.trim_end().len());
    assert_eq!(site["spelling"], source.trim_end());
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
}

#[test]
fn references_python_direct_root_wildcard_expands_public_declaration_owner() {
    let input = snapshot(&[
        ("use.py", "from provider import *\n"),
        ("provider.py", "def public_name():\n    pass\n"),
    ]);
    let data = references::project(&input, &["use.py".into()], 20, &mut || Ok(()))
        .unwrap()
        .data;
    assert!(has_edge(&data, "use.py", "provider.py"), "{data}");
    assert_eq!(
        edge(&data, "production", "use.py", "provider.py")["count"],
        1
    );
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
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
    assert_eq!(production["evidenceTier"], "syntax");
    assert_eq!(data["semanticComplete"], false);
    for site in production["sites"].as_array().unwrap() {
        assert_eq!(site["role"], "production");
        assert!(site["callId"].as_str().unwrap().starts_with("occ:v2:"));
        assert!(site["lines"]["start"].as_u64().unwrap() > 0);
        // Structural sites carry no binding proof (#523).
        assert_eq!(site["mapping"], "structural");
        assert!(site["provenanceId"].is_null());
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
        ("pkg/__init__.py", ""),
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
    assert_eq!(
        edge(&data, "production", "pkg/a.py", "pkg/__init__.py")["count"],
        1
    );
    let python_unknowns: Vec<_> = data["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["source"]["path"] == "pkg/a.py")
        .collect();
    assert_eq!(python_unknowns.len(), 1);
    assert_eq!(python_unknowns[0]["site"]["spelling"], "external");
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
        ("pkg/__init__.py", ""),
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

#[cfg(feature = "test-support")]
#[test]
fn references_parallel_failures_are_typed_and_atomic() {
    let mut input = snapshot(&[
        (
            "src/lib.rs",
            "mod a; mod b; mod c; fn run() { crate::a::run(); }",
        ),
        ("src/a.rs", "pub fn run() {}"),
        ("src/b.rs", "pub fn run() {}"),
        ("src/c.rs", "pub fn run() {}"),
    ]);
    let last = input
        .documents
        .iter()
        .position(|doc| doc.key.path == "src/c.rs")
        .unwrap();
    for (source, code) in [
        ("fn broken( {".to_owned(), "invalid_source_syntax"),
        (" ".repeat(2 * 1024 * 1024 + 1), "bounded_work"),
    ] {
        input.documents[last].file.text = source;
        for workers in [1, 4] {
            // An Err has no projection: earlier successful files cannot escape.
            let error = references::project_with_workers(&input, &[], 5, workers, &mut || Ok(()))
                .unwrap_err();
            assert!(
                error.to_string().starts_with(code),
                "workers={workers}: {error:#}"
            );
        }
    }
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
fn references_rust_normal_checkout_all_sources_share_scaled_work_budget() {
    let methods = |count: usize| {
        (0..count)
            .map(|method| {
                format!(
                    r#"pub fn calculate{method}(amount: i64, multiplier: i64) -> i64 {{
    let base = (amount + 7) * (multiplier + 1);
    let weight = (amount + 11) * (multiplier + 2) + (amount & 255);
    let mut result = base + weight;
    for step in 0..4 {{
        if (step & 1) == 0 {{
            result = result * 31 + amount * step;
        }} else {{
            result = result - multiplier * step;
        }}
    }}
    (result + base) * (multiplier + 1) + (weight & 1023)
}}
"#
                )
            })
            .collect::<String>()
    };
    let mut root = String::new();
    for index in 0..499 {
        root.push_str(&format!("pub mod service_{index:03};"));
        if index % 5 == 4 {
            root.push('\n');
        }
    }
    root.push('\n');
    root.push_str(&methods(1));
    let body = methods(9);
    let mut files = vec![("src/lib.rs".to_owned(), root)];
    for index in 0..499 {
        let mut source = String::from("pub fn run() {\n");
        for offset in 1..=3 {
            let target = (index + offset) % 499;
            source.push_str(&format!(
                "    crate::service_{target:03}::calculate0(7, 3);\n"
            ));
        }
        source.push_str("}\n");
        source.push_str(&body);
        files.push((format!("src/service_{index:03}.rs"), source));
    }
    assert_eq!(files.len(), 500);
    assert!(
        files
            .iter()
            .all(|(_, text)| (100..=200).contains(&text.lines().count()))
    );
    let borrowed: Vec<_> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    let input = snapshot(&borrowed);
    assert_eq!(input.documents.len(), 500);
    let data = scaled_checkout_projection(&input, 1);
    assert_eq!(data["counts"]["mappedSites"], 499 * 3);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
    assert_eq!(data["production"].as_array().unwrap().len(), 499 * 3);
    assert_eq!(module_paths(&data).len(), 500);
    assert!(data["tests"].as_array().unwrap().is_empty());
    assert!(data["unresolved"].as_array().unwrap().is_empty());
    for index in 0..499 {
        let source = format!("src/service_{index:03}.rs");
        for offset in 1..=3 {
            let target = (index + offset) % 499;
            let pair = edge(
                &data,
                "production",
                &source,
                &format!("src/service_{target:03}.rs"),
            );
            assert_eq!(pair["count"], 1);
            assert_eq!(pair["sites"].as_array().unwrap().len(), 1);
            assert!(
                pair["sites"][0]["ownerSyntaxId"]
                    .as_str()
                    .is_some_and(|sid| !sid.is_empty())
            );
        }
    }
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
    let data = if all {
        scaled_checkout_projection(input, 1)
    } else {
        references::project(input, paths, 1, &mut || Ok(()))
            .unwrap()
            .data
    };
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
            assert_eq!(pair["evidenceTier"], "syntax");
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
fn references_java_review_maven_one_file_fits_scaled_work_budget() {
    assert_java_review_maven_projection(
        &["src/main/java/com/example/p000/Service000.java".into()],
        false,
    );
}

#[test]
fn references_java_review_maven_all_files_fit_scaled_work_budget() {
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

#[test]
fn references_javax_jdk_exact_packages_share_the_existing_family_identity() {
    let packages = [
        ("javax.crypto", "Cipher"),
        ("javax.net", "SocketFactory"),
        ("javax.net.ssl", "SSLContext"),
        ("javax.sql", "DataSource"),
        ("javax.xml.parsers", "DocumentBuilder"),
        ("javax.naming", "Context"),
        ("javax.management", "MBeanServer"),
        ("javax.security.auth", "Subject"),
        ("javax.swing", "JButton"),
        ("javax.tools", "JavaCompiler"),
        ("javax.lang.model", "SourceVersion"),
        ("javax.annotation.processing", "Processor"),
    ];
    let mut source = String::from("package app;\n");
    for (package, name) in packages {
        source.push_str(&format!("import {package}.*;\nimport {package}.{name};\n"));
    }
    source.push_str("class Use {\n");
    for (index, (package, name)) in packages.iter().enumerate() {
        source.push_str(&format!("    {package}.{name} field{index};\n"));
    }
    source.push_str("}\n");
    let input = snapshot(&[("app/Use.java", &source)]);
    let data = project_java_source(&input, 50);
    assert_eq!(data["production"].as_array().unwrap().len(), 1, "{data}");
    assert_eq!(data["externalPackages"].as_array().unwrap().len(), 1);
    assert_eq!(data["counts"]["mappedSites"], packages.len() * 3);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
    let pair = &data["production"][0];
    assert_eq!(
        pair["target"],
        serde_json::json!({"kind":"externalPackage", "sourceSetId":input.documents[0].key.source_set_id, "language":"java", "package":"javax", "standardLibrary":true})
    );
    assert_eq!(pair["count"], packages.len() * 3);
    assert_eq!(pair["omittedSites"], 0);
    let sites = pair["sites"].as_array().unwrap();
    for (package, name) in packages {
        for spelling in [
            format!("import {package}.*;"),
            format!("import {package}.{name};"),
            format!("{package}.{name}"),
        ] {
            assert!(sites.iter().any(|site| site["spelling"] == spelling));
        }
    }
    let descriptor = &data["externalPackages"][0];
    assert_eq!(descriptor["identity"], pair["target"]);
    assert_eq!(descriptor["ecosystem"], "java");
    for evidence in descriptor["evidence"].as_array().unwrap() {
        assert_eq!(evidence["kind"], "standardLibraryCatalog");
        assert_eq!(evidence["entry"], "javax");
        assert!(!evidence["catalog"].as_str().unwrap().is_empty());
    }
    assert_eq!(module_paths(&data), ["app/Use.java"]);
}

#[test]
fn references_javax_jdk_non_jdk_and_prefix_only_packages_remain_unknown() {
    let packages = [
        "javax.inject",
        "javax.servlet",
        "javax.persistence",
        "javax.ws.rs",
        "javax",
        "javax.security",
        "javax.swing.fake",
        "javax.swingish",
        "javax.net.ssl.fake",
        "javax.xml.bind",
    ];
    let mut source = String::from("package app;\n");
    let mut expected = Vec::new();
    for (index, package) in packages.iter().enumerate() {
        for spelling in [
            format!("import {package}.*;"),
            format!("import {package}.Widget{index};"),
        ] {
            source.push_str(&format!("{spelling}\n"));
            expected.push(spelling);
        }
    }
    source.push_str("class Use {\n");
    for (index, package) in packages.iter().enumerate() {
        let spelling = format!("{package}.Widget{index}");
        source.push_str(&format!("    {spelling} field{index};\n"));
        expected.push(spelling);
    }
    source.push_str("}\n");
    let input = snapshot(&[("app/Use.java", &source)]);
    let data = project_java_source(&input, 50);
    assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
    assert!(data["externalPackages"].as_array().unwrap().is_empty());
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert_eq!(data["counts"]["unresolvedSites"], expected.len());
    let actual: Vec<_> = data["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["site"]["spelling"].as_str().unwrap())
        .collect();
    assert_eq!(actual, expected);
    assert_eq!(module_paths(&data), ["app/Use.java"]);
}

#[test]
fn references_javax_jdk_native_ownership_and_ambiguity_never_fall_back_to_stdlib() {
    for ambiguous in [false, true] {
        let mut files = vec![
            (
                "app/Use.java",
                "package app; import javax.swing.*; import javax.swing.Local; class Use { javax.swing.Local field; }",
            ),
            (
                "types/Local.java",
                "package javax.swing; public class Local {}",
            ),
        ];
        if ambiguous {
            files.push((
                "other/Local.java",
                "package javax.swing; public class Local {}",
            ));
        }
        let input = snapshot(&files);
        let data = project_java_source(&input, 20);
        assert!(
            data["externalPackages"].as_array().unwrap().is_empty(),
            "{data}"
        );
        assert_eq!(data["counts"]["mappedSites"], if ambiguous { 0 } else { 2 });
        assert_eq!(
            data["counts"]["unresolvedSites"],
            if ambiguous { 3 } else { 1 }
        );
        if ambiguous {
            assert!(data["production"].as_array().unwrap().is_empty());
        } else {
            let pair = edge(&data, "production", "app/Use.java", "types/Local.java");
            assert_eq!(pair["count"], 2);
            assert_eq!(pair["sites"][0]["spelling"], "import javax.swing.Local;");
            assert_eq!(pair["sites"][1]["spelling"], "javax.swing.Local");
        }
    }
    let input = snapshot(&[
        (
            "app/Use.java",
            "package app; import pkg.*; import javax.servlet.*; class Use { Local field; }",
        ),
        ("types/Local.java", "package pkg; public class Local {}"),
    ]);
    let data = project_java_source(&input, 20);
    assert_eq!(
        edge(&data, "production", "app/Use.java", "types/Local.java")["count"],
        1
    );
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
    assert_eq!(
        data["unresolved"][0]["site"]["spelling"],
        "import javax.servlet.*;"
    );
    assert!(data["externalPackages"].as_array().unwrap().is_empty());
}

fn project_python_source(input: &Snapshot, limit: usize) -> Value {
    references::project(input, &["use.py".into()], limit, &mut || Ok(()))
        .unwrap()
        .data
}

#[test]
fn references_python_public_top_level_names_share_one_owner_edge() {
    for definition in [
        "def public_name():\n    pass\n",
        "class PublicType:\n    pass\n",
        "VALUE = 1\n",
    ] {
        let input = snapshot(&[
            ("use.py", "from provider import *\n"),
            ("provider.py", definition),
        ]);
        let data = project_python_source(&input, 20);
        assert_eq!(
            edge(&data, "production", "use.py", "provider.py")["count"],
            1
        );
        assert_eq!(data["counts"]["mappedSites"], 1);
        assert_eq!(data["counts"]["unresolvedSites"], 0);
    }
    let input = snapshot(&[
        ("use.py", "from provider import *\n"),
        (
            "provider.py",
            "def function():\n    pass\nclass PublicType:\n    def method(self):\n        pass\nVALUE = 1\ndef _private():\n    pass\n",
        ),
    ]);
    let data = project_python_source(&input, 20);
    assert_eq!(data["production"].as_array().unwrap().len(), 1, "{data}");
    assert_eq!(
        edge(&data, "production", "use.py", "provider.py")["count"],
        1
    );
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
    for private_only in [
        "def _private():\n    pass\n",
        "class _Private:\n    def public_method(self):\n        pass\n",
        "def _outer():\n    def public_nested():\n        pass\n",
        "_PRIVATE = 1\n",
    ] {
        let input = snapshot(&[
            ("use.py", "from provider import *\n"),
            ("provider.py", private_only),
        ]);
        let data = project_python_source(&input, 20);
        assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
        assert_eq!(data["counts"]["mappedSites"], 0);
        assert_eq!(data["counts"]["unresolvedSites"], 1);
    }
}

#[test]
fn references_python_literal_all_selects_exact_names_including_private_and_empty() {
    for (selection, expected_owner) in [
        ("['public_name']", Some("public.py")),
        ("['_private']", Some("private.py")),
        ("['public_name', 'public_name']", Some("public.py")),
        ("[]", None),
    ] {
        let provider = format!(
            "from public import public_name\nfrom private import _private\n__all__ = {selection}\n"
        );
        let input = snapshot(&[
            ("use.py", "from provider import *\n"),
            ("provider.py", &provider),
            ("public.py", "def public_name():\n    pass\n"),
            ("private.py", "def _private():\n    pass\n"),
        ]);
        let data = project_python_source(&input, 20);
        match expected_owner {
            Some(owner) => {
                assert_eq!(data["production"].as_array().unwrap().len(), 1, "{data}");
                assert_eq!(edge(&data, "production", "use.py", owner)["count"], 1);
                assert_eq!(data["counts"]["mappedSites"], 1);
                assert_eq!(data["counts"]["unresolvedSites"], 0);
            }
            None => {
                assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
                assert_eq!(data["counts"]["mappedSites"], 0);
                assert_eq!(data["counts"]["unresolvedSites"], 1);
            }
        }
        assert!(!has_edge(&data, "use.py", "provider.py"));
    }
}

#[test]
fn references_python_all_literal_names_do_not_evaluate_expressions_or_mutations() {
    for exports in [
        "__all__ = names\n",
        "__all__ = ('public_name',)\n",
        "__all__ = ['public_' + 'name']\n",
        "__all__ = [f'public_name']\n",
        "__all__ = [*names]\n",
        "__all__ = [name]\n",
        "__all__ = ['public_name']\n__all__ = names\n",
        "__all__ = ['public_name']\n__all__ += names\n",
        "__all__ = ['public_name']\n__all__.append('other')\n",
        "__all__ = ['public_name']\n__all__[0] = 'other'\n",
        "__all__ = ['public_name']\ndef mutate():\n    __all__.append('other')\n",
        "__all__ = ['public_name']\ndef mutate():\n    __all__[0] = 'other'\n",
        "if enabled:\n    __all__ = ['public_name']\n",
        "try:\n    __all__ = ['public_name']\nexcept Exception:\n    pass\n",
    ] {
        let provider = format!("def public_name():\n    pass\n{exports}");
        let input = snapshot(&[
            ("use.py", "from provider import *\n"),
            ("provider.py", &provider),
        ]);
        let data = project_python_source(&input, 20);
        assert!(
            data["production"].as_array().unwrap().is_empty(),
            "{exports}: {data}"
        );
        assert_eq!(data["counts"]["mappedSites"], 0, "{exports}");
        assert_eq!(data["counts"]["unresolvedSites"], 1, "{exports}");
    }
}

#[test]
fn references_python_all_missing_name_keeps_independent_proved_name() {
    let input = snapshot(&[
        ("use.py", "from provider import *\n"),
        (
            "provider.py",
            "def good():\n    pass\n__all__ = ['good', 'missing']\n",
        ),
    ]);
    let data = project_python_source(&input, 20);
    assert_eq!(
        edge(&data, "production", "use.py", "provider.py")["count"],
        1
    );
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
    assert_eq!(data["unresolved"].as_array().unwrap().len(), 1);
}

#[test]
fn references_python_literal_all_preserves_string_spelling_not_source_tokens() {
    for exports in [
        "__all__ = [\"public_name\"]\n",
        "__all__ = ['public_name']\n",
        "__all__ = ['public\\x5fname']\n",
        "__all__ = ['public\\u005fname']\n",
        "__all__ = ['public\\U0000005fname']\n",
        "__all__ = ['''public_name''']\n",
        "__all__ = [r'public_name']\n",
    ] {
        let provider = format!("def public_name():\n    pass\n{exports}");
        let input = snapshot(&[
            ("use.py", "from provider import *\n"),
            ("provider.py", &provider),
        ]);
        let data = project_python_source(&input, 20);
        assert_eq!(
            edge(&data, "production", "use.py", "provider.py")["count"],
            1
        );
        assert_eq!(data["counts"]["unresolvedSites"], 0);
    }

    let input = snapshot(&[
        ("use.py", "from provider import *\n"),
        (
            "provider.py",
            "def public_name():\n    pass\n__all__ = ['public\\N{LOW LINE}name']\n",
        ),
    ]);
    let data = project_python_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
}

#[test]
fn references_python_dynamic_namespace_cannot_prove_wildcard_uniqueness() {
    for dynamic in [
        "exec(source)\n",
        "globals()[name] = value\n",
        "def mutate():\n    globals().update({\"public_name\": replacement})\n",
    ] {
        let provider = format!("def public_name():\n    pass\n{dynamic}");
        let input = snapshot(&[
            ("use.py", "from provider import *\n"),
            ("provider.py", &provider),
        ]);
        let data = project_python_source(&input, 20);
        assert!(
            data["production"].as_array().unwrap().is_empty(),
            "{dynamic}: {data}"
        );
        assert_eq!(data["counts"]["mappedSites"], 0);
        assert_eq!(data["counts"]["unresolvedSites"], 1);
    }
    let input = snapshot(&[
        ("use.py", "from provider import *\n"),
        (
            "provider.py",
            "def good():\n    pass\nif enabled:\n    def maybe():\n        pass\n",
        ),
    ]);
    let data = project_python_source(&input, 20);
    assert_eq!(
        edge(&data, "production", "use.py", "provider.py")["count"],
        1
    );
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
}

#[test]
fn references_python_missing_or_nonunique_native_export_proof_is_not_invented() {
    for duplicate in [false, true] {
        let mut input = snapshot(&[
            ("use.py", "from provider import *\n"),
            (
                "provider.py",
                "def good():\n    pass\ndef uncertain():\n    pass\n",
            ),
        ]);
        let provider = input
            .documents
            .iter_mut()
            .find(|doc| doc.key.path == "provider.py")
            .unwrap();
        let row = provider
            .declarations
            .iter()
            .find(|row| row.name.as_deref() == Some("uncertain"))
            .unwrap()
            .clone();
        if duplicate {
            let mut other = row;
            other.syntax_id = "sid:v1:duplicate-python-export".into();
            provider.declarations.push(other);
        } else {
            provider
                .declarations
                .retain(|row| row.name.as_deref() != Some("uncertain"));
        }
        let data = project_python_source(&input, 20);
        assert_eq!(
            edge(&data, "production", "use.py", "provider.py")["count"],
            1
        );
        assert_eq!(data["counts"]["mappedSites"], 1);
        assert_eq!(data["counts"]["unresolvedSites"], 1);
    }
}

#[test]
fn references_python_explicit_local_and_unproved_names_block_matching_star_names() {
    for source in [
        "from provider import *\ndef same():\n    pass\n",
        "def same():\n    pass\nfrom provider import *\n",
        "from provider import *\nfrom missing import original as same\n",
    ] {
        let input = snapshot(&[
            ("use.py", source),
            ("provider.py", "def same():\n    pass\n"),
        ]);
        let data = project_python_source(&input, 20);
        assert!(
            !has_edge(&data, "use.py", "provider.py"),
            "{source}: {data}"
        );
        assert_eq!(data["counts"]["mappedSites"], 0);
        assert!(!data["unresolved"].as_array().unwrap().is_empty());
    }
    let input = snapshot(&[
        (
            "use.py",
            "from provider import *\nfrom other import original as same\n",
        ),
        ("provider.py", "def same():\n    pass\n"),
        ("other.py", "def original():\n    pass\n"),
    ]);
    let data = project_python_source(&input, 20);
    assert!(!has_edge(&data, "use.py", "provider.py"), "{data}");
    assert_eq!(edge(&data, "production", "use.py", "other.py")["count"], 1);
    let input = snapshot(&[
        (
            "use.py",
            "from provider import *\ndef unrelated():\n    same = 1\n",
        ),
        ("provider.py", "def same():\n    pass\n"),
    ]);
    let data = project_python_source(&input, 20);
    assert_eq!(
        edge(&data, "production", "use.py", "provider.py")["count"],
        1
    );
    assert_eq!(data["counts"]["unresolvedSites"], 0);
}

#[test]
fn references_python_distinct_star_names_collide_without_source_order_precedence() {
    for source in [
        "from left import *\nfrom right import *\n",
        "from right import *\nfrom left import *\n",
    ] {
        let input = snapshot(&[
            ("use.py", source),
            ("left.py", "def same():\n    pass\ndef good():\n    pass\n"),
            ("right.py", "def same():\n    pass\n"),
        ]);
        let data = project_python_source(&input, 20);
        assert_eq!(data["production"].as_array().unwrap().len(), 1, "{data}");
        assert_eq!(edge(&data, "production", "use.py", "left.py")["count"], 1);
        assert!(!has_edge(&data, "use.py", "right.py"));
        assert_eq!(data["counts"]["mappedSites"], 1);
        assert_eq!(data["counts"]["unresolvedSites"], 2);
    }
}

#[test]
fn references_python_external_and_unproved_star_sets_veto_local_wildcard_names() {
    for external in ["json", "not_a_proved_package"] {
        for source in [
            format!("from provider import *\nfrom {external} import *\n"),
            format!("from {external} import *\nfrom provider import *\n"),
        ] {
            let input = snapshot(&[
                ("use.py", &source),
                ("provider.py", "def public_name():\n    pass\n"),
            ]);
            let data = project_python_source(&input, 20);
            assert!(!has_edge(&data, "use.py", "provider.py"), "{data}");
            assert_eq!(data["counts"]["unresolvedSites"], 2);
            if external == "json" {
                let pairs = data["production"].as_array().unwrap();
                assert_eq!(pairs.len(), 1, "{data}");
                assert_eq!(pairs[0]["target"]["kind"], "externalPackage");
                assert_eq!(pairs[0]["target"]["package"], "json");
                assert_eq!(pairs[0]["count"], 1);
                assert_eq!(data["counts"]["mappedSites"], 1);
            } else {
                assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
                assert!(data["externalPackages"].as_array().unwrap().is_empty());
                assert_eq!(data["counts"]["mappedSites"], 0);
            }
        }
    }
}

#[test]
fn references_python_literal_alias_forwarding_uses_original_owner_not_facade() {
    let input = snapshot(&[
        ("use.py", "from pkg import *\n"),
        (
            "pkg/__init__.py",
            "from .model import original as alias\n__all__ = ['alias']\n",
        ),
        ("pkg/model.py", "def original():\n    pass\n"),
        ("unrelated.py", "def unused():\n    pass\n"),
    ]);
    let data = project_python_source(&input, 20);
    assert_eq!(data["production"].as_array().unwrap().len(), 1, "{data}");
    assert_eq!(
        edge(&data, "production", "use.py", "pkg/model.py")["count"],
        1
    );
    assert!(!has_edge(&data, "use.py", "pkg/__init__.py"));
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
    assert_eq!(
        module_paths(&data),
        ["pkg/__init__.py", "pkg/model.py", "use.py"]
    );
}

#[test]
fn references_python_repeated_routes_deduplicate_original_identity_not_sites() {
    let input = snapshot(&[
        ("use.py", "from left import *\nfrom right import *\n"),
        (
            "left.py",
            "from origin import original as same\n__all__ = ['same']\n",
        ),
        (
            "right.py",
            "from origin import original as same\n__all__ = ['same']\n",
        ),
        ("origin.py", "def original():\n    pass\n"),
    ]);
    let data = project_python_source(&input, 20);
    assert_eq!(data["production"].as_array().unwrap().len(), 1, "{data}");
    assert_eq!(edge(&data, "production", "use.py", "origin.py")["count"], 2);
    assert_eq!(data["counts"]["mappedSites"], 2);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
}

#[test]
fn references_python_forwarding_depth_counts_initial_lookup_and_cycles_stay_unknown() {
    for (m2, m4, extra, resolves) in [
        (
            "from m3 import forwarded\n",
            "def forwarded():\n    pass\n",
            None,
            true,
        ),
        (
            "from m3 import forwarded\n",
            "from m5 import forwarded\n",
            Some(("m5.py", "def forwarded():\n    pass\n")),
            false,
        ),
        ("from m1 import forwarded\n", "", None, false),
    ] {
        let mut files = vec![
            ("use.py", "from m1 import *\n"),
            ("m1.py", "def good():\n    pass\nfrom m2 import forwarded\n"),
            ("m2.py", m2),
            ("m3.py", "from m4 import forwarded\n"),
            ("m4.py", m4),
        ];
        if let Some(extra) = extra {
            files.push(extra);
        }
        let input = snapshot(&files);
        let data = project_python_source(&input, 20);
        assert_eq!(edge(&data, "production", "use.py", "m1.py")["count"], 1);
        assert_eq!(has_edge(&data, "use.py", "m4.py"), resolves, "{data}");
        assert!(!has_edge(&data, "use.py", "m2.py"));
        assert!(!has_edge(&data, "use.py", "m3.py"));
        assert!(!has_edge(&data, "use.py", "m5.py"));
        assert_eq!(data["counts"]["mappedSites"], 1);
        assert_eq!(data["counts"]["unresolvedSites"], usize::from(!resolves));
    }
}

#[test]
fn references_python_relative_packages_require_every_unique_prefix() {
    let input = snapshot(&[
        ("pkg/__init__.py", ""),
        ("pkg/sub/__init__.py", ""),
        ("pkg/sub/use.py", "from ..model import *\n"),
        ("pkg/model.py", "def public_name():\n    pass\n"),
    ]);
    let data = references::project(&input, &["pkg/sub/use.py".into()], 20, &mut || Ok(()))
        .unwrap()
        .data;
    assert_eq!(
        edge(&data, "production", "pkg/sub/use.py", "pkg/model.py")["count"],
        1
    );
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
    for files in [
        vec![
            ("use.py", "from provider import *\n"),
            ("provider.py", "def public_name():\n    pass\n"),
            ("provider/__init__.py", "def other():\n    pass\n"),
        ],
        vec![
            ("use.py", "from pkg.model import *\n"),
            ("pkg.py", ""),
            ("pkg/__init__.py", ""),
            ("pkg/model.py", "def public_name():\n    pass\n"),
        ],
        vec![
            ("use.py", "from pkg.sub.model import *\n"),
            ("pkg/__init__.py", ""),
            ("pkg/sub/model.py", "def public_name():\n    pass\n"),
        ],
    ] {
        let input = snapshot(&files);
        let data = project_python_source(&input, 20);
        assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
        assert_eq!(data["counts"]["mappedSites"], 0);
        assert_eq!(data["counts"]["unresolvedSites"], 1);
    }
}

#[test]
fn references_python_missing_package_roots_never_guess_namespaces_sys_path_or_suffixes() {
    for files in [
        vec![
            ("use.py", "from provider import *\n"),
            ("src/provider.py", "def public_name():\n    pass\n"),
        ],
        vec![
            ("use.py", "from provider import *\n"),
            ("other/provider.py", "def public_name():\n    pass\n"),
        ],
        vec![
            ("use.py", "from pkg.model import *\n"),
            ("pkg/model.py", "def public_name():\n    pass\n"),
        ],
    ] {
        let input = snapshot(&files);
        let data = project_python_source(&input, 20);
        assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
        assert_eq!(data["counts"]["mappedSites"], 0);
        assert_eq!(data["counts"]["unresolvedSites"], 1);
    }
    for import in ["from .model import *\n", "from ..model import *\n"] {
        let input = snapshot(&[
            ("pkg/use.py", import),
            ("pkg/model.py", "def public_name():\n    pass\n"),
        ]);
        let data = references::project(&input, &["pkg/use.py".into()], 20, &mut || Ok(()))
            .unwrap()
            .data;
        assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
        assert_eq!(data["counts"]["mappedSites"], 0);
        assert_eq!(data["counts"]["unresolvedSites"], 1);
    }
}

#[test]
fn references_python_full_catalog_and_source_sets_bound_resolution() {
    let mut input = snapshot(&[
        ("use.py", "from left import *\nfrom right import *\n"),
        ("left.py", "def same():\n    pass\n"),
        ("right.py", "def same():\n    pass\n"),
    ]);
    let data = project_python_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
    assert_eq!(data["counts"]["scannedDocuments"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 2);
    let separate = snapshot(&[("missing.py", "def public_name():\n    pass\n")]);
    let separate_source_set = separate.documents[0].key.source_set_id.clone();
    assert_ne!(input.documents[0].key.source_set_id, separate_source_set);
    input.documents.extend(separate.documents);
    let mut isolated = snapshot(&[("use.py", "from missing import *\n")]);
    isolated.documents.extend(
        input
            .documents
            .into_iter()
            .filter(|doc| doc.key.source_set_id == separate_source_set),
    );
    let data = project_python_source(&isolated, 20);
    assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
}

#[test]
fn references_python_fanout_clipping_preserves_counts_roles_ranges_and_native_proof() {
    let mut input = snapshot(&[
        ("use.py", "from facade import *\nfrom facade import *\n"),
        ("tests/test_use.py", "from facade import *\n"),
        ("facade.py", "from left import a\nfrom right import b\n"),
        ("left.py", "def a():\n    pass\n"),
        ("right.py", "def b():\n    pass\n"),
        ("unrelated.py", "from unavailable import *\n"),
    ]);
    let paths = ["use.py".into(), "tests/test_use.py".into()];
    let complete = references::project(&input, &paths, 20, &mut || Ok(()))
        .unwrap()
        .data;
    let clipped = references::project(&input, &paths, 1, &mut || Ok(()))
        .unwrap()
        .data;
    for data in [&complete, &clipped] {
        assert_eq!(data["counts"]["scannedDocuments"], 2);
        assert_eq!(data["counts"]["mappedSites"], 3);
        assert_eq!(data["counts"]["unresolvedSites"], 0);
        assert_eq!(data["production"].as_array().unwrap().len(), 2);
        assert_eq!(data["tests"].as_array().unwrap().len(), 2);
        assert_eq!(
            module_paths(data),
            [
                "facade.py",
                "left.py",
                "right.py",
                "tests/test_use.py",
                "use.py"
            ]
        );
        for (section, role, path, count) in [
            ("production", "production", "use.py", 2),
            ("tests", "test", "tests/test_use.py", 1),
        ] {
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
            for target in ["left.py", "right.py"] {
                let pair = edge(data, section, path, target);
                assert_eq!(pair["count"], count);
                assert_eq!(pair["evidenceTier"], "syntax");
                for site in pair["sites"].as_array().unwrap() {
                    let start = site["range"]["start"].as_u64().unwrap() as usize;
                    let end = site["range"]["end"].as_u64().unwrap() as usize;
                    assert_eq!(
                        &doc.file.text[start..end],
                        site["spelling"].as_str().unwrap()
                    );
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
                }
            }
        }
        for module in data["modules"].as_array().unwrap() {
            let doc = input
                .documents
                .iter()
                .find(|doc| doc.key.path == module["identity"]["path"].as_str().unwrap())
                .unwrap();
            let root = doc
                .declarations
                .iter()
                .find(|row| row.kind == "module" && row.ancestors.is_empty())
                .unwrap();
            assert_eq!(module["identity"]["sid"], root.syntax_id);
            assert_eq!(module["revisionId"], root.revision_id);
            assert_eq!(module["provenanceId"], root.provenance_id);
            assert_eq!(module["contentHash"], doc.file.hash);
        }
    }
    assert_eq!(complete["modules"], clipped["modules"]);
    for target in ["left.py", "right.py"] {
        let pair = edge(&clipped, "production", "use.py", target);
        assert_eq!(pair["omittedSites"], 1);
        assert_eq!(pair["sites"].as_array().unwrap().len(), 1);
    }
    input.documents.reverse();
    for doc in &mut input.documents {
        doc.declarations.reverse();
    }
    assert_eq!(
        references::project(&input, &paths, 1, &mut || Ok(()))
            .unwrap()
            .data,
        clipped
    );
}

#[test]
fn references_python_stdlib_catalog_has_nonempty_shipped_evidence_and_unknown_nonmembers() {
    for root in ["json", "pathlib", "sys", "os", "logging", "http", "email"] {
        let source = format!("import {root}\n");
        let input = snapshot(&[("use.py", &source)]);
        let data = project_python_source(&input, 20);
        let descriptors = data["externalPackages"].as_array().unwrap();
        assert_eq!(descriptors.len(), 1, "{data}");
        let descriptor = &descriptors[0];
        assert_eq!(descriptor["identity"]["kind"], "externalPackage");
        assert_eq!(descriptor["identity"]["language"], "python");
        assert_eq!(descriptor["identity"]["package"], root);
        assert_eq!(descriptor["identity"]["standardLibrary"], true);
        assert_eq!(
            descriptor["identity"]["sourceSetId"],
            input.documents[0].key.source_set_id
        );
        assert_eq!(descriptor["ecosystem"], "python");
        let evidence = descriptor["evidence"].as_array().unwrap();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0]["kind"], "standardLibraryCatalog");
        assert_eq!(evidence[0]["catalog"], "python-standard-library:v2");
        assert_eq!(evidence[0]["entry"], root);
        let pairs = data["production"].as_array().unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0]["target"], descriptor["identity"]);
        assert_eq!(pairs[0]["count"], 1);
        assert_eq!(module_paths(&data), ["use.py"]);
        assert_eq!(data["counts"]["mappedSites"], 1);
        assert_eq!(data["counts"]["unresolvedSites"], 0);
    }
    for root in ["requests", "jsonish", "unknown_package"] {
        let source = format!("import {root}\n");
        let input = snapshot(&[("use.py", &source)]);
        let data = project_python_source(&input, 20);
        assert!(
            data["externalPackages"].as_array().unwrap().is_empty(),
            "{data}"
        );
        assert!(data["production"].as_array().unwrap().is_empty());
        assert_eq!(data["counts"]["mappedSites"], 0);
        assert_eq!(data["counts"]["unresolvedSites"], 1);
    }
}

#[test]
fn references_python_local_stdlib_roots_and_ambiguous_prefixes_never_fall_back_external() {
    for root in ["json", "pathlib", "sys"] {
        let source = format!("from {root} import *\n");
        let file = format!("{root}.py");
        let init = format!("{root}/__init__.py");
        for local in [&file, &init] {
            let input = snapshot(&[
                ("use.py", &source),
                (local, "def public_name():\n    pass\n"),
            ]);
            let data = project_python_source(&input, 20);
            assert_eq!(edge(&data, "production", "use.py", local)["count"], 1);
            assert!(
                data["externalPackages"].as_array().unwrap().is_empty(),
                "{data}"
            );
            assert_eq!(data["counts"]["unresolvedSites"], 0);
        }
        let input = snapshot(&[
            ("use.py", &source),
            (&file, "def public_name():\n    pass\n"),
            (&init, "def other():\n    pass\n"),
        ]);
        let data = project_python_source(&input, 20);
        assert!(
            data["externalPackages"].as_array().unwrap().is_empty(),
            "{data}"
        );
        assert!(data["production"].as_array().unwrap().is_empty());
        assert_eq!(data["counts"]["mappedSites"], 0);
        assert_eq!(data["counts"]["unresolvedSites"], 1);
    }

    for files in [
        vec![
            ("use.py", "import json.child\n"),
            ("json/child.py", "def public_name():\n    pass\n"),
        ],
        vec![
            ("use.py", "import json.missing\n"),
            ("json/__init__.py", ""),
        ],
    ] {
        let input = snapshot(&files);
        let data = project_python_source(&input, 20);
        assert!(
            data["externalPackages"].as_array().unwrap().is_empty(),
            "{data}"
        );
        assert!(data["production"].as_array().unwrap().is_empty());
        assert_eq!(data["counts"]["mappedSites"], 0);
        assert_eq!(data["counts"]["unresolvedSites"], 1);
    }
}

#[test]
fn references_python_test_only_export_does_not_prove_production_availability() {
    let input = snapshot(&[
        ("use.py", "from tests.provider import *\n"),
        ("tests/__init__.py", ""),
        ("tests/provider.py", "def test_only():\n    pass\n"),
    ]);
    let declaration = input
        .documents
        .iter()
        .find(|doc| doc.key.path == "tests/provider.py")
        .unwrap()
        .declarations
        .iter()
        .find(|row| row.name.as_deref() == Some("test_only"))
        .unwrap();
    assert_eq!(serde_json::to_value(declaration.role).unwrap(), "test");
    let data = project_python_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
}

#[test]
fn references_python_comments_strings_and_decorated_native_ranges_do_not_invent_exports() {
    let input = snapshot(&[
        ("use.py", "from provider import *\n"),
        (
            "provider.py",
            "# exec(source)\n_TEXT = 'globals()[name] = value'\ndef public_name():\n    pass\n",
        ),
    ]);
    let data = project_python_source(&input, 20);
    assert_eq!(
        edge(&data, "production", "use.py", "provider.py")["count"],
        1
    );
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 0);

    let input = snapshot(&[
        ("use.py", "from provider import *\n"),
        ("provider.py", "@decorator\ndef public_name():\n    pass\n"),
    ]);
    let declaration = input
        .documents
        .iter()
        .find(|doc| doc.key.path == "provider.py")
        .unwrap()
        .declarations
        .iter()
        .find(|row| row.name.as_deref() == Some("public_name"))
        .unwrap();
    assert_eq!(declaration.kind, "function");
    let data = project_python_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
}

#[test]
fn references_python_shadowing_one_name_preserves_other_final_owners() {
    let input = snapshot(&[
        ("use.py", "from facade import *\ndef same():\n    pass\n"),
        (
            "facade.py",
            "from collision import same\nfrom good import unique\n",
        ),
        ("collision.py", "def same():\n    pass\n"),
        ("good.py", "def unique():\n    pass\n"),
    ]);
    let data = project_python_source(&input, 20);
    assert_eq!(data["production"].as_array().unwrap().len(), 1, "{data}");
    assert_eq!(edge(&data, "production", "use.py", "good.py")["count"], 1);
    assert!(!has_edge(&data, "use.py", "collision.py"));
    assert!(!has_edge(&data, "use.py", "facade.py"));
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
}

#[test]
fn references_python_explicit_name_is_not_vetoed_by_external_star_uncertainty() {
    let input = snapshot(&[
        (
            "use.py",
            "from provider import *\nfrom json import *\nfrom origin import original as same\n",
        ),
        ("provider.py", "def same():\n    pass\n"),
        ("origin.py", "def original():\n    pass\n"),
    ]);
    let data = project_python_source(&input, 20);
    assert!(!has_edge(&data, "use.py", "provider.py"), "{data}");
    assert_eq!(edge(&data, "production", "use.py", "origin.py")["count"], 1);
    assert_eq!(data["externalPackages"].as_array().unwrap().len(), 1);
    assert_eq!(data["externalPackages"][0]["identity"]["package"], "json");
    assert_eq!(data["production"].as_array().unwrap().len(), 2);
}

#[test]
fn references_python_stdlib_dotted_paths_label_only_the_catalog_root() {
    let input = snapshot(&[(
        "use.py",
        "import json.decoder\nfrom json.decoder import JSONDecoder\n",
    )]);
    let data = project_python_source(&input, 20);
    let descriptors = data["externalPackages"].as_array().unwrap();
    assert_eq!(descriptors.len(), 1, "{data}");
    assert_eq!(descriptors[0]["identity"]["package"], "json");
    let pairs = data["production"].as_array().unwrap();
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0]["target"], descriptors[0]["identity"]);
    assert_eq!(pairs[0]["count"], 2);
    assert_eq!(data["counts"]["mappedSites"], 2);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
    assert_eq!(module_paths(&data), ["use.py"]);
}

#[test]
fn references_python_chained_assignments_are_not_proved_direct_exports() {
    for provider in ["a = b = 1\n", "a = b = 1\n__all__ = ['a', 'b']\n"] {
        let input = snapshot(&[
            ("use.py", "from provider import *\n"),
            ("provider.py", provider),
        ]);
        let data = project_python_source(&input, 20);
        assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
        assert_eq!(data["counts"]["mappedSites"], 0);
        assert_eq!(data["counts"]["unresolvedSites"], 1);
    }
}

#[test]
fn references_python_dotted_import_binds_root_unless_a_literal_alias_selects_child() {
    for (provider, owner) in [
        ("import pkg.child\n__all__ = ['pkg']\n", "pkg/__init__.py"),
        (
            "import pkg.child as alias\n__all__ = ['alias']\n",
            "pkg/child.py",
        ),
    ] {
        let input = snapshot(&[
            ("use.py", "from provider import *\n"),
            ("provider.py", provider),
            ("pkg/__init__.py", "def package_member():\n    pass\n"),
            ("pkg/child.py", "def child_member():\n    pass\n"),
        ]);
        let data = project_python_source(&input, 20);
        assert_eq!(data["production"].as_array().unwrap().len(), 1, "{data}");
        assert_eq!(edge(&data, "production", "use.py", owner)["count"], 1);
        assert!(!has_edge(&data, "use.py", "provider.py"));
        assert_eq!(data["counts"]["mappedSites"], 1);
        assert_eq!(data["counts"]["unresolvedSites"], 0);
    }
    let input = snapshot(&[
        ("use.py", "from provider import *\n"),
        (
            "provider.py",
            "import pkg.child\n__all__ = ['child_member']\n",
        ),
        ("pkg/__init__.py", ""),
        ("pkg/child.py", "def child_member():\n    pass\n"),
    ]);
    let data = project_python_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
}

#[test]
fn references_python_prior_simple_name_alias_preserves_original_identity_not_rebound_names() {
    let input = snapshot(&[
        ("use.py", "from provider import *\n"),
        (
            "provider.py",
            "def original():\n    pass\nalias = original\n__all__ = ['alias']\n",
        ),
    ]);
    let provider = input
        .documents
        .iter()
        .find(|doc| doc.key.path == "provider.py")
        .unwrap();
    let original = provider
        .declarations
        .iter()
        .find(|row| row.name.as_deref() == Some("original"))
        .unwrap();
    assert_eq!(original.kind, "function");
    let data = project_python_source(&input, 20);
    assert_eq!(
        edge(&data, "production", "use.py", "provider.py")["count"],
        1
    );
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
    for (provider, resolves) in [
        (
            "from origin import original\nalias = original\n__all__ = ['alias']\n",
            true,
        ),
        (
            "alias = original\nfrom origin import original\n__all__ = ['alias']\n",
            false,
        ),
        (
            "from origin import original\noriginal = 1\nalias = original\n__all__ = ['alias']\n",
            false,
        ),
        (
            "from origin import original\nalias = original\nalias = 1\n__all__ = ['alias']\n",
            false,
        ),
    ] {
        let input = snapshot(&[
            ("use.py", "from provider import *\n"),
            ("provider.py", provider),
            ("origin.py", "def original():\n    pass\n"),
        ]);
        let original = input
            .documents
            .iter()
            .find(|doc| doc.key.path == "origin.py")
            .unwrap()
            .declarations
            .iter()
            .find(|row| row.name.as_deref() == Some("original"))
            .unwrap();
        assert_eq!(original.kind, "function");
        let data = project_python_source(&input, 20);
        assert_eq!(
            has_edge(&data, "use.py", "origin.py"),
            resolves,
            "{provider}: {data}"
        );
        assert!(
            !has_edge(&data, "use.py", "provider.py"),
            "{provider}: {data}"
        );
        assert_eq!(
            data["production"].as_array().unwrap().len(),
            usize::from(resolves)
        );
        assert_eq!(data["counts"]["mappedSites"], usize::from(resolves));
        assert_eq!(data["counts"]["unresolvedSites"], usize::from(!resolves));
        if resolves {
            assert_eq!(edge(&data, "production", "use.py", "origin.py")["count"], 1);
        }
    }
}

#[test]
fn references_python_annotation_requires_a_value_to_prove_an_export() {
    for (provider, resolves) in [("value: int\n", false), ("value: int = 1\n", true)] {
        let input = snapshot(&[
            ("use.py", "from provider import *\n"),
            ("provider.py", provider),
        ]);
        let document = input
            .documents
            .iter()
            .find(|doc| doc.key.path == "provider.py")
            .unwrap();
        if resolves {
            let declaration = document
                .declarations
                .iter()
                .find(|row| row.kind == "variable" && row.name.as_deref() == Some("value"))
                .unwrap();
            assert_eq!(
                &document.file.text[declaration.range.start..declaration.range.end],
                "value: int = 1"
            );
        }
        let data = project_python_source(&input, 20);
        assert_eq!(has_edge(&data, "use.py", "provider.py"), resolves, "{data}");
        assert_eq!(data["counts"]["mappedSites"], usize::from(resolves));
        assert_eq!(data["counts"]["unresolvedSites"], usize::from(!resolves));
        if resolves {
            assert_eq!(
                edge(&data, "production", "use.py", "provider.py")["count"],
                1
            );
            let native_module = document
                .declarations
                .iter()
                .find(|row| row.kind == "module" && row.ancestors.is_empty())
                .unwrap();
            let module = data["modules"]
                .as_array()
                .unwrap()
                .iter()
                .find(|module| module["identity"]["path"] == "provider.py")
                .unwrap();
            assert_eq!(module["identity"]["sid"], native_module.syntax_id);
        } else {
            assert!(data["production"].as_array().unwrap().is_empty());
        }
    }
}

#[test]
fn references_python_named_child_fallback_requires_no_competing_package_attribute() {
    for (package, resolves) in [
        ("", true),
        ("import pkg.child as child\n", true),
        ("def child():\n    pass\n", false),
        ("if enabled:\n    child = 1\n", false),
        ("from missing import child\n", false),
        ("exec(source)\n", false),
    ] {
        let input = snapshot(&[
            ("use.py", "from facade import *\n"),
            (
                "facade.py",
                "from pkg import child\ndef good():\n    pass\n__all__ = ['child', 'good']\n",
            ),
            ("pkg/__init__.py", package),
            ("pkg/child.py", "def member():\n    pass\n"),
        ]);
        let data = project_python_source(&input, 20);
        assert_eq!(edge(&data, "production", "use.py", "facade.py")["count"], 1);
        assert_eq!(
            has_edge(&data, "use.py", "pkg/child.py"),
            resolves,
            "{package}: {data}"
        );
        assert!(
            !has_edge(&data, "use.py", "pkg/__init__.py"),
            "{package}: {data}"
        );
        assert_eq!(
            data["production"].as_array().unwrap().len(),
            1 + usize::from(resolves)
        );
        assert_eq!(data["counts"]["mappedSites"], 1);
        assert_eq!(data["counts"]["unresolvedSites"], usize::from(!resolves));
        if resolves {
            assert_eq!(
                edge(&data, "production", "use.py", "pkg/child.py")["count"],
                1
            );
        }
    }
}

#[test]
fn references_python_normal_checkout_catalog_and_selected_sources_share_scaled_work_budget() {
    let body = (0..12)
        .map(|index| {
            format!(
                "    subtotal_{index} = {index}\n    for step in range(4):\n        amount = step * 31 + {index}\n        if amount % 2 == 0:\n            subtotal_{index} += amount\n        else:\n            subtotal_{index} -= amount\n    weighted_{index} = subtotal_{index} * ({index} + 1)\n    clipped_{index} = weighted_{index} & 1023\n"
            )
        })
        .collect::<String>();
    let mut files = vec![
        (
            "shared_a.py".to_owned(),
            format!("__all__ = ['alpha']\ndef alpha():\n{body}"),
        ),
        (
            "shared_b.py".to_owned(),
            format!("__all__ = ['beta']\ndef beta():\n{body}"),
        ),
        (
            "shared_c.py".to_owned(),
            format!(
                "from shared_b import beta as gamma\n__all__ = ['gamma']\ndef _ordinary_work():\n{body}"
            ),
        ),
    ];
    for index in 0..497 {
        files.push((
            format!("consumer_{index:03}.py"),
            format!("from shared_a import *\nfrom shared_b import *\nfrom shared_c import *\ndef ordinary_work():\n{body}"),
        ));
    }
    assert_eq!(files.len(), 500);
    assert!(
        files
            .iter()
            .all(|(_, text)| (100..=200).contains(&text.lines().count()))
    );
    let borrowed: Vec<_> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    let input = snapshot(&borrowed);
    assert_eq!(input.documents.len(), 500);

    let selected = references::project(&input, &["consumer_000.py".into()], 20, &mut || Ok(()))
        .unwrap()
        .data;
    assert_eq!(selected["counts"]["scannedDocuments"], 1);
    assert_eq!(selected["counts"]["mappedSites"], 3);
    assert_eq!(selected["counts"]["unresolvedSites"], 0);
    assert_eq!(selected["production"].as_array().unwrap().len(), 2);
    assert_eq!(
        edge(&selected, "production", "consumer_000.py", "shared_a.py")["count"],
        1
    );
    assert_eq!(
        edge(&selected, "production", "consumer_000.py", "shared_b.py")["count"],
        2
    );
    assert!(!has_edge(&selected, "consumer_000.py", "shared_c.py"));
    assert_eq!(
        module_paths(&selected),
        [
            "consumer_000.py",
            "shared_a.py",
            "shared_b.py",
            "shared_c.py"
        ]
    );
    assert!(selected["tests"].as_array().unwrap().is_empty());
    assert!(selected["externalPackages"].as_array().unwrap().is_empty());

    let complete = scaled_checkout_projection(&input, 20);
    assert_eq!(complete["counts"]["scannedDocuments"], 500);
    assert_eq!(complete["counts"]["mappedSites"], 497 * 3 + 1);
    assert_eq!(complete["counts"]["unresolvedSites"], 0);
    assert_eq!(
        complete["production"].as_array().unwrap().len(),
        497 * 2 + 1
    );
    assert_eq!(module_paths(&complete).len(), 500);
    for index in 0..497 {
        let source = format!("consumer_{index:03}.py");
        assert_eq!(
            edge(&complete, "production", &source, "shared_a.py")["count"],
            1
        );
        assert_eq!(
            edge(&complete, "production", &source, "shared_b.py")["count"],
            2
        );
        assert!(!has_edge(&complete, &source, "shared_c.py"));
    }
    assert_eq!(
        edge(&complete, "production", "shared_c.py", "shared_b.py")["count"],
        1
    );
    assert!(complete["tests"].as_array().unwrap().is_empty());
    assert!(complete["externalPackages"].as_array().unwrap().is_empty());
}

#[test]
fn references_python_review_shared_200_name_facade_fits_full_checkout_budget() {
    let mut files = Vec::new();
    let mut facade = String::new();
    let mut exports = Vec::new();
    for module in 0..20 {
        let mut provider = String::new();
        let mut names = Vec::new();
        for item in 0..10 {
            let name = format!("value_{module:02}_{item:02}");
            provider.push_str(&format!("def {name}():\n    return {item}\n"));
            names.push(name.clone());
            exports.push(format!("'{name}'"));
        }
        facade.push_str(&format!(
            "from .model_{module:02} import {}\n",
            names.join(", ")
        ));
        files.push((format!("models/model_{module:02}.py"), provider));
    }
    facade.push_str(&format!("__all__ = [{}]\n", exports.join(", ")));
    files.push(("models/__init__.py".to_owned(), facade));
    for consumer in 0..480 {
        files.push((
            format!("consumer_{consumer:03}.py"),
            "from models import *\n".to_owned(),
        ));
    }
    let borrowed: Vec<_> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    let input = snapshot(&borrowed);
    assert_eq!(input.documents.len(), 501);
    let data = references::project(&input, &["**".into()], 1, &mut || Ok(()))
        .unwrap()
        .data;
    assert_eq!(data["counts"]["scannedDocuments"], 501);
    assert_eq!(data["counts"]["mappedSites"], 500);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
    assert_eq!(data["production"].as_array().unwrap().len(), 480 * 20 + 20);
    assert!(data["tests"].as_array().unwrap().is_empty());
    assert!(data["externalPackages"].as_array().unwrap().is_empty());
    let pairs: std::collections::BTreeMap<_, _> = data["production"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pair| {
            (
                (
                    pair["source"]["path"].as_str().unwrap().to_owned(),
                    pair["target"]["path"].as_str().unwrap().to_owned(),
                ),
                pair["count"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(pairs.len(), 480 * 20 + 20);
    for consumer in 0..480 {
        let source = format!("consumer_{consumer:03}.py");
        assert!(!pairs.contains_key(&(source.clone(), "models/__init__.py".to_owned())));
        for module in 0..20 {
            let target = format!("models/model_{module:02}.py");
            assert_eq!(pairs.get(&(source.clone(), target)), Some(&1));
        }
    }
    for module in 0..20 {
        let target = format!("models/model_{module:02}.py");
        assert_eq!(
            pairs.get(&("models/__init__.py".to_owned(), target)),
            Some(&1)
        );
    }
    assert_eq!(module_paths(&data).len(), 501);
}

#[test]
fn references_python_review_later_star_can_invalidate_an_earlier_function_export() {
    for all in ["", "__all__ = ['foo']\n"] {
        for (bindings, resolves) in [
            ("def foo():\n    pass\nfrom other import *\n", false),
            ("from other import *\ndef foo():\n    pass\n", true),
        ] {
            let facade = format!("{bindings}{all}");
            let input = snapshot(&[
                ("use.py", "from facade import *\n"),
                ("facade.py", &facade),
                ("other.py", "def foo():\n    pass\n"),
            ]);
            let data = project_python_source(&input, 20);
            assert_eq!(
                has_edge(&data, "use.py", "facade.py"),
                resolves,
                "{facade}: {data}"
            );
            assert!(!has_edge(&data, "use.py", "other.py"), "{facade}: {data}");
            assert_eq!(
                data["production"].as_array().unwrap().len(),
                usize::from(resolves)
            );
            assert_eq!(data["counts"]["mappedSites"], usize::from(resolves));
            assert_eq!(data["counts"]["unresolvedSites"], usize::from(!resolves));
        }
    }
}

#[test]
fn references_python_review_later_star_can_invalidate_an_earlier_named_import() {
    for all in ["", "__all__ = ['foo']\n"] {
        for (bindings, resolves) in [
            ("from original import foo\nfrom other import *\n", false),
            ("from other import *\nfrom original import foo\n", true),
        ] {
            let facade = format!("{bindings}{all}");
            let input = snapshot(&[
                ("use.py", "from facade import *\n"),
                ("facade.py", &facade),
                ("original.py", "def foo():\n    pass\n"),
                ("other.py", "def foo():\n    pass\n"),
            ]);
            let data = project_python_source(&input, 20);
            assert_eq!(
                has_edge(&data, "use.py", "original.py"),
                resolves,
                "{facade}: {data}"
            );
            assert!(!has_edge(&data, "use.py", "other.py"), "{facade}: {data}");
            assert!(!has_edge(&data, "use.py", "facade.py"));
            assert_eq!(
                data["production"].as_array().unwrap().len(),
                usize::from(resolves)
            );
            assert_eq!(data["counts"]["mappedSites"], usize::from(resolves));
            assert_eq!(data["counts"]["unresolvedSites"], usize::from(!resolves));
        }
    }
}

#[test]
fn references_python_review_later_star_known_exclusion_preserves_earlier_binding() {
    for binding in ["def foo():\n    pass\n", "from original import foo\n"] {
        let facade = format!("{binding}from other import *\n__all__ = ['foo']\n");
        let input = snapshot(&[
            ("use.py", "from facade import *\n"),
            ("facade.py", &facade),
            ("original.py", "def foo():\n    pass\n"),
            (
                "other.py",
                "def foo():\n    pass\ndef safe():\n    pass\n__all__ = ['safe']\n",
            ),
        ]);
        let data = project_python_source(&input, 20);
        let target = if binding.starts_with("def ") {
            "facade.py"
        } else {
            "original.py"
        };
        assert_eq!(edge(&data, "production", "use.py", target)["count"], 1);
        assert_eq!(data["production"].as_array().unwrap().len(), 1, "{data}");
        assert!(!has_edge(&data, "use.py", "other.py"));
        assert_eq!(data["counts"]["mappedSites"], 1);
        assert_eq!(data["counts"]["unresolvedSites"], 0);
    }
}

#[test]
fn references_python_review_implicit_child_module_name_blocks_earlier_star_collision() {
    let input = snapshot(&[
        ("use.py", "from earlier import *\nfrom pkg import *\n"),
        ("earlier.py", "def sub():\n    pass\n"),
        ("pkg/__init__.py", "from .sub import helper\n"),
        ("pkg/sub.py", "def helper():\n    pass\n"),
    ]);
    let data = project_python_source(&input, 20);
    assert!(!has_edge(&data, "use.py", "earlier.py"), "{data}");
    assert!(!has_edge(&data, "use.py", "pkg/__init__.py"));
    assert_eq!(
        edge(&data, "production", "use.py", "pkg/sub.py")["count"],
        1
    );
    assert_eq!(data["production"].as_array().unwrap().len(), 1);
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 2);
}

#[test]
fn references_python_review_type_alias_is_an_unknown_binding_not_a_star_fallback() {
    for facade in [
        "from original import *\ntype X = int\n",
        "type X = int\nfrom original import *\n",
    ] {
        let input = snapshot(&[
            ("use.py", "from facade import *\n"),
            ("facade.py", facade),
            ("original.py", "class X:\n    pass\n"),
        ]);
        let data = project_python_source(&input, 20);
        assert!(
            data["production"].as_array().unwrap().is_empty(),
            "{facade}: {data}"
        );
        assert_eq!(data["counts"]["mappedSites"], 0);
        assert_eq!(data["counts"]["unresolvedSites"], 1);
    }
}

#[test]
fn references_python_review_simple_alias_cannot_bypass_later_star_uncertainty() {
    for (facade, other, resolves) in [
        (
            "from original import foo\nalias = foo\nfrom other import *\n__all__ = ['alias']\n",
            "def alias():\n    pass\n",
            false,
        ),
        (
            "from original import foo\nfrom other import *\nalias = foo\n__all__ = ['alias']\n",
            "def foo():\n    pass\n",
            false,
        ),
        (
            "from original import foo\nalias = foo\nfrom other import *\n__all__ = ['alias']\n",
            "def alias():\n    pass\ndef safe():\n    pass\n__all__ = ['safe']\n",
            true,
        ),
    ] {
        let input = snapshot(&[
            ("use.py", "from facade import *\n"),
            ("facade.py", facade),
            ("original.py", "def foo():\n    pass\n"),
            ("other.py", other),
        ]);
        let data = project_python_source(&input, 20);
        assert_eq!(
            has_edge(&data, "use.py", "original.py"),
            resolves,
            "{facade}: {data}"
        );
        assert!(!has_edge(&data, "use.py", "facade.py"));
        assert!(!has_edge(&data, "use.py", "other.py"));
        assert_eq!(
            data["production"].as_array().unwrap().len(),
            usize::from(resolves)
        );
        assert_eq!(data["counts"]["mappedSites"], usize::from(resolves));
        assert_eq!(data["counts"]["unresolvedSites"], usize::from(!resolves));
        if resolves {
            assert_eq!(
                edge(&data, "production", "use.py", "original.py")["count"],
                1
            );
        }
    }
}

#[test]
fn references_python_declared_distributions_do_not_prove_import_names() {
    use trellis::references::external::ManifestInput;
    for (path, manifest, root) in [
        ("requirements.txt", "requests==2.32.0\n", "requests"),
        (
            "requirements.txt",
            "some-distribution==1\n",
            "some_distribution",
        ),
        (
            "pyproject.toml",
            "[project]\nname = 'app'\nversion = '1'\ndependencies = ['requests>=2']\n",
            "requests",
        ),
        (
            "pyproject.toml",
            "[project\ndependencies = ???\n",
            "requests",
        ),
    ] {
        let source = format!("import {root}\n");
        let mut input = snapshot(&[("use.py", &source)]);
        input.manifests.push(ManifestInput {
            source_set_id: input.documents[0].key.source_set_id.clone(),
            native_revision_id: input.documents[0].declarations[0].revision_id.clone(),
            dependency_hash: "captured-python-dependencies".into(),
            path: path.into(),
            content_hash: Some(hex::encode(Sha256::digest(manifest.as_bytes()))),
            bytes: Some(manifest.as_bytes().to_vec()),
        });
        let data = project_python_source(&input, 20);
        assert!(
            data["externalPackages"].as_array().unwrap().is_empty(),
            "{data}"
        );
        assert!(data["production"].as_array().unwrap().is_empty());
        assert_eq!(data["counts"]["mappedSites"], 0);
        assert_eq!(data["counts"]["unresolvedSites"], 1);
    }
}

fn project_javascript_source(input: &Snapshot, limit: usize) -> Value {
    references::project(input, &["use.js".into()], limit, &mut || Ok(()))
        .unwrap()
        .data
}

fn assert_javascript_native_modules(input: &Snapshot, data: &Value) {
    for module in data["modules"].as_array().unwrap() {
        let document = input
            .documents
            .iter()
            .find(|document| module["identity"]["path"] == document.key.path)
            .unwrap();
        let native = document
            .declarations
            .iter()
            .find(|row| row.kind == "module" && row.ancestors.is_empty())
            .unwrap();
        assert_eq!(module["identity"]["sid"], native.syntax_id);
        assert_eq!(
            module["identity"]["sourceSetId"],
            document.key.source_set_id
        );
        assert_eq!(module["revisionId"], native.revision_id);
        assert_eq!(module["contentHash"], document.file.hash);
        assert_eq!(module["provenanceId"], native.provenance_id);
    }
}

#[test]
fn references_javascript_native_declarations_join_inner_ranges_and_export_lists() {
    let declarations = [
        ("function", "value", "function value() {}"),
        ("function", "generator", "function* generator() {}"),
        ("type", "Thing", "class Thing { #private; method() {} }"),
        ("variable", "first", "first = 1"),
        ("variable", "second", "second = 2"),
        ("variable", "uninitialized", "uninitialized"),
    ];
    for (kind, name, inner) in declarations {
        let provider = match name {
            "first" | "second" => "export const first = 1, second = 2;".to_owned(),
            "uninitialized" => "export let uninitialized;".to_owned(),
            _ => format!("export {inner}"),
        };
        let input = snapshot(&[
            ("use.js", "export * from './provider.js';"),
            ("provider.js", &provider),
        ]);
        let document = input
            .documents
            .iter()
            .find(|doc| doc.key.path == "provider.js")
            .unwrap();
        let row = document
            .declarations
            .iter()
            .find(|row| row.name.as_deref() == Some(name))
            .unwrap();
        assert_eq!(row.kind, kind);
        assert_eq!(&document.file.text[row.range.start..row.range.end], inner);
        let data = project_javascript_source(&input, 20);
        assert_eq!(
            edge(&data, "production", "use.js", "provider.js")["count"],
            1
        );
        assert_eq!(data["counts"]["mappedSites"], 1);
        assert_eq!(data["counts"]["unresolvedSites"], 0);
        assert_javascript_native_modules(&input, &data);
        let mut missing = input;
        missing
            .documents
            .iter_mut()
            .find(|doc| doc.key.path == "provider.js")
            .unwrap()
            .declarations
            .retain(|row| row.kind == "module");
        let unknown = project_javascript_source(&missing, 20);
        assert!(
            unknown["production"].as_array().unwrap().is_empty(),
            "{kind}: {unknown}"
        );
        assert_eq!(unknown["counts"]["unresolvedSites"], 1);
    }
    for provider in [
        "const original = 1; export { original as visible };",
        "export { later as visible }; function later() {}",
    ] {
        let input = snapshot(&[
            ("use.js", "export * from './provider.js';"),
            ("provider.js", provider),
        ]);
        let data = project_javascript_source(&input, 20);
        assert_eq!(
            edge(&data, "production", "use.js", "provider.js")["count"],
            1
        );
        assert_eq!(data["counts"]["unresolvedSites"], 0);
    }
}

#[test]
fn references_javascript_default_and_private_bindings_do_not_leak_through_stars() {
    for provider in [
        "export default function Hidden() {}",
        "function privateLocal() { const inner = 1; } class Private { #secret; method() {} }",
    ] {
        let input = snapshot(&[
            ("use.js", "export * from './provider.js';"),
            ("provider.js", provider),
        ]);
        let data = project_javascript_source(&input, 20);
        assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
        assert_eq!(data["counts"]["mappedSites"], 0);
        assert_eq!(data["counts"]["unresolvedSites"], 1);
        assert_eq!(
            data["unresolved"][0]["site"]["spelling"],
            "export * from './provider.js';"
        );
    }
    let input = snapshot(&[
        ("use.js", "export * from './facade.js';"),
        (
            "facade.js",
            "export { default as visible } from './provider.js';",
        ),
        ("provider.js", "export default function Original() {}"),
    ]);
    let data = project_javascript_source(&input, 20);
    assert_eq!(
        edge(&data, "production", "use.js", "provider.js")["count"],
        1
    );
    assert!(!has_edge(&data, "use.js", "facade.js"));
    assert_eq!(data["counts"]["unresolvedSites"], 0);
    for provider in ["export default function () {}", "export default 1;"] {
        let input = snapshot(&[
            ("use.js", "export * from './facade.js';"),
            (
                "facade.js",
                "export { default as visible } from './provider.js';",
            ),
            ("provider.js", provider),
        ]);
        let data = project_javascript_source(&input, 20);
        assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
        assert_eq!(data["counts"]["unresolvedSites"], 1);
    }
    let input = snapshot(&[
        ("use.js", "export * from './facade.js';"),
        (
            "facade.js",
            "export * from './hidden.js'; export * from './visible.js';",
        ),
        ("hidden.js", "export default function Hidden() {}"),
        ("visible.js", "export function visible() {}"),
    ]);
    let data = project_javascript_source(&input, 20);
    assert!(has_edge(&data, "use.js", "visible.js"));
    assert!(!has_edge(&data, "use.js", "hidden.js"));
    assert_eq!(data["counts"]["unresolvedSites"], 0);
}

#[test]
fn references_javascript_aliases_keep_original_identity_and_require_stable_bindings() {
    for facade in [
        "export { original as visible } from './origin.js';",
        "import { original as local } from './origin.js'; export { local as visible };",
        "import Original from './origin.js'; export { Original as visible };",
    ] {
        let mut input = snapshot(&[
            ("use.js", "export * from './facade.js';"),
            ("facade.js", facade),
            (
                "origin.js",
                "export default function original() {} export { original };",
            ),
        ]);
        let data = project_javascript_source(&input, 20);
        assert_eq!(edge(&data, "production", "use.js", "origin.js")["count"], 1);
        assert!(!has_edge(&data, "use.js", "facade.js"));
        assert_eq!(data["counts"]["unresolvedSites"], 0);
        if facade.contains("as local") {
            let document = input
                .documents
                .iter()
                .find(|doc| doc.key.path == "facade.js")
                .unwrap();
            assert!(
                document
                    .declarations
                    .iter()
                    .any(|row| row.kind == "alias" && row.name.as_deref() == Some("local"))
            );
        }
        input
            .documents
            .iter_mut()
            .find(|doc| doc.key.path == "origin.js")
            .unwrap()
            .declarations
            .retain(|row| row.name.as_deref() != Some("original"));
        let missing = project_javascript_source(&input, 20);
        assert!(
            missing["production"].as_array().unwrap().is_empty(),
            "{facade}: {missing}"
        );
        assert_eq!(missing["counts"]["unresolvedSites"], 1);
    }
    for (provider, resolves) in [
        (
            "function original() {} const alias = original; export { alias };",
            true,
        ),
        (
            "function original() {} const alias = original; function inner() { let alias = 0; alias = 1; } export { alias };",
            true,
        ),
        (
            "const alias = original; function original() {} export { alias };",
            false,
        ),
        (
            "function original() {} let alias = original; alias = replacement; export { alias };",
            false,
        ),
        ("const alias = object.member; export { alias };", false),
        ("const { alias } = object; export { alias };", false),
    ] {
        let input = snapshot(&[
            ("use.js", "export * from './provider.js';"),
            ("provider.js", provider),
        ]);
        let data = project_javascript_source(&input, 20);
        assert_eq!(
            has_edge(&data, "use.js", "provider.js"),
            resolves,
            "{provider}: {data}"
        );
        assert_eq!(data["counts"]["mappedSites"], usize::from(resolves));
        assert_eq!(data["counts"]["unresolvedSites"], usize::from(!resolves));
    }
    let mut input = snapshot(&[
        ("use.js", "export * from './provider.js';"),
        (
            "provider.js",
            r"function \u006friginal() {} const alias = original; export { alias as visible };",
        ),
    ]);
    let provider = input
        .documents
        .iter()
        .find(|doc| doc.key.path == "provider.js")
        .unwrap();
    let original = provider
        .declarations
        .iter()
        .find(|row| row.kind == "function")
        .unwrap();
    assert_eq!(
        &provider.file.text[original.range.start..original.range.end],
        r"function \u006friginal() {}"
    );
    let data = project_javascript_source(&input, 20);
    assert_eq!(
        edge(&data, "production", "use.js", "provider.js")["count"],
        1
    );
    assert_eq!(data["counts"]["unresolvedSites"], 0);
    input
        .documents
        .iter_mut()
        .find(|doc| doc.key.path == "provider.js")
        .unwrap()
        .declarations
        .retain(|row| row.kind != "function");
    let missing = project_javascript_source(&input, 20);
    assert!(
        missing["production"].as_array().unwrap().is_empty(),
        "{missing}"
    );
    assert_eq!(missing["counts"]["unresolvedSites"], 1);
}

#[test]
fn references_javascript_explicit_exports_shadow_but_unexported_locals_do_not() {
    for (prefix, shadows) in [
        ("export function same() {}", true),
        ("function same() {} export { same };", true),
        ("function same() {}", false),
        ("const same = 1;", false),
    ] {
        for source in [
            format!("{prefix}\nexport * from './provider.js';"),
            format!("export * from './provider.js';\n{prefix}"),
        ] {
            let input = snapshot(&[
                ("use.js", &source),
                ("provider.js", "export function same() {}"),
            ]);
            let data = project_javascript_source(&input, 20);
            assert_eq!(
                has_edge(&data, "use.js", "provider.js"),
                !shadows,
                "{source}: {data}"
            );
            assert_eq!(data["counts"]["mappedSites"], usize::from(!shadows));
        }
    }
    let input = snapshot(&[
        ("use.js", "export * from './facade.js';"),
        (
            "facade.js",
            "const same = object.member; export { same }; export * from './provider.js';",
        ),
        (
            "provider.js",
            "export function same() {} export function good() {}",
        ),
    ]);
    let data = project_javascript_source(&input, 20);
    assert_eq!(
        edge(&data, "production", "use.js", "provider.js")["count"],
        1
    );
    assert!(!has_edge(&data, "use.js", "facade.js"));
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
}

#[test]
fn references_javascript_star_collisions_compare_declaration_identity_not_owner_file() {
    for right in [
        "export { first as same } from './origin.js';",
        "export { second as same } from './origin.js';",
    ] {
        for source in [
            "export * from './left.js'; export * from './right.js';",
            "export * from './right.js'; export * from './left.js';",
        ] {
            let input = snapshot(&[
                ("use.js", source),
                ("left.js", "export { first as same } from './origin.js';"),
                ("right.js", right),
                (
                    "origin.js",
                    "export function first() {} export function second() {}",
                ),
            ]);
            let rows: Vec<_> = input
                .documents
                .iter()
                .find(|doc| doc.key.path == "origin.js")
                .unwrap()
                .declarations
                .iter()
                .filter(|row| row.kind == "function")
                .collect();
            assert_eq!(rows.len(), 2);
            assert_ne!(rows[0].syntax_id, rows[1].syntax_id);
            let same = right.contains("first");
            let data = project_javascript_source(&input, 20);
            assert_eq!(
                has_edge(&data, "use.js", "origin.js"),
                same,
                "{right}: {data}"
            );
            assert_eq!(data["counts"]["mappedSites"], if same { 2 } else { 0 });
            assert_eq!(data["counts"]["unresolvedSites"], if same { 0 } else { 2 });
            if same {
                assert_eq!(edge(&data, "production", "use.js", "origin.js")["count"], 2);
            }
        }
    }
    for source in [
        "export * from './left.js'; export * from './right.js';",
        "export * from './right.js'; export * from './left.js';",
    ] {
        let input = snapshot(&[
            ("use.js", source),
            (
                "left.js",
                "export function same() {} export function good() {}",
            ),
            ("right.js", "export function same() {}"),
        ]);
        let data = project_javascript_source(&input, 20);
        assert_eq!(edge(&data, "production", "use.js", "left.js")["count"], 1);
        assert!(!has_edge(&data, "use.js", "right.js"));
        assert_eq!(data["counts"]["mappedSites"], 1);
        assert_eq!(data["counts"]["unresolvedSites"], 2);
        assert_eq!(data["unresolved"].as_array().unwrap().len(), 2);
    }
}

#[test]
fn references_javascript_missing_and_duplicate_native_rows_leave_partial_unknowns() {
    for duplicate in [false, true] {
        let mut input = snapshot(&[
            ("use.js", "export * from './provider.js';"),
            (
                "provider.js",
                "export function good() {} export function uncertain() {}",
            ),
        ]);
        let provider = input
            .documents
            .iter_mut()
            .find(|doc| doc.key.path == "provider.js")
            .unwrap();
        let row = provider
            .declarations
            .iter()
            .find(|row| row.name.as_deref() == Some("uncertain"))
            .unwrap()
            .clone();
        if duplicate {
            provider.declarations.push(row);
        } else {
            provider
                .declarations
                .retain(|row| row.name.as_deref() != Some("uncertain"));
        }
        let data = project_javascript_source(&input, 20);
        assert_eq!(
            edge(&data, "production", "use.js", "provider.js")["count"],
            1
        );
        assert_eq!(data["counts"]["mappedSites"], 1);
        assert_eq!(data["counts"]["unresolvedSites"], 1);
        assert_eq!(data["unresolved"].as_array().unwrap().len(), 1);
    }
}

#[test]
fn references_javascript_source_filter_preserves_module_competitors_and_source_sets() {
    for competitor in ["provider.mjs", "provider.cjs", "provider/index.js"] {
        let input = snapshot(&[
            ("use.js", "export * from './provider';"),
            ("provider.js", "export function original() {}"),
            (competitor, "export function competitor() {}"),
        ]);
        let data = project_javascript_source(&input, 20);
        assert!(
            data["production"].as_array().unwrap().is_empty(),
            "{competitor}: {data}"
        );
        assert_eq!(data["counts"]["scannedDocuments"], 1);
        assert_eq!(data["counts"]["unresolvedSites"], 1);
    }
    let input = snapshot(&[
        ("use.js", "export * from './provider.js';"),
        ("provider.js", "export function original() {}"),
        ("provider.mjs", "export function competitor() {}"),
        ("provider.cjs", "export function competitor() {}"),
        ("provider/index.js", "export function competitor() {}"),
    ]);
    let data = project_javascript_source(&input, 20);
    assert_eq!(
        edge(&data, "production", "use.js", "provider.js")["count"],
        1
    );
    assert_eq!(data["counts"]["unresolvedSites"], 0);
    for specifier in ["./missing.js", "../provider.js", ".not-relative"] {
        let source = format!("export * from '{specifier}';");
        let input = snapshot(&[
            ("use.js", &source),
            ("provider.js", "export function original() {}"),
        ]);
        let data = project_javascript_source(&input, 20);
        assert!(
            data["production"].as_array().unwrap().is_empty(),
            "{specifier}: {data}"
        );
        assert_eq!(data["counts"]["unresolvedSites"], 1);
    }
    let mut input = snapshot(&[("use.js", "export * from './provider.js';")]);
    let separate = snapshot(&[("provider.js", "export function original() {}")]);
    assert_ne!(
        input.documents[0].key.source_set_id,
        separate.documents[0].key.source_set_id
    );
    input.documents.extend(separate.documents);
    let data = project_javascript_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
    assert_eq!(data["counts"]["unresolvedSites"], 1);
}

#[test]
fn references_javascript_forwarding_counts_initial_edge_and_detects_short_cycles() {
    for (m2, m4, extra, resolves) in [
        (
            "export { forwarded } from './m3.js';",
            "export function forwarded() {}",
            None,
            true,
        ),
        (
            "export { forwarded } from './m3.js';",
            "export { forwarded } from './m5.js';",
            Some(("m5.js", "export function forwarded() {}")),
            false,
        ),
        ("export { forwarded } from './m1.js';", "", None, false),
    ] {
        let mut files = vec![
            ("use.js", "export * from './m1.js';"),
            (
                "m1.js",
                "export function good() {} export { forwarded } from './m2.js';",
            ),
            ("m2.js", m2),
            ("m3.js", "export { forwarded } from './m4.js';"),
            ("m4.js", m4),
        ];
        if let Some(extra) = extra {
            files.push(extra);
        }
        let input = snapshot(&files);
        let data = project_javascript_source(&input, 20);
        assert_eq!(edge(&data, "production", "use.js", "m1.js")["count"], 1);
        assert_eq!(has_edge(&data, "use.js", "m4.js"), resolves, "{data}");
        for intermediate in ["m2.js", "m3.js", "m5.js"] {
            assert!(!has_edge(&data, "use.js", intermediate));
        }
        assert_eq!(data["counts"]["mappedSites"], 1);
        assert_eq!(data["counts"]["unresolvedSites"], usize::from(!resolves));
    }
}

#[test]
fn references_javascript_followed_hazards_do_not_poison_unrelated_modules() {
    for hazard in [
        "module.exports = object;",
        "exports[name] = object;",
        "Object.assign(exports, object);",
    ] {
        let provider = format!("export function good() {{}} {hazard}");
        let input = snapshot(&[
            ("use.js", "export * from './provider.js';"),
            ("provider.js", &provider),
        ]);
        let data = project_javascript_source(&input, 20);
        assert!(
            data["production"].as_array().unwrap().is_empty(),
            "{hazard}: {data}"
        );
        assert_eq!(data["counts"]["unresolvedSites"], 1);
        let input = snapshot(&[
            ("use.js", "export * from './provider.js';"),
            ("provider.js", "export function good() {}"),
            ("unrelated.js", &provider),
        ]);
        let data = project_javascript_source(&input, 20);
        assert_eq!(
            edge(&data, "production", "use.js", "provider.js")["count"],
            1
        );
        assert_eq!(data["counts"]["unresolvedSites"], 0);
        assert!(!module_paths(&data).contains(&"unrelated.js"));
    }
    let input = snapshot(&[
        ("use.js", "export * from './facade.js';"),
        (
            "facade.js",
            "export * as ns from './other.js'; export * from './provider.js';",
        ),
        (
            "provider.js",
            "export function ns() {} export function good() {}",
        ),
        ("other.js", "export function member() {}"),
    ]);
    let data = project_javascript_source(&input, 20);
    assert_eq!(
        edge(&data, "production", "use.js", "provider.js")["count"],
        1
    );
    assert!(!has_edge(&data, "use.js", "other.js"));
    assert!(!has_edge(&data, "use.js", "facade.js"));
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
    let input = snapshot(&[
        (
            "use.js",
            "// export * from './decoy.js';\nconst text = \"export * from './decoy.js';\";\nrequire('./decoy.js'); import('./decoy.js');",
        ),
        ("decoy.js", "export function hidden() {}"),
    ]);
    let data = project_javascript_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
    assert_eq!(data["counts"]["unresolvedSites"], 2);
    let spellings: Vec<_> = data["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["site"]["spelling"].as_str().unwrap())
        .collect();
    assert!(
        spellings
            .iter()
            .any(|spelling| spelling.starts_with("require("))
    );
    assert!(
        spellings
            .iter()
            .any(|spelling| spelling.starts_with("import("))
    );
}

#[test]
fn references_javascript_explicit_module_sites_survive_missing_members() {
    let input = snapshot(&[
        (
            "use.js",
            "import { missing as alias } from './provider.js'; export { missing as named } from './provider.js'; export * from './facade.js';",
        ),
        (
            "facade.js",
            "import { missing as alias } from './provider.js'; export { alias };",
        ),
        ("provider.js", "export function other() {}"),
    ]);
    let data = project_javascript_source(&input, 20);
    let pair = edge(&data, "production", "use.js", "provider.js");
    assert_eq!(pair["count"], 2);
    assert!(
        pair["sites"]
            .as_array()
            .unwrap()
            .iter()
            .all(|site| site["spelling"] == "'./provider.js'")
    );
    assert!(!has_edge(&data, "use.js", "facade.js"));
    assert_eq!(data["counts"]["mappedSites"], 2);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
}

#[test]
fn references_javascript_fanout_clipping_preserves_native_proof_roles_and_order() {
    let source = "export * from './facade.js';\nexport * from './facade.js';\n";
    let mut input = snapshot(&[
        ("use.js", source),
        ("tests/use.js", "export * from '../facade.js';"),
        (
            "facade.js",
            "export { a } from './left.js'; export { b } from './right.js';",
        ),
        ("left.js", "export function a() {}"),
        ("right.js", "export function b() {}"),
        ("unrelated.js", "export * from './missing.js';"),
    ]);
    let paths = ["use.js".into(), "tests/use.js".into()];
    let complete = references::project(&input, &paths, 20, &mut || Ok(()))
        .unwrap()
        .data;
    let clipped = references::project(&input, &paths, 1, &mut || Ok(()))
        .unwrap()
        .data;
    for data in [&complete, &clipped] {
        assert_eq!(data["counts"]["scannedDocuments"], 2);
        assert_eq!(data["counts"]["mappedSites"], 3);
        assert_eq!(data["counts"]["unresolvedSites"], 0);
        assert_eq!(data["production"].as_array().unwrap().len(), 2);
        assert_eq!(data["tests"].as_array().unwrap().len(), 2);
        assert_eq!(
            module_paths(data),
            ["facade.js", "left.js", "right.js", "tests/use.js", "use.js"]
        );
        assert_javascript_native_modules(&input, data);
        for (section, role, path, count) in [
            ("production", "production", "use.js", 2),
            ("tests", "test", "tests/use.js", 1),
        ] {
            let document = input
                .documents
                .iter()
                .find(|doc| doc.key.path == path)
                .unwrap();
            let owner = document
                .declarations
                .iter()
                .find(|row| row.kind == "module" && row.ancestors.is_empty())
                .unwrap();
            assert_eq!(serde_json::to_value(owner.role).unwrap(), role);
            for target in ["left.js", "right.js"] {
                let pair = edge(data, section, path, target);
                assert_eq!(pair["count"], count);
                for site in pair["sites"].as_array().unwrap() {
                    let start = site["range"]["start"].as_u64().unwrap() as usize;
                    let end = site["range"]["end"].as_u64().unwrap() as usize;
                    assert_eq!(
                        &document.file.text[start..end],
                        site["spelling"].as_str().unwrap()
                    );
                    assert_eq!(site["ownerSyntaxId"], owner.syntax_id);
                    assert_eq!(site["role"], role);
                    assert_eq!(site["relationKind"], "import");
                    assert!(site["callId"].is_null());
                    assert_eq!(
                        site["lines"]["start"],
                        document.file.text[..start]
                            .bytes()
                            .filter(|byte| *byte == b'\n')
                            .count()
                            + 1
                    );
                }
            }
        }
    }
    assert_eq!(complete["modules"], clipped["modules"]);
    for target in ["left.js", "right.js"] {
        let pair = edge(&clipped, "production", "use.js", target);
        assert_eq!(pair["omittedSites"], 1);
        assert_eq!(pair["sites"].as_array().unwrap().len(), 1);
        assert_eq!(pair["sites"][0]["range"]["start"], 0);
        assert_eq!(
            pair["sites"][0]["range"]["end"],
            source.lines().next().unwrap().len()
        );
    }
    input.documents.reverse();
    for document in &mut input.documents {
        document.declarations.reverse();
    }
    assert_eq!(
        references::project(&input, &paths, 1, &mut || Ok(()))
            .unwrap()
            .data,
        clipped
    );
}

#[test]
fn references_javascript_test_only_export_is_not_unconditional_production_proof() {
    let input = snapshot(&[
        ("use.js", "export * from './tests/provider.js';"),
        ("tests/provider.js", "export function testOnly() {}"),
    ]);
    let native = input
        .documents
        .iter()
        .find(|doc| doc.key.path == "tests/provider.js")
        .unwrap()
        .declarations
        .iter()
        .find(|row| row.name.as_deref() == Some("testOnly"))
        .unwrap();
    assert_eq!(serde_json::to_value(native.role).unwrap(), "test");
    let data = project_javascript_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
    assert_eq!(data["counts"]["unresolvedSites"], 1);
}

fn snapshot_javascript_manifest(files: &[(&str, &str)], manifest: &str) -> Snapshot {
    let root = tempfile::tempdir().unwrap();
    for (path, text) in files
        .iter()
        .copied()
        .chain(std::iter::once(("package.json", manifest)))
    {
        let path = root.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        &identity.record_id,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let manifest_path = std::fs::canonicalize(root.path())
        .unwrap()
        .join("package.json");
    let bytes = capture.bytes(&manifest_path).unwrap().to_vec();
    assert_eq!(bytes, manifest.as_bytes());
    let manifest = references::external::ManifestInput {
        source_set_id: native.revision.source_set_id.clone(),
        native_revision_id: native.revision.id.clone(),
        dependency_hash: native.revision.dependency_hash.clone(),
        path: "package.json".into(),
        content_hash: Some(hex::encode(Sha256::digest(&bytes))),
        bytes: Some(bytes),
    };
    let documents = graph
        .files
        .into_iter()
        .map(|file| {
            let key = native
                .revision
                .documents
                .iter()
                .find(|doc| doc.key.path == file.path)
                .unwrap()
                .key
                .clone();
            DocumentInput {
                semantic: Default::default(),
                declarations: native
                    .declarations
                    .iter()
                    .filter(|row| row.document == key)
                    .cloned()
                    .collect(),
                calls: native
                    .calls
                    .iter()
                    .filter(|row| row.document == key)
                    .cloned()
                    .collect(),
                key,
                file,
            }
        })
        .collect();
    Snapshot {
        pin: IndexPin {
            index_generation: uuid::Uuid::new_v4(),
            index_revision: 7,
        },
        documents,
        manifests: vec![manifest],
    }
}

#[test]
fn references_javascript_external_stars_veto_native_names_in_both_orders_but_explicit_settles() {
    for (specifier, package, stdlib) in [
        ("fixture-package", Some("fixture-package"), false),
        ("node:fs", Some("fs"), true),
        ("unknown-package", None, false),
    ] {
        for external_first in [false, true] {
            let local = "export * from './provider.js';";
            let external = format!("export * from '{specifier}';");
            let source = if external_first {
                format!("{external}\n{local}")
            } else {
                format!("{local}\n{external}")
            };
            let settled = format!("{source}\nexport {{ value }} from './provider.js';");
            let input = snapshot_javascript_manifest(
                &[
                    ("use.js", &source),
                    ("probe.js", "export * from './settled.js';"),
                    ("settled.js", &settled),
                    ("provider.js", "export function value() {}"),
                ],
                r#"{"dependencies":{"fixture-package":"^1.2.3"}}"#,
            );
            let paths = ["use.js".into(), "probe.js".into()];
            let data = references::project(&input, &paths, 20, &mut || Ok(()))
                .unwrap()
                .data;
            assert!(
                !has_edge(&data, "use.js", "provider.js"),
                "{source}: {data}"
            );
            assert_eq!(
                edge(&data, "production", "probe.js", "provider.js")["count"],
                1
            );
            assert!(!has_edge(&data, "probe.js", "settled.js"));
            assert_eq!(
                data["counts"]["mappedSites"],
                1 + usize::from(package.is_some())
            );
            assert_eq!(data["counts"]["unresolvedSites"], 3);
            assert_eq!(
                data["unresolved"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|row| row["source"]["path"] == "use.js")
                    .count(),
                2
            );
            let packages = data["externalPackages"].as_array().unwrap();
            assert_eq!(packages.len(), usize::from(package.is_some()));
            if let Some(package) = package {
                let pairs: Vec<_> = data["production"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|pair| pair["target"]["kind"] == "externalPackage")
                    .collect();
                assert_eq!(pairs.len(), 1);
                let pair = pairs[0];
                assert_eq!(pair["source"]["path"], "use.js");
                assert_eq!(pair["target"]["package"], package);
                assert_eq!(pair["target"]["standardLibrary"], stdlib);
                assert!(pair["target"].get("sid").is_none());
                assert!(pair["target"].get("path").is_none());
                assert_eq!(pair["count"], 1);
                assert_eq!(pair["sites"][0]["spelling"], external);
                assert_eq!(packages[0]["identity"], pair["target"]);
                if !stdlib {
                    let retained = &input.manifests[0];
                    let evidence = &packages[0]["evidence"][0];
                    assert_eq!(evidence["nativeRevisionId"], retained.native_revision_id);
                    assert_eq!(evidence["dependencyHash"], retained.dependency_hash);
                    assert_eq!(
                        evidence["contentHash"],
                        retained.content_hash.as_deref().unwrap()
                    );
                    assert_eq!(evidence["path"], retained.path);
                }
            }
            assert!(!module_paths(&data).contains(&"package.json"));
            assert_javascript_native_modules(&input, &data);
        }
    }
}

#[test]
fn references_javascript_normal_checkout_selected_and_all_sources_share_scaled_budget() {
    let statements = (0..14)
        .map(|index| {
            format!(
                "    for (let step = 0; step < 4; step++) {{\n        const amount = step * 31 + {index};\n        if ((amount & 1) === 0) {{\n            value += amount;\n        }} else {{\n            value -= amount;\n        }}\n    }}\n"
            )
        })
        .collect::<String>();
    let body = format!(
        "function ordinary_work() {{\n    let value = 0;\n{statements}    return value;\n}}\n"
    );
    let mut files = vec![
        (
            "provider.js".to_owned(),
            format!("export function f() {{}}\n{body}"),
        ),
        (
            "facade.js".to_owned(),
            format!("export {{ f }} from './provider.js';\n{body}"),
        ),
        (
            "bridge.js".to_owned(),
            format!("export * from './facade.js';\n{body}"),
        ),
    ];
    for index in 0..497 {
        files.push((format!("consumer_{index:03}.js"), format!("import './provider.js';\nexport {{ f as alias }} from './facade.js';\nexport * from './bridge.js';\n{body}")));
    }
    assert_eq!(files.len(), 500);
    assert!(
        files
            .iter()
            .all(|(_, text)| (100..=200).contains(&text.lines().count()))
    );
    let borrowed: Vec<_> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    let input = snapshot(&borrowed);
    assert_eq!(input.documents.len(), 500);
    let selected = references::project(&input, &["consumer_000.js".into()], 20, &mut || Ok(()))
        .unwrap()
        .data;
    assert_eq!(selected["counts"]["scannedDocuments"], 1);
    assert_eq!(selected["counts"]["mappedSites"], 3);
    assert_eq!(selected["counts"]["unresolvedSites"], 0);
    assert_eq!(selected["production"].as_array().unwrap().len(), 2);
    assert_eq!(
        edge(&selected, "production", "consumer_000.js", "provider.js")["count"],
        2
    );
    assert_eq!(
        edge(&selected, "production", "consumer_000.js", "facade.js")["count"],
        1
    );
    assert!(!has_edge(&selected, "consumer_000.js", "bridge.js"));
    assert_eq!(
        module_paths(&selected),
        ["bridge.js", "consumer_000.js", "facade.js", "provider.js"]
    );
    assert_javascript_native_modules(&input, &selected);
    let complete = scaled_checkout_projection(&input, 20);
    assert_eq!(complete["counts"]["scannedDocuments"], 500);
    assert_eq!(complete["counts"]["mappedSites"], 1493);
    assert_eq!(complete["counts"]["unresolvedSites"], 0);
    assert_eq!(complete["production"].as_array().unwrap().len(), 996);
    assert_eq!(module_paths(&complete).len(), 500);
    for index in 0..497 {
        let source = format!("consumer_{index:03}.js");
        assert_eq!(
            edge(&complete, "production", &source, "provider.js")["count"],
            2
        );
        assert_eq!(
            edge(&complete, "production", &source, "facade.js")["count"],
            1
        );
        assert!(!has_edge(&complete, &source, "bridge.js"));
    }
    assert_eq!(
        edge(&complete, "production", "facade.js", "provider.js")["count"],
        1
    );
    assert_eq!(
        edge(&complete, "production", "bridge.js", "provider.js")["count"],
        1
    );
    for data in [&selected, &complete] {
        assert!(data["tests"].as_array().unwrap().is_empty());
        assert!(data["externalPackages"].as_array().unwrap().is_empty());
    }
}

#[test]
fn references_javascript_module_and_exports_spellings_are_lexical_not_commonjs_hazards() {
    for name in ["module", "exports"] {
        for facade in [
            format!(
                "import {{ original as {name} }} from './origin.js'; export {{ {name} as visible }};"
            ),
            format!("export {{ original as {name} }} from './origin.js';"),
        ] {
            let input = snapshot(&[
                ("use.js", "export * from './facade.js';"),
                ("facade.js", &facade),
                ("origin.js", "export function original() {}"),
            ]);
            let data = project_javascript_source(&input, 20);
            assert_eq!(
                edge(&data, "production", "use.js", "origin.js")["count"],
                1,
                "{facade}: {data}"
            );
            assert!(!has_edge(&data, "use.js", "facade.js"));
            assert_eq!(data["counts"]["mappedSites"], 1);
            assert_eq!(data["counts"]["unresolvedSites"], 0);
            assert_javascript_native_modules(&input, &data);
        }
        let provider = format!("const {name} = 1; export {{ {name} as visible }};");
        let input = snapshot(&[
            ("use.js", "export * from './provider.js';"),
            ("provider.js", &provider),
        ]);
        let data = project_javascript_source(&input, 20);
        assert_eq!(
            edge(&data, "production", "use.js", "provider.js")["count"],
            1,
            "{provider}: {data}"
        );
        assert_eq!(data["counts"]["mappedSites"], 1);
        assert_eq!(data["counts"]["unresolvedSites"], 0);
    }
}

#[test]
fn references_javascript_unstable_writes_invalidate_only_the_owned_alias() {
    let mut cases = Vec::new();
    for operator in ["of", "in"] {
        for (target, resolves) in [("alias", false), ("let alias", true)] {
            cases.push((
                format!("for ({target} {operator} replacements) {{}}"),
                resolves,
            ));
        }
    }
    cases.extend([
        ("const text = `${alias = replacement}`;".into(), false),
        ("const text = `alias = replacement`;".into(), true),
    ]);
    for (write, resolves) in cases {
        let provider =
            format!("function original() {{}} let alias = original; {write} export {{ alias }};");
        let input = snapshot(&[
            ("use.js", "export * from './provider.js';"),
            ("provider.js", &provider),
        ]);
        let data = project_javascript_source(&input, 20);
        assert_eq!(
            has_edge(&data, "use.js", "provider.js"),
            resolves,
            "{provider}: {data}"
        );
        assert_eq!(data["counts"]["mappedSites"], usize::from(resolves));
        assert_eq!(data["counts"]["unresolvedSites"], usize::from(!resolves));
        assert_eq!(
            data["production"].as_array().unwrap().len(),
            usize::from(resolves)
        );
        assert_eq!(
            data["unresolved"].as_array().unwrap().len(),
            usize::from(!resolves)
        );
    }
}

#[test]
fn references_javascript_review_local_alias_bindings_do_not_collapse_star_identity() {
    for declaration in ["const", "let", "var"] {
        for source in [
            "export * from './a.js'; export * from './b.js';",
            "export * from './b.js'; export * from './a.js';",
        ] {
            let facade = format!("import {{ f as g }} from './a.js'; export {declaration} f = g;");
            let input = snapshot(&[
                ("use.js", source),
                ("a.js", "export function f() {}"),
                ("b.js", &facade),
            ]);
            let a = input
                .documents
                .iter()
                .find(|doc| doc.key.path == "a.js")
                .unwrap()
                .declarations
                .iter()
                .find(|row| row.name.as_deref() == Some("f"))
                .unwrap();
            let b = input
                .documents
                .iter()
                .find(|doc| doc.key.path == "b.js")
                .unwrap()
                .declarations
                .iter()
                .find(|row| row.kind == "variable" && row.name.as_deref() == Some("f"))
                .unwrap();
            assert_ne!(a.syntax_id, b.syntax_id);
            let data = project_javascript_source(&input, 20);
            assert!(
                data["production"].as_array().unwrap().is_empty(),
                "{declaration}: {source}: {data}"
            );
            assert_eq!(data["counts"]["mappedSites"], 0);
            assert_eq!(data["counts"]["unresolvedSites"], 2);
        }
    }
    for declaration in ["const", "let", "var"] {
        let facade = format!("import {{ f as g }} from './a.js'; export {declaration} f = g;");
        let mut input = snapshot(&[
            ("use.js", "export * from './b.js';"),
            ("a.js", "export function f() {}"),
            ("b.js", &facade),
        ]);
        let data = project_javascript_source(&input, 20);
        assert_eq!(edge(&data, "production", "use.js", "b.js")["count"], 1);
        assert!(!has_edge(&data, "use.js", "a.js"));
        assert_eq!(data["counts"]["unresolvedSites"], 0);
        input
            .documents
            .iter_mut()
            .find(|doc| doc.key.path == "b.js")
            .unwrap()
            .declarations
            .retain(|row| !(row.kind == "variable" && row.name.as_deref() == Some("f")));
        let missing = project_javascript_source(&input, 20);
        assert!(
            missing["production"].as_array().unwrap().is_empty(),
            "{declaration}: {missing}"
        );
        assert_eq!(missing["counts"]["unresolvedSites"], 1);
    }
    for source in [
        "export * from './a.js'; export * from './b.js';",
        "export * from './b.js'; export * from './a.js';",
    ] {
        let input = snapshot(&[
            ("use.js", source),
            ("a.js", "export function f() {}"),
            (
                "b.js",
                "import { f as g } from './a.js'; export { g as f };",
            ),
        ]);
        let data = project_javascript_source(&input, 20);
        assert_eq!(edge(&data, "production", "use.js", "a.js")["count"], 2);
        assert!(!has_edge(&data, "use.js", "b.js"));
        assert_eq!(data["counts"]["mappedSites"], 2);
        assert_eq!(data["counts"]["unresolvedSites"], 0);
    }
}

#[test]
fn references_javascript_review_trailing_slash_never_selects_sibling_file() {
    let input = snapshot(&[
        ("use.js", "export * from './lib/';"),
        ("lib.js", "export function decoy() {}"),
    ]);
    let data = project_javascript_source(&input, 20);
    assert!(!has_edge(&data, "use.js", "lib.js"), "{data}");
    assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert_eq!(data["counts"]["unresolvedSites"], 1);

    let input = snapshot(&[
        ("use.js", "export * from './lib/';"),
        ("lib.js", "export function decoy() {}"),
        ("lib/index.js", "export function actual() {}"),
    ]);
    let data = project_javascript_source(&input, 20);
    assert!(!has_edge(&data, "use.js", "lib.js"), "{data}");
    assert_eq!(
        edge(&data, "production", "use.js", "lib/index.js")["count"],
        1
    );
    assert_eq!(data["counts"]["mappedSites"], 1);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
}

#[test]
fn references_javascript_review_stars_into_commonjs_files_remain_unknown() {
    let input = snapshot(&[
        ("use.js", "export * from './provider.cjs';"),
        ("provider.cjs", "export function value() {}"),
    ]);
    let data = project_javascript_source(&input, 20);
    assert!(data["production"].as_array().unwrap().is_empty(), "{data}");
    assert_eq!(data["counts"]["mappedSites"], 0);
    assert_eq!(data["counts"]["unresolvedSites"], 1);
}

#[test]
fn references_javascript_star_export_fanout_exhausts_scaled_work_atomically() {
    let provider = (0..256)
        .map(|index| format!("export function value_{index:03}() {{}}\n"))
        .collect::<String>();
    let consumer = "export * from './provider.js';\n".repeat(512);
    let input = snapshot(&[("provider.js", &provider), ("consumer.js", &consumer)]);
    assert_eq!(input.documents.len(), 2);
    assert!(provider.len() + consumer.len() < 32 * 1024);

    // Repeated star clauses merge 256 real exported names 512 times. This
    // exhausts the normally sized work allowance, not a byte/AST/site cap.
    // An Err from the public projection API contains no partial projection.
    let error = references::project(&input, &[], 1, &mut || Ok(())).unwrap_err();
    assert_eq!(error.to_string(), "bounded_work: reference owner lookup");
}

#[test]
fn references_javascript_review_500_consumers_share_a_200_name_barrel_within_budget() {
    let statements = (0..110)
        .map(|index| format!("    value += {index};\n"))
        .collect::<String>();
    let body = format!(
        "function ordinary_work() {{\n    let value = 0;\n{statements}    return value;\n}}\n"
    );
    let mut files = Vec::new();
    let mut barrel = String::new();
    for provider in 0..20 {
        let exports = (0..10)
            .map(|name| format!("export function value_{provider:02}_{name:02}() {{}}\n"))
            .collect::<String>();
        files.push((
            format!("provider_{provider:02}.js"),
            format!("{exports}{body}"),
        ));
        barrel.push_str(&format!("export * from './provider_{provider:02}.js';\n"));
    }
    files.push(("barrel.js".to_owned(), format!("{barrel}{body}")));
    for consumer in 0..500 {
        files.push((
            format!("consumer_{consumer:03}.js"),
            format!("export * from './barrel.js';\n{body}"),
        ));
    }
    assert_eq!(files.len(), 521);
    assert!(
        files
            .iter()
            .all(|(_, text)| (100..=200).contains(&text.lines().count()))
    );
    let borrowed: Vec<_> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    let input = snapshot(&borrowed);
    assert_eq!(input.documents.len(), 521);
    let data = references::project(&input, &["**".into()], 1, &mut || Ok(()))
        .unwrap()
        .data;
    assert_eq!(data["counts"]["scannedDocuments"], 521);
    assert_eq!(data["counts"]["mappedSites"], 520);
    assert_eq!(data["counts"]["unresolvedSites"], 0);
    assert_eq!(data["production"].as_array().unwrap().len(), 10020);
    assert_eq!(module_paths(&data).len(), 521);
    for provider in 0..20 {
        let target = format!("provider_{provider:02}.js");
        assert_eq!(edge(&data, "production", "barrel.js", &target)["count"], 1);
        for consumer in 0..500 {
            let source = format!("consumer_{consumer:03}.js");
            let pair = edge(&data, "production", &source, &target);
            assert_eq!(pair["count"], 1);
            assert_eq!(pair["omittedSites"], 0);
            assert_eq!(pair["sites"].as_array().unwrap().len(), 1);
        }
    }
    for consumer in 0..500 {
        assert!(!has_edge(
            &data,
            &format!("consumer_{consumer:03}.js"),
            "barrel.js"
        ));
    }
    assert!(data["tests"].as_array().unwrap().is_empty());
    assert!(data["externalPackages"].as_array().unwrap().is_empty());
}

/// One compile-shaped file of each kind that nests past the walk bounds.
fn over_deep_sources(depth: usize) -> Vec<(&'static str, String)> {
    let nest = |open: &str, close: &str, inner: &str| {
        format!("{}{inner}{}", open.repeat(depth), close.repeat(depth))
    };
    vec![
        (
            "deep.rs",
            format!("pub fn deep() {{ custom!({}); }}\n", nest("(", ")", "")),
        ),
        (
            "deep.rs",
            format!("pub fn deep() -> i32 {{ {} }}\n", nest("(", ")", "1")),
        ),
        (
            "deep.rs",
            format!(
                "pub fn deep() {{ {} }}\n",
                nest("{", "}", "crate::ok::run();")
            ),
        ),
        (
            "deep.rs",
            format!("{}\n", nest("mod a {", "}", "pub fn run() {}")),
        ),
        ("deep.rs", format!("use a::{};\n", nest("{b::", "}", "c"))),
        (
            "deep.js",
            format!("function deep() {{ return {}; }}\n", nest("[", "]", "")),
        ),
        (
            "deep.js",
            format!("function deep() {{ let {} = 1; }}\n", nest("[", "]", "x")),
        ),
        (
            "deep.js",
            format!("function deep() {{ return a{}; }}\n", ".b".repeat(depth)),
        ),
        (
            "Deep.java",
            format!(
                "class Deep {{ int d() {{ return {}; }} }}\n",
                nest("(", ")", "1")
            ),
        ),
        ("Deep.java", format!("{}\n", nest("class A {", "}", ""))),
        (
            "deep.py",
            format!("def deep():\n    return {}\n", nest("(", ")", "1")),
        ),
    ]
}

#[test]
fn an_over_deep_file_is_partial_and_the_rest_of_the_checkout_is_served() {
    for (path, source) in over_deep_sources(400) {
        let input = snapshot(&[
            ("src/lib.rs", "pub mod ok; pub mod other;"),
            ("src/ok.rs", "pub fn run() { crate::other::run(); }"),
            ("src/other.rs", "pub fn run() {}"),
            (path, source.as_str()),
        ]);
        let projection = references::project(&input, &[], 5, &mut || Ok(())).unwrap();
        let data = &projection.data;
        let partial = data["partialFiles"].as_array().unwrap();
        assert_eq!(partial.len(), 1, "{path}: {source:.60}: {partial:?}");
        assert_eq!(partial[0]["path"], path);
        assert_eq!(partial[0]["reason"], "depth_limit");
        assert!(partial[0]["subtrees"].as_u64().unwrap() >= 1);
        let range = partial[0]["range"]["start"].as_u64().unwrap() as usize
            ..partial[0]["range"]["end"].as_u64().unwrap() as usize;
        assert!(range.start > 0 && range.end <= source.len());
        assert!(
            data["limitations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|limit| limit.as_str().unwrap().contains("partialFiles"))
        );
        assert!(
            data["production"].as_array().unwrap().iter().any(|edge| {
                edge["source"]["path"] == "src/ok.rs" && edge["target"]["path"] == "src/other.rs"
            }),
            "{path}: {data}"
        );
    }
}

#[test]
fn a_path_filter_still_reports_a_partial_file_that_feeds_resolution() {
    let (path, source) = over_deep_sources(400).remove(5);
    let input = snapshot(&[("ok.js", "export const ok = 1;"), (path, source.as_str())]);
    let projection = references::project(&input, &["ok.js".to_owned()], 5, &mut || Ok(())).unwrap();
    assert_eq!(projection.data["partialFiles"][0]["path"], path);
}

#[test]
fn nesting_at_any_depth_around_the_bound_indexes_and_projects() {
    // An enclosing node records its name, callee and invocation before walking its children,
    // so a child skipped at the bound can span one: the gap must not claim that candidate.
    let shapes: [(&str, &str, &str, &str, &str); 7] = [
        ("deep.rs", "fn f() { ", "g(", "1", ")"),
        ("deep.rs", "fn f() { ", "a::g(", "1", ")"),
        ("deep.rs", "fn f() { ", "x.g(", "1", ")"),
        ("deep.rs", "", "fn g() {", "", "}"),
        ("deep.js", "function f() { ", "a.g(", "1", ")"),
        (
            "Deep.java",
            "class Deep { int d() { return ",
            "a.g(",
            "1",
            ")",
        ),
        ("deep.py", "x = ", "a.g(", "1", ")"),
    ];
    for (path, prefix, open, inner, close) in shapes {
        for depth in 30..140 {
            let tail = match (prefix, path) {
                ("", _) => "",
                (_, "deep.rs") => "; }",
                (_, "deep.js") => "; }",
                (_, "Deep.java") => "; } }",
                _ => "",
            };
            let source = format!(
                "{prefix}{}{inner}{}{tail}\n",
                open.repeat(depth),
                close.repeat(depth)
            );
            let input = snapshot(&[(path, source.as_str())]);
            references::project(&input, &[], 5, &mut || Ok(()))
                .unwrap_or_else(|error| panic!("{path} {open} at {depth}: {error:#}"));
        }
    }
}

#[test]
fn extremely_deep_sources_stay_bounded_and_never_overflow_the_stack() {
    for (path, source) in over_deep_sources(50_000) {
        let input = snapshot(&[("ok.rs", "pub fn run() {}"), (path, source.as_str())]);
        let projection = references::project(&input, &[], 5, &mut || Ok(())).unwrap();
        assert_eq!(projection.data["partialFiles"][0]["path"], path);
    }
}

#[test]
fn a_file_past_the_visit_bound_is_partial_too() {
    let source = format!("const v = [{}];\n", "0,".repeat(1_000_001));
    let input = snapshot(&[
        ("ok.js", "export const ok = 1;"),
        ("wide.js", source.as_str()),
    ]);
    let projection = references::project(&input, &[], 5, &mut || Ok(())).unwrap();
    assert_eq!(projection.data["partialFiles"][0]["path"], "wide.js");
    assert_eq!(projection.data["partialFiles"][0]["reason"], "visit_limit");
}

#[test]
fn a_hazard_past_the_walk_bounds_keeps_the_export_proof_unknown() {
    let resolve = |deep: usize| {
        let barrel = format!(
            "export * from './p.js';\nconst d = {}module{};\n",
            "[".repeat(deep),
            "]".repeat(deep)
        );
        let input = snapshot(&[
            ("barrel.js", barrel.as_str()),
            ("p.js", "export const name = 1;\n"),
            ("use.js", "import { name } from './barrel.js';\nname;\n"),
        ]);
        let data = references::project(&input, &[], 5, &mut || Ok(()))
            .unwrap()
            .data;
        (
            data["production"].clone(),
            data["unresolved"].as_array().unwrap().len(),
        )
    };
    let (shallow, shallow_unresolved) = resolve(2);
    let (deep, deep_unresolved) = resolve(200);
    assert!(shallow_unresolved >= 1);
    assert_eq!(deep_unresolved, shallow_unresolved);
    assert_eq!(
        deep.as_array().unwrap().len(),
        shallow.as_array().unwrap().len()
    );

    let resolve = |deep: usize| {
        let module = format!(
            "from p import *\n{}exec(code){}\n",
            "[".repeat(deep),
            "]".repeat(deep)
        );
        let input = snapshot(&[
            ("a.py", module.as_str()),
            ("p.py", "name = 1\n"),
            ("use.py", "from a import name\n"),
        ]);
        let data = references::project(&input, &[], 5, &mut || Ok(()))
            .unwrap()
            .data;
        data["unresolved"].as_array().unwrap().len()
    };
    assert_eq!(resolve(200), resolve(2));
}

#[test]
fn a_rust_use_list_past_the_walk_bounds_names_its_file() {
    let deep = format!("use a::{}c{};\n", "{b::".repeat(100), "}".repeat(100));
    let input = snapshot(&[("deep.rs", deep.as_str())]);
    let data = references::project(&input, &[], 5, &mut || Ok(()))
        .unwrap()
        .data;
    assert_eq!(data["partialFiles"][0]["path"], "deep.rs");
}

/// Production edges and unresolved sites, for comparing a shallow and a deep spelling.
fn outcome(files: &[(&str, &str)]) -> (Vec<(String, String)>, usize) {
    let input = snapshot(files);
    let data = references::project(&input, &[], 5, &mut || Ok(()))
        .unwrap()
        .data;
    let mut edges: Vec<_> = data["production"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edge| {
            (
                edge["source"]["path"].as_str().unwrap().to_owned(),
                edge["target"]["path"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect();
    edges.sort();
    (edges, data["unresolved"].as_array().unwrap().len())
}

#[test]
fn a_rust_import_past_the_bound_never_lets_a_wildcard_win() {
    let target = |depth: usize| {
        let consumer = format!(
            "use crate::p::*; use {}crate::q as foo{}; fn run() {{ foo::f(); }}\n",
            "{".repeat(depth),
            "}".repeat(depth)
        );
        outcome(&[
            ("src/lib.rs", "mod p; mod q; mod consumer;\n"),
            ("src/p.rs", "pub mod foo { pub fn f() {} }\n"),
            ("src/q.rs", "pub fn f() {}\n"),
            ("src/consumer.rs", consumer.as_str()),
        ])
        .0
    };
    let wrong = ("src/consumer.rs".to_owned(), "src/p.rs".to_owned());
    let right = ("src/consumer.rs".to_owned(), "src/q.rs".to_owned());
    assert!(target(2).contains(&right));
    assert!(!target(2).contains(&wrong));
    assert!(!target(130).contains(&wrong), "{:?}", target(130));
}

#[test]
fn a_java_import_past_the_bound_keeps_the_type_root_unknown() {
    let edges = |depth: usize| {
        let package = "a.".repeat(depth);
        let path = format!("{}Outer.java", package.replace('.', "/"));
        let outer = format!(
            "package {};\npublic class Outer {{ public static class pkg {{ public static class Target {{}} }} }}\n",
            package.trim_end_matches('.')
        );
        let consumer =
            format!("import static {package}Outer.pkg;\nclass Use {{ pkg.Target value; }}\n");
        outcome(&[
            (path.as_str(), outer.as_str()),
            ("pkg/Target.java", "package pkg;\npublic class Target {}\n"),
            ("Use.java", consumer.as_str()),
        ])
        .0
    };
    let wrong = ("Use.java".to_owned(), "pkg/Target.java".to_owned());
    assert!(!edges(2).contains(&wrong));
    assert!(!edges(140).contains(&wrong), "{:?}", edges(140));
}

#[test]
fn a_java_package_past_the_bound_leaves_its_own_names_unknown() {
    let deep = format!(
        "package {};\nclass Use {{ pkg.Target value; }}\nclass pkg {{ static class Target {{}} }}\n",
        ["a"; 140].join(".")
    );
    let (edges, _) = outcome(&[
        ("pkg/Target.java", "package pkg;\npublic class Target {}\n"),
        ("Use.java", deep.as_str()),
    ]);
    assert!(
        edges.iter().all(|(_, target)| target != "pkg/Target.java"),
        "{edges:?}"
    );
}

#[test]
fn a_binding_past_the_bound_never_lets_a_star_export_win() {
    // A deep explicit export shadows the star's name, as a shallow one does.
    let target = |depth: usize| {
        let barrel = format!(
            "export * from './p.js';\nexport const {}name{} = [];\n",
            "[".repeat(depth),
            "]".repeat(depth)
        );
        outcome(&[
            ("barrel.js", barrel.as_str()),
            ("p.js", "export const name = 1;\n"),
            ("use.js", "import { name } from './barrel.js';\nname;\n"),
        ])
    };
    let wrong = ("barrel.js".to_owned(), "p.js".to_owned());
    assert_eq!(target(2).0.contains(&wrong), target(130).0.contains(&wrong));

    let target = |depth: usize| {
        let module = format!(
            "from p import *\n{}name{} = 1\n",
            "(".repeat(depth),
            ")".repeat(depth)
        );
        outcome(&[
            ("a.py", module.as_str()),
            ("p.py", "name = 1\n"),
            ("use.py", "from a import name\n"),
        ])
    };
    let shallow = target(2);
    let deep = target(130);
    assert!(
        !deep
            .0
            .iter()
            .any(|(source, target)| source == "use.py" && target == "p.py")
            || shallow
                .0
                .iter()
                .any(|(source, target)| source == "use.py" && target == "p.py"),
        "{shallow:?} {deep:?}"
    );

    // A Python import nested past the bound shadows a star the same way.
    let target = |depth: usize| {
        let nest: String = (0..depth)
            .map(|level| format!("{}if x:\n", " ".repeat(level)))
            .collect();
        let module = format!(
            "from p import *\n{nest}{}from q import name\n",
            " ".repeat(depth)
        );
        outcome(&[
            ("a.py", module.as_str()),
            ("p.py", "name = 1\n"),
            ("q.py", "name = 2\n"),
            ("use.py", "from a import name\n"),
        ])
    };
    let wrong = ("use.py".to_owned(), "p.py".to_owned());
    assert!(!target(130).0.contains(&wrong), "{:?}", target(130));
}
