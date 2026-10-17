mod common;
fn test_pin(n: u64) -> trellis::model::IndexPin {
    trellis::model::IndexPin {
        index_generation: uuid::Uuid::from_u128(0x00000000000040008000000000000001),
        index_revision: n,
    }
}
use std::{
    collections::HashMap,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    behavior::{Participant, SequenceStep, SequenceView, build_sequence},
    dependencies::{Catalog, CatalogSymbol, Package},
    dependency_links::annotate,
    indexer::{IndexOptions, index_workspace, index_workspace_bundle},
    model::*,
};

fn fixture(source: &str) -> (Graph, SequenceView) {
    fixture_named(source, "run")
}
fn fixture_named(source: &str, name: &str) -> (Graph, SequenceView) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("fixture.rs"), source).unwrap();
    let graph = index_workspace(
        &IndexOptions::new(dir.path().to_owned()),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let seed = graph.nodes.iter().find(|n| n.name == name).unwrap();
    let view = build_sequence(test_pin(7), seed, &graph.files[0], &graph.calls, false).unwrap();
    (graph, view)
}
fn catalog() -> Catalog {
    Catalog {
        id: "catalog:fixture".into(),
        workspace_revision: test_pin(7),
        packages: vec![Package {
            id: "package:std".into(),
            ecosystem: "cargo".into(),
            name: "std".into(),
            version: "1.90".into(),
            source: "stdlib".into(),
            aliases: vec![],
            source_state: "present".into(),
            index_state: "complete".into(),
            warnings: vec![],
        }],
        symbols: vec![CatalogSymbol {
            id: "catalog-symbol:OpenOptions".into(),
            package_id: "package:std".into(),
            name: "OpenOptions".into(),
            qualified_name: "std::fs::OpenOptions".into(),
            kind: "struct".into(),
            parent: None,
            owner_expression: None,
            signature: "pub struct OpenOptions".into(),
            source_ref: "source:std:fs".into(),
            path: "src/fs.rs".into(),
            range: SourceRange::default(),
        }],
        warnings: vec![],
        sources: HashMap::new(),
    }
}
fn flatten(steps: &[SequenceStep]) -> Vec<&SequenceStep> {
    let mut out = vec![];
    let mut pending: Vec<_> = steps.iter().collect();
    while let Some(step) = pending.pop() {
        out.push(step);
        pending.extend(&step.children);
        pending.extend(&step.alternate);
    }
    out
}
fn candidates(view: &SequenceView) -> Vec<&Participant> {
    view.participants
        .iter()
        .filter(|p| p.kind == "externalCandidate")
        .collect()
}
fn unchanged(source: &str, catalog: &Catalog) {
    let (graph, mut view) = fixture(source);
    let before = view.clone();
    annotate(&mut view, &graph.files[0], catalog);
    assert_eq!(view, before, "unexpected annotation: {source}");
}

#[test]
fn exact_path_adds_terminal_type_and_preserves_all_measured_evidence() {
    let (graph, mut view) = fixture("fn run() { std::fs::OpenOptions::new(); }");
    // Labels are presentation strings, not a lookup key.
    view.steps
        .iter_mut()
        .find(|s| s.call_id.is_some())
        .unwrap()
        .label = "new".into();
    let before = view.clone();
    let original_graph = graph.clone();
    annotate(&mut view, &graph.files[0], &catalog());
    assert_eq!(view, before);
    assert!(candidates(&view).is_empty());
    assert!(flatten(&view.steps).iter().all(|s| s.target.is_none()));
    assert_eq!(graph, original_graph);
    let once = view.clone();
    annotate(&mut view, &graph.files[0], &catalog());
    assert_eq!(view, once);
}

#[test]
fn plain_and_group_imports_are_explicit_syntax_candidates() {
    for source in [
        "use std::fs::OpenOptions; fn run() { OpenOptions::new(); }",
        "use std::{fs::{OpenOptions}}; fn run() { OpenOptions::new(); }",
        "use std::fs; fn run() { fs::OpenOptions::new(); }",
        "use std::fs::{self, OpenOptions}; fn run() { fs::OpenOptions::new(); }",
        "fn run() { use std::fs::OpenOptions; OpenOptions::new(); }",
    ] {
        let (graph, mut view) = fixture(source);
        annotate(&mut view, &graph.files[0], &catalog());
        assert_eq!(candidates(&view).len(), 0, "{source}");
    }
}

