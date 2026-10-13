use std::{
    fs,
    path::Path,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    indexer::{IndexOptions, index_workspace},
    model::*,
};
fn cancel() -> CancelFlag {
    Arc::new(AtomicBool::new(false))
}
fn run(options: &IndexOptions) -> Graph {
    index_workspace(options, &cancel(), |_| {}).unwrap()
}
fn write(root: &Path, path: &str, text: &str) {
    fs::write(root.join(path), text).unwrap();
}
fn fixture(name: &str) -> IndexOptions {
    IndexOptions::new(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/extraction")
            .join(name),
    )
}
#[test]
fn pure_fixture_and_determinism() {
    let options = fixture("fixture");
    let graph = run(&options);
    assert_eq!(graph, run(&options));
    assert_eq!(graph.stats.semantic_state, SemanticState::Unavailable);
    assert!(graph.nodes.iter().all(|n| n.id.starts_with("sid:v1:")));
    assert!(graph.calls.iter().all(|c| c.id.starts_with("occ:v2:")));
    assert!(graph.regions.iter().all(|r| r.id.starts_with("occ:v2:")));
    assert!(
        graph
            .calls
            .iter()
            .all(|c| c.provenance.semantic == SemanticState::Unavailable)
    );
    assert!(
        graph
            .nodes
            .iter()
            .all(|n| n.provenance.semantic == SemanticState::Unavailable)
    );
    assert!(
        serde_json::to_string(&graph)
            .unwrap()
            .find("candidateSymbols")
            .is_none()
    );
    assert!(
        serde_json::to_string(&graph)
            .unwrap()
            .find("callbackArguments")
            .is_none()
    );
    assert!(
        serde_json::to_string(&graph)
            .unwrap()
            .find("\"target\"")
            .is_none()
    );
    assert!(
        serde_json::to_string(&graph)
            .unwrap()
            .find("\"resolution\"")
            .is_none()
    );
}
#[test]
fn discovery_ignores_secrets_builds_and_symlinks() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    write(p, ".gitignore", "ignored.js\nignored/\n");
    for name in [
        "keep.js",
        "keep.mjs",
        "keep.cjs",
        "ignored.js",
        ".secret.js",
        ".env",
    ] {
        write(p, name, "f()");
    }
    for name in [
        "node_modules",
        "dist",
        "build",
        "target",
        ".trellis",
        "ignored",
    ] {
        fs::create_dir(p.join(name)).unwrap();
        write(p, &format!("{name}/hidden.js"), "f()");
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(p.join("keep.js"), p.join("linked.txt")).unwrap();
        std::os::unix::fs::symlink(p, p.join("linked-dir")).unwrap();
    }
    let g = run(&IndexOptions::new(p.to_owned()));
    assert_eq!(
        g.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
        ["keep.cjs", "keep.js", "keep.mjs"]
    );
}
#[test]
fn measured_callees_are_terminal_not_targets() {
    let d = tempfile::tempdir().unwrap();
    let examples = [
        ("T.java", "class T { void go() { obj.foo(); } }", "foo"),
        ("t.js", "function go() { obj.foo(); }", "foo"),
        ("t.py", "def go():\n    obj.foo()\n", "foo"),
        ("t.rs", "fn go() { obj.foo(); }", "foo"),
    ];
    for (path, source, _) in examples {
        write(d.path(), path, source);
    }
    let graph = run(&IndexOptions::new(d.path().to_owned()));
    for (path, source, token) in examples {
        let call = graph.calls.iter().find(|c| c.path == path).unwrap();
        assert_eq!(call.callee_text.as_deref(), Some(token), "{path}");
        let r = call.callee_range.as_ref().expect("measured exact token");
        assert_eq!(&source[r.start_byte..r.end_byte], token, "{path}");
        assert!(graph.nodes.iter().any(|n| n.id == call.caller));
    }
}

#[test]
fn declaration_ids_survive_body_only_change_but_calls_are_revision_local() {
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("main.rs");
    write(d.path(), "main.rs", "fn same() { foo(); }\n");
    let first = run(&IndexOptions::new(d.path().to_owned()));
    write(d.path(), "main.rs", "fn same() { bar(); }\n");
    let second = run(&IndexOptions::new(d.path().to_owned()));
    let id = |graph: &Graph| {
        graph
            .nodes
            .iter()
            .find(|n| n.name == "same")
            .unwrap()
            .id
            .clone()
    };
    assert_eq!(id(&first), id(&second));
    assert_ne!(first.calls[0].id, second.calls[0].id);
    assert_ne!(first.files[0].hash, second.files[0].hash);
    assert_eq!(file.file_name().unwrap(), "main.rs");
}

#[test]
fn graph_ids_and_ranges_are_exact_native_artifact_projection() {
    use trellis::{indexer::index_workspace_with_native, store::topology::WorkspaceIdentity};
    let d = tempfile::tempdir().unwrap();
    for (path, source) in [
        ("T.java", "class T { void go() { obj.foo(); } }"),
        ("t.js", "function go() { obj.foo(); }"),
        ("t.py", "def go():\n    obj.foo()\n"),
        ("t.rs", "fn go() { obj.foo(); }"),
    ] {
        write(d.path(), path, source);
    }
    let identity = WorkspaceIdentity::discover(Some(d.path()), d.path()).unwrap();
    let (graph, native) = index_workspace_with_native(
        &IndexOptions::new(d.path().to_owned()),
        &identity.record_id,
        &cancel(),
        |_| {},
    )
    .unwrap();
    assert_eq!(graph.calls.len(), native.calls.len());
    assert_eq!(graph.regions.len(), native.control_regions.len());
    assert_eq!(graph.nodes.len(), native.declarations.len());
    for call in &graph.calls {
        let native_call = native.calls.iter().find(|c| c.id == call.id).unwrap();
        assert_eq!(call.caller, native_call.owner_syntax_id);
        assert_eq!(call.regions, native_call.region_ids);
        assert_eq!(call.range.start_byte, native_call.range.start);
        assert_eq!(call.range.end_byte, native_call.range.end);
        assert_eq!(call.callee_text.as_deref(), native_call.spelling.as_deref());
        assert_eq!(
            call.callee_range
                .as_ref()
                .map(|r| (r.start_byte, r.end_byte)),
            native_call.callee_range.as_ref().map(|r| (r.start, r.end))
        );
    }
    let graph_text = serde_json::to_string(&graph).unwrap();
    for forbidden in [
        "\"target\"",
        "candidateSymbols",
        "callbackArguments",
        "\"resolution\"",
    ] {
        assert!(
            !graph_text.contains(forbidden),
            "unsafe graph field: {forbidden}"
        );
    }
}

#[test]
fn scip_label_requires_exact_captured_hash_and_name_token_and_never_supplies_identity() {
    use protobuf::Message;
    use sha2::{Digest, Sha256};
    let d = tempfile::tempdir().unwrap();
    let source = "function f() {}\nf();\n";
    write(d.path(), "main.js", source);
    let mut index = scip::types::Index::new();
    let mut document = scip::types::Document::new();
    document.relative_path = "main.js".into();
    let mut occurrence = scip::types::Occurrence::new();
    occurrence.range = vec![0, 9, 10];
    occurrence.symbol_roles = 1; // exact declaration-name token, not a call target
    occurrence.symbol = "scip npm display 1 main.js/f().".into();
    document.occurrences.push(occurrence);
    index.documents.push(document);
    fs::write(d.path().join("index.scip"), index.write_to_bytes().unwrap()).unwrap();
    let source_hash = hex::encode(Sha256::digest(source.as_bytes()));
    fs::write(
        d.path().join("manifest.json"),
        serde_json::to_vec(&serde_json::json!({"main.js":source_hash})).unwrap(),
    )
    .unwrap();
    let base = run(&IndexOptions::new(d.path().to_owned()));
    let mut options = IndexOptions::new(d.path().to_owned());
    options.scip_path = Some(d.path().join("index.scip"));
    options.manifest_path = Some(d.path().join("manifest.json"));
    let with_label = run(&options);
    let declaration = with_label.nodes.iter().find(|n| n.name == "f").unwrap();
    assert_eq!(
        declaration.display_label.as_deref(),
        Some("scip npm display 1 main.js/f().")
    );
    assert_eq!(
        base.nodes.iter().map(|n| &n.id).collect::<Vec<_>>(),
        with_label.nodes.iter().map(|n| &n.id).collect::<Vec<_>>()
    );
    assert_eq!(base.calls, with_label.calls);
    assert_eq!(declaration.provenance.semantic, SemanticState::Unavailable);
    write(d.path(), "manifest.json", "{\"main.js\":\"stale\"}");
    assert!(
        run(&options)
            .nodes
            .iter()
            .all(|n| n.display_label.is_none())
    );
    fs::write(
        d.path().join("manifest.json"),
        serde_json::to_vec(&serde_json::json!({"main.js":source_hash})).unwrap(),
    )
    .unwrap();
    index.documents[0].occurrences[0].range = vec![1, 0, 1]; // call site, not definition
    fs::write(d.path().join("index.scip"), index.write_to_bytes().unwrap()).unwrap();
    assert!(
        run(&options)
            .nodes
            .iter()
            .all(|n| n.display_label.is_none())
    );
    index.documents[0].occurrences[0].range = vec![0, 9, 10];
    index.documents[0].occurrences[0].symbol = "scip npm display 1 other().".into();
    fs::write(d.path().join("index.scip"), index.write_to_bytes().unwrap()).unwrap();
    assert!(
        run(&options)
            .nodes
            .iter()
            .all(|n| n.display_label.is_none())
    );
}

