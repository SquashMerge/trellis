#[cfg(feature = "test-support")]
use rusqlite::Connection;
use std::{
    fs,
    sync::{Arc, atomic::AtomicBool},
};
#[cfg(feature = "test-support")]
use trellis::{
    indexer::{IndexOptions, index_workspace_bundle},
    model::IndexPin,
};
use trellis::{model::CancelFlag, store::Store};

// Pin every SQL tamper to the admitted revision. Historical versions and
// projections can retain the same source path or measured symbol ID.
#[cfg(feature = "test-support")]
fn live_revision(db: &Connection) -> String {
    db.query_row(
        "SELECT r.id FROM native_revisions r JOIN index_metadata m ON m.index_revision=r.published_index_revision",
        [],
        |row| row.get(0),
    ).unwrap()
}
#[cfg(feature = "test-support")]
fn live_document_version(db: &Connection, path: &str) -> String {
    db.query_row(
        "SELECT d.document_version_id FROM revision_documents d WHERE d.revision_id=?1 AND d.path=?2",
        rusqlite::params![live_revision(db), path],
        |row| row.get(0),
    ).unwrap()
}
#[cfg(feature = "test-support")]
fn live_graph_projection(db: &Connection, path: &str) -> String {
    db.query_row(
        "SELECT d.graph_projection_id FROM revision_documents d WHERE d.revision_id=?1 AND d.path=?2",
        rusqlite::params![live_revision(db), path],
        |row| row.get(0),
    ).unwrap()
}
#[cfg(feature = "test-support")]
fn live_class_projection(db: &Connection, path: &str) -> String {
    db.query_row(
        "SELECT d.class_projection_id FROM revision_documents d WHERE d.revision_id=?1 AND d.path=?2",
        rusqlite::params![live_revision(db), path],
        |row| row.get(0),
    ).unwrap()
}
// Simulate a prior current-v8 cache whose additive maintenance extension has
// not yet been installed. The first new publication must preserve this inode,
// generation, selected pin, and durable request queue.
#[cfg(feature = "test-support")]
fn remove_maintenance_extension_for_v8_test(path: &std::path::Path) {
    let db = Connection::open(path).unwrap();
    db.execute_batch(
        "PRAGMA foreign_keys=OFF; BEGIN IMMEDIATE;
         DROP TABLE native_release_candidate_classes;
         DROP TABLE native_release_candidate_graphs;
         DROP TABLE native_release_candidate_versions;
         DROP TABLE native_revision_release_debt;
         DROP INDEX native_supersessions_due;
         DROP INDEX native_supersessions_clock;
         DROP INDEX native_version_declarations_owner;
         DROP INDEX native_version_regions_parent;
         DROP INDEX native_revisions_header_cover;
         DROP TABLE native_revision_supersessions;
         CREATE TABLE native_revision_supersessions(revision_id TEXT PRIMARY KEY REFERENCES native_revisions(id),superseded_at INTEGER NOT NULL CHECK(superseded_at BETWEEN 0 AND 9007199254740991));
         COMMIT; PRAGMA foreign_keys=ON;",
    ).unwrap();
}
fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Store, CancelFlag) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for (name, text) in [
        (
            "flow.js",
            "function hello(x) { if (x) { obj.go(); } }
",
        ),
        (
            "flow.java",
            "class Demo { void go() { if (true) { run(); } } }
",
        ),
        (
            "flow.rs",
            "fn main() { if true { run(); } }
",
        ),
        (
            "flow.py",
            "def go():
    if True:
        run()
",
        ),
    ] {
        fs::write(root.path().join(name), text).unwrap();
    }
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    (state, root, store, Arc::new(AtomicBool::new(false)))
}
#[cfg(feature = "test-support")]
fn publish(
    store: &Store,
    root: &std::path::Path,
    cancel: &CancelFlag,
    expected: IndexPin,
    leader: &trellis::store::topology::WriterGuard,
) -> anyhow::Result<IndexPin> {
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.to_owned()),
        store.root_id(),
        cancel,
        |_| {},
    )?;
    assert_eq!(capture.graph_projection_count(), 1);
    assert!(
        capture
            .source_operations
            .values()
            .all(|operations| operations.opens == 1
                && operations.complete_reads == 1
                && operations.hashes == 1)
    );
    let revision = store.publish_native(&graph, &capture, &native, leader, expected, cancel)?;
    assert_eq!(capture.graph_projection_count(), 1);
    Ok(revision)
}
#[cfg(feature = "test-support")]
fn assert_current_corruption(error: anyhow::Error) {
    assert_eq!(
        error.to_string(),
        "incompatible_index: reconciliation required after invalid current index"
    );
}
#[test]
#[cfg(feature = "test-support")]
fn four_languages_normalized_rows_and_pinned_bytes_are_coherent() {
    let (state, root, store, cancel) = fixture();
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("index_not_ready")
    );
    let baseline = store.index_baseline().unwrap();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(&store, root.path(), &cancel, baseline, &leader).unwrap();
    // A current v8 empty bootstrap is the same generation; only a rebuild rotates it.
    assert_eq!(baseline.index_generation, pin.index_generation);
    assert_eq!(
        store.status().unwrap().evidence_format.as_deref(),
        Some("terminal-native-graph-v1")
    );
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let path = state
        .path()
        .join("cache/indexes")
        .join(&identity.root_key)
        .join("index.db");
    let before_gc = fs::read(&path).unwrap();
    let roots = trellis::store::topology::TopologyRoots::isolated_for_tests(
        state.path().join("cache"),
        state.path().join("data"),
    );
    let report = roots.gc_report().unwrap();
    assert_eq!(
        (report.derived[0].status, report.derived[0].reason),
        ("busy", "use_lock_busy")
    );
    assert_eq!(fs::read(&path).unwrap(), before_gc);
    let (graph, artifact, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    for document in &artifact.revision.documents {
        let (stored, bytes) = store.native_source_at(pin, &document.key).unwrap().unwrap();
        assert_eq!(stored, *document);
        assert_eq!(
            bytes,
            capture
                .files
                .iter()
                .find(|f| f.path == document.key.path)
                .unwrap()
                .text
                .as_bytes()
        );
        let coverage = store
            .native_coverage_at(pin, &document.key)
            .unwrap()
            .unwrap();
        assert_eq!(
            coverage,
            *artifact
                .coverage
                .iter()
                .find(|c| c.document_path == document.key.path)
                .unwrap()
        );
        let (_, source) = store
            .source_at(&document.key.path, Some(pin))
            .unwrap()
            .unwrap();
        assert_eq!(source.text.as_bytes(), bytes);
    }
    for declaration in &artifact.declarations {
        if let Some(lookup) = &declaration.lookup_key {
            let found = store
                .native_declarations_at(pin, &declaration.document.language, lookup)
                .unwrap();
            assert!(
                found.contains(declaration),
                "missing typed declaration: {}",
                declaration.syntax_id
            );
        }
        assert_eq!(
            store.native_calls_at(pin, &declaration.syntax_id).unwrap(),
            artifact
                .calls
                .iter()
                .filter(|c| c.owner_syntax_id == declaration.syntax_id)
                .cloned()
                .collect::<Vec<_>>()
        );
        assert_eq!(
            store
                .native_control_regions_at(pin, &declaration.syntax_id)
                .unwrap(),
            artifact
                .control_regions
                .iter()
                .filter(|r| r.owner_syntax_id == declaration.syntax_id)
                .cloned()
                .collect::<Vec<_>>()
        );
    }
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        8
    );
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name LIKE 'native_%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(count >= 18);
    for node in &graph.nodes {
        let parent: Option<String> = db
            .query_row(
                "SELECT owner_syntax_id FROM native_version_declarations WHERE version_id=?1 AND syntax_id=?2",
                rusqlite::params![live_document_version(&db, &node.path), node.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            parent, node.parent,
            "native parent differs from measured graph parent"
        );
    }
    assert_eq!(
        store
            .publish(&graph, &leader, pin, &cancel)
            .unwrap_err()
            .to_string(),
        "native_evidence_required: graph-only publication refused"
    );
    let closed = store
        .native_source_at(baseline, &artifact.revision.documents[0].key)
        .unwrap_err();
    assert_eq!(
        closed.to_string(),
        "revision conflict: foreign or missing native pin"
    );
    drop(db);
    let next = publish(&store, root.path(), &cancel, pin, &leader).unwrap();
    assert_eq!(next.index_generation, pin.index_generation);
    assert_eq!(next.index_revision, pin.index_revision + 1);
    assert!(
        !store
            .native_declarations_at(pin, "java", "Demo")
            .unwrap()
            .is_empty()
    );
}