#[test]
fn cargo_alias_maps_to_declared_crate_prefix_not_package_name() {
    let mut catalog = catalog();
    catalog.packages[0].name = "package-with-other-name".into();
    catalog.packages[0].aliases = vec!["disk-access".into()];
    for source in [
        "fn run() { disk_access::fs::OpenOptions::new(); }",
        "use disk_access::fs::OpenOptions; fn run() { OpenOptions::new(); }",
    ] {
        let (graph, mut view) = fixture(source);
        annotate(&mut view, &graph.files[0], &catalog);
        assert_eq!(candidates(&view).len(), 0);
    }
}

#[test]
fn known_bindings_modules_aliases_and_renamed_imports_block_candidates() {
    for source in [
        "mod std {} fn run() { std::fs::OpenOptions::new(); }",
        "type std = Local; fn run() { std::fs::OpenOptions::new(); }",
        "fn run(std: Local) { std::fs::OpenOptions::new(); }",
        "fn run() { let std = local(); std::fs::OpenOptions::new(); }",
        "fn run() { let (std, other) = local(); std::fs::OpenOptions::new(); }",
        "fn run<std>() { std::fs::OpenOptions::new(); }",
        "fn run() { let f = |std| std::fs::OpenOptions::new(); std::fs::OpenOptions::new(); }",
        "use other as std; fn run() { std::fs::OpenOptions::new(); }",
        "use other::{Thing as std}; fn run() { std::fs::OpenOptions::new(); }",
        "use std::fs::OpenOptions as Options; fn run() { Options::new(); }",
        "use std::fs::OpenOptions; type OpenOptions = Local; fn run() { OpenOptions::new(); }",
        "use other::OpenOptions; use std::fs::OpenOptions; fn run() { OpenOptions::new(); }",
        "use other::*; fn run() { std::fs::OpenOptions::new(); }",
        "mod sibling { use std::fs::OpenOptions; } fn run() { OpenOptions::new(); }",
        "fn run() { { use std::fs::OpenOptions; } OpenOptions::new(); }",
    ] {
        unchanged(source, &catalog());
    }
}

#[test]
fn workspace_paths_and_bare_names_are_not_global_matches() {
    for source in [
        "fn run() { OpenOptions::new(); }",
        "fn run() { OpenOptions(); }",
        "fn run() { crate::std::fs::OpenOptions::new(); }",
        "fn run() { self::std::fs::OpenOptions::new(); }",
        "fn run() { super::std::fs::OpenOptions::new(); }",
        "use crate::std::fs::OpenOptions; fn run() { OpenOptions::new(); }",
    ] {
        unchanged(source, &catalog());
    }
}

#[test]
fn duplicate_declarations_and_package_versions_are_ambiguous() {
    let source = "fn run() { std::fs::OpenOptions::new(); }";
    let mut c = catalog();
    let mut duplicate = c.symbols[0].clone();
    duplicate.id.push_str(":duplicate");
    c.symbols.push(duplicate);
    unchanged(source, &c);
    let mut c = catalog();
    let mut version = c.packages[0].clone();
    version.id.push_str(":v2");
    version.version = "2".into();
    let mut duplicate = c.symbols[0].clone();
    duplicate.id.push_str(":v2");
    duplicate.package_id = version.id.clone();
    c.packages.push(version);
    c.symbols.push(duplicate);
    unchanged(source, &c);
    c.packages[0].aliases = vec!["std_v1".into()];
    let (g, mut v) = fixture("fn run() { std_v1::fs::OpenOptions::new(); }");
    annotate(&mut v, &g.files[0], &c);
    assert_eq!(candidates(&v).len(), 0);
    c.packages[1].aliases = vec!["std_v1".into()];
    unchanged("fn run() { std_v1::fs::OpenOptions::new(); }", &c);
}