#[test]
fn root_dependency_drift_changes_native_revision_not_stable_or_occurrence_ids() {
    use trellis::{indexer::index_workspace_with_native, store::topology::WorkspaceIdentity};
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "main.js", "function same() { foo(); }");
    let root_id = WorkspaceIdentity::discover(Some(d.path()), d.path())
        .unwrap()
        .record_id;
    let options = IndexOptions::new(d.path().to_owned());
    let (before, first) =
        index_workspace_with_native(&options, &root_id, &cancel(), |_| {}).unwrap();
    write(
        d.path(),
        "Cargo.lock",
        "changed immutable dependency selector",
    );
    let (after, second) =
        index_workspace_with_native(&options, &root_id, &cancel(), |_| {}).unwrap();
    assert_ne!(first.revision.id, second.revision.id);
    assert_eq!(before.nodes, after.nodes);
    // Decision 0003: the native producer declares no dependency input, so an unchanged
    // document keeps its occ:v2 IDs while its records move to the new containing revision.
    assert_eq!(before.calls[0].id, after.calls[0].id);
    assert_eq!(first.calls[0].id, second.calls[0].id);
    assert_eq!(first.calls[0].revision_id, first.revision.id);
    assert_eq!(second.calls[0].revision_id, second.revision.id);
    assert_eq!(before.files, after.files);
    assert!(
        after
            .calls
            .iter()
            .all(|c| c.provenance.semantic == SemanticState::Unavailable)
    );
}

#[test]
fn python_assignment_rhs_calls_remain_owned_by_enclosing_function() {
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        "main.py",
        "def run():\n    agent = make_agent()\n    x: annotation_call() = rhs()\n",
    );
    let graph = run(&IndexOptions::new(d.path().to_owned()));
    let function = graph.nodes.iter().find(|n| n.name == "run").unwrap();
    let variable = graph.nodes.iter().find(|n| n.name == "agent").unwrap();
    assert_eq!(variable.parent.as_deref(), Some(function.id.as_str()));
    for name in ["make_agent", "rhs"] {
        let call = graph
            .calls
            .iter()
            .find(|c| c.callee_text.as_deref() == Some(name))
            .unwrap();
        assert_eq!(
            call.caller, function.id,
            "{name} must execute in defining scope"
        );
    }
}

#[test]
fn rust_impl_and_trait_methods_keep_measured_method_kind_and_free_functions() {
    use trellis::{indexer::index_workspace_with_native, store::topology::WorkspaceIdentity};
    let d = tempfile::tempdir().unwrap();
    let source = "trait Work { fn required(&self); fn defaulted(&self) { fallback(); } }\nstruct T; impl Work for T { fn required(&self) { helper(); } }\nfn free() { helper(); }\n";
    write(d.path(), "main.rs", source);
    let identity = WorkspaceIdentity::discover(Some(d.path()), d.path()).unwrap();
    let (graph, native) = index_workspace_with_native(
        &IndexOptions::new(d.path().to_owned()),
        &identity.record_id,
        &cancel(),
        |_| {},
    )
    .unwrap();
    let methods: Vec<_> = graph
        .nodes
        .iter()
        .filter(|n| ["required", "defaulted"].contains(&n.name.as_str()))
        .collect();
    assert_eq!(methods.len(), 3);
    assert!(methods.iter().all(|node| node.kind == SymbolKind::Method));
    for node in methods {
        assert_eq!(
            native
                .declarations
                .iter()
                .find(|d| d.syntax_id == node.id)
                .unwrap()
                .kind,
            "method"
        );
        assert!(
            node.parent
                .as_ref()
                .is_some_and(|id| graph.nodes.iter().any(|n| n.id == *id))
        );
    }
    let free = graph.nodes.iter().find(|n| n.name == "free").unwrap();
    assert_eq!(free.kind, SymbolKind::Function);
    assert_eq!(
        native
            .declarations
            .iter()
            .find(|d| d.syntax_id == free.id)
            .unwrap()
            .kind,
        "function"
    );
    for name in ["fallback", "helper"] {
        assert!(
            graph
                .calls
                .iter()
                .any(|c| c.callee_text.as_deref() == Some(name))
        );
    }
}

#[test]
fn initializer_calls_keep_executable_owner_and_control_guard_in_four_languages() {
    use trellis::{indexer::index_workspace_with_native, store::topology::WorkspaceIdentity};
    let d = tempfile::tempdir().unwrap();
    for (path, source) in [
        (
            "T.java",
            "class T { void run() { if (true) { int x = rhs(); } } }",
        ),
        ("t.js", "function run() { if (true) { let x = rhs(); } }"),
        ("t.py", "def run():\n    if True:\n        x = rhs()\n"),
        ("t.rs", "fn run() { if true { let x = rhs(); } }"),
    ] {
        write(d.path(), path, source);
    }
    let identity = WorkspaceIdentity::discover(Some(d.path()), d.path()).unwrap();
    let (graph, native) = index_workspace_with_native(
        &IndexOptions::new(d.path().to_owned()),
        &identity.record_id,
        &cancel(),
        |_| {},
    )
    .unwrap();
    for path in ["T.java", "t.js", "t.py", "t.rs"] {
        let function = graph
            .nodes
            .iter()
            .find(|n| n.path == path && n.name == "run")
            .unwrap();
        let call = graph
            .calls
            .iter()
            .find(|c| c.path == path && c.callee_text.as_deref() == Some("rhs"))
            .unwrap();
        assert_eq!(
            call.caller, function.id,
            "initializer call owner for {path}"
        );
        let native_call = native.calls.iter().find(|c| c.id == call.id).unwrap();
        assert_eq!(
            native_call.owner_syntax_id, function.id,
            "native owner for {path}"
        );
        assert!(
            !native_call.region_ids.is_empty(),
            "guard region for {path}"
        );
        assert_eq!(call.regions, native_call.region_ids);
        for id in &call.regions {
            assert!(
                graph
                    .regions
                    .iter()
                    .any(|region| region.id == *id && region.owner == function.id)
            );
        }
    }
}

#[test]
fn native_null_callee_spelling_remains_explicit_json_null() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "T.java", "class T { void f() { new T(); } }");
    let graph = run(&IndexOptions::new(d.path().to_owned()));
    let call = graph.calls.iter().find(|c| c.path == "T.java").unwrap();
    assert_eq!(call.callee_text, None);
    assert_eq!(call.callee_range, None);
    let serialized = serde_json::to_value(call).unwrap();
    assert!(serialized.get("calleeText").unwrap().is_null());
    assert!(serialized.get("calleeRange").unwrap().is_null());
    let mut missing = serialized.as_object().unwrap().clone();
    missing.remove("calleeText");
    assert!(serde_json::from_value::<CallSite>(serde_json::Value::Object(missing)).is_err());
}

#[test]
fn old_lexical_callsite_fields_are_rejected_on_deserialization() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "main.js", "function f() { foo(); }");
    let call = run(&IndexOptions::new(d.path().to_owned())).calls.remove(0);
    let mut value = serde_json::to_value(&call).unwrap();
    for unsafe_field in [
        "target",
        "candidateSymbols",
        "resolution",
        "callbackArguments",
    ] {
        value
            .as_object_mut()
            .unwrap()
            .insert(unsafe_field.into(), serde_json::Value::Null);
        assert!(
            serde_json::from_value::<CallSite>(value.clone()).is_err(),
            "accepted {unsafe_field}"
        );
        value.as_object_mut().unwrap().remove(unsafe_field);
    }
}