#[test]
#[cfg(feature = "test-support")]
fn invalid_native_and_conflict_never_change_published_pair() {
    let (state, root, store, cancel) = fixture();
    let baseline = store.index_baseline().unwrap();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(&store, root.path(), &cancel, baseline, &leader).unwrap();
    let path = state
        .path()
        .join("cache/indexes")
        .join(
            trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
                .unwrap()
                .root_key,
        )
        .join("index.db");
    let before = fs::read(&path).unwrap();
    let (graph, mut native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    native.provenance[0].derived_from = Some(serde_json::json!({"forged":true}));
    assert!(
        store
            .publish_native(&graph, &capture, &native, &leader, pin, &cancel)
            .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    native.provenance[0].derived_from = None;
    let mut forged_graph = graph.clone();
    forged_graph.nodes[0].name = "not-source-measured".into();
    assert!(
        store
            .publish_native(&forged_graph, &capture, &native, &leader, pin, &cancel)
            .unwrap_err()
            .to_string()
            .contains("graph declaration differs from measured native row")
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(
        store
            .publish_native(&graph, &capture, &native, &leader, baseline, &cancel)
            .unwrap_err()
            .to_string()
            .contains("revision conflict")
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    let mut value = serde_json::to_value(&native).unwrap();
    value["calls"][0]["target"] = serde_json::json!("unmeasured");
    assert!(serde_json::from_value::<trellis::native_evidence::Artifact>(value).is_err());
    assert_eq!(store.index_baseline().unwrap(), pin);
    assert_eq!(store.status().unwrap().revision, pin);
    assert_eq!(store.graph().unwrap(), graph);
    for document in &native.revision.documents {
        let (stored, bytes) = store.native_source_at(pin, &document.key).unwrap().unwrap();
        assert_eq!(stored, *document);
        assert_eq!(
            bytes,
            capture
                .files
                .iter()
                .find(|file| file.path == document.key.path)
                .unwrap()
                .text
                .as_bytes()
        );
    }
}

#[test]
#[cfg(feature = "test-support")]
fn empty_workspace_has_native_pair_without_document_rows() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let original = store.index_baseline().unwrap();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(&store, root.path(), &cancel, original, &leader).unwrap();
    assert_eq!(store.status().unwrap().revision, pin);
    let db = Connection::open(
        state
            .path()
            .join("cache/indexes")
            .join(
                trellis::store::topology::WorkspaceIdentity::discover(
                    Some(root.path()),
                    root.path(),
                )
                .unwrap()
                .root_key,
            )
            .join("index.db"),
    )
    .unwrap();
    for table in [
        "document_versions",
        "revision_documents",
        "graph_projections",
        "graph_nodes",
        "graph_calls",
        "graph_regions",
        "class_projections",
        "native_version_calls",
        "native_version_declarations",
    ] {
        let count: i64 = db
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "{table}");
    }
    assert_eq!(
        db.query_row("SELECT count(*) FROM native_revisions", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
#[cfg(feature = "test-support")]
fn metadata_status_and_selected_source_reads_do_not_conflate_other_documents() {
    let (state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let path = state
        .path()
        .join("cache/indexes")
        .join(
            trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
                .unwrap()
                .root_key,
        )
        .join("index.db");
    let db = Connection::open(path).unwrap();
    let source_set_id = format!("source-set:v1:{}", store.root_id());
    let unaffected = trellis::native_evidence::DocumentKey {
        source_set_id: source_set_id.clone(),
        language: "java".into(),
        path: "flow.java".into(),
    };
    let corrupted = trellis::native_evidence::DocumentKey {
        source_set_id,
        language: "rust".into(),
        path: "flow.rs".into(),
    };

    // Operational lock contention may fail before the selected query. It must
    // never authorize recovery or close a pre-existing Store clone.
    let operational_clone = store.clone();
    db.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let operational = store.native_source_at(pin, &corrupted).unwrap_err();
    let raw_lock = matches!(
        operational.downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::SqliteFailure(info, _))
            if matches!(
                info.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    );
    assert!(
        operational.to_string().starts_with("storage_busy:") || raw_lock,
        "{operational:#}"
    );
    db.execute_batch("ROLLBACK").unwrap();
    assert_eq!(operational_clone.status().unwrap().revision, pin);

    let mut bytes: Vec<u8> = db
        .query_row(
            "SELECT source_bytes FROM document_versions WHERE id=?1",
            [live_document_version(&db, "flow.rs")],
            |r| r.get(0),
        )
        .unwrap();
    bytes[0] ^= 1;
    db.execute(
        "UPDATE document_versions SET source_bytes=?1 WHERE id=?2",
        rusqlite::params![bytes, live_document_version(&db, "flow.rs")],
    )
    .unwrap();
    drop(db);
    // Status is a bounded metadata/pin check; selected reads validate the exact stored BLOB.
    assert_eq!(store.status().unwrap().revision, pin);
    assert!(store.native_source_at(pin, &unaffected).unwrap().is_some());
    let original_clone = store.clone();
    let selected = store.native_source_at(pin, &corrupted).unwrap_err();
    assert!(
        selected.to_string().contains("incompatible_index")
            && selected.to_string().contains(
                "incompatible_index: selected evidence decode failed: incompatible_index: source hash mismatch"
            ),
        "{selected:#}"
    );
    let closed = original_clone.source_at("flow.rs", Some(pin)).unwrap_err();
    assert_current_corruption(closed);

    let reopened = Store::open_for_tests(state.path(), root.path()).unwrap();
    assert_eq!(reopened.status().unwrap().revision, pin);
    assert_eq!(reopened.index_baseline().unwrap(), pin);
    let reopened_clone = reopened.clone();
    let selected = reopened.source_at("flow.rs", Some(pin)).unwrap_err();
    assert!(
        selected.to_string().contains("incompatible_index")
            && selected.to_string().contains("source hash mismatch"),
        "{selected:#}"
    );
    for closed in [
        reopened_clone.status().unwrap_err(),
        reopened.source_at("flow.rs", Some(pin)).unwrap_err(),
    ] {
        assert_current_corruption(closed);
    }
}

#[test]
#[cfg(feature = "test-support")]
fn status_many_documents_only_checks_paired_metadata_not_every_blob() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for n in 0..64 {
        fs::write(
            root.path().join(format!("source{n:02}.js")),
            format!(
                "function item{n}() {{ return {n}; }}\n{}",
                "// captured bytes\n".repeat(256)
            ),
        )
        .unwrap();
    }
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let db_path = state
        .path()
        .join("cache/indexes")
        .join(
            trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
                .unwrap()
                .root_key,
        )
        .join("index.db");
    let db = Connection::open(db_path).unwrap();
    let mut bytes: Vec<u8> = db
        .query_row(
            "SELECT source_bytes FROM document_versions WHERE id=?1",
            [live_document_version(&db, "source63.js")],
            |r| r.get(0),
        )
        .unwrap();
    bytes[0] ^= 1;
    db.execute(
        "UPDATE document_versions SET source_bytes=?1 WHERE id=?2",
        rusqlite::params![bytes, live_document_version(&db, "source63.js")],
    )
    .unwrap();
    drop(db);
    for _ in 0..64 {
        assert_eq!(store.status().unwrap().revision, pin);
    }
    let key = trellis::native_evidence::DocumentKey {
        source_set_id: format!("source-set:v1:{}", store.root_id()),
        language: "javascript".into(),
        path: "source63.js".into(),
    };
    assert!(store.native_source_at(pin, &key).is_err());
}

#[test]
#[cfg(feature = "test-support")]
fn index_from_another_native_producer_version_is_rebuilt_not_served() {
    for old_version in ["native-v2", "native-v3"] {
        let (state, root, store, cancel) = fixture();
        let leader = store.publication_for_tests().unwrap();
        let first = publish(
            &store,
            root.path(),
            &cancel,
            store.index_baseline().unwrap(),
            &leader,
        )
        .unwrap();
        drop(leader);
        drop(store);
        // Both withdrawn v2 and immediate v3 predecessor must be refused; v3 used occ:v2 IDs.
        let path = published_db(state.path(), root.path());
        let db = Connection::open(&path).unwrap();
        // Rewrite every producer-version FK in one deferred transaction. This
        // models a coherent index from a withdrawn producer, not a broken FK.
        db.pragma_update(None, "foreign_keys", "ON").unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert_eq!(
            db.execute("UPDATE native_producers SET version=?1", [old_version])
                .unwrap(),
            1
        );
        db.execute(
            "UPDATE native_producer_languages SET producer_version=?1",
            [old_version],
        )
        .unwrap();
        db.execute(
            "UPDATE native_producer_inputs SET producer_version=?1",
            [old_version],
        )
        .unwrap();
        db.execute(
            "UPDATE document_versions SET producer_version=?1",
            [old_version],
        )
        .unwrap();
        db.execute_batch("COMMIT").unwrap();
        let violations: i64 = db
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(violations, 0, "fixture must preserve producer FKs");
        drop(db);
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        assert_eq!(store.index_baseline().unwrap(), first);
        let leader = store.publication_for_tests().unwrap();
        for error in [
            store.status().map(|_| ()).unwrap_err(),
            store.graph().map(|_| ()).unwrap_err(),
            store
                .native_declarations_at(first, "javascript", "hello")
                .map(|_| ())
                .unwrap_err(),
        ] {
            assert!(
                error.to_string().contains("incompatible_index"),
                "{error:#}"
            );
        }
        let pin = publish(&store, root.path(), &cancel, first, &leader).unwrap();
        assert_ne!(pin.index_generation, first.index_generation);
        assert_eq!(store.status().unwrap().revision, pin);
        let db = Connection::open(&path).unwrap();
        let version: String = db
            .query_row("SELECT version FROM native_producers", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, "native-v4");
        let calls: Vec<String> = db
            .prepare("SELECT id FROM native_version_calls")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(!calls.is_empty() && calls.iter().all(|id| id.starts_with("occ:v2:")));
        let old_key = trellis::native_evidence::DocumentKey {
            source_set_id: format!("source-set:v1:{}", store.root_id()),
            language: "javascript".into(),
            path: "flow.js".into(),
        };
        assert_eq!(
            store
                .native_source_at(first, &old_key)
                .unwrap_err()
                .to_string(),
            "revision conflict: foreign or missing native pin",
            "{old_version} old pin must conflict"
        );
    }
}
#[cfg(feature = "test-support")]
fn published_db(state: &std::path::Path, root: &std::path::Path) -> std::path::PathBuf {
    state
        .join("cache/indexes")
        .join(
            trellis::store::topology::WorkspaceIdentity::discover(Some(root), root)
                .unwrap()
                .root_key,
        )
        .join("index.db")
}

#[test]
#[cfg(feature = "test-support")]
fn selected_typed_rows_reject_constraint_preserving_sql_forgery_at_same_pin() {
    let (state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let db = Connection::open(published_db(state.path(), root.path())).unwrap();
    let key = trellis::native_evidence::DocumentKey {
        source_set_id: format!("source-set:v1:{}", store.root_id()),
        language: "javascript".into(),
        path: "flow.js".into(),
    };
    let owner: String = db
        .query_row(
            "SELECT syntax_id FROM native_version_declarations WHERE version_id=?1 AND name='hello'",
            [live_document_version(&db, "flow.js")],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        !store
            .native_declarations_at(pin, "javascript", "hello")
            .unwrap()
            .is_empty()
    );
    assert!(!store.native_calls_at(pin, &owner).unwrap().is_empty());
    assert!(
        !store
            .native_control_regions_at(pin, &owner)
            .unwrap()
            .is_empty()
    );
    assert!(store.native_coverage_at(pin, &key).unwrap().is_some());
    // Every edit preserves row identity, pin, ordinals, ranges and FK constraints.
    let version = live_document_version(&db, "flow.js");
    let revision = live_revision(&db);
    let cases = [
        (
            "native_version_headers",
            "result_type",
            "syntax_id=?3",
            owner.as_str(),
            "Fabricated",
            "selected native declarations differ",
        ),
        (
            "native_version_calls",
            "spelling",
            "owner_syntax_id=?3",
            owner.as_str(),
            "fabricated",
            "selected native calls differ",
        ),
        (
            "native_version_control_regions",
            "arm",
            "owner_syntax_id=?3",
            owner.as_str(),
            "fabricated",
            "selected native regions differ",
        ),
        // Coverage selection belongs to the admitted revision manifest, not to the immutable native version.
        (
            "revision_documents",
            "coverage_selected",
            "path=?3",
            "flow.js",
            "0",
            "selected native coverage differs",
        ),
    ];
    for (table, column, predicate, row_key, forged, expected) in cases {
        let key_column = if table == "revision_documents" {
            "revision_id"
        } else {
            "version_id"
        };
        let parent = if table == "revision_documents" {
            &revision
        } else {
            &version
        };
        let select_predicate = predicate.replace("?3", "?2");
        let where_clause = format!("{key_column}=?1 AND {select_predicate}");
        let old: rusqlite::types::Value = db
            .query_row(
                &format!("SELECT {column} FROM {table} WHERE {where_clause} LIMIT 1"),
                rusqlite::params![parent, row_key],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            db.execute(
                &format!("UPDATE {table} SET {column}=?1 WHERE {key_column}=?2 AND {predicate}"),
                rusqlite::params![forged, parent, row_key],
            )
            .unwrap()
                > 0,
            "{table} fixture row"
        );
        let selected_store = Store::open_for_tests(state.path(), root.path()).unwrap();
        assert_eq!(
            selected_store.status().unwrap().revision,
            pin,
            "pin must not explain refusal"
        );
        let selected_clone = selected_store.clone();
        let error = match table {
            "native_version_headers" => selected_store
                .native_declarations_at(pin, "javascript", "hello")
                .map(|_| ()),
            "native_version_calls" => selected_store.native_calls_at(pin, &owner).map(|_| ()),
            "native_version_control_regions" => selected_store
                .native_control_regions_at(pin, &owner)
                .map(|_| ()),
            _ => selected_store.native_coverage_at(pin, &key).map(|_| ()),
        }
        .unwrap_err();
        assert!(
            error.to_string().contains("incompatible_index")
                && error.to_string().contains(expected),
            "{table}.{column}: {error:#}"
        );
        let java = trellis::native_evidence::DocumentKey {
            source_set_id: key.source_set_id.clone(),
            language: "java".into(),
            path: "flow.java".into(),
        };
        let closed = selected_clone.native_source_at(pin, &java).unwrap_err();
        assert_current_corruption(closed);
        db.execute(
            &format!("UPDATE {table} SET {column}=?1 WHERE {key_column}=?2 AND {predicate}"),
            rusqlite::params![old, parent, row_key],
        )
        .unwrap();
    }
}

#[test]
#[cfg(feature = "test-support")]
fn selected_sources_bind_paired_hash_bytes_and_graph_path_without_pin_change() {
    use sha2::{Digest, Sha256};
    let (state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let db = Connection::open(published_db(state.path(), root.path())).unwrap();
    let key = trellis::native_evidence::DocumentKey {
        source_set_id: format!("source-set:v1:{}", store.root_id()),
        language: "javascript".into(),
        path: "flow.js".into(),
    };
    let (original, bytes) = store.native_source_at(pin, &key).unwrap().unwrap();
    let mut forged = bytes.clone();
    let offset = forged.iter().position(|b| *b == b'o').unwrap();
    forged[offset] = b'x';
    let forged_hash = hex::encode(Sha256::digest(&forged));
    db.execute_batch("BEGIN").unwrap();
    db.execute(
        "UPDATE document_versions SET source_bytes=?1, content_hash=?2 WHERE id=?3",
        rusqlite::params![forged, forged_hash, live_document_version(&db, "flow.js")],
    )
    .unwrap();
    // Revision-scoped provenance is derived from the manifest and revision header.
    // Do not update a nonexistent native_provenance table.
    db.execute_batch("COMMIT").unwrap();
    assert_eq!(store.status().unwrap().revision, pin);
    let native_clone = store.clone();
    let native_error = store.native_source_at(pin, &key).unwrap_err();
    assert!(
        native_error.to_string().contains("incompatible_index")
            && native_error.to_string().contains("selected native"),
        "{native_error:#}"
    );
    let closed = native_clone.source_at("flow.java", Some(pin)).unwrap_err();
    assert_current_corruption(closed);

    let graph_store = Store::open_for_tests(state.path(), root.path()).unwrap();
    assert_eq!(graph_store.status().unwrap().revision, pin);
    let graph_clone = graph_store.clone();
    let graph_error = graph_store.source_at("flow.js", Some(pin)).unwrap_err();
    assert!(
        graph_error.to_string().contains("incompatible_index")
            && graph_error.to_string().contains("selected native"),
        "{graph_error:#}"
    );
    let closed = graph_clone.source_at("flow.java", Some(pin)).unwrap_err();
    assert_current_corruption(closed);

    db.execute_batch("BEGIN").unwrap();
    db.execute(
        "UPDATE document_versions SET source_bytes=?1, content_hash=?2 WHERE id=?3",
        rusqlite::params![
            bytes,
            original.content_hash,
            live_document_version(&db, "flow.js")
        ],
    )
    .unwrap();
    // The admitted manifest still names the same immutable document version.
    db.execute_batch("COMMIT").unwrap();
    // v8 has no duplicated files.payload. Forge the selected graph row's
    // serialized path without changing its projection, document version or pin.
    let projection = live_graph_projection(&db, "flow.js");
    assert_eq!(db.execute(
        "UPDATE graph_nodes SET payload=json_set(payload,'$.path','forged.js') WHERE projection_id=?1 AND path='flow.js' AND id=(SELECT id FROM graph_nodes WHERE projection_id=?1 AND path='flow.js' ORDER BY id LIMIT 1)",
        [&projection],
    ).unwrap(), 1);
    let payload_store = Store::open_for_tests(state.path(), root.path()).unwrap();
    assert_eq!(payload_store.status().unwrap().revision, pin);
    let payload_clone = payload_store.clone();
    let payload_error = payload_store.source_at("flow.js", Some(pin)).unwrap_err();
    assert!(
        payload_error.to_string().contains("incompatible_index")
            && !payload_error.to_string().contains("forged.js"),
        "{payload_error:#}"
    );
    let closed = payload_clone.source_at("flow.java", Some(pin)).unwrap_err();
    assert_current_corruption(closed);
}

#[test]
#[cfg(feature = "test-support")]
fn direct_native_ranges_regions_and_coverage_reject_selected_sql_edits() {
    let (state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let db = Connection::open(published_db(state.path(), root.path())).unwrap();
    let owner: String = db
        .query_row(
            "SELECT syntax_id FROM native_version_declarations WHERE version_id=?1 AND name='hello'",
            [live_document_version(&db, "flow.js")],
            |r| r.get(0),
        )
        .unwrap();
    let key = trellis::native_evidence::DocumentKey {
        source_set_id: format!("source-set:v1:{}", store.root_id()),
        language: "javascript".into(),
        path: "flow.js".into(),
    };
    let original_end: i64 = db
        .query_row(
            "SELECT end_byte FROM native_version_declarations WHERE version_id=?1 AND syntax_id=?2",
            rusqlite::params![live_document_version(&db, "flow.js"), owner],
            |r| r.get(0),
        )
        .unwrap();
    db.execute(
        "UPDATE native_version_declarations SET end_byte=end_byte+1 WHERE version_id=?1 AND syntax_id=?2",
        rusqlite::params![live_document_version(&db, "flow.js"), owner],
    )
    .unwrap();
    let declarations_clone = store.clone();
    let error = store
        .native_declarations_at(pin, "javascript", "hello")
        .unwrap_err();
    assert!(
        error.to_string().contains("incompatible_index")
            && error
                .to_string()
                .contains("selected native declarations differ"),
        "{error:#}"
    );
    assert_current_corruption(declarations_clone.status().unwrap_err());
    db.execute(
        "UPDATE native_version_declarations SET end_byte=?1 WHERE version_id=?2 AND syntax_id=?3",
        rusqlite::params![original_end, live_document_version(&db, "flow.js"), owner],
    )
    .unwrap();
    let original_kind: String = db
        .query_row(
            "SELECT kind FROM native_version_control_regions WHERE version_id=?1 ORDER BY id LIMIT 1",
            [live_document_version(&db, "flow.js")],
            |r| r.get(0),
        )
        .unwrap();
    db.execute(
        "UPDATE native_version_control_regions SET kind='while_statement' WHERE version_id=?1",
        [live_document_version(&db, "flow.js")],
    )
    .unwrap();
    let regions_store = Store::open_for_tests(state.path(), root.path()).unwrap();
    assert_eq!(regions_store.status().unwrap().revision, pin);
    let regions_clone = regions_store.clone();
    let error = regions_store
        .native_control_regions_at(pin, &owner)
        .unwrap_err();
    assert!(
        error.to_string().contains("incompatible_index")
            && error.to_string().contains("selected native regions differ"),
        "{error:#}"
    );
    assert_current_corruption(regions_clone.status().unwrap_err());
    db.execute(
        "UPDATE native_version_control_regions SET kind=?1 WHERE version_id=?2",
        rusqlite::params![original_kind, live_document_version(&db, "flow.js")],
    )
    .unwrap();
    let original: (String, Option<String>) = db
        .query_row(
            "SELECT coverage_state,coverage_diagnostic FROM revision_documents WHERE revision_id=?1 AND path='flow.js'",
            [live_revision(&db)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    db.execute("UPDATE revision_documents SET coverage_state='partial',coverage_diagnostic='fabricated' WHERE revision_id=?1 AND path='flow.js'", [live_revision(&db)]).unwrap();
    let coverage_store = Store::open_for_tests(state.path(), root.path()).unwrap();
    assert_eq!(coverage_store.status().unwrap().revision, pin);
    let coverage_clone = coverage_store.clone();
    let error = coverage_store.native_coverage_at(pin, &key).unwrap_err();
    assert!(
        error.to_string().contains("incompatible_index")
            && error
                .to_string()
                .contains("selected native coverage differs"),
        "{error:#}"
    );
    let closed = coverage_clone
        .native_coverage_at(
            pin,
            &trellis::native_evidence::DocumentKey {
                source_set_id: key.source_set_id,
                language: "java".into(),
                path: "flow.java".into(),
            },
        )
        .unwrap_err();
    assert_current_corruption(closed);
    db.execute(
        "UPDATE revision_documents SET coverage_state=?1,coverage_diagnostic=?2 WHERE revision_id=?3 AND path='flow.js'",
        rusqlite::params![original.0, original.1, live_revision(&db)],
    )
    .unwrap();
}

#[test]
#[cfg(feature = "test-support")]
fn pinned_graph_call_payload_must_match_native_before_query_and_sequence() {
    let (state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let db = Connection::open(published_db(state.path(), root.path())).unwrap();
    let owner: String = db
        .query_row(
            "SELECT syntax_id FROM native_version_declarations WHERE version_id=?1 AND name='hello'",
            [live_document_version(&db, "flow.js")],
            |r| r.get(0),
        )
        .unwrap();
    let query: trellis::model::ViewQuery =
        serde_json::from_value(serde_json::json!({"seed":owner})).unwrap();
    assert!(!store.query_view(&query).unwrap().unwrap().calls.is_empty());
    assert!(store.sequence_at(&owner, pin, true).unwrap().is_some());
    let call_id: String = db
        .query_row(
            "SELECT id FROM graph_calls WHERE projection_id=?1 ORDER BY id LIMIT 1",
            [live_graph_projection(&db, "flow.js")],
            |r| r.get(0),
        )
        .unwrap();
    let original: String = db
        .query_row(
            "SELECT payload FROM graph_calls WHERE projection_id=?1 AND id=?2",
            rusqlite::params![live_graph_projection(&db, "flow.js"), call_id],
            |r| r.get(0),
        )
        .unwrap();
    let original_value: serde_json::Value = serde_json::from_str(&original).unwrap();
    assert!(
        original_value["calleeText"].is_string(),
        "fixture must contain measured calleeText"
    );
    let mut forged = original_value.clone();
    forged["calleeText"] = serde_json::json!("fabricatedCallee");
    db.execute(
        "UPDATE graph_calls SET payload=?1 WHERE projection_id=?2 AND id=?3",
        rusqlite::params![
            forged.to_string(),
            live_graph_projection(&db, "flow.js"),
            call_id
        ],
    )
    .unwrap();
    assert_eq!(store.status().unwrap().revision, pin);
    let query_clone = store.clone();
    let query_error = store.query_view(&query).unwrap_err();
    assert!(
        query_error.to_string().contains("native_evidence_required")
            && query_error
                .to_string()
                .contains("graph call differs from measured native row"),
        "{query_error:#}"
    );
    assert_current_corruption(query_clone.status().unwrap_err());

    let sequence_store = Store::open_for_tests(state.path(), root.path()).unwrap();
    assert_eq!(sequence_store.status().unwrap().revision, pin);
    let sequence_clone = sequence_store.clone();
    let sequence_error = sequence_store.sequence_at(&owner, pin, true).unwrap_err();
    assert!(
        sequence_error
            .to_string()
            .contains("native_evidence_required")
            && sequence_error
                .to_string()
                .contains("graph call differs from measured native row"),
        "{sequence_error:#}"
    );
    let java: String = db
        .query_row(
            "SELECT syntax_id FROM native_version_declarations WHERE version_id=?1 AND name='go'",
            [live_document_version(&db, "flow.java")],
            |r| r.get(0),
        )
        .unwrap();
    let unaffected: trellis::model::ViewQuery =
        serde_json::from_value(serde_json::json!({"seed":java})).unwrap();
    for closed in [
        sequence_clone.query_view(&unaffected).unwrap_err(),
        sequence_store.sequence_at(&java, pin, true).unwrap_err(),
    ] {
        assert_current_corruption(closed);
    }
}

#[test]
#[cfg(feature = "test-support")]
fn extra_fk_valid_native_declaration_with_new_lookup_key_cannot_escape_source_witness() {
    let (state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let db = Connection::open(published_db(state.path(), root.path())).unwrap();
    db.pragma_update(None, "foreign_keys", "ON").unwrap();
    let original: String = db
        .query_row(
            "SELECT syntax_id FROM native_version_declarations WHERE version_id=?1 AND name='hello'",
            [live_document_version(&db, "flow.js")],
            |r| r.get(0),
        )
        .unwrap();
    let fake = "sid:v1:ffffffffffffffffffffffffffffffff";
    let version = live_document_version(&db, "flow.js");
    db.execute("INSERT INTO native_version_declarations(version_id,syntax_id,owner_syntax_id,kind,name,lookup_key,key_signature_present,key_type_parameter_count,key_variadic,key_ordinal,start_byte,end_byte,name_start,name_end)
        SELECT version_id,?1,owner_syntax_id,kind,'phantom','phantom',key_signature_present,key_type_parameter_count,key_variadic,key_ordinal,start_byte,end_byte,name_start,name_end
        FROM native_version_declarations WHERE version_id=?2 AND syntax_id=?3",
        rusqlite::params![fake,version,original]).unwrap();
    db.execute(
        "INSERT INTO native_version_headers(version_id,syntax_id,kind,name,result_type)
        SELECT version_id,?1,kind,'phantom',result_type FROM native_version_headers WHERE version_id=?2 AND syntax_id=?3",
        rusqlite::params![fake, version, original],
    )
    .unwrap();
    let fk_count: i64 = db
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(fk_count, 0, "forgery must retain valid SQL FKs");
    assert_eq!(store.status().unwrap().revision, pin);
    let selected_clone = store.clone();
    let error = store
        .native_declarations_at(pin, "javascript", "phantom")
        .unwrap_err();
    assert!(
        error.to_string().contains("incompatible_index")
            && error
                .to_string()
                .contains("incompatible_index: selected evidence decode failed: incompatible_index: selected native declarations differ from source"),
        "{error:#}"
    );
    let closed = selected_clone
        .native_declarations_at(pin, "java", "go")
        .unwrap_err();
    assert_current_corruption(closed);
}

#[test]
#[cfg(feature = "test-support")]
fn amplified_selected_ancillary_rows_refuse_before_typed_materialization() {
    let (state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let db = Connection::open(published_db(state.path(), root.path())).unwrap();
    db.pragma_update(None, "foreign_keys", "ON").unwrap();
    let owner: String = db
        .query_row(
            "SELECT syntax_id FROM native_version_declarations WHERE version_id=?1 AND name='hello'",
            [live_document_version(&db, "flow.js")],
            |r| r.get(0),
        )
        .unwrap();
    db.execute(
        "WITH RECURSIVE n(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM n WHERE v<1500)
        INSERT INTO native_version_header_items(version_id,syntax_id,item_kind,ordinal,value)
        SELECT ?1,?2,'modifier',10000+v,'amplified' FROM n",
        rusqlite::params![live_document_version(&db, "flow.js"), owner],
    )
    .unwrap();
    let fk_count: i64 = db
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(fk_count, 0, "amplification must retain valid SQL FKs");
    assert_eq!(store.status().unwrap().revision, pin);
    let selected_clone = store.clone();
    let error = store
        .native_declarations_at(pin, "javascript", "hello")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("selected evidence row budget exceeded"),
        "{error:#}"
    );
    let closed = selected_clone.status().unwrap_err();
    assert_current_corruption(closed);
    let closed = store.native_declarations_at(pin, "java", "go").unwrap_err();
    assert_current_corruption(closed);
}

#[test]
#[cfg(feature = "test-support")]
fn single_oversized_fk_valid_native_child_text_refuses_before_materialization() {
    let (state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let db = Connection::open(published_db(state.path(), root.path())).unwrap();
    db.pragma_update(None, "foreign_keys", "ON").unwrap();
    let owner: String = db
        .query_row(
            "SELECT syntax_id FROM native_version_declarations WHERE version_id=?1 AND name='hello'",
            [live_document_version(&db, "flow.js")],
            |r| r.get(0),
        )
        .unwrap();
    db.execute("INSERT INTO native_version_header_items(version_id,syntax_id,item_kind,ordinal,value) VALUES(?1,?2,'modifier',10000,?3)",
        rusqlite::params![live_document_version(&db, "flow.js"),owner,"X".repeat(64*1024)]).unwrap();
    let fk_count: i64 = db
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(fk_count, 0, "single oversized child remains FK-valid");
    assert_eq!(store.status().unwrap().revision, pin);
    let selected_clone = store.clone();
    let error = store
        .native_declarations_at(pin, "javascript", "hello")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("selected evidence byte budget exceeded"),
        "{error:#}"
    );
    let closed = selected_clone.status().unwrap_err();
    assert_current_corruption(closed);
    let closed = store.native_declarations_at(pin, "java", "go").unwrap_err();
    assert_current_corruption(closed);
}

#[test]
#[cfg(feature = "test-support")]
fn selected_class_payload_oversize_refuses_before_json_decode() {
    let (state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let db = Connection::open(published_db(state.path(), root.path())).unwrap();
    let id: String = db
        .query_row(
            "SELECT id FROM classes WHERE projection_id=?1 ORDER BY id LIMIT 1",
            [live_class_projection(&db, "flow.java")],
            |r| r.get(0),
        )
        .unwrap();
    db.execute(
        "UPDATE classes SET payload=?1 WHERE projection_id=?2 AND id=?3",
        rusqlite::params![
            "not-valid-class-json".repeat(8000),
            live_class_projection(&db, "flow.java"),
            id
        ],
    )
    .unwrap();
    let selected_clone = store.clone();
    let error = store.symbol_at(&id, Some(pin)).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("selected evidence byte budget exceeded"),
        "{error:#}"
    );
    let rust_id: String = db
        .query_row(
            "SELECT syntax_id FROM native_version_declarations WHERE version_id=?1 AND name='main'",
            [live_document_version(&db, "flow.rs")],
            |r| r.get(0),
        )
        .unwrap();
    let closed = selected_clone.symbol_at(&rust_id, Some(pin)).unwrap_err();
    assert_current_corruption(closed);
}

#[test]
#[cfg(feature = "test-support")]
fn selected_native_text_total_budget_is_not_count_times_single_row_limit() {
    let (state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let db = Connection::open(published_db(state.path(), root.path())).unwrap();
    db.pragma_update(None, "foreign_keys", "ON").unwrap();
    let owner: String = db
        .query_row(
            "SELECT syntax_id FROM native_version_declarations WHERE version_id=?1 AND name='hello'",
            [live_document_version(&db, "flow.js")],
            |r| r.get(0),
        )
        .unwrap();
    db.execute(
        "WITH RECURSIVE n(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM n WHERE v<40)
        INSERT INTO native_version_header_items(version_id,syntax_id,item_kind,ordinal,value)
        SELECT ?1,?2,'modifier',30000+v,?3 FROM n",
        rusqlite::params![
            live_document_version(&db, "flow.js"),
            owner,
            "Y".repeat(8 * 1024)
        ],
    )
    .unwrap();
    let fk_count: i64 = db
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(fk_count, 0);
    assert_eq!(store.status().unwrap().revision, pin);
    let selected_clone = store.clone();
    let error = store
        .native_declarations_at(pin, "javascript", "hello")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("selected evidence byte budget exceeded"),
        "{error:#}"
    );
    let closed = selected_clone
        .native_declarations_at(pin, "java", "go")
        .unwrap_err();
    assert_current_corruption(closed);
}

#[test]
#[cfg(feature = "test-support")]
fn graph_node_call_and_region_payloads_each_have_predecode_byte_envelope() {
    let (state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let db = Connection::open(published_db(state.path(), root.path())).unwrap();
    for table in ["graph_nodes", "graph_calls", "graph_regions"] {
        // These three fixed SQL identifiers are the selected graph row families.
        let projection = live_graph_projection(&db, "flow.js");
        let query =
            format!("SELECT id,payload FROM {table} WHERE projection_id=?1 ORDER BY id LIMIT 1");
        let (id, original): (String, String) = db
            .query_row(&query, [&projection], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        db.execute(
            &format!("UPDATE {table} SET payload=?1 WHERE projection_id=?2 AND id=?3"),
            rusqlite::params!["invalid-json".repeat(8000), projection, id],
        )
        .unwrap();
        let selected_store = Store::open_for_tests(state.path(), root.path()).unwrap();
        assert_eq!(selected_store.status().unwrap().revision, pin);
        let selected_clone = selected_store.clone();
        let error = selected_store
            .native_declarations_at(pin, "javascript", "hello")
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("selected graph row byte budget exceeded"),
            "{table}: {error:#}"
        );
        let closed = selected_clone
            .native_declarations_at(pin, "java", "go")
            .unwrap_err();
        assert_current_corruption(closed);
        db.execute(
            &format!("UPDATE {table} SET payload=?1 WHERE projection_id=?2 AND id=?3"),
            rusqlite::params![original, projection, id],
        )
        .unwrap();
    }
}

#[test]
#[cfg(feature = "test-support")]
fn selected_graph_rows_share_a_source_scoped_aggregate_byte_envelope() {
    let (state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let db = Connection::open(published_db(state.path(), root.path())).unwrap();
    db.pragma_update(None, "foreign_keys", "ON").unwrap();
    db.execute(
        "WITH RECURSIVE n(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM n WHERE v<40)
        INSERT INTO graph_nodes(projection_id,id,name,path,payload)
        SELECT ?1,printf('forged-graph-%d',v),'forged','flow.js',?2 FROM n",
        rusqlite::params![
            live_graph_projection(&db, "flow.js"),
            "invalid-json".repeat(800)
        ],
    )
    .unwrap();
    let fk_count: i64 = db
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(fk_count, 0);
    let selected_clone = store.clone();
    let error = store
        .native_declarations_at(pin, "javascript", "hello")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("selected graph row byte budget exceeded"),
        "{error:#}"
    );
    let closed = selected_clone
        .native_declarations_at(pin, "java", "go")
        .unwrap_err();
    assert_current_corruption(closed);
}

#[cfg(feature = "test-support")]
fn assert_retained_native_family_parity(
    store: &Store,
    pin: IndexPin,
    measured: &trellis::native_evidence::Artifact,
) {
    use std::collections::BTreeSet;

    assert!(
        measured
            .calls
            .iter()
            .any(|call| call.document.path == "flow.js")
    );
    assert!(
        measured
            .control_regions
            .iter()
            .any(|region| region.document.path == "flow.java")
    );
    let lookups = measured
        .declarations
        .iter()
        .filter_map(|declaration| {
            declaration
                .lookup_key
                .as_ref()
                .map(|key| (declaration.document.language.clone(), key.clone()))
        })
        .collect::<BTreeSet<_>>();
    for (language, lookup) in lookups {
        let mut actual = store
            .native_declarations_at(pin, &language, &lookup)
            .unwrap();
        let mut expected = measured
            .declarations
            .iter()
            .filter(|declaration| {
                declaration.document.language == language
                    && declaration.lookup_key.as_deref() == Some(lookup.as_str())
            })
            .cloned()
            .collect::<Vec<_>>();
        actual.sort_by(|left, right| left.syntax_id.cmp(&right.syntax_id));
        expected.sort_by(|left, right| left.syntax_id.cmp(&right.syntax_id));
        // Whole DTO equality includes the requested document revision and provenance.
        assert_eq!(actual, expected, "{pin:?}: declaration {language}/{lookup}");
    }
    for declaration in &measured.declarations {
        let mut actual_calls = store.native_calls_at(pin, &declaration.syntax_id).unwrap();
        let mut expected_calls = measured
            .calls
            .iter()
            .filter(|call| call.owner_syntax_id == declaration.syntax_id)
            .cloned()
            .collect::<Vec<_>>();
        actual_calls.sort_by(|left, right| left.id.cmp(&right.id));
        expected_calls.sort_by(|left, right| left.id.cmp(&right.id));
        assert_eq!(
            actual_calls, expected_calls,
            "{pin:?}: calls for {}",
            declaration.syntax_id
        );

        let mut actual_regions = store
            .native_control_regions_at(pin, &declaration.syntax_id)
            .unwrap();
        let mut expected_regions = measured
            .control_regions
            .iter()
            .filter(|region| region.owner_syntax_id == declaration.syntax_id)
            .cloned()
            .collect::<Vec<_>>();
        actual_regions.sort_by(|left, right| left.id.cmp(&right.id));
        expected_regions.sort_by(|left, right| left.id.cmp(&right.id));
        assert_eq!(
            actual_regions, expected_regions,
            "{pin:?}: control regions for {}",
            declaration.syntax_id
        );
    }
}

#[test]
#[cfg(feature = "test-support")]
fn retained_full_rewrite_pins_survive_edits_delete_release_and_reference_safe_gc() {
    use trellis::native_evidence::DocumentKey;
    let (state, root, store, cancel) = fixture();
    store.set_retention_clock_for_tests(1_000, 0);
    // Independent full-rewrite measurements, separate from publish()'s capture.
    let (_, measured_r1, _) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let leader = store.publication_for_tests().unwrap();
    let r1 = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let key = DocumentKey {
        source_set_id: format!("source-set:v1:{}", store.root_id()),
        language: "javascript".into(),
        path: "flow.js".into(),
    };
    let source1 = store.native_source_at(r1, &key).unwrap().unwrap();
    let coverage1 = store.native_coverage_at(r1, &key).unwrap().unwrap();
    let graph1 = store.graph().unwrap();
    fs::write(
        root.path().join("flow.js"),
        "function changed() { measured(); }\n",
    )
    .unwrap();
    let (_, measured_r2, _) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    store.set_retention_clock_for_tests(1_001, 1);
    let r2 = publish(&store, root.path(), &cancel, r1, &leader).unwrap();
    let source2 = store.native_source_at(r2, &key).unwrap().unwrap();
    assert_ne!(source1.1, source2.1);
    fs::remove_file(root.path().join("flow.js")).unwrap();
    store.set_retention_clock_for_tests(1_002, 2);
    let r3 = publish(&store, root.path(), &cancel, r2, &leader).unwrap();
    assert!(store.native_source_at(r3, &key).unwrap().is_none());
    assert_eq!(store.native_source_at(r1, &key).unwrap().unwrap(), source1);
    assert_eq!(
        store.native_coverage_at(r1, &key).unwrap().unwrap(),
        coverage1
    );
    assert_eq!(
        store
            .source_at("flow.js", Some(r1))
            .unwrap()
            .unwrap()
            .1
            .text
            .as_bytes(),
        source1.1
    );
    assert_eq!(store.graph_at(Some(r1)).unwrap(), graph1);
    // After both the edit and delete, both old pins must select their own typed
    // native evidence, not the head's or each other's projection.
    assert_retained_native_family_parity(&store, r1, &measured_r1);
    assert_retained_native_family_parity(&store, r2, &measured_r2);
    store.set_retention_clock_for_tests(1_901, 901);
    store.release_revision(r1, &leader).unwrap();
    assert!(
        store
            .native_source_at(r1, &key)
            .unwrap_err()
            .to_string()
            .contains("pin_expired")
    );
    // Release is durable before collection: lose the owner/process now, then
    // reopen the index while r1's unreachable rows still await fenced GC.
    drop(leader);
    drop(store);
    let cold = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cold_leader = cold.publication_for_tests().unwrap();
    // The validated prior head and surviving retained pin remain readable
    // before takeover publication; the released exact pin remains expired.
    assert_eq!(cold.status().unwrap().revision, r3);
    assert_eq!(cold.native_source_at(r2, &key).unwrap().unwrap(), source2);
    assert!(cold.native_source_at(r3, &key).unwrap().is_none());
    assert!(
        cold.native_source_at(r1, &key)
            .unwrap_err()
            .to_string()
            .contains("pin_expired")
    );

    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let db_path = state
        .path()
        .join("cache/indexes")
        .join(identity.root_key)
        .join("index.db");
    let r2_id = format!("pin:v1:{}:{}", r2.index_generation, r2.index_revision);
    let db = Connection::open(&db_path).unwrap();
    let (version, graph_projection, class_projection): (String, String, String) = db
        .query_row(
            "SELECT document_version_id,graph_projection_id,class_projection_id
             FROM revision_documents WHERE revision_id=?1 AND path='flow.java'",
            [&r2_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    // A surviving revision owns all three materialized record families before
    // GC. Recheck these exact IDs after GC rather than only checking FK health.
    let retained_rows = [
        ("document_versions", version),
        ("graph_projections", graph_projection),
        ("class_projections", class_projection),
    ];
    for (table, id) in &retained_rows {
        let count: i64 = db
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE id=?1"),
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "cold reopen lost retained {table}/{id} before GC");
    }
    drop(db);

    let r4 = publish(&cold, root.path(), &cancel, r3, &cold_leader).unwrap();
    assert_eq!(r4.index_generation, r3.index_generation);
    assert_eq!(r4.index_revision, r3.index_revision + 1);
    assert_eq!(cold.status().unwrap().revision, r4);
    assert!(
        cold.native_source_at(r1, &key)
            .unwrap_err()
            .to_string()
            .contains("pin_expired")
    );
    assert_eq!(cold.native_source_at(r2, &key).unwrap().unwrap(), source2);
    assert!(cold.native_source_at(r3, &key).unwrap().is_none());
    assert_retained_native_family_parity(&cold, r2, &measured_r2);

    cold.collect_unreferenced(&cold_leader).unwrap();
    assert_eq!(cold.native_source_at(r2, &key).unwrap().unwrap(), source2);
    assert!(cold.native_source_at(r3, &key).unwrap().is_none());
    assert_retained_native_family_parity(&cold, r2, &measured_r2);
    let db = Connection::open(&db_path).unwrap();
    for (table, id) in &retained_rows {
        let count: i64 = db
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE id=?1"),
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "fenced GC lost retained {table}/{id}");
    }
    let missing: i64 = db
        .query_row(
            "SELECT count(*) FROM document_versions v WHERE NOT EXISTS
        (SELECT 1 FROM revision_documents m WHERE m.document_version_id=v.id)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(missing, 0);
    let fk: i64 = db
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(fk, 0);
    let tombstone: String = db
        .query_row(
            "SELECT payload FROM revision_capture_inputs
        WHERE revision_id=?1 AND input_key='__released:v1'",
            [format!(
                "pin:v1:{}:{}",
                r1.index_generation, r1.index_revision
            )],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tombstone, "released:v1");
    assert_eq!(cold.status().unwrap().revision, r4);
}

#[test]
#[cfg(feature = "test-support")]
fn malformed_and_foreign_release_pins_do_not_write_and_header_gaps_remain_corruption() {
    let (state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let r1 = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    fs::write(root.path().join("flow.js"), "function next() { next(); }\n").unwrap();
    let r2 = publish(&store, root.path(), &cancel, r1, &leader).unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let path = state
        .path()
        .join("cache/indexes")
        .join(identity.root_key)
        .join("index.db");
    let before = fs::read(&path).unwrap();
    for pin in [
        r2,
        trellis::model::IndexPin {
            index_generation: uuid::Uuid::new_v4(),
            index_revision: r1.index_revision,
        },
        trellis::model::IndexPin {
            index_generation: r1.index_generation,
            index_revision: r2.index_revision + 1,
        },
    ] {
        assert!(
            store
                .release_revision(pin, &leader)
                .unwrap_err()
                .to_string()
                .contains("revision conflict")
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "invalid release touched the database"
        );
    }
    drop(leader);
    drop(store);
    let db = Connection::open(&path).unwrap();
    db.pragma_update(None, "foreign_keys", "OFF").unwrap();
    db.execute(
        "DELETE FROM revision_documents WHERE revision_id=?1",
        [format!(
            "pin:v1:{}:{}",
            r1.index_generation, r1.index_revision
        )],
    )
    .unwrap();
    db.execute(
        "DELETE FROM revision_capture_inputs WHERE revision_id=?1",
        [format!(
            "pin:v1:{}:{}",
            r1.index_generation, r1.index_revision
        )],
    )
    .unwrap();
    db.execute(
        "DELETE FROM native_revisions WHERE published_index_revision=?1",
        [r1.index_revision as i64],
    )
    .unwrap();
    drop(db);
    let malformed = fs::read(&path).unwrap();
    let reopened = Store::open_for_tests(state.path(), root.path()).unwrap();
    assert!(
        reopened
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        malformed,
        "malformed gap was rewritten"
    );
}

#[test]
#[cfg(feature = "test-support")]
fn retained_graph_header_preserves_distinct_stats_and_diagnostics_and_matches_live_head_bytes() {
    let (state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let initial = store.index_baseline().unwrap();
    let publish_measured = |expected| {
        let (graph, native, capture) = index_workspace_bundle(
            &IndexOptions::new(root.path().to_owned()),
            store.root_id(),
            &cancel,
            |_| {},
        )
        .unwrap();
        store
            .publish_native(&graph, &capture, &native, &leader, expected, &cancel)
            .unwrap()
    };
    let r1 = publish_measured(initial);
    let graph1 = store.graph_at(Some(r1)).unwrap();
    // A genuinely incomplete source yields native partial coverage, from which
    // the indexer derives the persisted diagnostic; never invent a graph row.
    fs::write(root.path().join("incomplete.js"), "function broken( {\n").unwrap();
    let r2 = publish_measured(r1);
    let graph2 = store.graph_at(Some(r2)).unwrap();
    assert_ne!(graph1.stats, graph2.stats);
    assert!(
        graph2
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.path.as_deref() == Some("incomplete.js"))
    );
    assert_ne!(graph1.diagnostics, graph2.diagnostics);
    assert_eq!(store.graph_at(Some(r1)).unwrap(), graph1);
    assert_eq!(store.graph_at(None).unwrap(), graph2);
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let db = Connection::open(
        state
            .path()
            .join("cache/indexes")
            .join(identity.root_key)
            .join("index.db"),
    )
    .unwrap();
    let (stats, diagnostics, head_stats, head_diagnostics): (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) = db.query_row(
        "SELECT CAST(r.graph_stats AS BLOB),CAST(r.graph_diagnostics AS BLOB),CAST(m.stats AS BLOB),CAST(m.diagnostics AS BLOB)
         FROM native_revisions r JOIN index_metadata m ON r.id='pin:v1:'||m.index_generation||':'||m.index_revision",
        [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
    ).unwrap();
    assert_eq!(stats, head_stats);
    assert_eq!(diagnostics, head_diagnostics);
}

#[test]
#[cfg(feature = "test-support")]
fn invalid_retained_header_json_refuses_old_pin_without_mutating_sqlite_or_sidecars() {
    let (state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let r1 = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    fs::write(root.path().join("flow.js"), "function changed() {}\n").unwrap();
    let r2 = publish(&store, root.path(), &cancel, r1, &leader).unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let path = state
        .path()
        .join("cache/indexes")
        .join(identity.root_key)
        .join("index.db");
    drop(store);
    let db = Connection::open(&path).unwrap();
    db.execute(
        "UPDATE native_revisions SET graph_diagnostics='[42]' WHERE published_index_revision=?1",
        [r1.index_revision as i64],
    )
    .unwrap();
    drop(db);
    let bytes = fs::read(&path).unwrap();
    let sidecars: Vec<_> = ["-wal", "-shm", "-journal"]
        .iter()
        .map(|s| path.with_file_name(format!("index.db{s}")))
        .filter(|p| p.exists())
        .collect();
    let reopened = Store::open_for_tests(state.path(), root.path()).unwrap();
    assert_eq!(reopened.status().unwrap().revision, r2);
    assert!(
        reopened
            .graph_at(Some(r1))
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(
        sidecars,
        ["-wal", "-shm", "-journal"]
            .iter()
            .map(|s| path.with_file_name(format!("index.db{s}")))
            .filter(|p| p.exists())
            .collect::<Vec<_>>()
    );
}

#[test]
#[cfg(feature = "test-support")]
fn predecessor_stamps_cover_full_and_unchanged_publications() {
    use std::sync::Mutex;
    use trellis::index_coordinator::IndexJobCoordinator;
    let (state, root, store, cancel) = fixture();
    let options = IndexOptions::new(root.path().to_owned());
    let session = store.writer_session_for_tests().unwrap();
    let modes = Mutex::new(Vec::new());
    let run = |expected, modes: &Mutex<Vec<String>>| {
        IndexJobCoordinator::prepare_with_session(&store, expected, session.clone())
            .unwrap()
            .run_serving(&options, &cancel, |p| {
                if p.phase.starts_with("mode:") {
                    modes.lock().unwrap().push(p.phase);
                }
            })
            .unwrap()
    };
    let r1 = run(None, &modes);
    let r2 = run(Some(r1), &modes);
    assert!(
        modes
            .lock()
            .unwrap()
            .iter()
            .any(|mode| mode == "mode:unchanged")
    );
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let path = state
        .path()
        .join("cache/indexes")
        .join(identity.root_key)
        .join("index.db");
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM native_revision_supersessions WHERE revision_id=?1",
            [format!(
                "pin:v1:{}:{}",
                r1.index_generation, r1.index_revision
            )],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        1,
        "unchanged publication must stamp predecessor in same commit"
    );
    drop(db);
    fs::write(
        root.path().join("flow.js"),
        "function hello(x) { if (x) { obj.run(); } }\n",
    )
    .unwrap();
    let r3 = run(Some(r2), &modes);
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let db = Connection::open(
        state
            .path()
            .join("cache/indexes")
            .join(identity.root_key)
            .join("index.db"),
    )
    .unwrap();
    let rows: Vec<(String, i64)> = db.prepare("SELECT revision_id,superseded_at FROM native_revision_supersessions ORDER BY revision_id")
        .unwrap().query_map([], |row| Ok((row.get(0)?, row.get(1)?))).unwrap()
        .collect::<rusqlite::Result<_>>().unwrap();
    assert_eq!(rows.len(), 2);
    for (pin, (key, second)) in [r1, r2].iter().zip(&rows) {
        assert_eq!(
            key,
            &format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision)
        );
        assert!(*second > 0 && *second <= 9_007_199_254_740_991);
        assert!(
            store.graph_at(Some(*pin)).is_ok(),
            "stamps must preserve old pinned graph"
        );
    }
    let head: i64 = db
        .query_row(
            "SELECT count(*) FROM native_revision_supersessions WHERE revision_id=?1",
            [format!(
                "pin:v1:{}:{}",
                r3.index_generation, r3.index_revision
            )],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(head, 0);
    assert_eq!(store.status().unwrap().revision, r3);
}

#[test]
#[cfg(feature = "test-support")]
fn prior_v8_additive_maintenance_keeps_inode_generation_and_unfinished_fifo() {
    use std::os::unix::fs::MetadataExt;
    let (state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let old = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    // First queue publication requires index-use EX, so release the test
    // writer guard before accepting the retained request.
    drop(leader);
    let pending = store
        .enqueue_request(&IndexOptions::new(root.path().to_owned()), None)
        .unwrap();
    drop(store);
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let path = state
        .path()
        .join("cache/indexes")
        .join(identity.root_key)
        .join("index.db");
    remove_maintenance_extension_for_v8_test(&path);
    let inode = fs::metadata(&path).unwrap().ino();
    let reopened = Store::open_for_tests(state.path(), root.path()).unwrap();
    let leader = reopened.publication_for_tests().unwrap();
    assert_eq!(
        reopened.maintenance_oldest_due_age_secs(&leader).unwrap(),
        None
    );
    fs::write(
        root.path().join("main.js"),
        "function changed() { return 1; }\n",
    )
    .unwrap();
    let next = publish(&reopened, root.path(), &cancel, old, &leader).unwrap();
    assert_eq!(next.index_generation, old.index_generation);
    assert_eq!(next.index_revision, old.index_revision + 1);
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    let db = Connection::open(&path).unwrap();
    let installed: i64 = db.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='native_revision_release_debt'", [], |r|r.get(0)).unwrap();
    assert_eq!(installed, 1);
    assert!(reopened.graph_at(Some(old)).is_ok());
    let preserved = reopened.request_by_id(&pending.id).unwrap().unwrap();
    assert_eq!(preserved.id, pending.id);
    assert_eq!(preserved.state, "queued");
    assert_eq!(reopened.status().unwrap().revision, next);
}

#[test]
#[cfg(feature = "test-support")]
fn prior_v8_retention_layout_adds_self_fk_indexes_without_replacing_pin() {
    use std::os::unix::fs::MetadataExt;
    let (state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let old = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    drop(leader);
    drop(store);
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let path = state
        .path()
        .join("cache/indexes")
        .join(identity.root_key)
        .join("index.db");
    let db = Connection::open(&path).unwrap();
    db.execute_batch(
        "DROP INDEX native_version_declarations_owner; DROP INDEX native_version_regions_parent; DROP INDEX native_revisions_header_cover;",
    )
    .unwrap();
    drop(db);
    let inode = fs::metadata(&path).unwrap().ino();
    let reopened = Store::open_for_tests(state.path(), root.path()).unwrap();
    let leader = reopened.publication_for_tests().unwrap();
    let next = publish(&reopened, root.path(), &cancel, old, &leader).unwrap();
    assert_eq!(next.index_generation, old.index_generation);
    assert_eq!(next.index_revision, old.index_revision + 1);
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    let db = Connection::open(&path).unwrap();
    for name in [
        "native_version_declarations_owner",
        "native_version_regions_parent",
        "native_revisions_header_cover",
    ] {
        let present: i64 = db
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name=?1",
                [name],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(present, 1, "{name} missing after additive upgrade");
    }
    assert!(reopened.graph_at(Some(old)).is_ok());
}

#[test]
#[cfg(feature = "test-support")]
fn prior_v8_released_tombstone_upgrades_without_losing_generation_or_pin() {
    use std::os::unix::fs::MetadataExt;
    let (state, root, store, cancel) = fixture();
    store.set_retention_clock_for_tests(1_000, 0);
    let leader = store.publication_for_tests().unwrap();
    let old = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let head = publish(&store, root.path(), &cancel, old, &leader).unwrap();
    store.set_retention_clock_for_tests(1_902, 902);
    store.maintain_revisions(&leader).unwrap();
    assert!(
        store
            .graph_at(Some(old))
            .unwrap_err()
            .downcast_ref::<trellis::store::PinExpired>()
            .is_some()
    );
    drop(leader);
    drop(store);
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let path = state
        .path()
        .join("cache/indexes")
        .join(identity.root_key)
        .join("index.db");
    remove_maintenance_extension_for_v8_test(&path);
    let db = Connection::open(&path).unwrap();
    db.execute(
        "INSERT INTO native_revision_supersessions(revision_id,superseded_at) VALUES(?1,1000)",
        [format!(
            "pin:v1:{}:{}",
            old.index_generation, old.index_revision
        )],
    )
    .unwrap();
    drop(db);
    let inode = fs::metadata(&path).unwrap().ino();
    let reopened = Store::open_for_tests(state.path(), root.path()).unwrap();
    let leader = reopened.publication_for_tests().unwrap();
    let next = publish(&reopened, root.path(), &cancel, head, &leader).unwrap();
    assert_eq!(next.index_generation, head.index_generation);
    assert_eq!(next.index_revision, head.index_revision + 1);
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    let db = Connection::open(&path).unwrap();
    let state: String = db
        .query_row(
            "SELECT state FROM native_revision_supersessions WHERE revision_id=?1",
            [format!(
                "pin:v1:{}:{}",
                old.index_generation, old.index_revision
            )],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "released");
    assert!(
        reopened
            .graph_at(Some(old))
            .unwrap_err()
            .downcast_ref::<trellis::store::PinExpired>()
            .is_some()
    );
}

#[test]
#[cfg(feature = "test-support")]
fn released_supersession_never_reenters_retained_due_inventory() {
    let (state, root, store, cancel) = fixture();
    store.set_retention_clock_for_tests(1_000, 0);
    let leader = store.publication_for_tests().unwrap();
    let r1 = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    store.set_retention_clock_for_tests(1_001, 1);
    let r2 = publish(&store, root.path(), &cancel, r1, &leader).unwrap();
    store.set_retention_clock_for_tests(1_002, 2);
    let r3 = publish(&store, root.path(), &cancel, r2, &leader).unwrap();
    store.set_retention_clock_for_tests(1_901, 901);
    store.maintain_revisions(&leader).unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let path = state
        .path()
        .join("cache/indexes")
        .join(identity.root_key)
        .join("index.db");
    let db = Connection::open(&path).unwrap();
    for (pin, expected) in [(r1, "released"), (r2, "retained")] {
        let id = format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision);
        let state: String = db
            .query_row(
                "SELECT state FROM native_revision_supersessions WHERE revision_id=?1",
                [&id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, expected);
    }
    let due: i64 = db
        .query_row(
            "SELECT count(*) FROM native_revision_supersessions INDEXED BY native_supersessions_due
        WHERE state='retained' AND superseded_at<=1001",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(due, 0);
    assert_eq!(store.status().unwrap().revision, r3);
}

#[test]
#[cfg(feature = "test-support")]
fn retention_boundary_preserves_younger_manifests_and_collects_only_unreferenced() {
    let (state, root, store, cancel) = fixture();
    store.set_retention_clock_for_tests(1_000, 0);
    let leader = store.publication_for_tests().unwrap();
    let r1 = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let original = store.source_at("flow.js", Some(r1)).unwrap().unwrap();
    fs::write(
        root.path().join("flow.js"),
        "function changed() { newer(); }\n",
    )
    .unwrap();
    let r2 = publish(&store, root.path(), &cancel, r1, &leader).unwrap();
    store.set_retention_clock_for_tests(1_001, 1);
    fs::write(
        root.path().join("flow.js"),
        "function latest() { newest(); }\n",
    )
    .unwrap();
    let r3 = publish(&store, root.path(), &cancel, r2, &leader).unwrap();
    let retained = store.source_at("flow.js", Some(r2)).unwrap().unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let db = Connection::open(
        state
            .path()
            .join("cache/indexes")
            .join(identity.root_key)
            .join("index.db"),
    )
    .unwrap();
    let key = format!("pin:v1:{}:{}", r1.index_generation, r1.index_revision);
    let key2 = format!("pin:v1:{}:{}", r2.index_generation, r2.index_revision);
    let shared: (String, String, String) = db.query_row(
        "SELECT document_version_id,graph_projection_id,class_projection_id FROM revision_documents WHERE revision_id=?1 AND path='flow.java'",
        [&key2], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    for (key, stamp) in [(&key, 1_000), (&key2, 1_001)] {
        assert_eq!(
            db.query_row(
                "SELECT superseded_at FROM native_revision_supersessions WHERE revision_id=?1",
                [key],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            stamp
        );
    }
    store.set_retention_clock_for_tests(1_899, 899);
    store.maintain_revisions(&leader).unwrap();
    assert_eq!(
        store.source_at("flow.js", Some(r1)).unwrap().unwrap(),
        original
    );
    assert_eq!(
        store.source_at("flow.js", Some(r2)).unwrap().unwrap(),
        retained
    );
    // At 900 seconds the first pin expires, but the second is 899 seconds old.
    store.set_retention_clock_for_tests(1_900, 900);
    store.maintain_revisions(&leader).unwrap();
    let expired = store.source_at("flow.js", Some(r1)).unwrap_err();
    assert!(
        expired
            .downcast_ref::<trellis::store::PinExpired>()
            .is_some(),
        "{expired:#}"
    );
    assert_eq!(
        store.source_at("flow.js", Some(r2)).unwrap().unwrap(),
        retained
    );
    assert_eq!(store.status().unwrap().revision, r3);
    assert!(store.source_at("flow.js", Some(r3)).unwrap().is_some());
    assert!(store.graph_at(Some(r2)).is_ok());
    assert!(
        store
            .classes_at(Some("flow.java"), "", Some(r2), 0, 10)
            .is_ok()
    );
    for (table, id) in [
        ("document_versions", &shared.0),
        ("graph_projections", &shared.1),
        ("class_projections", &shared.2),
    ] {
        assert_eq!(
            db.query_row(
                &format!("SELECT count(*) FROM {table} WHERE id=?1"),
                [id],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1,
            "shared {table} removed"
        );
    }
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM revision_documents WHERE revision_id=?1",
            [key],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert!(
        db.prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none()
    );
    let foreign = IndexPin {
        index_generation: uuid::Uuid::new_v4(),
        index_revision: r1.index_revision,
    };
    assert!(
        store
            .source_at("flow.js", Some(foreign))
            .unwrap_err()
            .downcast_ref::<trellis::store::PinExpired>()
            .is_none()
    );
}

#[test]
#[cfg(feature = "test-support")]
fn future_supersession_rollback_cannot_expire_unaged_pin() {
    let (_state, root, store, cancel) = fixture();
    store.set_retention_clock_for_tests(1_000, 0);
    let leader = store.publication_for_tests().unwrap();
    let first = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let second = publish(&store, root.path(), &cancel, first, &leader).unwrap();
    store.set_retention_clock_for_tests(5_000, 1);
    let head = publish(&store, root.path(), &cancel, second, &leader).unwrap();
    store.maintain_revisions(&leader).unwrap();
    store.set_retention_clock_for_tests(1_001, 2);
    store.maintain_revisions(&leader).unwrap();
    assert!(
        store.graph_at(Some(first)).is_ok(),
        "future stamp must not expire an unaged pin after rollback"
    );
    assert!(store.graph_at(Some(second)).is_ok());
    assert_eq!(store.status().unwrap().revision, head);
    store.set_retention_clock_for_tests(1_900, 900);
    store.maintain_revisions(&leader).unwrap();
    assert!(
        store.graph_at(Some(first)).is_ok(),
        "uncorroborated latest stamp still fences expiration"
    );
    store.set_retention_clock_for_tests(1_001, 4_000);
    store.maintain_revisions(&leader).unwrap();
    assert!(
        store
            .graph_at(Some(first))
            .unwrap_err()
            .downcast_ref::<trellis::store::PinExpired>()
            .is_some()
    );
    assert!(store.graph_at(Some(head)).is_ok());
}

#[test]
#[cfg(feature = "test-support")]
fn backward_step_keeps_young_pin_and_restart_uses_persisted_utc() {
    let (state, root, store, cancel) = fixture();
    store.set_retention_clock_for_tests(1_000, 0);
    let leader = store.publication_for_tests().unwrap();
    let first = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let head = publish(&store, root.path(), &cancel, first, &leader).unwrap();
    store.set_retention_clock_for_tests(900, 100);
    store.maintain_revisions(&leader).unwrap();
    assert!(store.graph_at(Some(first)).is_ok());
    drop(leader);
    drop(store);
    let reopened = Store::open_for_tests(state.path(), root.path()).unwrap();
    reopened.set_retention_clock_for_tests(1_900, 0);
    let leader = reopened.publication_for_tests().unwrap();
    // Takeover must reconcile before it may read/release the selected history.
    publish(&reopened, root.path(), &cancel, head, &leader).unwrap();
    reopened.maintain_revisions(&leader).unwrap();
    assert!(
        reopened
            .graph_at(Some(first))
            .unwrap_err()
            .downcast_ref::<trellis::store::PinExpired>()
            .is_some()
    );
}

#[test]
#[cfg(feature = "test-support")]
fn failed_publication_after_additive_v8_upgrade_preserves_old_pin() {
    use std::{os::unix::fs::MetadataExt, sync::atomic::Ordering};
    let (state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let old = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    // Keep the accepted request after startup reconciliation, but retire the test writer guard
    // before the virgin queue's EX-protected first publication.
    drop(leader);
    let accepted = store
        .enqueue_request(&IndexOptions::new(root.path().to_owned()), Some(old))
        .unwrap();
    assert_eq!(accepted.state, "queued");
    let queue = store.request_db_path();
    let queue_inode = fs::metadata(&queue).unwrap().ino();
    drop(store);
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let path = state
        .path()
        .join("cache/indexes")
        .join(identity.root_key)
        .join("index.db");
    remove_maintenance_extension_for_v8_test(&path);
    let inode = fs::metadata(&path).unwrap().ino();
    let reopened = Store::open_for_tests(state.path(), root.path()).unwrap();
    let leader = reopened.publication_for_tests().unwrap();
    let stop = cancel.clone();
    reopened
        .set_publication_before_commit_hook_for_tests(move || stop.store(true, Ordering::Release));
    let error = publish(&reopened, root.path(), &cancel, old, &leader).unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error:#}");
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);

    // Keep this observer open through the retry: closing an independent SQLite
    // handle while the Store is live could release the process's SQLite locks.
    let db = Connection::open(&path).unwrap();
    let extension = || -> i64 {
        db.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='native_revision_release_debt'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    let head = || -> (String, i64) {
        db.query_row(
            "SELECT index_generation,index_revision FROM index_metadata WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    };
    assert_eq!(
        extension(),
        0,
        "cancelled transaction rolls back extension DDL"
    );
    assert_eq!(
        head(),
        (old.index_generation.to_string(), old.index_revision as i64)
    );
    assert_eq!(
        reopened.source_at("flow.js", Some(old)).unwrap().unwrap().0,
        old
    );
    assert_eq!(fs::metadata(&queue).unwrap().ino(), queue_inode);
    let queued = reopened.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(queued.id, accepted.id);
    assert_eq!(queued.state, "queued");

    cancel.store(false, Ordering::Release);
    let next = publish(&reopened, root.path(), &cancel, old, &leader).unwrap();
    assert_eq!(next.index_generation, old.index_generation);
    assert_eq!(next.index_revision, old.index_revision + 1);
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert_eq!(
        extension(),
        1,
        "successful retry installs maintenance extension"
    );
    assert_eq!(
        head(),
        (old.index_generation.to_string(), next.index_revision as i64)
    );
    assert_eq!(
        reopened.source_at("flow.js", Some(old)).unwrap().unwrap().0,
        old
    );
    assert_eq!(fs::metadata(&queue).unwrap().ino(), queue_inode);
    assert_eq!(
        reopened.request_by_id(&accepted.id).unwrap().unwrap().state,
        "queued"
    );
    drop(leader);
    drop(reopened);
    drop(db);
}

#[test]
#[cfg(feature = "test-support")]
fn expired_pin_rejects_each_selected_store_consumer_before_lookup() {
    use trellis::native_evidence::DocumentKey;
    let (_state, root, store, cancel) = fixture();
    store.set_retention_clock_for_tests(1_000, 0);
    let leader = store.publication_for_tests().unwrap();
    let old = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let graph = store.graph_at(Some(old)).unwrap();
    let symbol = graph
        .nodes
        .iter()
        .find(|n| n.path == "flow.js")
        .unwrap()
        .id
        .clone();
    let key = DocumentKey {
        source_set_id: format!("source-set:v1:{}", store.root_id()),
        language: "javascript".into(),
        path: "flow.js".into(),
    };
    let head = publish(&store, root.path(), &cancel, old, &leader).unwrap();
    assert!(store.native_source_at(old, &key).unwrap().is_some());
    store.set_retention_clock_for_tests(1_900, 900);
    store.maintain_revisions(&leader).unwrap();
    macro_rules! expired {
        ($label:expr, $result:expr) => {{
            let error = $result.unwrap_err();
            assert!(
                error.downcast_ref::<trellis::store::PinExpired>().is_some(),
                "{}: {error:#}",
                $label
            );
        }};
    }
    expired!("native source", store.native_source_at(old, &key));
    expired!("native coverage", store.native_coverage_at(old, &key));
    expired!(
        "native declarations",
        store.native_declarations_at(old, "javascript", "hello")
    );
    expired!("native calls", store.native_calls_at(old, &symbol));
    expired!(
        "native regions",
        store.native_control_regions_at(old, &symbol)
    );
    expired!("graph", store.graph_at(Some(old)));
    expired!("source", store.source_at("flow.js", Some(old)));
    expired!("symbol", store.symbol_at(&symbol, Some(old)));
    expired!("files", store.files_at(Some(old), 0, 20));
    expired!("methods", store.methods_at("flow.js", Some(old)));
    expired!(
        "classes",
        store.classes_at(Some("flow.java"), "", Some(old), 0, 10)
    );
    assert_eq!(
        store.status().unwrap().revision,
        head,
        "reads never publish"
    );
    assert!(store.native_source_at(head, &key).unwrap().is_some());
    for invalid in [
        IndexPin {
            index_generation: uuid::Uuid::new_v4(),
            ..old
        },
        IndexPin {
            index_revision: head.index_revision + 1,
            ..old
        },
        IndexPin {
            index_revision: 0,
            ..old
        },
    ] {
        let error = store.native_source_at(invalid, &key).unwrap_err();
        assert!(
            error.to_string().starts_with("revision conflict"),
            "{error:#}"
        );
        assert!(error.downcast_ref::<trellis::store::PinExpired>().is_none());
    }
}

#[test]
#[cfg(feature = "test-support")]
fn due_release_blocked_by_maintenance_admission_must_not_report_success() {
    let (state, root, store, cancel) = fixture();
    store.set_retention_clock_for_tests(1_000, 0);
    let leader = store.publication_for_tests().unwrap();
    let old = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    store.set_retention_clock_for_tests(1_001, 1);
    let head = publish(&store, root.path(), &cancel, old, &leader).unwrap();
    store.set_retention_clock_for_tests(1_901, 901);
    let permit = store.maintenance_try_enter().unwrap();
    let error = store.release_revision(old, &leader).unwrap_err();
    assert!(
        error
            .downcast_ref::<trellis::store::SqliteContention>()
            .is_some(),
        "blocked due release must return typed retryable contention: {error:#}"
    );
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let db = Connection::open(
        state
            .path()
            .join("cache/indexes")
            .join(identity.root_key)
            .join("index.db"),
    )
    .unwrap();
    let id = format!("pin:v1:{}:{}", old.index_generation, old.index_revision);
    let (state, debt, tombstones): (String, i64, i64) = db
        .query_row(
            "SELECT state,
                (SELECT count(*) FROM native_revision_release_debt WHERE revision_id=?1),
                (SELECT count(*) FROM revision_capture_inputs WHERE revision_id=?1
                    AND input_key IN ('__pending_release:v1','__released:v1'))
             FROM native_revision_supersessions WHERE revision_id=?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((state, debt, tombstones), ("retained".into(), 0, 0));
    assert!(store.graph_at(Some(old)).is_ok(), "old pin stays readable");
    assert_eq!(store.status().unwrap().revision, head);
    drop(permit);
}

#[test]
#[cfg(feature = "test-support")]
fn v8_pending_partial_manifest_is_strict_and_pin_expired_then_replays_to_exact_tombstone() {
    use trellis::store::MaintenanceOutcome;
    let (state, root, store, cancel) = fixture();
    for number in 0..65 {
        fs::write(
            root.path().join(format!("extra{number}.js")),
            format!("function extra{number}() {{ return {number}; }}"),
        )
        .unwrap();
    }
    store.set_retention_clock_for_tests(1_000, 0);
    let leader = store.publication_for_tests().unwrap();
    let old = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    store.set_retention_clock_for_tests(1_001, 1);
    let head = publish(&store, root.path(), &cancel, old, &leader).unwrap();
    assert!(
        store
            .release_revision(old, &leader)
            .unwrap_err()
            .to_string()
            .contains("retention grace")
    );
    let id = format!("pin:v1:{}:{}", old.index_generation, old.index_revision);
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let path = state
        .path()
        .join("cache/indexes")
        .join(identity.root_key)
        .join("index.db");
    let original: i64 = Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM revision_documents WHERE revision_id=?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(original > 64);
    store.set_retention_clock_for_tests(1_901, 901);
    let permit = store.maintenance_try_enter().unwrap();
    let probe = store.open_maintenance_queue_probe().unwrap();
    assert_eq!(
        store
            .maintenance_step(&leader, &permit, &probe, || true)
            .unwrap(),
        MaintenanceOutcome::Progress
    );
    let db = Connection::open(&path).unwrap();
    let state: String = db
        .query_row(
            "SELECT state FROM native_revision_supersessions WHERE revision_id=?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "pending");
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM revision_documents WHERE revision_id=?1",
            [&id],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        original
    );
    assert_eq!(db.query_row("SELECT count(*) FROM revision_capture_inputs WHERE revision_id=?1 AND input_key='__pending_release:v1' AND payload='pending_release:v1'",[&id],|r|r.get::<_,i64>(0)).unwrap(),1);
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM native_revision_release_debt WHERE revision_id=?1",
            [&id],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert!(
        store
            .graph_at(Some(old))
            .unwrap_err()
            .downcast_ref::<trellis::store::PinExpired>()
            .is_some()
    );
    assert_eq!(store.status().unwrap().revision, head);
    assert_eq!(
        store
            .maintenance_step(&leader, &permit, &probe, || true)
            .unwrap(),
        MaintenanceOutcome::Progress
    );
    let partial: i64 = db
        .query_row(
            "SELECT count(*) FROM revision_documents WHERE revision_id=?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        partial > 0 && partial < original,
        "partial manifest durable after one unit"
    );
    assert_eq!(store.status().unwrap().revision, head);
    drop(probe);
    drop(permit);
    drop(db);
    store.maintain_revisions(&leader).unwrap();
    let db = Connection::open(&path).unwrap();
    let exact: (String, i64, i64, i64) = db
        .query_row(
            "SELECT state,
         (SELECT count(*) FROM revision_capture_inputs WHERE revision_id=?1),
         (SELECT count(*) FROM native_revision_release_debt WHERE revision_id=?1),
         (SELECT count(*) FROM revision_documents WHERE revision_id=?1)
         FROM native_revision_supersessions WHERE revision_id=?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(exact, ("released".into(), 1, 0, 0));
    assert_eq!(store.status().unwrap().revision, head);
}

#[test]
#[cfg(feature = "test-support")]
fn maintenance_first_delete_priority_change_rolls_back_to_whole_pending_state() {
    use std::sync::atomic::Ordering;
    use trellis::store::MaintenanceOutcome;
    let (state, root, store, cancel) = fixture();
    store.set_retention_clock_for_tests(1_000, 0);
    let leader = store.publication_for_tests().unwrap();
    let old = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    store.set_retention_clock_for_tests(1_001, 1);
    publish(&store, root.path(), &cancel, old, &leader).unwrap();
    store.set_retention_clock_for_tests(1_901, 901);
    let permit = store.maintenance_try_enter().unwrap();
    let probe = store.open_maintenance_queue_probe().unwrap();
    assert_eq!(
        store
            .maintenance_step(&leader, &permit, &probe, || true)
            .unwrap(),
        MaintenanceOutcome::Progress
    );
    let priority = Arc::new(AtomicBool::new(false));
    let signal = priority.clone();
    store.set_maintenance_after_first_delete_hook_for_tests(move || {
        signal.store(true, Ordering::Release)
    });
    assert_eq!(
        store
            .maintenance_step(&leader, &permit, &probe, || !priority
                .load(Ordering::Acquire))
            .unwrap(),
        MaintenanceOutcome::Deferred
    );
    assert!(priority.load(Ordering::Acquire));
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let path = state
        .path()
        .join("cache/indexes")
        .join(identity.root_key)
        .join("index.db");
    let id = format!("pin:v1:{}:{}", old.index_generation, old.index_revision);
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM revision_documents WHERE revision_id=?1",
            [&id],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        4
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM native_release_candidate_versions WHERE revision_id=?1",
            [&id],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        store.status().unwrap().revision.index_revision,
        old.index_revision + 1
    );
}

#[test]
#[cfg(feature = "test-support")]
fn maintenance_first_delete_callback_panic_defers_without_partial_release_or_leaked_handler() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use trellis::store::MaintenanceOutcome;
    let (state, root, store, cancel) = fixture();
    store.set_retention_clock_for_tests(1_000, 0);
    let leader = store.publication_for_tests().unwrap();
    let old = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    store.set_retention_clock_for_tests(1_001, 1);
    let head = publish(&store, root.path(), &cancel, old, &leader).unwrap();
    store.set_retention_clock_for_tests(1_901, 901);
    let permit = store.maintenance_try_enter().unwrap();
    let probe = store.open_maintenance_queue_probe().unwrap();
    assert_eq!(
        store
            .maintenance_step(&leader, &permit, &probe, || true)
            .unwrap(),
        MaintenanceOutcome::Progress
    );
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let path = state
        .path()
        .join("cache/indexes")
        .join(identity.root_key)
        .join("index.db");
    let db = Connection::open(&path).unwrap();
    let id = format!("pin:v1:{}:{}", old.index_generation, old.index_revision);
    let before: i64 = db
        .query_row(
            "SELECT count(*) FROM revision_documents WHERE revision_id=?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(before > 0);
    let deleted = Arc::new(AtomicBool::new(false));
    let triggered = deleted.clone();
    store.set_maintenance_after_first_delete_hook_for_tests(move || {
        triggered.store(true, Ordering::Release);
    });
    let panics = AtomicUsize::new(0);
    assert_eq!(
        store
            .maintenance_step(&leader, &permit, &probe, || {
                if deleted.load(Ordering::Acquire) {
                    panics.fetch_add(1, Ordering::Relaxed);
                    panic!("test-only maintenance priority panic after first DELETE");
                }
                true
            })
            .unwrap(),
        MaintenanceOutcome::Deferred
    );
    assert!(
        deleted.load(Ordering::Acquire),
        "the real manifest DELETE ran"
    );
    assert!(
        panics.load(Ordering::Relaxed) > 0,
        "priority panic was contained"
    );
    let snapshot: (String, i64, i64, i64, i64) = db
        .query_row(
            "SELECT state,
                (SELECT count(*) FROM revision_documents WHERE revision_id=?1),
                (SELECT count(*) FROM native_revision_release_debt WHERE revision_id=?1),
                (SELECT count(*) FROM revision_capture_inputs WHERE revision_id=?1
                    AND input_key='__pending_release:v1'),
                (SELECT count(*) FROM native_release_candidate_versions WHERE revision_id=?1)
             FROM native_revision_supersessions WHERE revision_id=?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(snapshot, ("pending".into(), before, 1, 1, 0));
    assert_eq!(store.status().unwrap().revision, head);
    assert_eq!(
        store
            .maintenance_step(&leader, &permit, &probe, || true)
            .unwrap(),
        MaintenanceOutcome::Progress,
        "a later statement must not inherit the panic handler"
    );
}

#[test]
#[cfg(feature = "test-support")]
fn v8_noop_retention_uses_covering_indexes_and_never_opens_writer() {
    use std::sync::atomic::Ordering;
    use trellis::store::MaintenanceOutcome;
    let (state, root, store, cancel) = fixture();
    store.set_retention_clock_for_tests(1_000, 0);
    let leader = store.publication_for_tests().unwrap();
    let old = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    store.set_retention_clock_for_tests(1_001, 1);
    publish(&store, root.path(), &cancel, old, &leader).unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let path = state
        .path()
        .join("cache/indexes")
        .join(identity.root_key)
        .join("index.db");
    let observer = Connection::open(&path).unwrap();
    let plans = [
        (
            "SELECT max(superseded_at) FROM native_revision_supersessions",
            "native_supersessions_clock",
        ),
        (
            "SELECT revision_id FROM native_revision_supersessions WHERE state='retained' AND superseded_at<=1000 ORDER BY superseded_at,revision_id LIMIT 1",
            "native_supersessions_due",
        ),
        (
            "SELECT revision_id FROM native_revision_release_debt LIMIT 1",
            "sqlite_autoindex_native_revision_release_debt",
        ),
    ];
    for (query, index) in plans {
        let detail: Vec<String> = observer
            .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
            .unwrap()
            .query_map([], |r| r.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            detail.iter().any(|entry| entry.contains(index)),
            "{query}: {detail:?}"
        );
    }
    let before: i64 = observer
        .pragma_query_value(None, "data_version", |r| r.get(0))
        .unwrap();
    let attempted = Arc::new(AtomicBool::new(false));
    let flag = attempted.clone();
    store.set_maintenance_before_writer_hook_for_tests(move || flag.store(true, Ordering::Release));
    store.set_retention_clock_for_tests(1_899, 899);
    let permit = store.maintenance_try_enter().unwrap();
    let probe = store.open_maintenance_queue_probe().unwrap();
    assert_eq!(
        store
            .maintenance_step(&leader, &permit, &probe, || true)
            .unwrap(),
        MaintenanceOutcome::Idle
    );
    assert!(!attempted.load(Ordering::Acquire));
    let after: i64 = observer
        .pragma_query_value(None, "data_version", |r| r.get(0))
        .unwrap();
    assert_eq!(before, after, "no-op made an index write");
    assert_eq!(store.oldest_due_debt_age(&leader).unwrap(), None);
    store.set_retention_clock_for_tests(1_901, 901);
    assert_eq!(
        store.oldest_due_debt_age(&leader).unwrap(),
        Some(std::time::Duration::from_secs(900))
    );
}

#[test]
#[cfg(feature = "test-support")]
fn v8_rejects_missing_debt_wrong_payload_and_dual_release_markers() {
    use trellis::store::MaintenanceOutcome;
    for mutation in [
        "DELETE FROM native_revision_release_debt WHERE revision_id=?1",
        "UPDATE revision_capture_inputs SET payload='invalid' WHERE revision_id=?1 AND input_key='__pending_release:v1'",
        "INSERT INTO revision_capture_inputs(revision_id,input_key,payload) VALUES(?1,'__released:v1','released:v1')",
    ] {
        let (state, root, store, cancel) = fixture();
        store.set_retention_clock_for_tests(1_000, 0);
        let leader = store.publication_for_tests().unwrap();
        let old = publish(
            &store,
            root.path(),
            &cancel,
            store.index_baseline().unwrap(),
            &leader,
        )
        .unwrap();
        store.set_retention_clock_for_tests(1_001, 1);
        publish(&store, root.path(), &cancel, old, &leader).unwrap();
        store.set_retention_clock_for_tests(1_901, 901);
        let permit = store.maintenance_try_enter().unwrap();
        let probe = store.open_maintenance_queue_probe().unwrap();
        assert_eq!(
            store
                .maintenance_step(&leader, &permit, &probe, || true)
                .unwrap(),
            MaintenanceOutcome::Progress
        );
        drop(probe);
        drop(permit);
        drop(leader);
        drop(store);
        let identity =
            trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
                .unwrap();
        let path = state
            .path()
            .join("cache/indexes")
            .join(identity.root_key)
            .join("index.db");
        let id = format!("pin:v1:{}:{}", old.index_generation, old.index_revision);
        Connection::open(path)
            .unwrap()
            .execute(mutation, [id])
            .unwrap();
        let reopened = Store::open_for_tests(state.path(), root.path()).unwrap();
        let error = reopened.status().unwrap_err();
        assert!(
            error.to_string().contains("incompatible_index"),
            "{mutation}: {error:#}"
        );
    }
}

#[test]
#[cfg(feature = "test-support")]
fn maintenance_index_writer_busy_defers_and_counts_only_confirmed_sqlite_busy() {
    use trellis::store::MaintenanceOutcome;
    let (state, root, store, cancel) = fixture();
    store.set_retention_clock_for_tests(1_000, 0);
    let leader = store.publication_for_tests().unwrap();
    let old = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    store.set_retention_clock_for_tests(1_001, 1);
    publish(&store, root.path(), &cancel, old, &leader).unwrap();
    store.set_retention_clock_for_tests(1_901, 901);
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let path = state
        .path()
        .join("cache/indexes")
        .join(identity.root_key)
        .join("index.db");
    let blocker = Connection::open(path).unwrap();
    blocker.busy_timeout(std::time::Duration::ZERO).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let permit = store.maintenance_try_enter().unwrap();
    let probe = store.open_maintenance_queue_probe().unwrap();
    let before = store.maintenance_sqlite_busy_attempts();
    assert_eq!(
        store
            .maintenance_step(&leader, &permit, &probe, || true)
            .unwrap(),
        MaintenanceOutcome::Deferred
    );
    assert!(store.maintenance_sqlite_busy_attempts() > before);
    blocker.execute_batch("ROLLBACK").unwrap();
    assert!(
        store.graph_at(Some(old)).is_ok(),
        "deferred writer did not release the pin"
    );
}

#[test]
fn publication_gate_is_shared_by_clones_and_independent_same_root_stores() {
    let (state, root, store, cancel) = fixture();
    let cloned = store.clone();
    let independent = Store::open_for_tests(state.path(), root.path()).unwrap();
    let publication = store
        .enter_publication(&cancel, std::time::Duration::ZERO)
        .unwrap();
    assert!(cloned.maintenance_try_enter().is_none());
    assert!(independent.maintenance_try_enter().is_none());
    drop(publication);
    let maintenance = independent.maintenance_try_enter().unwrap();
    let error = store
        .enter_publication(&cancel, std::time::Duration::ZERO)
        .unwrap_err();
    assert!(error.to_string().contains("storage_busy"));
    drop(maintenance);
    let permit = cloned
        .enter_publication(&cancel, std::time::Duration::ZERO)
        .unwrap();
    assert_eq!(
        permit.wait_reason(),
        trellis::store::PublicationWaitReason::None
    );
}

#[test]
#[cfg(feature = "test-support")]
fn compatible_pending_debt_replays_after_verified_leader_restart_in_same_generation() {
    use trellis::store::MaintenanceOutcome;
    let (state, root, store, cancel) = fixture();
    store.set_retention_clock_for_tests(1_000, 0);
    let leader = store.publication_for_tests().unwrap();
    let old = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    store.set_retention_clock_for_tests(1_001, 1);
    let head = publish(&store, root.path(), &cancel, old, &leader).unwrap();
    store.set_retention_clock_for_tests(1_901, 901);
    let permit = store.maintenance_try_enter().unwrap();
    let probe = store.open_maintenance_queue_probe().unwrap();
    assert_eq!(
        store
            .maintenance_step(&leader, &permit, &probe, || true)
            .unwrap(),
        MaintenanceOutcome::Progress
    );
    drop(probe);
    drop(permit);
    drop(leader);
    drop(store);
    let reopened = Store::open_for_tests(state.path(), root.path()).unwrap();
    reopened.set_retention_clock_for_tests(1_901, 0);
    let session = reopened.writer_session_for_tests().unwrap();
    let next = trellis::index_coordinator::IndexJobCoordinator::prepare_with_session(
        &reopened,
        None,
        session.clone(),
    )
    .unwrap()
    .run(&IndexOptions::new(root.path().to_owned()), &cancel, |_| {})
    .unwrap();
    assert_eq!(next.index_generation, head.index_generation);
    assert_eq!(next.index_revision, head.index_revision + 1);
    assert!(
        reopened
            .graph_at(Some(old))
            .unwrap_err()
            .downcast_ref::<trellis::store::PinExpired>()
            .is_some()
    );
    assert_eq!(
        reopened
            .maintenance_oldest_due_age_secs(session.writer_guard().unwrap())
            .unwrap(),
        Some(900)
    );
    let permit = reopened.maintenance_try_enter().unwrap();
    let probe = reopened.open_maintenance_queue_probe().unwrap();
    assert_eq!(
        reopened
            .maintenance_step(session.writer_guard().unwrap(), &permit, &probe, || true)
            .unwrap(),
        MaintenanceOutcome::Progress
    );
    drop(probe);
    drop(permit);
    reopened
        .maintain_revisions(session.writer_guard().unwrap())
        .unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let path = state
        .path()
        .join("cache/indexes")
        .join(identity.root_key)
        .join("index.db");
    let id = format!("pin:v1:{}:{}", old.index_generation, old.index_revision);
    let db = Connection::open(path).unwrap();
    let state: String = db
        .query_row(
            "SELECT state FROM native_revision_supersessions WHERE revision_id=?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        matches!(state.as_str(), "pending" | "released"),
        "bounded units may defer without losing debt"
    );
    assert_eq!(reopened.status().unwrap().revision, next);
}

#[test]
#[cfg(feature = "test-support")]
fn newer_disposable_schema_is_refused_without_rebuild_or_deletion() {
    use std::os::unix::fs::MetadataExt;
    let (state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let old = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    drop(leader);
    drop(store);
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let path = state
        .path()
        .join("cache/indexes")
        .join(identity.root_key)
        .join("index.db");
    Connection::open(&path)
        .unwrap()
        .execute_batch("PRAGMA user_version=10")
        .unwrap();
    let inode = fs::metadata(&path).unwrap().ino();
    let error = Store::open_for_tests(state.path(), root.path()).unwrap_err();
    assert!(
        error.to_string().contains("unsupported newer schema"),
        "{error:#}"
    );
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    let db = Connection::open(&path).unwrap();
    let version: i64 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    let generation: String = db
        .query_row(
            "SELECT index_generation FROM index_metadata WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(version, 10);
    assert_eq!(generation, old.index_generation.to_string());
}

#[test]
#[cfg(feature = "test-support")]
fn maintenance_releases_thousands_of_js_facts_without_permanent_deferral() {
    use trellis::store::MaintenanceOutcome;
    let (state, root, store, cancel) = fixture();
    let source = root.path().join("large.js");
    let body: String = (0..20_000)
        .map(|n| format!("function f{n}() {{ return {n}; }}\n"))
        .collect();
    fs::write(&source, &body).unwrap();
    store.set_retention_clock_for_tests(1_000, 0);
    let leader = store.publication_for_tests().unwrap();
    let old = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    fs::write(&source, format!("{body}\n// edited\n")).unwrap();
    store.set_retention_clock_for_tests(1_001, 1);
    let head = publish(&store, root.path(), &cancel, old, &leader).unwrap();
    let key = format!("pin:v1:{}:{}", old.index_generation, old.index_revision);
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let path = state
        .path()
        .join("cache/indexes")
        .join(identity.root_key)
        .join("index.db");
    let db = Connection::open(path).unwrap();
    let facts: i64 = db.query_row(
        "SELECT count(*) FROM native_version_declarations WHERE version_id=(SELECT document_version_id FROM revision_documents WHERE revision_id=?1 AND path='large.js')",
        [&key], |r| r.get(0)).unwrap();
    assert!(facts >= 20_000, "large version has {facts} declarations");
    store.set_retention_clock_for_tests(1_903, 903);
    let started = std::time::Instant::now();
    {
        let permit = store.maintenance_try_enter().unwrap();
        let probe = store.open_maintenance_queue_probe().unwrap();
        for _ in 0..4 {
            assert_eq!(
                store
                    .maintenance_step(&leader, &permit, &probe, || true)
                    .unwrap(),
                MaintenanceOutcome::Progress
            );
        }
    }
    // Publish another edit while the large prior document is partially drained.
    fs::write(&source, format!("{body}\n// edited again\n")).unwrap();
    let new_head = publish(&store, root.path(), &cancel, head, &leader).unwrap();
    let permit = store.maintenance_try_enter().unwrap();
    let probe = store.open_maintenance_queue_probe().unwrap();
    let mut progress = 4;
    let mut released = false;
    for _ in 0..500 {
        match store
            .maintenance_step(&leader, &permit, &probe, || true)
            .unwrap()
        {
            MaintenanceOutcome::Progress => progress += 1,
            MaintenanceOutcome::Deferred => {}
            MaintenanceOutcome::Idle => break,
        }
        let status: String = db
            .query_row(
                "SELECT state FROM native_revision_supersessions WHERE revision_id=?1",
                [&key],
                |r| r.get(0),
            )
            .unwrap();
        if status == "released" {
            released = true;
            break;
        }
    }
    assert!(
        released,
        "{facts} declarations remained pending after {progress} progress units"
    );
    assert!(
        started.elapsed() < trellis::testing::HANG_BACKSTOP,
        "large-document release and interleaved edit hung: {:?}",
        started.elapsed()
    );
    assert_eq!(store.status().unwrap().revision, new_head);
}

#[test]
#[cfg(feature = "test-support")]
fn large_revision_releases_in_batched_units_while_edits_continue() {
    use trellis::store::MaintenanceOutcome;
    let (state, root, store, cancel) = fixture();
    for n in 0..400 {
        fs::write(
            root.path().join(format!("batch{n}.js")),
            format!("function f{n}() {{ return {n}; }}\n"),
        )
        .unwrap();
    }
    store.set_retention_clock_for_tests(1_000, 0);
    let leader = store.publication_for_tests().unwrap();
    let old = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    fs::write(
        root.path().join("batch0.js"),
        "function changed0() { return 1; }\n",
    )
    .unwrap();
    store.set_retention_clock_for_tests(1_001, 1);
    let middle = publish(&store, root.path(), &cancel, old, &leader).unwrap();
    store.set_retention_clock_for_tests(1_902, 902);
    {
        let permit = store.maintenance_try_enter().unwrap();
        let probe = store.open_maintenance_queue_probe().unwrap();
        for _ in 0..2 {
            assert_eq!(
                store
                    .maintenance_step(&leader, &permit, &probe, || true)
                    .unwrap(),
                MaintenanceOutcome::Progress
            );
        }
    }
    // A real publication can interleave before the old manifest has drained.
    fs::write(
        root.path().join("batch1.js"),
        "function changed1() { return 2; }\n",
    )
    .unwrap();
    store.set_retention_clock_for_tests(1_903, 903);
    let head = publish(&store, root.path(), &cancel, middle, &leader).unwrap();
    let key = format!("pin:v1:{}:{}", old.index_generation, old.index_revision);
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let db = Connection::open(
        state
            .path()
            .join("cache/indexes")
            .join(identity.root_key)
            .join("index.db"),
    )
    .unwrap();
    let mut units = 2;
    let permit = store.maintenance_try_enter().unwrap();
    let probe = store.open_maintenance_queue_probe().unwrap();
    let mut released = false;
    for _ in 0..130 {
        match store
            .maintenance_step(&leader, &permit, &probe, || true)
            .unwrap()
        {
            MaintenanceOutcome::Progress => units += 1,
            MaintenanceOutcome::Deferred => {}
            MaintenanceOutcome::Idle => break,
        }
        let status: String = db
            .query_row(
                "SELECT state FROM native_revision_supersessions WHERE revision_id=?1",
                [&key],
                |r| r.get(0),
            )
            .unwrap();
        if status == "released" {
            released = true;
            break;
        }
    }
    assert!(
        released,
        "old 400-document revision remains pending after {units} units"
    );
    assert!(
        units < 30,
        "candidate cleanup must batch, not need one unit per document"
    );
    assert_eq!(store.status().unwrap().revision, head);
}
