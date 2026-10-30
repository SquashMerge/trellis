mod common;
#[cfg(feature = "test-support")]
use std::{
    collections::BTreeSet,
    sync::{Arc, atomic::AtomicBool},
};
use tempfile::TempDir;
use trellis::{model::*, store::Store};
fn private_state() -> TempDir {
    let state = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    state
}
#[test]
fn private_stage_build_is_unpublished_and_cleans_only_its_own_inode() {
    use std::{
        os::unix::fs::{MetadataExt, OpenOptionsExt},
        path::PathBuf,
    };
    let state = private_state();
    let work = tempfile::tempdir().unwrap();
    let mut first_stage: Option<PathBuf> = None;
    let error = Store::open_for_tests_with_index_stage_hook(state.path(), work.path(), |path| {
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("index.db.tmp-")
        );
        assert_eq!(path.parent().unwrap().file_name().unwrap().len(), 64);
        assert!(!path.with_file_name("index.db").exists());
        let named = std::fs::symlink_metadata(path)?;
        let opened = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let held = opened.metadata()?;
        assert!(named.is_file() && !named.file_type().is_symlink());
        assert_eq!((named.dev(), named.ino()), (held.dev(), held.ino()));
        assert_eq!(held.mode() & 0o777, 0o600);
        assert_eq!(held.nlink(), 1);
        let db = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let metadata_version: i64 = db.query_row(
            "SELECT schema_version FROM index_metadata WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        assert_eq!((version, metadata_version), (28, 28));
        for name in [
            "document_versions",
            "revision_documents",
            "graph_projections",
            "class_projections",
            "native_revisions",
        ] {
            let count: i64 = db.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [name],
                |row| row.get(0),
            )?;
            assert_eq!(count, 1, "missing staged v8 table {name}");
        }
        for name in [
            "class_catalog",
            "files",
            "nodes",
            "calls",
            "regions",
            "native_documents",
            "native_coverage",
            "native_provenance",
        ] {
            let count: i64 = db.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [name],
                |row| row.get(0),
            )?;
            assert_eq!(count, 0, "old table leaked into staged v8: {name}");
        }
        let manifest: i64 =
            db.query_row("SELECT count(*) FROM revision_documents", [], |r| r.get(0))?;
        assert_eq!(manifest, 0, "bootstrap must not publish a partial manifest");
        first_stage = Some(path.to_owned());
        anyhow::bail!("injected before-stage-publication refusal")
    })
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected before-stage-publication refusal")
    );
    let first_stage = first_stage.unwrap();
    assert!(!first_stage.exists());
    assert!(!first_stage.with_file_name("index.db").exists());

    let mut replacement_stage = None;
    let error = Store::open_for_tests_with_index_stage_hook(state.path(), work.path(), |path| {
        assert_ne!(path, first_stage.as_path());
        std::fs::remove_file(path)?;
        std::fs::write(path, b"foreign pathname")?;
        replacement_stage = Some(path.to_owned());
        anyhow::bail!("injected replaced-stage refusal")
    })
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected replaced-stage refusal")
    );
    let replacement_stage = replacement_stage.unwrap();
    assert_eq!(
        std::fs::read(&replacement_stage).unwrap(),
        b"foreign pathname"
    );
    std::fs::remove_file(&replacement_stage).unwrap();
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    assert_eq!(store.index_baseline().unwrap().index_revision, 0);
    assert!(!first_stage.exists());
}