#[test]
fn untrusted_display_metadata_never_promotes_graph_edges_or_ids() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "main.js", "function same() {} same();");
    let base = run(&IndexOptions::new(d.path().to_owned()));
    write(d.path(), "metadata.scip", "not a valid scip artifact");
    write(d.path(), "metadata.hashes", "{\"main.js\":\"forged\"}");
    let mut options = IndexOptions::new(d.path().to_owned());
    options.scip_path = Some(d.path().join("metadata.scip"));
    options.manifest_path = Some(d.path().join("metadata.hashes"));
    let with_metadata = run(&options);
    assert_eq!(base.nodes, with_metadata.nodes);
    assert_eq!(base.calls, with_metadata.calls);
    assert!(
        with_metadata
            .calls
            .iter()
            .all(|c| c.provenance.semantic == SemanticState::Unavailable)
    );
}

#[test]
fn cancellation_and_size_limits() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "large.js", "function f() { g(); }");
    let mut o = IndexOptions::new(d.path().to_owned());
    let c = Arc::new(AtomicBool::new(true));
    assert!(
        index_workspace(&o, &c, |_| {})
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    let c = cancel();
    assert!(
        index_workspace(&o, &c, |p| if p.phase == "scan" {
            c.store(true, std::sync::atomic::Ordering::Relaxed)
        })
        .is_err()
    );
    o.max_file_bytes = 2;
    assert!(
        index_workspace(&o, &cancel(), |_| {})
            .unwrap_err()
            .to_string()
            .contains("oversized input")
    );
}
#[test]
fn nested_calls_have_unique_ids_and_parse_errors_are_visible() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "nested.js", "factory()(); new (factory())();\n");
    write(d.path(), "broken.js", "function broken( {\n");
    let g = run(&IndexOptions::new(d.path().to_owned()));
    let ids: std::collections::BTreeSet<_> = g.calls.iter().map(|c| &c.id).collect();
    assert_eq!(ids.len(), g.calls.len());
    assert_eq!(g.calls.len(), 4);
    assert_eq!(g.stats.parse_error_files, 1);
    assert!(g.diagnostics.iter().any(|d| d.code == "parse-error"));
}
#[cfg(unix)]
#[test]
fn direct_symlink_artifacts_and_config_are_never_read() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    write(p, "main.js", "f();");
    write(p, "index.scip", "captured optional metadata");
    let mut o = IndexOptions::new(p.to_owned());
    std::os::unix::fs::symlink(p.join("main.js"), p.join("package.json")).unwrap();
    assert!(index_workspace(&o, &cancel(), |_| {}).is_err());
    fs::remove_file(p.join("package.json")).unwrap();
    std::os::unix::fs::symlink(p.join("index.scip"), p.join("linked.scip")).unwrap();
    o.scip_path = Some(p.join("linked.scip"));
    assert!(index_workspace(&o, &cancel(), |_| {}).is_err());
}

#[test]
fn java_python_discovery_respects_ignored_environments_and_source_symlinks() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    write(p, ".gitignore", "venv/\ngenerated/\n");
    write(
        p,
        "Example.java",
        "class Example { void run() { work(); } }",
    );
    write(p, "example.py", "def run():\n    work()\n");
    for dir in [
        ".venv",
        "venv",
        "generated",
        "build",
        "target",
        "node_modules",
    ] {
        fs::create_dir(p.join(dir)).unwrap();
        write(p, &format!("{dir}/Ignored.java"), "class Ignored {}");
        write(p, &format!("{dir}/ignored.py"), "def ignored(): pass");
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(p.join("example.py"), p.join("link.py")).unwrap();
        std::os::unix::fs::symlink(p.join("Example.java"), p.join("Link.java")).unwrap();
    }
    assert!(
        index_workspace(&IndexOptions::new(p.to_owned()), &cancel(), |_| {})
            .unwrap_err()
            .to_string()
            .contains("unsafe source")
    );
    fs::remove_file(p.join("link.py")).unwrap();
    fs::remove_file(p.join("Link.java")).unwrap();
    let g = run(&IndexOptions::new(p.to_owned()));
    assert_eq!(
        g.files
            .iter()
            .map(|f| (f.path.as_str(), f.language.as_str()))
            .collect::<Vec<_>>(),
        [("Example.java", "java"), ("example.py", "python")]
    );
}

#[test]
fn conventional_java_source_packages_are_not_confused_with_build_outputs() {
    let d = tempfile::tempdir().unwrap();
    for path in [
        "buildSrc/src/main/java/com/example/build/BuildPlugin.java",
        "src/test/java/com/example/target/TargetTest.java",
        "module/src/testFixtures/java/com/example/dist/Fixture.java",
        "build/generated/src/main/java/com/example/Generated.java",
        "module/build/generated/Generated.java",
    ] {
        let file = d.path().join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, "class Example { void run() { work(); } }").unwrap();
    }
    let g = run(&IndexOptions::new(d.path().to_owned()));
    assert_eq!(
        g.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
        [
            "buildSrc/src/main/java/com/example/build/BuildPlugin.java",
            "module/src/testFixtures/java/com/example/dist/Fixture.java",
            "src/test/java/com/example/target/TargetTest.java",
        ]
    );
}

#[test]
fn capture_rejects_cutoff_source_and_directory_inventory_drift() {
    for scenario in [
        "write",
        "add",
        "delete",
        "rename",
        "add-dir",
        "delete-dir",
        "config",
        "ignore",
        "same-size-mtime",
    ] {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        write(root, "main.js", "f();");
        fs::create_dir(root.join("visited")).unwrap();
        write(root, "visited/other.py", "pass\n");
        let err = index_workspace(&IndexOptions::new(root.to_owned()), &cancel(), |p| {
            if p.phase != "parse" || p.completed != 1 {
                return;
            }
            match scenario {
                "write" => write(root, "main.js", "g();"),
                "add" => write(root, "new.py", "pass"),
                "delete" => fs::remove_file(root.join("main.js")).unwrap(),
                "rename" => fs::rename(root.join("main.js"), root.join("renamed.js")).unwrap(),
                "add-dir" => fs::create_dir(root.join("another")).unwrap(),
                "delete-dir" => {
                    fs::remove_file(root.join("visited/other.py")).unwrap();
                    fs::remove_dir(root.join("visited")).unwrap();
                }
                "config" => write(root, "package.json", "{}"),
                "ignore" => write(root, "visited/.ignore", "*.js\n"),
                "same-size-mtime" => {
                    use std::os::unix::fs::MetadataExt;
                    let path = root.join("main.js");
                    let before = fs::metadata(&path).unwrap();
                    write(root, "main.js", "g();");
                    let times = [
                        libc::timespec {
                            tv_sec: before.atime(),
                            tv_nsec: before.atime_nsec(),
                        },
                        libc::timespec {
                            tv_sec: before.mtime(),
                            tv_nsec: before.mtime_nsec(),
                        },
                    ];
                    use std::os::unix::ffi::OsStrExt;
                    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
                    assert_eq!(
                        unsafe {
                            libc::utimensat(libc::AT_FDCWD, name.as_ptr(), times.as_ptr(), 0)
                        },
                        0
                    );
                    assert_eq!(fs::metadata(path).unwrap().mtime(), before.mtime());
                }
                _ => unreachable!(),
            }
        })
        .unwrap_err();
        assert!(err.to_string().contains("drift"), "{scenario}: {err:#}");
    }
}

#[test]
fn capture_rejects_absent_present_and_cancellation_but_accepts_empty_root() {
    let d = tempfile::tempdir().unwrap();
    let empty = run(&IndexOptions::new(d.path().to_owned()));
    assert!(empty.files.is_empty());
    for initially_present in [false, true] {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        write(root, "main.js", "f();");
        if initially_present {
            write(root, "Cargo.toml", "[package]\n");
        }
        let error = index_workspace(&IndexOptions::new(root.to_owned()), &cancel(), |p| {
            if p.phase == "parse" {
                if initially_present {
                    fs::remove_file(root.join("Cargo.toml")).unwrap();
                } else {
                    write(root, "Cargo.toml", "[package]\n");
                }
            }
        })
        .unwrap_err();
        assert!(error.to_string().contains("drift"), "{error:#}");
    }
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "main.js", "f();");
    let flag = cancel();
    let error = index_workspace(&IndexOptions::new(d.path().to_owned()), &flag, |p| {
        if p.phase == "parse" {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    })
    .unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error:#}");
}