#[test]
fn fluent_chain_children_are_visited_but_return_types_and_receivers_are_unknown() {
    let (graph, mut view) = fixture(
        r#"fn run() {
        let file = std::fs::OpenOptions::new().write(true).open("x");
        file.sync_all();
    }"#,
    );
    assert!(flatten(&view.steps).iter().any(|s| s.kind == "group"));
    let before = view.clone();
    annotate(&mut view, &graph.files[0], &catalog());
    assert_eq!(candidates(&view).len(), 0);
    assert_eq!(view, before);
    assert!(flatten(&view.steps).iter().all(|s| s.target.is_none()));
    unchanged("fn run() { let x = unknown(); x.new(); }", &catalog());
}

#[test]
fn stale_revision_non_rust_wrong_source_and_oversized_source_do_nothing() {
    let source = "fn run() { std::fs::OpenOptions::new(); }";
    let mut stale = catalog();
    stale.workspace_revision.index_revision += 1;
    unchanged(source, &stale);
    for change in 0..3 {
        let (mut graph, mut view) = fixture(source);
        let before = view.clone();
        match change {
            0 => graph.files[0].language = "javascript".into(),
            1 => graph.files[0].path = "other.rs".into(),
            _ => graph.files[0].text.push_str(&" ".repeat(2 * 1024 * 1024)),
        }
        annotate(&mut view, &graph.files[0], &catalog());
        assert_eq!(view, before);
    }
}

#[test]
fn participant_cap_retains_original_target_on_overflow() {
    let (graph, mut view) = fixture("fn run() { std::fs::OpenOptions::new(); }");
    while view.participants.len() < 20 {
        view.participants.push(Participant {
            id: format!("padding:{}", view.participants.len()),
            label: "padding".into(),
            kind: "boundary".into(),
            identification: "test".into(),
        });
    }
    let before = view.clone();
    annotate(&mut view, &graph.files[0], &catalog());
    assert_eq!(view, before);
}

#[test]
fn only_type_declarations_and_non_resolved_calls_are_candidates() {
    for kind in ["function", "method", "module", "impl", "union", "type"] {
        let mut c = catalog();
        c.symbols[0].kind = kind.into();
        unchanged("fn run() { std::fs::OpenOptions::new(); }", &c);
    }
    for kind in ["struct", "enum", "trait"] {
        let mut c = catalog();
        c.symbols[0].kind = kind.into();
        let (g, mut v) = fixture("fn run() { std::fs::OpenOptions::new(); }");
        annotate(&mut v, &g.files[0], &c);
        assert_eq!(candidates(&v).len(), 0);
    }
    for resolution in [
        Resolution::Internal,
        Resolution::External,
        Resolution::Ambiguous,
    ] {
        let (g, mut v) = fixture("fn run() { std::fs::OpenOptions::new(); }");
        v.steps
            .iter_mut()
            .find(|s| s.call_id.is_some())
            .unwrap()
            .resolution = Some(resolution);
        let before = v.clone();
        annotate(&mut v, &g.files[0], &catalog());
        assert_eq!(v, before);
    }
}

#[test]
fn macros_are_opaque_not_apparent_binding_sources() {
    let (g, mut v) =
        fixture("fn run() { opaque!(let std = thing;); std::fs::OpenOptions::new(); }");
    annotate(&mut v, &g.files[0], &catalog());
    assert_eq!(candidates(&v).len(), 0);
}