fn fixture() -> (TempDir, TempDir, Store) {
    let state = private_state();
    let work = tempfile::tempdir().unwrap();
    let store = crate::common::open_store(state.path(), work.path()).unwrap();
    (state, work, store)
}
#[cfg(feature = "test-support")]
fn pin(store: &Store, revision: u64) -> IndexPin {
    IndexPin {
        index_generation: store.status().unwrap().revision.index_generation,
        index_revision: revision,
    }
}
fn index_db(state: &std::path::Path) -> std::path::PathBuf {
    std::fs::read_dir(state.join("cache/indexes"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
        .join("index.db")
}
#[cfg(feature = "test-support")]
fn cancel() -> CancelFlag {
    Arc::new(AtomicBool::new(false))
}
#[cfg(feature = "test-support")]
fn bundle(
    store: &Store,
    work: &TempDir,
) -> (
    Graph,
    trellis::native_evidence::Artifact,
    trellis::capture::Capture,
) {
    trellis::indexer::index_workspace_bundle(
        &trellis::indexer::IndexOptions::new(work.path().to_owned()),
        store.root_id(),
        &cancel(),
        |_| {},
    )
    .unwrap()
}
#[cfg(feature = "test-support")]
fn publish_bundle(
    store: &Store,
    bundle: &(
        Graph,
        trellis::native_evidence::Artifact,
        trellis::capture::Capture,
    ),
    expected: IndexPin,
    leader: &trellis::store::topology::WriterGuard,
) -> IndexPin {
    store
        .publish_native(&bundle.0, &bundle.2, &bundle.1, leader, expected, &cancel())
        .unwrap()
}
#[cfg(feature = "test-support")]
fn write_source(work: &TempDir) {
    std::fs::write(
        work.path().join("a.js"),
        "function a() { b(); c(); }
function b() { c(); }
function c() { a(); }
",
    )
    .unwrap();
}
#[cfg(feature = "test-support")]
fn symbol_id(graph: &Graph, name: &str) -> String {
    graph
        .nodes
        .iter()
        .find(|node| node.name == name)
        .unwrap()
        .id
        .clone()
}
fn node(id: &str) -> Symbol {
    Symbol {
        id: id.into(),
        name: id.into(),
        kind: SymbolKind::Function,
        path: "a.js".into(),
        range: SourceRange {
            start_line: 1,
            start_column: 1,
            end_line: 1,
            end_column: 1,
            ..SourceRange::default()
        },
        parent: None,
        accessor: false,
        provenance: Provenance {
            source: "syntax".into(),
            semantic: SemanticState::Unavailable,
        },
        display_label: None,
    }
}
fn call(id: &str, from: &str, to: &str) -> CallSite {
    CallSite {
        id: id.into(),
        caller: from.into(),
        callee_text: Some(to.into()),
        path: "a.js".into(),
        range: SourceRange {
            start_line: 1,
            start_column: 1,
            end_line: 1,
            end_column: 1,
            ..SourceRange::default()
        },
        callee_range: None,
        ordinal: 0,
        regions: vec![],
        provenance: node("").provenance,
    }
}

#[cfg(feature = "test-support")]
fn query() -> ViewQuery {
    serde_json::from_str(r#"{"seed":"a"}"#).unwrap()
}
#[test]
#[cfg(feature = "test-support")]
fn publication_is_atomic_and_reopens() {
    let (state, work, store) = fixture();
    write_source(&work);
    let captured = bundle(&store, &work);
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("index_not_ready")
    );
    let baseline = store.index_baseline().unwrap();
    let leader = store.publication_for_tests().unwrap();
    // A local writer guard does not publish. Snapshot now to prove
    // rejected publications write zero bytes.
    let before = std::fs::read(index_db(state.path())).unwrap();
    for error in [
        store
            .publish(&captured.0, &leader, baseline, &cancel())
            .unwrap_err(),
        store
            .publish_captured(&captured.0, &captured.2, &leader, baseline, &cancel())
            .unwrap_err(),
    ] {
        assert!(
            error.to_string().contains("native_evidence_required"),
            "{error:#}"
        );
    }
    drop(leader);
    assert_eq!(std::fs::read(index_db(state.path())).unwrap(), before);
    assert_eq!(store.index_baseline().unwrap(), baseline);
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("index_not_ready")
    );
    let leader = store.publication_for_tests().unwrap();
    let first = publish_bundle(&store, &captured, baseline, &leader);
    assert_eq!(first.index_revision, 1);
    let old = store.graph().unwrap();
    let a = symbol_id(&old, "a");
    let assert_unchanged = || {
        assert_eq!(store.index_baseline().unwrap(), first);
        assert_eq!(store.status().unwrap().revision, first);
        assert_eq!(store.graph().unwrap(), old);
        let reopened = Store::open_for_tests(state.path(), work.path()).unwrap();
        assert_eq!(reopened.status().unwrap().revision, first);
        assert_eq!(reopened.graph().unwrap(), old);
        for document in &captured.1.revision.documents {
            let (stored, bytes) = reopened
                .native_source_at(first, &document.key)
                .unwrap()
                .unwrap();
            assert_eq!(stored, *document);
            assert_eq!(
                bytes,
                captured
                    .2
                    .files
                    .iter()
                    .find(|file| file.path == document.key.path)
                    .unwrap()
                    .text
                    .as_bytes()
                    .to_vec()
            );
        }
    };
    let mut duplicate = old.clone();
    duplicate.nodes.push(old.nodes[0].clone());
    assert!(
        store
            .publish_native(
                &duplicate,
                &captured.2,
                &captured.1,
                &leader,
                first,
                &cancel()
            )
            .is_err()
    );
    assert_unchanged();
    assert!(
        store
            .publish_native(
                &captured.0,
                &captured.2,
                &captured.1,
                &leader,
                baseline,
                &cancel()
            )
            .unwrap_err()
            .to_string()
            .starts_with("revision conflict")
    );
    assert_unchanged();
    assert!(
        store
            .publish_native(
                &captured.0,
                &captured.2,
                &captured.1,
                &leader,
                first,
                &Arc::new(AtomicBool::new(true))
            )
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    assert_unchanged();
    drop(store);
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    assert_eq!(store.graph().unwrap(), old);
    assert_eq!(
        store.source("a.js").unwrap().unwrap().text,
        captured.0.files[0].text
    );
    for document in &captured.1.revision.documents {
        let (stored, bytes) = store
            .native_source_at(first, &document.key)
            .unwrap()
            .unwrap();
        assert_eq!(stored, *document);
        assert_eq!(
            bytes,
            captured
                .2
                .files
                .iter()
                .find(|file| file.path == document.key.path)
                .unwrap()
                .text
                .as_bytes()
                .to_vec()
        );
    }
    assert!(
        store
            .source_at("a.js", Some(pin(&store, 0)))
            .unwrap_err()
            .to_string()
            .starts_with("revision conflict")
    );
    assert_eq!(store.symbol_at(&a, Some(first)).unwrap().unwrap().0, first);
    assert_eq!(store.symbols_at("", 5).unwrap().0, first);
    let mut reverse = captured.0.clone();
    reverse.nodes.reverse();
    reverse.calls.reverse();
    let second = store
        .publish_native(
            &reverse,
            &captured.2,
            &captured.1,
            &leader,
            first,
            &cancel(),
        )
        .unwrap();
    assert_eq!(second.index_revision, 2);
    assert_eq!(store.graph().unwrap(), old);
}

#[test]
#[cfg(feature = "test-support")]
fn delete_reader_pins_snapshot_and_blocks_publish() {
    let (state, work, store) = fixture();
    write_source(&work);
    let captured = bundle(&store, &work);
    let leader = store.publication_for_tests().unwrap();
    let first = publish_bundle(&store, &captured, store.index_baseline().unwrap(), &leader);
    let db = rusqlite::Connection::open(index_db(state.path())).unwrap();
    db.busy_timeout(std::time::Duration::ZERO).unwrap();
    db.execute_batch("BEGIN").unwrap();
    let revision = || {
        db.query_row("SELECT index_revision FROM index_metadata", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
    };
    assert_eq!(revision(), 1);
    assert!(
        store
            .publish_native(
                &captured.0,
                &captured.2,
                &captured.1,
                &leader,
                first,
                &cancel()
            )
            .is_err()
    );
    assert_eq!(revision(), 1);
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM graph_nodes n JOIN revision_documents rd ON rd.gid=n.gid",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        captured.0.nodes.len() as i64
    );
    db.execute_batch("COMMIT").unwrap();
    let next = store
        .publish_native(
            &captured.0,
            &captured.2,
            &captured.1,
            &leader,
            first,
            &cancel(),
        )
        .unwrap();
    assert_eq!(revision(), 2);
    assert_eq!(next.index_revision, 2);
    drop(store);
    drop(leader);
    drop(db);
}

#[test]
fn incompatible_index_refuses_without_touching_legacy_state() {
    let (state, work, store) = fixture();
    let index = index_db(state.path());
    drop(store);
    let legacy = state.path().join("cache.db");
    std::fs::write(&legacy, "untouched legacy bytes").unwrap();
    let db = rusqlite::Connection::open(&index).unwrap();
    db.pragma_update(None, "user_version", 99).unwrap();
    drop(db);
    let before = std::fs::read(&index).unwrap();
    let error = Store::open_for_tests(state.path(), work.path()).unwrap_err();
    assert!(
        error.to_string().contains("unsupported newer schema"),
        "{error:#}"
    );
    assert_eq!(std::fs::read(&index).unwrap(), before);
    assert_eq!(std::fs::read(legacy).unwrap(), b"untouched legacy bytes");
}
#[test]
#[cfg(feature = "test-support")]
fn literal_search_and_snapshot_only_sources() {
    let (_state, work, store) = fixture();
    // Odd names are not valid JavaScript declarations. Search metacharacters must
    // still be literal rather than SQL wildcards against real indexed names.
    std::fs::write(
        work.path().join("a.js"),
        "function abc() {}
function a_c() {}
function percent() {}
",
    )
    .unwrap();
    let captured = bundle(&store, &work);
    let leader = store.publication_for_tests().unwrap();
    publish_bundle(&store, &captured, store.index_baseline().unwrap(), &leader);
    assert_eq!(store.symbols("a_", 1000).unwrap().len(), 1);
    assert!(store.symbols("%_", 1000).unwrap().is_empty());
    assert!(store.symbols("' OR 1=1 --no", 1000).unwrap().is_empty());
    assert!(store.symbol("' OR 1=1 --").unwrap().is_none());
    std::fs::write(work.path().join("a.js"), "changed live source").unwrap();
    assert_eq!(
        store.source("a.js").unwrap().unwrap().text,
        captured.0.files[0].text
    );
    assert!(store.source("../a.js").unwrap().is_none());
}

#[test]
#[cfg(feature = "test-support")]
fn traversal_cycles_bounds_callbacks_and_boundaries() {
    let (_state, work, store) = fixture();
    write_source(&work);
    let captured = bundle(&store, &work);
    let leader = store.publication_for_tests().unwrap();
    publish_bundle(&store, &captured, store.index_baseline().unwrap(), &leader);
    let mut q = query();
    q.seed = symbol_id(&captured.0, "a");
    let seed = store.query_view(&q).unwrap().unwrap();
    assert_eq!(seed.nodes.len(), 1);
    assert_eq!(seed.nodes[0].id, q.seed);
    assert!(!seed.calls.is_empty());
    q.include_callbacks = true;
    q.depth = 5;
    let inert = store.query_view(&q).unwrap().unwrap();
    assert_eq!(inert.nodes, seed.nodes);
    assert_eq!(inert.calls, seed.calls);
    q.max_nodes = 1;
    assert!(!store.query_view(&q).unwrap().unwrap().truncated);
    q.max_calls = 1;
    let bounded = store.query_view(&q).unwrap().unwrap();
    assert_eq!(bounded.calls.len(), 1);
    assert!(bounded.truncated);
    assert_eq!(
        bounded.warnings,
        vec!["Measured calls truncated at the request limit.".to_string()]
    );
    // This captured source has exactly two calls from a. At max_calls + 1,
    // the overflow probe sees the second; at the boundary neither is lost.
    q.max_calls = 2;
    let at_boundary = store.query_view(&q).unwrap().unwrap();
    assert_eq!(at_boundary.calls.len(), 2);
    assert!(!at_boundary.truncated);
    assert!(at_boundary.warnings.is_empty());
    assert_eq!(bounded.calls, at_boundary.calls[..1]);
    q.max_calls = 0;
    assert!(store.query_view(&q).is_err());
    q.max_calls = 1;
    q.exclude_paths.push("a.js".into());
    let excluded = store.query_view(&q).unwrap().unwrap();
    assert_eq!(excluded.nodes.len(), 1);
    assert_eq!(excluded.nodes[0], seed.nodes[0]);
    assert!(excluded.calls.is_empty());
    assert!(!excluded.truncated);
    assert!(excluded.warnings.is_empty());
    q.seed = "missing".into();
    assert!(store.query_view(&q).unwrap().is_none());
    q.depth = 6;
    assert!(store.query_view(&q).is_err());
}

#[test]
#[cfg(feature = "test-support")]
fn main_nested_query_view_keeps_call_order_limit_and_exclusion() {
    // Frozen main (0532a70) SQL payloads and CLI query results. The fixture is
    // added separately on the integration branch; do not derive expectations
    // from this branch's writer or the candidate query result. Symbol and call
    // IDs include the source-set root ID, so their raw bytes (and CLI/payload
    // hashes containing them) cannot match between independent checkout roots.
    let oracle: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/graph-payload-main-0532a70-ties/oracle.json"
    ))
    .unwrap();
    let source = include_str!("fixtures/graph-payload-main-0532a70-ties/nested.js");
    let (_state, work, store) = fixture();
    std::fs::write(work.path().join("nested.js"), source).unwrap();
    let captured = bundle(&store, &work);
    let leader = store.publication_for_tests().unwrap();
    publish_bundle(&store, &captured, store.index_baseline().unwrap(), &leader);

    let main_seed_row = oracle["rows"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == oracle["seed"])
        .unwrap();
    let main_seed: serde_json::Value =
        serde_json::from_str(main_seed_row["payload"].as_str().unwrap()).unwrap();
    // Locate this root's seed from the independently frozen main name, path,
    // and range. Never obtain an expected order from the candidate graph.
    let seeds: Vec<_> = captured
        .0
        .nodes
        .iter()
        .filter(|node| {
            node.name == main_seed["name"].as_str().unwrap()
                && node.path == main_seed["path"].as_str().unwrap()
                && serde_json::to_value(&node.range).unwrap() == main_seed["range"]
        })
        .collect();
    assert_eq!(seeds.len(), 1, "main seed identity is not unique");
    let mut candidate_seed = serde_json::to_value(seeds[0]).unwrap();
    candidate_seed["id"] = main_seed["id"].clone();
    assert_eq!(
        candidate_seed, main_seed,
        "seed differs from main apart from root-scoped ID"
    );

    let mut query = query();
    query.seed = seeds[0].id.clone();
    let main_queries = &oracle["queries"];
    for case in ["max1", "max2", "excluded"] {
        let expected = &main_queries[case];
        query.max_calls = expected["maxCalls"].as_u64().unwrap() as usize;
        query.exclude_paths = expected["excludePaths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|path| path.as_str().unwrap().to_owned())
            .collect();
        let actual = store.query_view(&query).unwrap().unwrap();
        assert_eq!(actual.nodes.len(), 1, "{case}: seed missing");
        assert_eq!(actual.nodes[0].id, query.seed, "{case}: wrong seed");
        let mut actual_seed = serde_json::to_value(&actual.nodes[0]).unwrap();
        actual_seed["id"] = main_seed["id"].clone();
        assert_eq!(actual_seed, main_seed, "{case}: seed differs from main");
        let main_ids = expected["calls"].as_array().unwrap();
        assert_eq!(
            actual.calls.len(),
            main_ids.len(),
            "{case}: main call count"
        );
        let mut observed_ids = BTreeSet::new();
        for (call, main_id) in actual.calls.iter().zip(main_ids) {
            assert!(observed_ids.insert(&call.id), "{case}: duplicate call ID");
            assert_eq!(call.caller, query.seed, "{case}: wrong caller");
            let main_row = oracle["rows"]["calls"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["id"] == *main_id)
                .unwrap();
            let main_call: serde_json::Value =
                serde_json::from_str(main_row["payload"].as_str().unwrap()).unwrap();
            let mut observed = serde_json::to_value(call).unwrap();
            observed["id"] = main_call["id"].clone();
            observed["caller"] = main_call["caller"].clone();
            // Compare each call in MAIN's frozen order, including start/end
            // bytes, ordinal, callee range, and provenance. Only root-scoped
            // references are normalized; no candidate row defines an oracle.
            assert_eq!(observed, main_call, "{case}: call/order differs from main");
        }
        assert_eq!(
            actual.truncated,
            expected["truncated"].as_bool().unwrap(),
            "{case}"
        );
        let main_warnings: Vec<&str> = expected["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|warning| warning.as_str().unwrap())
            .collect();
        assert_eq!(actual.warnings, main_warnings, "{case}");
        assert!(actual.regions.is_empty(), "{case}: unexpected region");
    }
    // Main's first call is the longer of two calls starting at byte 75.
    // This is main-produced evidence for endByte DESC, not a synthetic full tie.
    let main_calls = main_queries["max2"]["calls"].as_array().unwrap();
    let main_rows = oracle["rows"]["calls"].as_array().unwrap();
    let first = main_rows
        .iter()
        .find(|row| row["id"] == main_calls[0])
        .unwrap();
    let second = main_rows
        .iter()
        .find(|row| row["id"] == main_calls[1])
        .unwrap();
    let first_payload: serde_json::Value =
        serde_json::from_str(first["payload"].as_str().unwrap()).unwrap();
    let second_payload: serde_json::Value =
        serde_json::from_str(second["payload"].as_str().unwrap()).unwrap();
    assert_eq!(
        first_payload["range"]["startByte"],
        second_payload["range"]["startByte"]
    );
    assert!(
        first_payload["range"]["endByte"].as_u64().unwrap()
            > second_payload["range"]["endByte"].as_u64().unwrap()
    );
}

#[test]
fn query_view_structurally_sorts_then_limits_then_excludes() {
    // An excluded call at LIMIT is impossible for this seed-scoped query:
    // all its calls have the seed's path. Guard the otherwise unobservable
    // ordering of the three stages in the implementation as approved in S-3.
    let source = include_str!("../src/store.rs");
    let method = source.split_once("fn query_view_in_for(").unwrap().1;
    let method = method.split_once("\n    }\n").unwrap().0;
    let sort = method
        .find("ordered.sort_by(crate::indexer::compare_reader_calls)")
        .unwrap();
    let limit = method
        .find("ordered.into_iter().take(query.max_calls + 1)")
        .unwrap();
    let exclude = method
        .find("query.exclude_paths.iter().any(|p| call.path.starts_with(p))")
        .unwrap();
    assert!(sort < limit && limit < exclude);
}

#[test]
#[cfg(feature = "test-support")]
fn live_current_decode_failure_hard_latches_shared_reads() {
    let (stats_state, stats_work, stats_store) = fixture();
    write_source(&stats_work);
    let captured = bundle(&stats_store, &stats_work);
    let session = stats_store.writer_session_for_tests().unwrap();
    publish_bundle(
        &stats_store,
        &captured,
        stats_store.index_baseline().unwrap(),
        session.writer_guard().unwrap(),
    );
    assert_eq!(stats_store.status().unwrap().revision.index_revision, 1);
    assert!(stats_store.source("a.js").unwrap().is_some());
    let stats_clone = stats_store.clone();
    let stats_path = index_db(stats_state.path());
    let stats_db = rusqlite::Connection::open(&stats_path).unwrap();
    stats_db
        .execute("UPDATE index_metadata SET stats='not-json'", [])
        .unwrap();
    let first = stats_store.symbols("a", 10).unwrap_err();
    assert!(
        first.to_string().starts_with("incompatible_index:")
            && format!("{first:#}").contains("native header/control graph metadata mismatch"),
        "{first:#}"
    );
    assert!(stats_store.recovery_pending_for_tests());
    for closed in [
        stats_store.status().unwrap_err(),
        stats_clone.source("a.js").unwrap_err(),
        stats_clone.query_view(&query()).unwrap_err(),
    ] {
        assert_eq!(
            closed.to_string(),
            "incompatible_index: reconciliation required after invalid current index",
            "every selected shared read must reject the latched corrupt current head"
        );
    }
    drop(stats_clone);
    drop(stats_store);
    drop(session);
    drop(stats_db);
}

#[test]
#[cfg(feature = "test-support")]
fn concurrent_publish_cas_has_one_winner() {
    let (_state, work, store) = fixture();
    write_source(&work);
    let captured = Arc::new(bundle(&store, &work));
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let baseline = store.index_baseline().unwrap();
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let store = store.clone();
            let barrier = barrier.clone();
            let captured = captured.clone();
            std::thread::spawn(move || {
                barrier.wait();
                match store.publication_for_tests() {
                    Ok(leader) => {
                        let result = store.publish_native(
                            &captured.0,
                            &captured.2,
                            &captured.1,
                            &leader,
                            baseline,
                            &cancel(),
                        );
                        (result, Some(leader))
                    }
                    Err(error) => (Err(error), None),
                }
            })
        })
        .collect();
    barrier.wait();
    let outcomes: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert_eq!(
        outcomes.iter().filter(|(result, _)| result.is_ok()).count(),
        1
    );
    assert!(
        outcomes
            .iter()
            .find_map(|(result, _)| result.as_ref().err())
            .unwrap()
            .to_string()
            .starts_with("revision conflict")
            || outcomes.iter().any(|(result, _)| result
                .as_ref()
                .err()
                .is_some_and(|e| e.to_string().starts_with("storage_busy")))
            || outcomes
                .iter()
                .any(|(result, _)| result.as_ref().err().is_some_and(
                    |e| e.to_string() == "incompatible_index: cache changed after admission"
                )),
        "unexpected typed CAS loser: {:?}",
        outcomes
            .iter()
            .filter_map(|(result, _)| result.as_ref().err())
            .map(|e| format!("{e:#}"))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|(result, leader)| result.is_ok() && leader.is_some())
            .count(),
        1
    );
    assert_eq!(store.status().unwrap().revision.index_revision, 1);
}