#[test]
fn identity_aliases_and_nonregular_sources_fail_admission() {
    for (alias, field) in [
        ("lexical", "scip"),
        ("hardlink", "scip"),
        ("lexical", "manifest"),
        ("hardlink", "manifest"),
    ] {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "main.js", "f();");
        let path = if alias == "lexical" {
            d.path().join("./main.js")
        } else {
            let alias = d.path().join("display.scip");
            fs::hard_link(d.path().join("main.js"), &alias).unwrap();
            alias
        };
        let mut options = IndexOptions::new(d.path().to_owned());
        if field == "scip" {
            options.scip_path = Some(path);
        } else {
            options.manifest_path = Some(path);
        }
        let error = index_workspace(&options, &cancel(), |_| {}).unwrap_err();
        assert!(
            error.to_string().contains("aliases source"),
            "{alias}/{field}: {error:#}"
        );
    }
    let d = tempfile::tempdir().unwrap();
    let fifo = d.path().join("blocked.js");
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let error =
        index_workspace(&IndexOptions::new(d.path().to_owned()), &cancel(), |_| {}).unwrap_err();
    assert!(error.to_string().contains("unsafe source"), "{error:#}");
}

#[test]
fn stable_capture_counts_one_open_read_hash_per_source_and_verifies_unchanged() {
    use trellis::capture::{Capture, SourceOperations};
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "one.js", "f();");
    write(d.path(), "two.py", "def f(): pass\n");
    let cancel = cancel();
    let capture =
        Capture::admit(&IndexOptions::new(d.path().to_owned()), &cancel, &|_| {}).unwrap();
    assert_eq!(capture.source_operations.len(), 2);
    for operations in capture.source_operations.values() {
        assert_eq!(
            *operations,
            SourceOperations {
                opens: 1,
                complete_reads: 1,
                hashes: 1
            }
        );
    }
    capture.verify(&cancel).unwrap();
    assert_eq!(run(&IndexOptions::new(d.path().to_owned())).files.len(), 2);
}

#[test]
fn declared_inputs_and_root_refuse_cutoff_drift() {
    for scenario in [
        "root",
        "root-replaced",
        "toolchain",
        "config",
        "ignore",
        "display",
        "manifest",
    ] {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        write(root, "main.js", "f();");
        write(root, "rust-toolchain", "stable\n");
        write(root, "package.json", "{}\n");
        write(root, ".ignore", "*.tmp\n");
        write(root, "display.scip", "invalid-scip");
        write(root, "display.hashes", "{}\n");
        let mut options = IndexOptions::new(root.to_owned());
        options.scip_path = Some(root.join("display.scip"));
        options.manifest_path = Some(root.join("display.hashes"));
        let moved = root.with_extension("moved");
        let error = index_workspace(&options, &cancel(), |progress| {
            if progress.phase != "parse" {
                return;
            }
            match scenario {
                "root" => fs::rename(root, &moved).unwrap(),
                "root-replaced" => {
                    fs::rename(root, &moved).unwrap();
                    fs::create_dir(root).unwrap();
                }
                "toolchain" => write(root, "rust-toolchain", "nightly\n"),
                "config" => write(root, "package.json", "[]\n"),
                "ignore" => write(root, ".ignore", "*.log\n"),
                "display" => write(root, "display.scip", "changed-scip"),
                "manifest" => write(root, "display.hashes", "[]\n"),
                _ => unreachable!(),
            }
        })
        .unwrap_err();
        if scenario == "root" {
            fs::rename(&moved, root).unwrap();
        }
        if scenario == "root-replaced" {
            fs::remove_dir(root).unwrap();
            fs::rename(&moved, root).unwrap();
        }
        assert!(error.to_string().contains("drift"), "{scenario}: {error:#}");
    }
}

#[test]
fn hard_linked_nonsource_inputs_share_immutable_bytes() {
    use trellis::capture::Capture;
    let d = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(d.path()).unwrap();
    let root = root.as_path();
    write(root, "main.js", "f();");
    write(root, "package.json", "{}\n");
    fs::hard_link(root.join("package.json"), root.join("display.scip")).unwrap();
    let mut options = IndexOptions::new(root.to_owned());
    options.scip_path = Some(root.join("display.scip"));
    let flag = cancel();
    let capture = Capture::admit(&options, &flag, &|_| {}).unwrap();
    assert_eq!(
        capture.bytes(&root.join("package.json")),
        Some(b"{}\n".as_slice())
    );
    assert_eq!(
        capture.bytes(&root.join("display.scip")),
        capture.bytes(&root.join("package.json"))
    );
    capture.verify(&flag).unwrap();
}

