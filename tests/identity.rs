#[cfg(feature = "test-support")]
use trellis::model::*;
mod common;
use std::{
    fs,
    sync::{Arc, atomic::AtomicBool},
};
use tempfile::TempDir;
use trellis::indexer::{IndexOptions, index_workspace};
#[cfg(feature = "test-support")]
#[test]
fn real_syntax_rename_orphans_notes_and_cached_graph_preserves_call_order() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("source");
    let state = temp.path().join("state");
    fs::create_dir(&root).unwrap();
    let text = "function first() { a(); b(); c(); d(); e(); f(); g(); h(); i(); j(); k(); l(); m(); n(); o(); p(); q(); r(); s(); t(); u(); v(); w(); x(); y(); z(); }\n";
    fs::write(root.join("flow.js"), text).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let opts = IndexOptions::new(root.canonicalize().unwrap());
    let graph = index_workspace(&opts, &cancel, |_| {}).unwrap();
    let again = index_workspace(&opts, &cancel, |_| {}).unwrap();
    assert_eq!(graph, again);
    let symbol = graph.nodes.iter().find(|n| n.name == "first").unwrap();
    let old_id = symbol.id.clone();
    let store = crate::common::open_store(&state, &root).unwrap();
    let session = store.writer_session_for_tests().unwrap();
    let rev = publish_bundle(
        &store,
        &graph,
        &root,
        session.writer_guard().unwrap(),
        trellis::model::IndexPin {
            index_generation: store.index_baseline().unwrap().index_generation,
            index_revision: 0,
        },
        &cancel,
    )
    .unwrap();
    assert_eq!(store.graph().unwrap(), graph);
    store
        .put_annotation(&Annotation {
            id: "note".into(),
            node_id: old_id.clone(),
            body: "belongs to first".into(),
        })
        .unwrap();
    let query = ViewQuery {
        seed: old_id.clone(),
        depth: 1,
        max_nodes: 40,
        max_calls: 3,
        include_callbacks: false,
        exclude_paths: vec![],
    };
    let view = store.query_view(&query).unwrap().unwrap();
    assert!(view.truncated);
    assert_eq!(
        view.calls
            .iter()
            .map(|c| c.callee_text.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("a"), Some("b"), Some("c")]
    );
    fs::write(root.join("flow.js"), text.replace("first", "other")).unwrap();
    let changed = index_workspace(&opts, &cancel, |_| {}).unwrap();
    assert!(changed.nodes.iter().all(|n| n.id != old_id));
    publish_bundle(
        &store,
        &changed,
        &root,
        session.writer_guard().unwrap(),
        rev,
        &cancel,
    )
    .unwrap();
    assert!(store.annotations().unwrap()[0].orphaned);
    assert_eq!(
        store.annotations().unwrap()[0].annotation.body,
        "belongs to first"
    );
}

#[test]
fn class_field_calls_keep_measured_declaration_owners_without_execution_claims() {
    let temp = TempDir::new().unwrap();
    let text = "function make() { if (ok) return class C { [fieldKey()] = build(); static eager = now(); [key()]() { body(); } }; }\n";
    fs::write(temp.path().join("fields.js"), text).unwrap();
    let graph = index_workspace(
        &IndexOptions::new(temp.path().canonicalize().unwrap()),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let calls = ["fieldKey", "now", "key", "build"];
    for callee in calls {
        let measured = graph
            .calls
            .iter()
            .find(|call| call.callee_text.as_deref() == Some(callee))
            .unwrap();
        let source = &graph.files[0].text;
        let token = measured.callee_range.as_ref().unwrap();
        assert_eq!(&source[token.start_byte..token.end_byte], callee);
        assert_eq!(
            source
                .get(measured.range.start_byte..measured.range.end_byte)
                .unwrap(),
            format!("{callee}()")
        );
        let owner = graph
            .nodes
            .iter()
            .find(|node| node.id == measured.caller)
            .unwrap();
        assert_eq!(
            owner.name, "C",
            "{callee} remains under its measured class syntax"
        );
        assert!(
            measured.regions.is_empty(),
            "field syntax is not an invented control region"
        );
        let wire = serde_json::to_value(measured).unwrap();
        for unsafe_field in [
            "target",
            "resolution",
            "candidateSymbols",
            "callbackArguments",
        ] {
            assert!(wire.get(unsafe_field).is_none());
        }
    }
    assert!(
        !graph
            .diagnostics
            .iter()
            .any(|d| d.code == "instance-initializer-boundary")
    );
}
#[cfg(feature = "test-support")]
fn index_db(state: &std::path::Path) -> std::path::PathBuf {
    std::fs::read_dir(state.join("cache/indexes"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
        .join("index.db")
}
#[cfg(feature = "test-support")]
#[test]
fn injected_sql_failure_after_insert_preserves_previous_revision() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("source");
    let state = temp.path().join("state");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("flow.js"), "function before() { a(); b(); }\n").unwrap();
    let options = IndexOptions::new(root.canonicalize().unwrap());
    let cancel = Arc::new(AtomicBool::new(false));
    let first = index_workspace(&options, &cancel, |_| {}).unwrap();
    let store = crate::common::open_store(&state, &root).unwrap();
    let session = store.writer_session_for_tests().unwrap();
    let revision = publish_bundle(
        &store,
        &first,
        &root,
        session.writer_guard().unwrap(),
        trellis::model::IndexPin {
            index_generation: store.index_baseline().unwrap().index_generation,
            index_revision: 0,
        },
        &cancel,
    )
    .unwrap();
    fs::write(root.join("flow.js"), "function after() { c(); d(); }\n").unwrap();
    let next = index_workspace(&options, &cancel, |_| {}).unwrap();
    let id = next.calls[1].id.replace('\'', "''");
    let db = rusqlite::Connection::open(index_db(&state)).unwrap();
    db.execute_batch(&format!("CREATE TRIGGER abort_second_call BEFORE INSERT ON native_version_calls WHEN NEW.version_id IS NOT NULL AND NEW.id='{id}' BEGIN SELECT RAISE(ABORT,'injected post-write failure'); END;")).unwrap();
    drop(db);
    let unchanged = fs::read(index_db(&state)).unwrap();
    let failure = publish_bundle(
        &store,
        &next,
        &root,
        session.writer_guard().unwrap(),
        revision,
        &cancel,
    )
    .unwrap_err();
    assert!(
        failure
            .to_string()
            .contains("incompatible_index: unknown cache object")
    );
    assert_eq!(fs::read(index_db(&state)).unwrap(), unchanged);
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
    let db = rusqlite::Connection::open(index_db(&state)).unwrap();
    db.execute_batch("DROP TRIGGER abort_second_call").unwrap();
    drop(db);
    assert_eq!(store.index_baseline().unwrap(), revision);
    for error in [store.status().unwrap_err(), store.graph().unwrap_err()] {
        assert_eq!(
            error.to_string(),
            "incompatible_index: reconciliation required after invalid current index"
        );
    }
    let next_revision = publish_bundle(
        &store,
        &next,
        &root,
        session.writer_guard().unwrap(),
        revision,
        &cancel,
    )
    .unwrap();
    assert_ne!(next_revision.index_generation, revision.index_generation);
    assert_eq!(next_revision.index_revision, 1);
    assert_eq!(store.status().unwrap().revision, next_revision);
    assert_eq!(store.graph().unwrap(), next);
}

#[cfg(feature = "test-support")]
fn publish_bundle(
    store: &trellis::store::Store,
    graph: &trellis::model::Graph,
    workspace: &std::path::Path,
    leader: &trellis::store::topology::WriterGuard,
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