#[test]
#[cfg(feature = "test-support")]
fn current_reconcile_options_corruption_rejects_selected_reads() {
    let (state, work, store) = fixture();
    write_source(&work);
    let captured = bundle(&store, &work);
    let leader = store.publication_for_tests().unwrap();
    publish_bundle(&store, &captured, store.index_baseline().unwrap(), &leader);
    let clone = store.clone();
    let db = rusqlite::Connection::open(index_db(state.path())).unwrap();
    db.execute(
        "UPDATE index_metadata SET reconcile_options='{}' WHERE singleton=1",
        [],
    )
    .unwrap();
    for error in [
        store.symbols("a", 10).unwrap_err(),
        clone.source("a.js").unwrap_err(),
    ] {
        assert!(
            error.to_string().contains("incompatible_index"),
            "{error:#}"
        );
    }
    drop(leader);
    drop(clone);
    drop(store);
    drop(db);
}

#[test]
#[cfg(feature = "test-support")]
fn selected_root_mismatch_rejects_shared_reads() {
    let (state, work, store) = fixture();
    write_source(&work);
    let captured = bundle(&store, &work);
    let leader = store.publication_for_tests().unwrap();
    publish_bundle(&store, &captured, store.index_baseline().unwrap(), &leader);
    drop(leader);
    let db = rusqlite::Connection::open(index_db(state.path())).unwrap();
    db.execute(
        "UPDATE index_metadata SET root_inode='0' WHERE singleton=1",
        [],
    )
    .unwrap();
    let acquisition = store
        .evidence_response_daemon_local()
        .err()
        .expect("selected read must refuse mismatched root");
    assert!(
        acquisition.to_string().contains("root_changed"),
        "{acquisition:#}"
    );
    for error in [
        store.status().unwrap_err(),
        store.symbols("a", 10).unwrap_err(),
        store.source("a.js").unwrap_err(),
        store.query_view(&query()).unwrap_err(),
    ] {
        assert!(error.to_string().contains("root_changed"), "{error:#}");
        assert!(!error.to_string().contains("index_not_ready"), "{error:#}");
    }
    drop(store);
    drop(db);
}