#[test]
fn four_language_noncallable_native_declarations_keep_exact_kind_and_are_not_executable() {
    use trellis::{indexer::index_workspace_with_native, store::topology::WorkspaceIdentity};
    let d = tempfile::tempdir().unwrap();
    let samples = [
        (
            "main.js",
            "class Js { field = 1; method(arg) { const local = arg; return local; } }\nfunction jsFun(arg) { return arg; }\n",
        ),
        (
            "Main.java",
            "class JavaThing { int field; void method(int arg) { int local = arg; } }\n",
        ),
        (
            "main.py",
            "class PythonThing:\n    field = 1\n    def method(self, arg):\n        local = arg\n        return local\n",
        ),
        (
            "main.rs",
            "type Alias = u32;\nstruct RustThing { field: Alias }\nfn rust_fun(arg: Alias) { let local = arg; }\n",
        ),
    ];
    for (path, text) in samples {
        write(d.path(), path, text);
    }
    let root_id = WorkspaceIdentity::discover(Some(d.path()), d.path())
        .unwrap()
        .record_id;
    let (graph, native) = index_workspace_with_native(
        &IndexOptions::new(d.path().to_owned()),
        &root_id,
        &cancel(),
        |_| {},
    )
    .unwrap();
    assert_eq!(
        graph.nodes.len(),
        native.declarations.len(),
        "native declarations must remain visible"
    );
    for declaration in &native.declarations {
        let node = graph
            .nodes
            .iter()
            .find(|n| n.id == declaration.syntax_id)
            .unwrap();
        let source = graph.files.iter().find(|f| f.path == node.path).unwrap();
        assert_eq!(
            (node.range.start_byte, node.range.end_byte),
            (declaration.range.start, declaration.range.end)
        );
        assert_eq!(
            &source.text[node.range.start_byte..node.range.end_byte],
            &source.text[declaration.range.start..declaration.range.end]
        );
        let expected = match declaration.kind.as_str() {
            "module" | "namespace" => SymbolKind::Module,
            "type" | "implementation" => SymbolKind::Class,
            "function" | "anonymousFunction" => SymbolKind::Function,
            "method" | "constructor" => SymbolKind::Method,
            "field" => SymbolKind::Field,
            "variable" => SymbolKind::Variable,
            "parameter" => SymbolKind::Parameter,
            "typeParameter" => SymbolKind::TypeParameter,
            "alias" => SymbolKind::Alias,
            other => panic!("unexpected native declaration kind: {other}"),
        };
        assert_eq!(node.kind, expected, "{} {}", node.path, node.name);
        assert_eq!(node.provenance.semantic, SemanticState::Unavailable);
    }
    // These are witnessed #22 declarations, not names inferred from the graph.
    for (path, expected) in [
        (
            "main.js",
            [
                ("field", SymbolKind::Field),
                ("local", SymbolKind::Variable),
                ("method", SymbolKind::Method),
            ],
        ),
        (
            "Main.java",
            [
                ("field", SymbolKind::Field),
                ("arg", SymbolKind::Parameter),
                ("local", SymbolKind::Variable),
            ],
        ),
        (
            "main.py",
            [
                ("field", SymbolKind::Variable),
                ("local", SymbolKind::Variable),
                ("method", SymbolKind::Function),
            ],
        ),
        (
            "main.rs",
            [
                ("Alias", SymbolKind::Alias),
                ("field", SymbolKind::Field),
                ("arg", SymbolKind::Parameter),
            ],
        ),
    ] {
        for (name, kind) in expected {
            let declaration = native
                .declarations
                .iter()
                .find(|d| d.document.path == path && d.name.as_deref() == Some(name))
                .unwrap();
            let node = graph
                .nodes
                .iter()
                .find(|n| n.id == declaration.syntax_id)
                .unwrap();
            assert_eq!(node.kind, kind, "{path} {name}");
        }
        assert!(
            graph
                .nodes
                .iter()
                .any(|n| n.path == path
                    && matches!(n.kind, SymbolKind::Function | SymbolKind::Method)),
            "{path}: real callable"
        );
    }
    let state = tempfile::tempdir().unwrap();
    let store = trellis::store::Store::open_for_tests(state.path(), d.path()).unwrap();
    let session = store.leader_session().unwrap();
    let pin = publish_bundle(
        &store,
        &graph,
        d.path(),
        session.leader_guard().unwrap(),
        store.index_baseline().unwrap(),
        &cancel(),
    )
    .unwrap();
    let saved = store.graph().unwrap();
    assert_eq!(saved.nodes, graph.nodes);
    assert_eq!(saved.calls, graph.calls);
    for (path, text) in samples {
        let methods = store.methods_at(path, Some(pin)).unwrap().unwrap();
        let items = methods["items"].as_array().unwrap();
        let expected: Vec<_> = graph
            .nodes
            .iter()
            .filter(|n| {
                n.path == path && matches!(n.kind, SymbolKind::Function | SymbolKind::Method)
            })
            .collect();
        assert_eq!(items.len(), expected.len(), "{path}");
        let files = store.files_at(Some(pin), 0, 100).unwrap();
        let file = files["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["path"] == path)
            .unwrap();
        assert_eq!(file["methodCount"], expected.len());
        for n in graph.nodes.iter().filter(|n| {
            n.path == path
                && matches!(
                    n.kind,
                    SymbolKind::Field
                        | SymbolKind::Variable
                        | SymbolKind::Parameter
                        | SymbolKind::Alias
                        | SymbolKind::TypeParameter
                )
        }) {
            assert_eq!(store.symbol(&n.id).unwrap().unwrap().kind, n.kind);
            assert!(
                store
                    .symbols(&n.name, 150)
                    .unwrap()
                    .iter()
                    .any(|candidate| candidate.id == n.id && candidate.kind == n.kind)
            );
            assert!(
                !items.iter().any(|item| item["symbol"]["id"] == n.id),
                "{path}: {} is not a method",
                n.name
            );
            assert!(
                store.sequence_at(&n.id, pin, false).is_err(),
                "{path}: {} is not executable",
                n.name
            );
            let request = serde_json::from_value(serde_json::json!({
                "seed": n.id, "question": "Which exact source declares this name?", "expectedRevision": pin
            })).unwrap();
            let packet = trellis::planning::prepare(&store, request).unwrap();
            assert_eq!(
                packet.context.nodes,
                vec![n.clone()],
                "{path}: source-only declaration"
            );
            assert!(
                packet.context.calls.is_empty() && packet.context.regions.is_empty(),
                "{path}: no executable call evidence"
            );
            assert_eq!(packet.source_files.len(), 1);
            assert_eq!(packet.source_files[0].text, text);
            assert!(
                packet
                    .warnings
                    .iter()
                    .any(|w| w.contains("source-only") && w.contains("noncallable"))
            );
        }
        assert_eq!(store.source(path).unwrap().unwrap().text, text);
    }
}

fn publish_bundle(
    store: &trellis::store::Store,
    graph: &trellis::model::Graph,
    workspace: &std::path::Path,
    leader: &trellis::store::topology::LeaderGuard,
    expected: trellis::model::IndexPin,
    cancel: &trellis::model::CancelFlag,
) -> anyhow::Result<trellis::model::IndexPin> {
    let (indexed, native, capture) = trellis::indexer::index_workspace_bundle(
        &trellis::indexer::IndexOptions::new(workspace.to_owned()),
        store.root_id(),
        cancel,
        |_| {},
    )?;
    assert_eq!(
        &indexed, graph,
        "published graph must match captured source"
    );
    store.publish_native(&indexed, &capture, &native, leader, expected, cancel)
}

#[test]
fn admitted_body_edit_measures_only_changed_document_not_unrelated_lookup_owners() {
    use trellis::{
        capture::Capture,
        index_coordinator::IndexJobCoordinator,
        indexer::{CapturedChange, index_workspace_bundle, measure_captured_change},
        store::topology::WorkspaceIdentity,
    };
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        "a.js",
        "function f(){ if (ready()) { return 10; } return 0; }\n",
    );
    // An unrelated pre-existing unresolved/ambiguous lookup cannot force fallback.
    write(
        d.path(),
        "z.js",
        "function bad(){ missing(); missing(); }\n",
    );
    let options = IndexOptions::new(d.path().to_owned());
    let first = Capture::admit(&options, &cancel(), &|_| {}).unwrap();
    write(
        d.path(),
        "a.js",
        "function f(){ if (ready()) { return 20; } return 0; }\n",
    );
    let second = Capture::admit(&options, &cancel(), &|_| {}).unwrap();
    assert_eq!(
        first.files[0].text.as_bytes(),
        b"function f(){ if (ready()) { return 10; } return 0; }\n"
    );
    assert_eq!(
        second.files[0].text.as_bytes(),
        b"function f(){ if (ready()) { return 20; } return 0; }\n"
    );
    let choice = measure_captured_change(&first, &second);
    let root = d.path().canonicalize().unwrap();
    let root_id = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .record_id;
    let mut measured = vec![];
    let stage = IndexJobCoordinator::staged_capture_measurement(
        &first,
        &second,
        &root,
        &root_id,
        &cancel(),
        |key| measured.push(key.path.clone()),
    )
    .unwrap();
    assert_eq!(stage.decision, choice);
    let selected = stage.selected.unwrap();
    assert_eq!(
        choice,
        CapturedChange::DocumentLocal {
            path: "a.js".into()
        }
    );
    assert_eq!(measured, ["a.js"]); // callback ran only AFTER real native extract succeeded
    let mut rejected = vec![];
    assert!(
        IndexJobCoordinator::staged_capture_measurement(
            &first,
            &second,
            &root,
            "incorrect-root-id",
            &cancel(),
            |key| rejected.push(key.path.clone()),
        )
        .is_err()
    );
    assert!(
        rejected.is_empty(),
        "failed native authentication cannot report assembly"
    );
    assert_eq!(first.files[1], second.files[1]);
    assert!(
        selected
            .declarations
            .iter()
            .any(|row| row.name.as_deref() == Some("f"))
    );
    assert!(!selected.calls.is_empty());
    assert!(!selected.control_regions.is_empty());
    assert!(selected.calls.iter().all(|row| row.document.path == "a.js"));
    assert!(selected.calls.iter().all(|row| {
        selected
            .declarations
            .iter()
            .any(|owner| owner.syntax_id == row.owner_syntax_id)
    }));
    assert!(
        selected
            .declarations
            .iter()
            .all(|row| row.document.path == "a.js")
    );
    let (_graph, full, _separate_capture) =
        index_workspace_bundle(&options, &root_id, &cancel(), |_| {}).unwrap();
    assert_eq!(selected.producer, full.producer);
    assert_eq!(selected.source_set, full.source_set);
    assert_eq!(selected.revision, full.revision);
    assert_eq!(
        selected.document,
        *full
            .revision
            .documents
            .iter()
            .find(|row| row.key.path == "a.js")
            .unwrap()
    );
    assert_eq!(
        selected.coverage,
        *full
            .coverage
            .iter()
            .find(|row| row.document_path == "a.js")
            .unwrap()
    );
    assert_eq!(
        selected.provenance,
        *full
            .provenance
            .iter()
            .find(|row| row.document.path == "a.js")
            .unwrap()
    );
    assert_eq!(
        selected.declarations,
        full.declarations
            .iter()
            .filter(|row| row.document.path == "a.js")
            .cloned()
            .collect::<Vec<_>>()
    );
    assert_eq!(
        selected.calls,
        full.calls
            .iter()
            .filter(|row| row.document.path == "a.js")
            .cloned()
            .collect::<Vec<_>>()
    );
    assert_eq!(
        selected.control_regions,
        full.control_regions
            .iter()
            .filter(|row| row.document.path == "a.js")
            .cloned()
            .collect::<Vec<_>>()
    );
    let selected_bytes = serde_json::to_vec(&(
        &selected.revision,
        &selected.document,
        &selected.coverage,
        &selected.provenance,
        &selected.declarations,
        &selected.calls,
        &selected.control_regions,
    ))
    .unwrap();
    let oracle_bytes = serde_json::to_vec(&(
        &full.revision,
        full.revision
            .documents
            .iter()
            .find(|row| row.key.path == "a.js")
            .unwrap(),
        full.coverage
            .iter()
            .find(|row| row.document_path == "a.js")
            .unwrap(),
        full.provenance
            .iter()
            .find(|row| row.document.path == "a.js")
            .unwrap(),
        full.declarations
            .iter()
            .filter(|row| row.document.path == "a.js")
            .collect::<Vec<_>>(),
        full.calls
            .iter()
            .filter(|row| row.document.path == "a.js")
            .collect::<Vec<_>>(),
        full.control_regions
            .iter()
            .filter(|row| row.document.path == "a.js")
            .collect::<Vec<_>>(),
    ))
    .unwrap();
    assert_eq!(
        selected_bytes, oracle_bytes,
        "selected native bytes differ from the full captured oracle"
    );
    assert_eq!(
        measured,
        ["a.js"],
        "the separate full oracle is not a stage observation"
    );
}