#[test]
fn candidate_terminal_id_cannot_be_used_as_workspace_sequence_root() {
    let (graph, mut view) = fixture("fn run() { std::fs::OpenOptions::new(); }");
    annotate(&mut view, &graph.files[0], &catalog());
    assert!(candidates(&view).is_empty());
    // An old dependency candidate ID never becomes a native declaration.
    let id = "dep-type:catalog-symbol:OpenOptions";
    assert!(!graph.nodes.iter().any(|n| n.id == id));
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("fixture.rs"),
        "fn run() { std::fs::OpenOptions::new(); }",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let store = crate::common::open_store(state.path(), workspace.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (bundle_graph, _, _) = index_workspace_bundle(
        &IndexOptions::new(workspace.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    assert_eq!(bundle_graph.files, graph.files);
    assert_eq!(bundle_graph.calls.len(), graph.calls.len());
    assert_eq!(bundle_graph.calls[0].range, graph.calls[0].range);
    assert!(!bundle_graph.nodes.iter().any(|node| node.id == id));
    let (revision, _session) = trellis::index_coordinator::reconcile_workspace(
        &store,
        &IndexOptions::new(workspace.path().to_owned()),
        &cancel,
        |_| {},
    )
    .unwrap();
    assert_eq!(store.graph().unwrap(), bundle_graph);
    assert!(store.sequence_at(id, revision, false).unwrap().is_none());
    assert!(store.symbol(id).unwrap().is_none());
    assert_eq!(store.graph().unwrap().calls, bundle_graph.calls);
}

#[test]
fn transitive_crate_names_and_renamed_package_names_are_not_call_prefixes() {
    let mut c = catalog();
    c.packages[0].source = "registry".into();
    unchanged("fn run() { std::fs::OpenOptions::new(); }", &c);
    c.packages[0].aliases = vec!["direct_alias".into()];
    unchanged("fn run() { std::fs::OpenOptions::new(); }", &c);
    let (g, mut v) = fixture("fn run() { direct_alias::fs::OpenOptions::new(); }");
    annotate(&mut v, &g.files[0], &c);
    assert_eq!(candidates(&v).len(), 0);
}

#[test]
fn shared_package_alias_is_ambiguous_even_when_one_version_has_no_type() {
    let mut c = catalog();
    c.packages[0].aliases = vec!["direct_alias".into()];
    let mut other = c.packages[0].clone();
    other.id = "package:other-version".into();
    other.version = "other".into();
    c.packages.push(other);
    unchanged("fn run() { direct_alias::fs::OpenOptions::new(); }", &c);
    unchanged("fn run() { std::fs::OpenOptions::new(); }", &c);
}

#[test]
fn globs_in_sibling_scopes_do_not_hide_candidate_but_enclosing_globs_do() {
    for source in [
        "mod tests { use super::*; } fn run() { std::fs::OpenOptions::new(); }",
        "fn helper() { use other::*; } fn run() { std::fs::OpenOptions::new(); }",
        "fn run() { { use other::*; } std::fs::OpenOptions::new(); }",
    ] {
        let (graph, mut view) = fixture(source);
        annotate(&mut view, &graph.files[0], &catalog());
        assert_eq!(candidates(&view).len(), 0, "{source}");
    }
    for source in [
        "use other::*; fn run() { std::fs::OpenOptions::new(); }",
        "fn run() { use other::*; std::fs::OpenOptions::new(); }",
        "mod enclosing { use other::*; fn run() { std::fs::OpenOptions::new(); } }",
        "fn run() { { use other::*; std::fs::OpenOptions::new(); } }",
    ] {
        unchanged(source, &catalog());
    }
}

#[test]
fn actual_acp_cached_source_links_constructor_but_never_infers_open_receiver() {
    let source =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/acp.rs")).unwrap();
    let (graph, mut view) = fixture_named(&source, "check_file_with_unlinked");
    // fixture directory is gone: annotation must use only this cached source.
    let before = view.clone();
    annotate(&mut view, &graph.files[0], &catalog());
    assert_eq!(candidates(&view).len(), 0);
    let original = flatten(&before.steps);
    let mut constructors = 0;
    let mut open_calls = 0;
    for step in flatten(&view.steps) {
        let old = original.iter().find(|s| s.id == step.id).unwrap();
        if step.call_id.is_some() && step.label == "std::fs::OpenOptions::new" {
            constructors += 1;
            assert!(step.target.is_none());
        } else {
            assert_eq!(step.target, old.target, "{}", step.label);
        }
        if step.call_id.is_some() && step.label == "open" {
            open_calls += 1;
            assert!(
                !step
                    .target
                    .as_deref()
                    .unwrap_or("")
                    .starts_with("dep-type:")
            );
        }
        assert_eq!(step.range, old.range);
        assert_eq!(step.call_id, old.call_id);
        assert_eq!(step.resolution, old.resolution);
        if let Some(id) = &step.call_id {
            let call = graph.calls.iter().find(|c| &c.id == id).unwrap();
            assert_eq!(step.range, call.range);
            assert!(step.resolution.is_none());
        }
    }
    assert_eq!(constructors, 1);
    assert_eq!(open_calls, 1);
}