#[test]
#[cfg(feature = "test-support")]
fn cancelled_reconciliation_keeps_validated_prior_head_selected() {
    let (_state, work, store) = fixture();
    write_source(&work);
    let captured = bundle(&store, &work);
    let leader = store.publication_for_tests().unwrap();
    let prior = publish_bundle(&store, &captured, store.index_baseline().unwrap(), &leader);
    drop(leader);
    let failed_takeover = store.writer_session_for_tests().unwrap();
    let coordinator = trellis::index_coordinator::IndexJobCoordinator::prepare_with_session(
        &store,
        None,
        failed_takeover.clone(),
    )
    .unwrap();
    let cancelled: CancelFlag = Arc::new(AtomicBool::new(true));
    let error = coordinator
        .run(
            &trellis::indexer::IndexOptions::new(work.path().to_owned()),
            &cancelled,
            |_| {},
        )
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error:#}");
    drop(failed_takeover);
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("index_not_ready")
    );
    // A canceled startup reconciliation keeps public status latched, but daemon-local
    // selected evidence can still read the validated prior head.
    let selected = store.evidence_response_daemon_local().unwrap();
    assert_eq!(selected.status().unwrap().revision, prior);
    let (revision, source) = selected.source_at("a.js", Some(prior)).unwrap().unwrap();
    assert_eq!(revision, prior);
    assert_eq!(source, captured.0.files[0]);
    let a = symbol_id(&captured.0, "a");
    let (revision, symbol) = selected.symbol_at(&a, Some(prior)).unwrap().unwrap();
    assert_eq!(revision, prior);
    assert_eq!(symbol.id, a);
    selected.finish(()).unwrap();
}