#[test]
fn captured_classifier_falls_back_on_each_unproved_surface_effect() {
    use trellis::{
        capture::Capture,
        indexer::{CapturedChange, measure_captured_change},
    };
    let samples = [
        ("function f(){ return 1; }\n", "function g(){ return 1; }\n"), // declaration
        (
            "function f(){ return 1; }\n",
            "function f(){ const x=1; return x; }\n",
        ), // scope
        (
            "function f(){ return 1; }\n",
            "import {x} from './x.js'; function f(){ return 1; }\n",
        ), // import
        (
            "export function f(){ return 1; }\n",
            "export {f}; function f(){ return 1; }\n",
        ), // re-export
        (
            "function f(){ return old(); }\n",
            "function f(){ return newer(); }\n",
        ), // lookup observation
        (
            "function f(){ return 1; }\n",
            "function f(){ return (1; }\n",
        ), // parse error
    ];
    for (old, new) in samples {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.js", old);
        let options = IndexOptions::new(d.path().to_owned());
        let before = Capture::admit(&options, &cancel(), &|_| {}).unwrap();
        write(d.path(), "a.js", new);
        let after = Capture::admit(&options, &cancel(), &|_| {}).unwrap();
        assert!(
            matches!(
                measure_captured_change(&before, &after),
                CapturedChange::FullNative { .. }
            ),
            "{old:?} -> {new:?}"
        );
    }
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        "a.java",
        "class A extends Base { int f(){ return 1; } }\n",
    );
    let options = IndexOptions::new(d.path().to_owned());
    let before = Capture::admit(&options, &cancel(), &|_| {}).unwrap();
    write(
        d.path(),
        "a.java",
        "class A extends Other { int f(){ return 1; } }\n",
    );
    let after = Capture::admit(&options, &cancel(), &|_| {}).unwrap();
    assert!(matches!(
        measure_captured_change(&before, &after),
        CapturedChange::FullNative { .. }
    ));
    for transition in ["add", "delete", "rename"] {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.js", "function f(){ return 1; }\n");
        if transition == "delete" || transition == "rename" {
            write(d.path(), "b.js", "function b(){}\n");
        }
        let options = IndexOptions::new(d.path().to_owned());
        let before = Capture::admit(&options, &cancel(), &|_| {}).unwrap();
        match transition {
            "add" => write(d.path(), "b.js", "function b(){}\n"),
            "delete" => fs::remove_file(d.path().join("b.js")).unwrap(),
            _ => fs::rename(d.path().join("b.js"), d.path().join("c.js")).unwrap(),
        }
        let after = Capture::admit(&options, &cancel(), &|_| {}).unwrap();
        assert_eq!(
            measure_captured_change(&before, &after),
            CapturedChange::FullNative {
                reason: "source inventory changed (add/delete/rename)"
            },
            "{transition}"
        );
    }
}

#[test]
fn measured_scip_label_and_cutoff_change_projection_but_not_native_fingerprint() {
    use protobuf::Message;
    use sha2::{Digest, Sha256};
    use trellis::{
        indexer::{index_workspace_bundle, measure_document_fingerprint},
        store::topology::WorkspaceIdentity,
    };
    for (language, path, source) in [
        ("javascript", "main.js", "function f(){ return 1; }\n"),
        ("java", "Main.java", "class Main { int f(){ return 1; } }\n"),
        ("python", "main.py", "def f():\n    return 1\n"),
        ("rust", "main.rs", "fn f(){ 1; }\n"),
    ] {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), path, source);
        let mut options = IndexOptions::new(d.path().to_owned());
        let source_hash = hex::encode(Sha256::digest(source.as_bytes()));
        let manifest = serde_json::json!({path:source_hash});
        fs::write(
            d.path().join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let mut index = scip::types::Index::new();
        let mut document = scip::types::Document::new();
        document.relative_path = path.into();
        let mut occurrence = scip::types::Occurrence::new();
        let column = source.find("f(").unwrap();
        assert!(column < 30);
        occurrence.range = vec![0, column as i32, column as i32 + 1];
        occurrence.symbol_roles = 1;
        occurrence.symbol = "scip display f() version one".into();
        document.occurrences.push(occurrence);
        index.documents.push(document);
        options.scip_path = Some(d.path().join("index.scip"));
        options.manifest_path = Some(d.path().join("manifest.json"));
        let root_id = WorkspaceIdentity::discover(Some(d.path()), d.path())
            .unwrap()
            .record_id;
        fs::write(d.path().join("index.scip"), index.write_to_bytes().unwrap()).unwrap();
        let (first_graph, first_native, first_capture) =
            index_workspace_bundle(&options, &root_id, &cancel(), |_| {}).unwrap();
        let first = measure_document_fingerprint(
            &first_capture,
            &first_native,
            &first_graph,
            path,
            &options,
        )
        .unwrap();
        index.documents[0].occurrences[0].symbol = "scip display f() version two".into();
        fs::write(d.path().join("index.scip"), index.write_to_bytes().unwrap()).unwrap();
        let (second_graph, second_native, second_capture) =
            index_workspace_bundle(&options, &root_id, &cancel(), |_| {}).unwrap();
        let second = measure_document_fingerprint(
            &second_capture,
            &second_native,
            &second_graph,
            path,
            &options,
        )
        .unwrap();
        assert!(first.reusable_native(&second), "{language}");
        assert_eq!(
            first_native
                .declarations
                .iter()
                .map(|d| &d.syntax_id)
                .collect::<Vec<_>>(),
            second_native
                .declarations
                .iter()
                .map(|d| &d.syntax_id)
                .collect::<Vec<_>>(),
            "{language}"
        );
        if language == "rust" {
            assert!(first.reusable_projection(&second));
            assert!(second_graph.nodes.iter().all(|n| n.display_label.is_none()));
        } else {
            assert!(!first.reusable_projection(&second), "{language}");
            assert!(
                second_graph.nodes.iter().any(|n| n.name == "f"
                    && n.display_label.as_deref() == Some("scip display f() version two")),
                "{language}"
            );
        }
        if language != "javascript" {
            continue;
        }
        // Exactly 1,000 documents is admitted; 1,001 drops all optional labels.
        for n in 1..1_000 {
            let mut ghost = scip::types::Document::new();
            ghost.relative_path = format!("absent-{n}.js");
            index.documents.push(ghost);
        }
        fs::write(d.path().join("index.scip"), index.write_to_bytes().unwrap()).unwrap();
        let (at_graph, at_native, at_capture) =
            index_workspace_bundle(&options, &root_id, &cancel(), |_| {}).unwrap();
        let at = measure_document_fingerprint(&at_capture, &at_native, &at_graph, path, &options)
            .unwrap();
        assert!(second.reusable_projection(&at));
        assert_eq!(index.documents.len(), 1_000);
        let mut ghost = scip::types::Document::new();
        ghost.relative_path = "absent-1000.js".into();
        index.documents.push(ghost);
        fs::write(d.path().join("index.scip"), index.write_to_bytes().unwrap()).unwrap();
        let (over_graph, over_native, over_capture) =
            index_workspace_bundle(&options, &root_id, &cancel(), |_| {}).unwrap();
        let over =
            measure_document_fingerprint(&over_capture, &over_native, &over_graph, path, &options)
                .unwrap();
        assert!(at.reusable_native(&over));
        assert!(!at.reusable_projection(&over));
        assert!(over_graph.nodes.iter().all(|n| n.display_label.is_none()));
    }
}