#[test]
#[cfg(feature = "test-support")]
fn wrong_current_native_header_latches_shared_reads() {
    let (state, work, store) = fixture();
    write_source(&work);
    let captured = bundle(&store, &work);
    let leader = store.publication_for_tests().unwrap();
    publish_bundle(&store, &captured, store.index_baseline().unwrap(), &leader);
    let clone = store.clone();
    let db = rusqlite::Connection::open(index_db(state.path())).unwrap();
    db.execute(
        "UPDATE native_revisions SET graph_stats='{}' WHERE published_index_revision=1",
        [],
    )
    .unwrap();
    // Keep the independent SQLite observer open until the Store, clone, and
    // local writer guard release this index inode.
    let first = store.status().unwrap_err();
    assert!(
        first.to_string().contains("incompatible_index"),
        "{first:#}"
    );
    assert!(store.recovery_pending_for_tests());
    for latched in [
        store.symbols("a", 10).unwrap_err(),
        clone.source("a.js").unwrap_err(),
        clone.query_view(&query()).unwrap_err(),
    ] {
        assert_eq!(
            latched.to_string(),
            "incompatible_index: reconciliation required after invalid current index",
            "{latched:#}"
        );
    }
    drop(leader);
    drop(clone);
    drop(store);
    drop(db);
}

#[test]
#[cfg(feature = "test-support")]
fn virgin_head_without_selected_revision_is_explicitly_not_ready() {
    let (state, work, store) = fixture();
    let leader = store.publication_for_tests().unwrap();
    // Acquiring a local publication guard must not create a selected revision.
    let before = std::fs::read(index_db(state.path())).unwrap();
    let error = store.status().unwrap_err();
    assert!(error.to_string().contains("index_not_ready"), "{error:#}");
    assert!(!error.to_string().contains("no such column"), "{error:#}");
    for error in [
        store.source("a.js").unwrap_err(),
        store.symbols("a", 10).unwrap_err(),
        store.query_view(&query()).unwrap_err(),
        store
            .evidence_response_daemon_local()
            .err()
            .expect("virgin head must not select a revision"),
    ] {
        assert!(error.to_string().contains("index_not_ready"), "{error:#}");
    }
    assert_eq!(store.index_baseline().unwrap().index_revision, 0);
    assert_eq!(std::fs::read(index_db(state.path())).unwrap(), before);
    drop(leader);
    drop(work);
}

#[test]
#[cfg(feature = "test-support")]
fn malformed_graph_rolls_back_and_structural_stats_are_recounted() {
    let (state, work, store) = fixture();
    write_source(&work);
    let captured = bundle(&store, &work);
    let leader = store.publication_for_tests().unwrap();
    let first = publish_bundle(&store, &captured, store.index_baseline().unwrap(), &leader);
    let baseline = store.graph().unwrap();
    assert_eq!(baseline.stats.symbols, captured.0.nodes.len());
    assert_eq!(baseline.stats.internal, 0);
    assert_eq!(baseline.stats.unresolved, captured.0.calls.len());
    assert!(!baseline.calls.is_empty());
    for kind in 0..7 {
        let mut bad = captured.0.clone();
        match kind {
            0 => bad.calls[0].caller = "missing".into(),
            1 => bad.calls[0].id = bad.calls[1].id.clone(),
            2 => bad.calls[0].regions.push("missing".into()),
            3 => bad.calls[0].range.end_byte = usize::MAX,
            4 => bad.nodes[0].parent = Some("missing".into()),
            5 => bad.nodes[0].range.end_byte = usize::MAX,
            _ => bad.nodes[0].range.start_line = 0,
        }
        assert!(
            store
                .publish_native(&bad, &captured.2, &captured.1, &leader, first, &cancel())
                .is_err(),
            "case {kind}"
        );
        assert_eq!(store.index_baseline().unwrap(), first);
        assert_eq!(store.status().unwrap().revision, first);
        assert_eq!(store.graph().unwrap(), baseline);
        let reopened = Store::open_for_tests(state.path(), work.path()).unwrap();
        assert_eq!(reopened.status().unwrap().revision, first);
        assert_eq!(reopened.graph().unwrap(), baseline);
        for document in &captured.1.revision.documents {
            let (stored, bytes) = reopened
                .native_source_at(first, &document.key)
                .unwrap()
                .unwrap();
            assert_eq!(stored, *document);
            assert_eq!(
                bytes,
                captured
                    .2
                    .files
                    .iter()
                    .find(|file| file.path == document.key.path)
                    .unwrap()
                    .text
                    .as_bytes()
                    .to_vec()
            );
        }
    }
    let next = publish_bundle(&store, &captured, store.index_baseline().unwrap(), &leader);
    assert_eq!(store.status().unwrap().revision, next);
    assert_eq!(store.graph().unwrap(), baseline);
}