#[test]
fn full_reuse_fingerprint_authenticates_bytes_admission_producer_coverage_owner_and_projection() {
    use trellis::{
        indexer::{index_workspace_bundle, measure_document_fingerprint},
        store::topology::WorkspaceIdentity,
    };
    let d = tempfile::tempdir().unwrap();
    let path = "main.js";
    write(d.path(), path, "function f(){ return 10; }\n");
    let options = IndexOptions::new(d.path().to_owned());
    let root_id = WorkspaceIdentity::discover(Some(d.path()), d.path())
        .unwrap()
        .record_id;
    let (graph, native, capture) =
        index_workspace_bundle(&options, &root_id, &cancel(), |_| {}).unwrap();
    let fingerprint =
        measure_document_fingerprint(&capture, &native, &graph, path, &options).unwrap();
    assert!(fingerprint.reusable_projection(&fingerprint));
    let mut changed_coverage = native.clone();
    changed_coverage.coverage[0].state = "partial".into();
    let coverage =
        measure_document_fingerprint(&capture, &changed_coverage, &graph, path, &options).unwrap();
    assert!(!fingerprint.reusable_native(&coverage));
    let mut changed_owner = native.clone();
    changed_owner.declarations[0].key.kind = "field".into();
    let owner =
        measure_document_fingerprint(&capture, &changed_owner, &graph, path, &options).unwrap();
    assert!(!fingerprint.reusable_native(&owner));
    let mut changed_graph = graph.clone();
    changed_graph.nodes[0].display_label = Some("untrusted display only".into());
    let projection =
        measure_document_fingerprint(&capture, &native, &changed_graph, path, &options).unwrap();
    assert!(fingerprint.reusable_native(&projection));
    assert!(!fingerprint.reusable_projection(&projection));
    let mut changed_admission = options.clone();
    changed_admission.max_file_bytes += 1;
    let (admission_graph, admission_native, admission_capture) =
        index_workspace_bundle(&changed_admission, &root_id, &cancel(), |_| {}).unwrap();
    let admission = measure_document_fingerprint(
        &admission_capture,
        &admission_native,
        &admission_graph,
        path,
        &changed_admission,
    )
    .unwrap();
    assert!(!fingerprint.reusable_native(&admission));
    write(d.path(), path, "function f(){ return 20; }\n");
    let (changed_graph, changed_native, changed_capture) =
        index_workspace_bundle(&options, &root_id, &cancel(), |_| {}).unwrap();
    let changed = measure_document_fingerprint(
        &changed_capture,
        &changed_native,
        &changed_graph,
        path,
        &options,
    )
    .unwrap();
    assert!(!fingerprint.reusable_native(&changed));
    assert!(!fingerprint.reusable_projection(&changed));
    assert_eq!(
        changed_capture.files[0].text.as_bytes(),
        b"function f(){ return 20; }\n"
    );
}

#[test]
fn captured_optional_labels_are_projection_only_but_admission_or_toolchain_changes_fallback() {
    use trellis::{
        capture::Capture,
        indexer::{CapturedChange, measure_captured_change},
    };
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "main.js", "function f(){ return 10; }\n");
    let mut options = IndexOptions::new(d.path().to_owned());
    options.scip_path = Some(d.path().join("absent.scip"));
    options.manifest_path = Some(d.path().join("absent.json"));
    let old = Capture::admit(&options, &cancel(), &|_| {}).unwrap();
    fs::write(
        d.path().join("absent.scip"),
        b"invalid optional presentation",
    )
    .unwrap();
    let optional = Capture::admit(&options, &cancel(), &|_| {}).unwrap();
    assert_eq!(
        measure_captured_change(&old, &optional),
        CapturedChange::Unchanged
    );
    write(d.path(), "Cargo.toml", "[package]\nname='different'\n");
    let config = Capture::admit(&options, &cancel(), &|_| {}).unwrap();
    assert_eq!(
        measure_captured_change(&optional, &config),
        CapturedChange::FullNative {
            reason: "captured native input changed",
        }
    );
    let mut altered = options.clone();
    altered.max_file_bytes += 1;
    let admitted = Capture::admit(&altered, &cancel(), &|_| {}).unwrap();
    assert_eq!(
        measure_captured_change(&config, &admitted),
        CapturedChange::FullNative {
            reason: "capture admission changed",
        }
    );
}

#[test]
fn optional_native_input_aliases_force_full_native_fallback() {
    use trellis::{
        capture::Capture,
        indexer::{CapturedChange, measure_captured_change, measure_captured_native_change},
        store::topology::WorkspaceIdentity,
    };
    for (relative, old_input, new_input) in [
        (
            "Cargo.toml",
            "[package]\nname='one'\n",
            "[package]\nname='two'\n",
        ),
        ("nested/.ignore", "# old ignore\n", "# changed ignore\n"),
    ] {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("nested")).unwrap();
        write(d.path(), "a.js", "function f(){ return 10; }\n");
        write(d.path(), relative, old_input);
        let mut options = IndexOptions::new(d.path().to_owned());
        // Same admitted pathname carries both presentation-scip and native
        // config/ignore roles. Malformed SCIP bytes remain optional UI copy.
        options.scip_path = Some(d.path().join(relative));
        let before = Capture::admit(&options, &cancel(), &|_| {}).unwrap();
        write(d.path(), relative, new_input);
        write(d.path(), "a.js", "function f(){ return 20; }\n");
        let after = Capture::admit(&options, &cancel(), &|_| {}).unwrap();
        assert_eq!(
            before.files[0].text.as_bytes(),
            b"function f(){ return 10; }\n"
        );
        assert_eq!(
            after.files[0].text.as_bytes(),
            b"function f(){ return 20; }\n"
        );
        assert_eq!(
            measure_captured_change(&before, &after),
            CapturedChange::FullNative {
                reason: "captured native input changed",
            },
            "{relative}"
        );
        let root = d.path().canonicalize().unwrap();
        let root_id = WorkspaceIdentity::discover(Some(&root), &root)
            .unwrap()
            .record_id;
        let mut observed = vec![];
        let staged =
            measure_captured_native_change(&before, &after, &root, &root_id, &cancel(), |key| {
                observed.push(key.path.clone())
            })
            .unwrap();
        assert!(matches!(staged.decision, CapturedChange::FullNative { .. }));
        assert!(staged.selected.is_none());
        assert!(
            observed.is_empty(),
            "fallback has no selected-only native measurement"
        );
    }
}

#[test]
fn optional_executable_alias_keeps_authenticated_native_role_without_editing_binary() {
    use trellis::{
        capture::Capture,
        indexer::{CapturedChange, measure_captured_change},
    };
    let executable = std::env::current_exe().unwrap();
    if fs::metadata(&executable).unwrap().len() > 256 * 1024 * 1024 {
        // The optional SCIP capture itself has a 256 MiB admission ceiling.
        return;
    }
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "a.js", "function f(){ return 10; }\n");
    let mut options = IndexOptions::new(d.path().to_owned());
    options.scip_path = Some(executable);
    let before = Capture::admit(&options, &cancel(), &|_| {}).unwrap();
    write(d.path(), "a.js", "function f(){ return 20; }\n");
    let after = Capture::admit(&options, &cancel(), &|_| {}).unwrap();
    assert_eq!(
        measure_captured_change(&before, &after),
        CapturedChange::DocumentLocal {
            path: "a.js".into()
        }
    );
}

#[test]
fn watcher_coalesces_renames_and_retains_signals_until_verified_ack() {
    use notify::{
        Event, EventKind,
        event::{ModifyKind, RenameMode},
    };
    use trellis::watch::WatchSignals;
    let root = tempfile::tempdir().unwrap();
    let mut watcher = WatchSignals::new(root.path().to_owned(), None, None);
    let takeover = watcher.drain();
    assert!(takeover.full);
    if !watcher.watching() {
        assert!(watcher.degraded());
        return; // Platform has no watcher: mandatory full scans remain active.
    }
    assert!(watcher.acknowledge(&takeover));
    let rename = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
        .add_path(root.path().join("old.js"))
        .add_path(root.path().join("new.js"));
    watcher.observe_event(&rename);
    let batch = watcher.drain();
    assert!(batch.paths.contains(std::path::Path::new("old.js")));
    assert!(batch.paths.contains(std::path::Path::new("new.js")));
    watcher.observe_event(&Event::new(EventKind::Any));
    assert!(
        !watcher.acknowledge(&batch),
        "a stale capture cannot clear a newer event"
    );
    let pending = watcher.drain();
    assert!(pending.full);
    assert!(watcher.acknowledge(&pending));
    assert!(!watcher.drain().full);
}

#[test]
fn watcher_promotes_ignore_unknown_and_bulk_changes_to_full_inventory() {
    use notify::{Event, EventKind, event::ModifyKind};
    use trellis::watch::WatchSignals;
    let root = tempfile::tempdir().unwrap();
    let mut watcher = WatchSignals::new(root.path().to_owned(), None, None);
    if !watcher.watching() {
        return;
    }
    let first = watcher.drain();
    assert!(watcher.acknowledge(&first));
    watcher.observe_event(
        &Event::new(EventKind::Modify(ModifyKind::Any))
            .add_path(root.path().join("nested/.ignore")),
    );
    assert!(watcher.drain().full);
    let first = watcher.drain();
    assert!(watcher.acknowledge(&first));
    for n in 0..257 {
        watcher.observe_event(
            &Event::new(EventKind::Modify(ModifyKind::Any))
                .add_path(root.path().join(format!("src/{n}.js"))),
        );
    }
    assert!(watcher.drain().full);
}