#[test]
#[cfg(feature = "test-support")]
fn cache_loss_never_reuses_revision_tokens_and_sql_enforces_foreign_keys() {
    let (state, work, store) = fixture();
    write_source(&work);
    let captured = bundle(&store, &work);
    let leader = store.publication_for_tests().unwrap();
    let rev = publish_bundle(&store, &captured, store.index_baseline().unwrap(), &leader);
    let db = rusqlite::Connection::open(index_db(state.path())).unwrap();
    db.pragma_update(None, "foreign_keys", true).unwrap();
    assert!(
        db.execute("DELETE FROM document_versions WHERE vid=(SELECT vid FROM revision_documents WHERE path='a.js')", [])
            .is_err()
    );
    // Graph nodes no longer have call-edge FKs in schema 13. Check the
    // retained graph_display_labels -> graph_nodes FK with a dangling label.
    let label_error = db
        .execute(
            "INSERT INTO graph_display_labels(gid,id,label) VALUES ((SELECT gid FROM revision_documents WHERE path='a.js'), ?1, 'orphan')",
            [vec![0_u8; 16]],
        )
        .unwrap_err();
    assert!(matches!(
        label_error,
        rusqlite::Error::SqliteFailure(ref error, _)
            if error.code == rusqlite::ErrorCode::ConstraintViolation
    ));
    drop(leader);
    drop(store);
    drop(db);
    std::fs::remove_file(index_db(state.path())).unwrap();
    let store = crate::common::open_store(state.path(), work.path()).unwrap();
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("index_not_ready")
    );
    let fresh = bundle(&store, &work);
    let leader = store.publication_for_tests().unwrap();
    assert!(
        store
            .publish_native(&fresh.0, &fresh.2, &fresh.1, &leader, rev, &cancel())
            .is_err()
    );
    let new_rev = publish_bundle(&store, &fresh, store.index_baseline().unwrap(), &leader);
    assert_ne!(new_rev.index_generation, rev.index_generation);
    assert!(store.source_at("a.js", Some(rev)).is_err());
}

#[test]
fn refuses_unversioned_existing_index_without_migration() {
    let (state, work, store) = fixture();
    let index = index_db(state.path());
    drop(store);
    let db = rusqlite::Connection::open(&index).unwrap();
    db.pragma_update(None, "user_version", 1).unwrap();
    drop(db);
    let deferred = Store::open_for_tests(state.path(), work.path()).unwrap();
    assert!(
        deferred.status().is_err(),
        "unversioned disposable index must not serve"
    );
    assert!(index.exists());
}

#[test]
fn graph_call_dto_rejects_lexical_proof_fields() {
    let call = call("ab", "a", "b");
    let mut value = serde_json::to_value(call).unwrap();
    for field in [
        "target",
        "candidateSymbols",
        "resolution",
        "callbackArguments",
    ] {
        value[field] = serde_json::json!("unsupported");
        assert!(
            serde_json::from_value::<CallSite>(value.clone()).is_err(),
            "{field}"
        );
        value.as_object_mut().unwrap().remove(field);
    }
}