#[test]
fn explicit_unchanged_capture_freshly_hashes_every_source() {
    use trellis::{
        capture::Capture,
        indexer::{CapturedChange, measure_captured_change},
    };
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "one.js", "f();");
    write(root.path(), "two.rs", "fn f() {}\n");
    let options = IndexOptions::new(root.path().to_owned());
    let first = Capture::admit(&options, &cancel(), &|_| {}).unwrap();
    let second = Capture::admit(&options, &cancel(), &|_| {}).unwrap();
    assert_eq!(
        measure_captured_change(&first, &second),
        CapturedChange::Unchanged
    );
    for operations in second.source_operations.values() {
        assert_eq!(operations.hashes, 1);
        assert_eq!(operations.complete_reads, 1);
    }
}

#[test]
fn watcher_registration_failure_keeps_full_inventory_active() {
    use trellis::watch::WatchSignals;
    let root = tempfile::tempdir().unwrap();
    let mut watcher = WatchSignals::new(root.path().join("missing-root"), None, None);
    assert!(watcher.degraded());
    assert!(!watcher.watching());
    let batch = watcher.drain();
    assert!(batch.full);
    assert!(watcher.acknowledge(&batch));
    assert!(
        watcher.drain().full,
        "degraded scans cannot rely on missing events"
    );
}

#[test]
fn watcher_callback_filters_capture_read_access_without_rediscovering_work() {
    use notify::{
        Event, EventKind,
        event::{AccessKind, AccessMode},
    };
    use trellis::watch::WatchSignals;
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "source.js", "f();");
    let mut watcher = WatchSignals::new(root.path().to_owned(), None, None);
    if !watcher.watching() {
        return;
    }
    let initial = watcher.drain();
    assert!(watcher.acknowledge(&initial));
    let before = watcher.drain();
    let _captured_bytes = fs::read(root.path().join("source.js")).unwrap();
    for kind in [
        EventKind::Access(AccessKind::Open(AccessMode::Any)),
        EventKind::Access(AccessKind::Open(AccessMode::Read)),
        EventKind::Access(AccessKind::Read),
        EventKind::Access(AccessKind::Close(AccessMode::Read)),
    ] {
        // Uses exactly the same bounded ingress as the installed notify callback.
        watcher.submit_event(Ok(Event::new(kind).add_path(root.path().join("source.js"))));
    }
    let after = watcher.drain();
    assert_eq!(after, before);
    assert!(!after.full);
    assert!(watcher.next_deadline().is_none());
    assert!(watcher.acknowledge(&after));
    watcher.submit_event(Ok(Event::new(EventKind::Access(AccessKind::Close(
        AccessMode::Write,
    )))
    .add_path(root.path().join("source.js"))));
    assert!(watcher.drain().full || !watcher.drain().paths.is_empty());
}

#[test]
fn watcher_bounded_callback_overflow_and_runtime_error_force_full() {
    use notify::{Event, EventKind, event::ModifyKind};
    use trellis::watch::WatchSignals;
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "source.js", "f();");
    let mut watcher = WatchSignals::synthetic_for_tests(root.path().to_owned(), None, None);
    let initial = watcher.drain();
    assert!(watcher.acknowledge(&initial));
    for _ in 0..1025 {
        watcher
            .submit_event(Ok(Event::new(EventKind::Modify(ModifyKind::Any))
                .add_path(root.path().join("source.js"))));
    }
    let overflow = watcher.drain();
    assert!(overflow.full, "dropped callback requires full inventory");
    assert!(watcher.acknowledge(&overflow));
    watcher.submit_event(Err(notify::Error::generic("backend lost events")));
    // A bounded drain may still have earlier queued events after overflow.
    let mut failed = watcher.drain();
    for _ in 0..4 {
        if watcher.degraded() {
            break;
        }
        failed = watcher.drain();
    }
    assert!(failed.full);
    assert!(watcher.degraded());
    assert!(watcher.acknowledge(&failed));
    assert!(watcher.drain().full);
}

#[test]
fn watcher_has_quiet_and_absolute_deadlines_without_blocking_drain() {
    use notify::{Event, EventKind, event::ModifyKind};
    use std::time::{Duration, Instant};
    use trellis::watch::WatchSignals;
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "source.js", "f();");
    let mut watcher = WatchSignals::synthetic_for_tests(root.path().to_owned(), None, None);
    let initial = watcher.drain();
    assert!(watcher.acknowledge(&initial));
    for attempt in 0..50 {
        let event =
            Event::new(EventKind::Modify(ModifyKind::Any)).add_path(root.path().join("source.js"));
        watcher.observe_event(&event);
        // The absolute bound runs from the first signal, which is no later
        // than this. A slow burst on a loaded host then hits that bound.
        let start = Instant::now();
        for _ in 1..257 {
            watcher.observe_event(&event);
        }
        let batch = watcher.drain();
        assert!(
            !batch.full,
            "synthetic burst became full on iteration {attempt}: {batch:?}"
        );
        let deadline = watcher.next_deadline().unwrap();
        assert!(deadline <= start + Duration::from_millis(250));
        assert!(!watcher.batch_ready_at(deadline - Duration::from_nanos(1)));
        assert!(watcher.batch_ready_at(deadline));
        assert!(watcher.acknowledge(&batch));
    }
}

#[test]
fn watcher_queued_burst_on_one_path_stays_partial_until_ack() {
    use notify::{Event, EventKind, event::ModifyKind};
    use trellis::watch::WatchSignals;
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "source.js", "f();");
    let mut watcher = WatchSignals::synthetic_for_tests(root.path().to_owned(), None, None);
    let initial = watcher.drain();
    assert!(watcher.acknowledge(&initial));
    for attempt in 0..50 {
        for _ in 0..257 {
            watcher.submit_event(Ok(Event::new(EventKind::Modify(ModifyKind::Any))
                .add_path(root.path().join("source.js"))));
        }
        let batch = watcher.drain();
        assert!(
            !batch.full,
            "a queued duplicate path became full on iteration {attempt}: {batch:?}"
        );
        assert!(watcher.acknowledge(&batch));
    }
}

#[test]
fn optional_input_and_symlink_suffix_force_full_reconcile() {
    use notify::{Event, EventKind, event::ModifyKind};
    use trellis::watch::WatchSignals;
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("ignored")).unwrap();
    write(root.path(), "ignored/.ignore", "presentation.js\n");
    write(root.path(), "ignored/presentation.js", "{ }\n");
    let optional = root.path().join("ignored/presentation.js");
    let mut watcher = WatchSignals::new(root.path().to_owned(), Some(optional.clone()), None);
    if !watcher.watching() {
        return;
    }
    let initial = watcher.drain();
    assert!(watcher.acknowledge(&initial));
    watcher.observe_event(&Event::new(EventKind::Modify(ModifyKind::Any)).add_path(optional));
    let batch = watcher.drain();
    assert!(
        batch.full,
        "ignored optional input is not a source-only change"
    );
    assert!(watcher.acknowledge(&batch));
    std::os::unix::fs::symlink(
        root.path().join("ignored/presentation.js"),
        root.path().join("alias.js"),
    )
    .unwrap();
    watcher.observe_event(
        &Event::new(EventKind::Modify(ModifyKind::Any)).add_path(root.path().join("alias.js")),
    );
    assert!(
        watcher.drain().full,
        "symlink suffix is not proof of regular source"
    );
}

#[test]
fn failed_capture_does_not_ack_relevant_input_cutoff_or_absent_input() {
    use notify::{Event, EventKind, event::ModifyKind};
    use trellis::{capture::Capture, watch::WatchSignals};
    for absent_at_admission in [false, true] {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "source.js", "f();");
        if !absent_at_admission {
            write(root.path(), "Cargo.toml", "abc");
        }
        let mut watcher = WatchSignals::new(root.path().to_owned(), None, None);
        let initial = watcher.drain();
        assert!(watcher.acknowledge(&initial));
        let capture = Capture::admit(
            &IndexOptions::new(root.path().to_owned()),
            &cancel(),
            &|_| {},
        )
        .unwrap();
        if absent_at_admission {
            write(root.path(), "Cargo.toml", "abc");
        } else {
            write(root.path(), "Cargo.toml", "xyz");
        }
        watcher.observe_event(
            &Event::new(EventKind::Modify(ModifyKind::Any))
                .add_path(root.path().join("Cargo.toml")),
        );
        let dirty = watcher.drain();
        assert!(dirty.full);
        let error = capture.verify(&cancel()).unwrap_err();
        assert!(error.to_string().contains("drift"), "{error:#}");
        // No publication happened; the leader must not call acknowledge.
        let pending = watcher.drain();
        assert!(pending.full);
        assert!(pending.generation >= dirty.generation);
        assert!(pending.paths.contains(Path::new("Cargo.toml")));
    }
}