#[test]
#[cfg(feature = "test-support")]
fn active_delete_journal_allows_prior_pair_or_busy_and_cold_journal_is_sqlite_managed() {
    let (state, work, store) = fixture();
    write_source(&work);
    let captured = bundle(&store, &work);
    let leader = store.publication_for_tests().unwrap();
    let previous = publish_bundle(&store, &captured, store.index_baseline().unwrap(), &leader);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let path = index_db(state.path());
    let writer_path = path.clone();
    let writer = std::thread::spawn(move || {
        let db = rusqlite::Connection::open(writer_path).unwrap();
        db.execute_batch(
            "BEGIN IMMEDIATE; UPDATE index_metadata SET index_revision=2 WHERE singleton=1",
        )
        .unwrap();
        ready_tx.send(()).unwrap();
        done_rx.recv().unwrap();
        db.execute_batch("ROLLBACK").unwrap();
        (db, leader)
    });
    ready_rx.recv().unwrap();

    // Capture every active-journal outcome before releasing the writer, so no
    // assertion can strand the writer thread or its leader guard.
    let journal = path.with_file_name("index.db-journal");
    let active_journal_exists = journal.exists();
    let active_status = store.status();
    let active_source = store.source_at("a.js", Some(previous));
    let raw_db = rusqlite::Connection::open(&path).unwrap();
    let raw_pair = (|| -> rusqlite::Result<(String, i64, String, String, Vec<u8>, String)> {
        raw_db.busy_timeout(std::time::Duration::ZERO)?;
        raw_db.query_row(
            "SELECT m.index_generation,m.index_revision,d.path,d.content_hash,d.source_bytes,d.content_hash FROM index_metadata m JOIN native_revisions r ON r.published_index_revision=m.index_revision JOIN revision_documents rd ON rd.revision_id=r.id JOIN document_versions d ON d.vid=rd.vid WHERE m.singleton=1 AND rd.path='a.js'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
    })();
    done_tx.send(()).unwrap();
    let (writer_db, writer_guard) = writer.join().unwrap();

    assert!(active_journal_exists);
    match active_status {
        Ok(status) => assert_eq!(status.revision, previous),
        Err(error) => {
            let text = error.to_string();
            assert!(
                text.contains("index_not_ready") || text.contains("storage_busy"),
                "{error:#}"
            );
        }
    }
    match active_source {
        Ok(Some((pin, source))) => {
            assert_eq!(pin, previous);
            assert_eq!(source.text, captured.0.files[0].text);
            assert_eq!(source.hash, captured.0.files[0].hash);
            assert_eq!(source.text.as_bytes(), captured.2.files[0].text.as_bytes());
        }
        Ok(None) => panic!("active DELETE journal hid the prior committed source"),
        Err(error) => {
            let text = error.to_string();
            assert!(
                text.contains("index_not_ready") || text.contains("storage_busy"),
                "{error:#}"
            );
        }
    }
    match raw_pair {
        Ok((generation, revision, path, file_hash, native_bytes, native_hash)) => {
            assert_eq!(generation, previous.index_generation.to_string());
            assert_eq!(revision, previous.index_revision as i64);
            assert_eq!(path, "a.js");
            assert_eq!(file_hash, captured.0.files[0].hash);
            assert_eq!(native_hash, captured.0.files[0].hash);
            assert_eq!(native_bytes, captured.0.files[0].text.as_bytes().to_vec());
        }
        Err(error) => assert!(
            matches!(
                error.sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
            ),
            "{error}"
        ),
    }
    assert_eq!(store.index_baseline().unwrap(), previous);
    // Rolling back the uncommitted writer leaves the prior selected head valid.
    assert_eq!(store.status().unwrap().revision, previous);
    let (pin, selected) = store.source_at("a.js", Some(previous)).unwrap().unwrap();
    assert_eq!(pin, previous);
    assert_eq!(selected.text, captured.0.files[0].text);
    assert_eq!(selected.hash, captured.0.files[0].hash);

    // A local writer guard must not unlink or rewrite a cold SQLite journal.
    // Only SQLite decides whether this journal participates in recovery.
    std::fs::write(&journal, [0u8; 512]).unwrap();
    let cold = std::fs::read(&journal).unwrap();
    let cold_writer = store.publication_for_tests().unwrap();
    assert_eq!(std::fs::read(&journal).unwrap(), cold);
    drop(cold_writer);
    drop(store);
    drop(writer_guard);
    drop(writer_db);
    drop(raw_db);
}

#[test]
#[cfg(feature = "test-support")]
fn safe_cache_unknown_view_blocks_public_status_and_source_until_owner_intervenes() {
    let (state, work, store) = fixture();
    write_source(&work);
    let captured = bundle(&store, &work);
    let leader = store.publication_for_tests().unwrap();
    let before = publish_bundle(&store, &captured, store.index_baseline().unwrap(), &leader);
    let clone = store.clone();
    let path = index_db(state.path());
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE VIEW unapproved_view AS SELECT 1")
        .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let reopened = Store::open_for_tests(state.path(), work.path()).unwrap();
    assert!(
        reopened
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
    assert!(
        store
            .graph()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
    let rejected = store
        .publish_native(
            &captured.0,
            &captured.2,
            &captured.1,
            &leader,
            before,
            &cancel(),
        )
        .unwrap_err();
    assert!(
        rejected.to_string().contains("revision conflict"),
        "{rejected:#}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    db.execute_batch("DROP VIEW unapproved_view").unwrap();
    assert_eq!(store.index_baseline().unwrap(), before);
    for error in [
        store.status().unwrap_err(),
        store.source("a.js").unwrap_err(),
        store.graph().unwrap_err(),
        clone.status().unwrap_err(),
        clone.source("a.js").unwrap_err(),
        clone.graph().unwrap_err(),
    ] {
        assert!(
            error.to_string().contains("incompatible_index"),
            "{error:#}"
        );
    }
    drop(leader);
    drop(reopened);
    drop(clone);
    drop(store);
    drop(db);
}

#[test]
#[cfg(feature = "test-support")]
fn dangling_selected_revision_fails_closed() {
    let (state, work, store) = fixture();
    std::fs::write(
        work.path().join("b.js"),
        "function target() { return 2; }\n",
    )
    .unwrap();
    let captured = bundle(&store, &work);
    let leader = store.publication_for_tests().unwrap();
    let pin = publish_bundle(&store, &captured, store.index_baseline().unwrap(), &leader);
    let before = store.source_at("b.js", Some(pin)).unwrap().unwrap();
    assert_eq!(before.0, pin);
    assert_eq!(before.1.text, captured.0.files[0].text);
    let cache = rusqlite::Connection::open(index_db(state.path())).unwrap();
    cache.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    cache
        .execute("UPDATE revision_documents SET vid=-1 WHERE path='b.js'", [])
        .unwrap();
    cache.execute_batch("PRAGMA foreign_keys=ON").unwrap();

    let first = store.source_at("b.js", Some(pin)).unwrap_err();
    let first_detail = format!("{first:#}");
    assert_eq!(
        first_detail,
        "incompatible_index: selected source version absent"
    );
    assert!(store.recovery_pending_for_tests());
    let second = store.query_view(&query()).unwrap_err();
    assert_eq!(
        second.to_string(),
        "incompatible_index: reconciliation required after invalid current index",
        "{second:#}"
    );
    drop(leader);
    drop(store);
    drop(cache);
}

#[test]
fn partial_v8_bootstrap_rejects_shared_reads() {
    let (state, _work, store) = fixture();
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("index_not_ready")
    );
    let db = rusqlite::Connection::open(index_db(state.path())).unwrap();
    db.execute(
        "INSERT INTO native_source_sets(id,root_id) VALUES('forged-bootstrap','forged-root')",
        [],
    )
    .unwrap();
    let clone = store.clone();
    let first = store.symbols("a", 10).unwrap_err();
    assert!(
        first.to_string().contains("incompatible_index"),
        "{first:#}"
    );
    let closed = clone.source("a.js").unwrap_err();
    assert!(
        closed.to_string().contains("incompatible_index"),
        "{closed:#}"
    );
    drop(clone);
    drop(store);
    drop(db);
}
