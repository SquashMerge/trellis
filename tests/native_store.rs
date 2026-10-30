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
fn live_document_version(db: &Connection, path: &str) -> i64 {
    db.query_row(
        "SELECT d.vid FROM revision_documents d WHERE d.revision_id=?1 AND d.path=?2",
        rusqlite::params![live_revision(db), path],
        |row| row.get(0),
    )
    .unwrap()
}
#[cfg(feature = "test-support")]
fn live_class_projection(db: &Connection, path: &str) -> i64 {
    db.query_row(
        "SELECT d.cid FROM revision_documents d WHERE d.revision_id=?1 AND d.path=?2",
        rusqlite::params![live_revision(db), path],
        |row| row.get(0),
    )
    .unwrap()
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
    // A current empty bootstrap is the same generation; only a rebuild rotates it.
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
            store
                .native_capability_report_at(pin, &document.key)
                .unwrap()
                .unwrap(),
            *artifact
                .capability_reports
                .iter()
                .find(|r| r.document == document.key)
                .unwrap()
        );
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
        assert_eq!(
            store
                .native_references_at(pin, &declaration.syntax_id)
                .unwrap(),
            artifact
                .references
                .iter()
                .filter(|r| r.owner_syntax_id == declaration.syntax_id)
                .cloned()
                .collect::<Vec<_>>()
        );
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
        29
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
                "SELECT CASE WHEN owner_syntax_id IS NULL THEN NULL ELSE 'sid:v1:'||lower(hex(owner_syntax_id)) END FROM native_version_declarations WHERE vid=?1 AND syntax_id=unhex(substr(?2,8))",
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

/// #225: plain caller-supplied artifacts get the same structural validation
/// without a determinism rebuild. Publication still checks graph consistency.
#[test]
#[cfg(feature = "test-support")]
fn caller_supplied_native_artifact_uses_structural_validation() {
    let (_state, root, store, cancel) = fixture();
    let baseline = store.index_baseline().unwrap();
    let leader = store.publication_for_tests().unwrap();
    let (graph, mut native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    // Modifiers are structurally valid and not carried in the graph projection.
    native
        .declarations
        .iter_mut()
        .find(|d| d.name.as_deref() == Some("hello"))
        .unwrap()
        .header
        .modifiers
        .push("public".into());
    let next = store
        .publish_native(&graph, &capture, &native, &leader, baseline, &cancel)
        .unwrap();
    assert_eq!(store.status().unwrap().revision, next);
    assert_eq!(store.graph().unwrap(), graph);
}

#[test]
#[cfg(feature = "test-support")]
fn caller_supplied_cross_document_proof_does_not_publish_or_change_head() {
    let (_state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let prior_graph = store.graph().unwrap();
    let prior_source = store.source_at("flow.js", Some(pin)).unwrap().unwrap();
    fs::write(
        root.path().join("flow.js"),
        "function updated(value) { if (value) { obj.changed(); } }\n",
    )
    .unwrap();
    let (graph, mut native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let canonical_root = fs::canonicalize(root.path()).unwrap();
    native
        .validate(&capture, &canonical_root, store.root_id(), &cancel)
        .unwrap();
    assert_ne!(graph, prior_graph);
    let call = native
        .calls
        .iter_mut()
        .find(|call| call.document.path == "flow.js")
        .unwrap();
    call.provenance_id = native
        .provenance
        .iter()
        .find(|proof| proof.document != call.document)
        .unwrap()
        .id
        .clone();
    assert!(
        store
            .publish_native(&graph, &capture, &native, &leader, pin, &cancel)
            .is_err(),
        "caller-supplied call accepted a proof belonging to another document"
    );
    assert_eq!(store.index_baseline().unwrap(), pin);
    assert_eq!(store.status().unwrap().revision, pin);
    assert_eq!(store.graph().unwrap(), prior_graph);
    assert_eq!(
        store.source_at("flow.js", Some(pin)).unwrap().unwrap(),
        prior_source
    );
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
        "graph_display_labels",
        "class_projections",
        "native_version_references",
        "native_version_capability_families",
        "native_version_capability_gaps",
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
            "SELECT source_bytes FROM document_versions WHERE vid=?1",
            [live_document_version(&db, "flow.rs")],
            |r| r.get(0),
        )
        .unwrap();
    bytes[0] ^= 1;
    db.execute(
        "UPDATE document_versions SET source_bytes=?1 WHERE vid=?2",
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
        selected.to_string().starts_with("incompatible_index:")
            && format!("{selected:#}").contains("source hash mismatch"),
        "{selected:#}"
    );
    assert!(store.recovery_pending_for_tests());
    let closed = original_clone.source_at("flow.rs", Some(pin)).unwrap_err();
    assert_current_corruption(closed);

    let reopened = Store::open_for_tests(state.path(), root.path()).unwrap();
    assert_eq!(reopened.status().unwrap().revision, pin);
    assert_eq!(reopened.index_baseline().unwrap(), pin);
    let reopened_clone = reopened.clone();
    let selected = reopened.source_at("flow.rs", Some(pin)).unwrap_err();
    assert!(
        selected.to_string().starts_with("incompatible_index:")
            && format!("{selected:#}").contains("source hash mismatch"),
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
            "SELECT source_bytes FROM document_versions WHERE vid=?1",
            [live_document_version(&db, "source63.js")],
            |r| r.get(0),
        )
        .unwrap();
    bytes[0] ^= 1;
    db.execute(
        "UPDATE document_versions SET source_bytes=?1 WHERE vid=?2",
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
    for old_version in [
        "native-v2",
        "native-v3",
        "native-v8",
        "native-v8.1",
        "native-v9",
    ] {
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
        // Withdrawn versions and both merge-parent producer versions must be refused.
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
        assert_eq!(version, "native-v11");
        let calls: Vec<String> = db
            .prepare("SELECT 'occ:v2:'||lower(hex(id)) FROM native_version_calls")
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
#[test]
#[cfg(feature = "test-support")]
fn physical_v8_native_v4_index_recreates_before_role_column_read() {
    use std::os::unix::fs::MetadataExt;
    use trellis::native_evidence::DeclarationRole;
    let (state, root, prior, cancel) = fixture();
    let old_root_id = prior.root_id().to_owned();
    prior
        .enqueue_request(&IndexOptions::new(root.path().to_owned()), None)
        .unwrap();
    let path = published_db(state.path(), root.path());
    drop(prior);
    fs::remove_file(&path).unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let old = Connection::open(&path).unwrap();
    old.execute_batch(include_str!("fixtures/gc-legacy/v8-current.sql"))
        .unwrap();
    old.pragma_update(None, "user_version", 8_i64).unwrap();
    old.execute(
        "INSERT INTO index_metadata(singleton,schema_version,extractor_version,root_spelling,root_device,root_inode,index_generation,index_revision,last_opened_at,indexed_at,stats,diagnostics) VALUES (1,8,'native-v4-delta-v1',?1,?2,?3,?4,0,0,'','{}','[]')",
        rusqlite::params![identity.root.to_str().unwrap(),identity.device.to_string(),identity.inode.to_string(),uuid::Uuid::new_v4().to_string()],
    ).unwrap();
    old.execute("INSERT INTO native_producers(id,version,executable_hash,kind,position_encoding) VALUES ('trellis.native.syntax','native-v4',?1,'native','utf8')", ["0".repeat(64)]).unwrap();
    let columns: Vec<String> = old
        .prepare("PRAGMA table_info(native_version_declarations)")
        .unwrap()
        .query_map([], |row| row.get(1))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(
        !columns.iter().any(|name| name == "role"),
        "fixture must be physical v8 without role"
    );
    drop(old);
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let old_inode = fs::metadata(&path).unwrap().ino();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    assert_eq!(store.root_id(), old_root_id);
    assert_eq!(
        Connection::open(&path)
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        8
    );
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("recovery_required")
    );
    let (pin, _session) = trellis::index_coordinator::reconcile_workspace(
        &store,
        &IndexOptions::new(root.path().to_owned()),
        &cancel,
        |_| {},
    )
    .unwrap();
    assert_ne!(
        fs::metadata(&path).unwrap().ino(),
        old_inode,
        "disposable index must recreate, not migrate"
    );
    let rebuilt = Connection::open(&path).unwrap();
    assert_eq!(
        rebuilt
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        29
    );
    let stamp: String = rebuilt
        .query_row("SELECT extractor_version FROM index_metadata", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(stamp, "native-v6-delta-v1");
    let version: String = rebuilt
        .query_row("SELECT version FROM native_producers", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, "native-v11");
    let roles = store
        .native_declarations_at(pin, "javascript", "hello")
        .unwrap();
    assert_eq!(roles.len(), 1);
    assert_eq!(roles[0].role, DeclarationRole::Production);
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
            "SELECT 'sid:v1:'||lower(hex(id)) FROM classes WHERE cid=?1 ORDER BY id LIMIT 1",
            [live_class_projection(&db, "flow.java")],
            |r| r.get(0),
        )
        .unwrap();
    db.execute(
        "UPDATE classes SET payload=?1 WHERE cid=?2 AND id=unhex(substr(?3,8))",
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
        error.to_string().starts_with("incompatible_index:")
            && format!("{error:#}").contains("selected evidence byte budget exceeded"),
        "{error:#}"
    );
    assert!(store.recovery_pending_for_tests());
    let rust_id: String = db
        .query_row(
            "SELECT 'sid:v1:'||lower(hex(syntax_id)) FROM native_version_declarations WHERE vid=?1 AND name='main'",
            [live_document_version(&db, "flow.rs")],
            |r| r.get(0),
        )
        .unwrap();
    let closed = selected_clone.symbol_at(&rust_id, Some(pin)).unwrap_err();
    assert_current_corruption(closed);
}

#[test]
#[cfg(feature = "test-support")]
fn every_encoded_selected_id_rejects_wrong_length_blob() {
    let (state, root, store, cancel) = fixture();
    let java = "class Base {}\nclass Demo<T> extends Base {\n  <U> U map(U input) { if (input != null) { if (input.hashCode() > 0) { helper(); } } return input; }\n  void helper() {}\n}\n";
    fs::write(root.path().join("flow.java"), java).unwrap();
    let options = IndexOptions::new(root.path().to_owned());
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let leader = store.publication_for_tests().unwrap();
    let pin = store
        .publish_native(
            &graph,
            &capture,
            &native,
            &leader,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let revision = format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision);
    assert!(store.methods_at("flow.java", Some(pin)).unwrap().is_some());
    let db = Connection::open(published_db(state.path(), root.path())).unwrap();
    // A wrong-length parent key deliberately violates FKs. Disable them only
    // on this mutation connection; the selected reader opens its own handle.
    db.pragma_update(None, "foreign_keys", "OFF").unwrap();
    // These are the SID/OCC columns a selected document read decodes,
    // including nullable parent keys. The fixture must populate every one.
    // Reads trust the published index (#261), but a row that will not decode
    // is corruption: it fails typed and schedules a re-index.
    // Pin each mutation to the Java fixture that populates all columns. When
    // changing a grouping key, remove the group's final ordinal: removing an
    // earlier row can expose a gap before the malformed key is decoded.
    let cases = [
        (
            "native_version_declarations",
            "syntax_id",
            "vid",
            "t.syntax_id",
        ),
        (
            "native_version_declarations",
            "owner_syntax_id",
            "vid",
            "t.syntax_id",
        ),
        (
            "native_version_declaration_ancestors",
            "syntax_id",
            "vid",
            "t.syntax_id,t.ordinal DESC",
        ),
        (
            "native_version_own_signature_types",
            "syntax_id",
            "vid",
            "t.syntax_id,t.ordinal DESC",
        ),
        (
            "native_version_ancestor_signature_types",
            "syntax_id",
            "vid",
            "t.syntax_id,t.ancestor_ordinal,t.ordinal DESC",
        ),
        ("native_version_headers", "syntax_id", "vid", "t.syntax_id"),
        (
            "native_version_header_items",
            "syntax_id",
            "vid",
            "t.syntax_id,t.item_kind,t.ordinal DESC",
        ),
        (
            "native_version_parameters",
            "syntax_id",
            "vid",
            "t.syntax_id,t.ordinal DESC",
        ),
        (
            "native_version_calls",
            "id",
            "vid",
            "t.owner_syntax_id,t.ordinal",
        ),
        (
            "native_version_calls",
            "owner_syntax_id",
            "vid",
            "t.owner_syntax_id,t.ordinal DESC",
        ),
        (
            "native_version_control_regions",
            "id",
            "vid",
            "t.owner_syntax_id,t.ordinal",
        ),
        (
            "native_version_control_regions",
            "owner_syntax_id",
            "vid",
            "t.owner_syntax_id,t.ordinal DESC",
        ),
        (
            "native_version_control_regions",
            "parent_id",
            "vid",
            "t.owner_syntax_id,t.ordinal",
        ),
        (
            "native_version_call_regions",
            "call_id",
            "vid",
            "t.call_id,t.ordinal DESC",
        ),
        (
            "native_version_call_regions",
            "region_id",
            "vid",
            "t.call_id,t.ordinal",
        ),
    ];
    for (table, column, scope, order) in cases {
        let selected = format!(
            "SELECT t.rowid,d.path FROM {table} t JOIN revision_documents d \
             ON t.{scope}=d.{scope} WHERE d.revision_id=?1 AND d.path='flow.java' \
             AND t.{column} IS NOT NULL ORDER BY {order} LIMIT 1"
        );
        let (rowid, path): (i64, String) = db
            .query_row(&selected, [&revision], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap_or_else(|error| panic!("missing populated {table}.{column}: {error}"));
        let original: Vec<u8> = db
            .query_row(
                &format!("SELECT {column} FROM {table} WHERE rowid=?1"),
                [rowid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(original.len(), 16, "{table}.{column}");
        // This deliberately breaks child FKs when an encoded primary key is
        // changed. It exercises read-side decode, not SQLite write validation.
        assert_eq!(
            db.execute(
                &format!("UPDATE {table} SET {column}=x'01' WHERE rowid=?1"),
                [rowid],
            )
            .unwrap(),
            1
        );
        let reader = Store::open_for_tests(state.path(), root.path()).unwrap();
        assert_eq!(reader.status().unwrap().revision, pin);
        let closed = reader.clone();
        let error = reader.methods_at(&path, Some(pin)).unwrap_err();
        assert!(
            error
                .chain()
                .any(|cause| cause.to_string().starts_with("incompatible_index:")),
            "{table}.{column}: {error:#}"
        );
        assert!(reader.recovery_pending_for_tests());
        // The failure latched a rebuild: status now requires reconciliation.
        assert_current_corruption(closed.status().unwrap_err());
        db.execute(
            &format!("UPDATE {table} SET {column}=?1 WHERE rowid=?2"),
            rusqlite::params![original, rowid],
        )
        .unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r
                .get::<_, i64>(
                0
            ))
            .unwrap(),
            0,
            "restored {table}.{column}"
        );
    }
    db.pragma_update(None, "foreign_keys", "ON").unwrap();
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
    for report in &measured.capability_reports {
        assert_eq!(
            store
                .native_capability_report_at(pin, &report.document)
                .unwrap()
                .as_ref(),
            Some(report)
        );
    }
    for declaration in &measured.declarations {
        assert_eq!(
            store
                .native_references_at(pin, &declaration.syntax_id)
                .unwrap(),
            measured
                .references
                .iter()
                .filter(|r| r.owner_syntax_id == declaration.syntax_id)
                .cloned()
                .collect::<Vec<_>>()
        );
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
    let (version, graph_projection, class_projection): (i64, i64, i64) = db
        .query_row(
            "SELECT vid,gid,cid
             FROM revision_documents WHERE revision_id=?1 AND path='flow.java'",
            [&r2_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    // A surviving revision owns all three materialized record families before
    // GC. Recheck these exact IDs after GC rather than only checking FK health.
    let retained_rows = [
        ("document_versions", "vid", version),
        ("graph_projections", "gid", graph_projection),
        ("class_projections", "cid", class_projection),
    ];
    for (table, column, id) in &retained_rows {
        let count: i64 = db
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE {column}=?1"),
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
    for (table, column, id) in &retained_rows {
        let count: i64 = db
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE {column}=?1"),
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "fenced GC lost retained {table}/{id}");
    }
    let missing: i64 = db
        .query_row(
            "SELECT count(*) FROM document_versions v WHERE NOT EXISTS
        (SELECT 1 FROM revision_documents m WHERE m.vid=v.vid)",
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
fn predecessor_stamps_cover_full_publications_and_unchanged_stamps_nothing() {
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
        r2, r1,
        "an unchanged capture keeps the head (Decision 0006 §3)"
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM native_revision_supersessions",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0,
        "an unchanged capture supersedes nothing"
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
    assert_eq!(rows.len(), 1);
    for (pin, (key, second)) in [r1].iter().zip(&rows) {
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
fn current_maintenance_keeps_inode_generation_and_unfinished_fifo() {
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
    assert_eq!(installed, 1, "maintenance table belongs to the base schema");
    assert!(reopened.graph_at(Some(old)).is_ok());
    let preserved = reopened.request_by_id(&pending.id).unwrap().unwrap();
    assert_eq!(preserved.id, pending.id);
    assert_eq!(preserved.state, "queued");
    assert_eq!(reopened.status().unwrap().revision, next);
}

#[test]
#[cfg(feature = "test-support")]
fn current_retention_indexes_survive_publication_without_replacing_pin() {
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
        "revision_documents_path",
    ] {
        let present: i64 = db
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name=?1",
                [name],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(present, 1, "{name} missing from current schema");
    }
    assert!(reopened.graph_at(Some(old)).is_ok());
}

#[test]
#[cfg(feature = "test-support")]
fn current_released_tombstone_survives_reopen_and_publication() {
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
    let shared: (i64, i64, i64) = db
        .query_row(
            "SELECT vid,gid,cid FROM revision_documents WHERE revision_id=?1 AND path='flow.java'",
            [&key2],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    let retired: (i64, i64, i64) = db
        .query_row(
            "SELECT vid,gid,cid FROM revision_documents WHERE revision_id=?1 AND path='flow.js'",
            [&key2],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
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
    for (table, column, id) in [
        ("document_versions", "vid", shared.0),
        ("graph_projections", "gid", shared.1),
        ("class_projections", "cid", shared.2),
    ] {
        assert_eq!(
            db.query_row(
                &format!("SELECT count(*) FROM {table} WHERE {column}=?1"),
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
    // Once the younger pin expires, its unique surrogate families are gone,
    // while all three unchanged Java families remain referenced by the head.
    store.set_retention_clock_for_tests(1_901, 901);
    store.maintain_revisions(&leader).unwrap();
    assert!(
        store
            .source_at("flow.js", Some(r2))
            .unwrap_err()
            .downcast_ref::<trellis::store::PinExpired>()
            .is_some()
    );
    for (table, column, shared_id, retired_id) in [
        ("document_versions", "vid", shared.0, retired.0),
        ("graph_projections", "gid", shared.1, retired.1),
        ("class_projections", "cid", shared.2, retired.2),
    ] {
        assert_ne!(shared_id, retired_id, "{table} must differ across paths");
        assert_eq!(
            db.query_row(
                &format!("SELECT count(*) FROM {table} WHERE {column}=?1"),
                [shared_id],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1,
            "shared {table}"
        );
        assert_eq!(
            db.query_row(
                &format!("SELECT count(*) FROM {table} WHERE {column}=?1"),
                [retired_id],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0,
            "orphaned {table}"
        );
    }
    assert_eq!(
        db.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r
            .get::<_, i64>(
            0
        ))
        .unwrap(),
        0
    );
    assert!(store.source_at("flow.js", Some(r3)).unwrap().is_some());
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
fn failed_current_publication_preserves_old_pin_and_queue() {
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
        1,
        "cancelled transaction keeps the current base schema"
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
        "successful retry keeps the current base schema"
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
fn schemas16_19_23_24_26_27_and28_recreate_exclusively_without_migration_and_preserve_queue() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    for old_version in [16, 19, 23, 24, 26, 27, 28] {
        let (state, root, prior, cancel) = fixture();
        let queued = prior
            .enqueue_request(&IndexOptions::new(root.path().to_owned()), None)
            .unwrap();
        let path = published_db(state.path(), root.path());
        let old_generation = prior.index_baseline().unwrap().index_generation;
        drop(prior);
        if old_version == 16 {
            fs::remove_file(&path).unwrap();
            let identity = trellis::store::topology::WorkspaceIdentity::discover(
                Some(root.path()),
                root.path(),
            )
            .unwrap();
            let db = Connection::open(&path).unwrap();
            db.execute_batch(include_str!("fixtures/gc-legacy/v16-current.sql"))
                .unwrap();
            db.pragma_update(None, "user_version", 16).unwrap();
            db.execute("INSERT INTO index_metadata VALUES(1,16,'native-v6-delta-v1',?1,?2,?3,?4,0,0,'','{}','[]',NULL)",
                rusqlite::params![identity.root.to_str().unwrap(),identity.device.to_string(),identity.inode.to_string(),old_generation.to_string()]).unwrap();
        } else {
            // Exercise paired obsolete markers, not historical schema layouts.
            let db = Connection::open(&path).unwrap();
            db.execute_batch(&format!(
                "PRAGMA ignore_check_constraints=ON; UPDATE index_metadata SET schema_version={old_version}; PRAGMA user_version={old_version};",
            ))
            .unwrap();
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let old_inode = fs::metadata(&path).unwrap().ino();
        let old_bytes = fs::read(&path).unwrap();
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().ino(), old_inode);
        assert_eq!(
            fs::read(&path).unwrap(),
            old_bytes,
            "opening must defer obsolete-index replacement"
        );
        let error = store.status().unwrap_err();
        assert!(
            error.to_string().contains("recovery_required"),
            "schema{old_version}: {error:#}"
        );
        let (pin, _owner) = trellis::index_coordinator::reconcile_workspace(
            &store,
            &IndexOptions::new(root.path().to_owned()),
            &cancel,
            |_| {},
        )
        .unwrap();
        assert_ne!(pin.index_generation, old_generation);
        assert_eq!(pin.index_revision, 1);
        assert_ne!(
            fs::metadata(&path).unwrap().ino(),
            old_inode,
            "disposable schema{old_version} must recreate, never migrate"
        );
        assert_eq!(
            store.request_by_id(&queued.id).unwrap().unwrap().seq,
            queued.seq
        );
        assert_eq!(
            store.request_by_id(&queued.id).unwrap().unwrap().state,
            "queued"
        );
        let rebuilt = Connection::open(&path).unwrap();
        assert_eq!(
            rebuilt
                .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            29
        );
        assert_eq!(
            rebuilt
                .query_row("SELECT schema_version FROM index_metadata", [], |row| row
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            29
        );
        for table in [
            "native_version_references",
            "native_version_capability_families",
            "native_version_capability_gaps",
        ] {
            assert_eq!(
                rebuilt
                    .query_row(
                        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                        [table],
                        |r| r.get::<_, i64>(0)
                    )
                    .unwrap(),
                1
            );
        }
    }
}
#[test]
#[cfg(feature = "test-support")]
fn selected_capture_keeps_r1_and_all_raw_proofs_until_last_selector_expires() {
    let (_state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    store.set_retention_clock_for_tests(1_000, 0);
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
    let (batch_id, capture_id) = store.with_index_sql_for_tests(|db| {
        let hash = "0".repeat(64);
        db.execute("INSERT INTO semantic_raw_batches(producer_id,producer_version,profile_id,source_set_id,captured_revision,source_manifest_hash,descriptor_sha256,descriptor_bytes,decoder_version) SELECT 'fixture','fixture','fixture',source_set_id,native_revision_id,?2,?2,x'02','fixture' FROM native_revisions WHERE published_index_revision=?1", rusqlite::params![r1.index_revision as i64, hash])?;
        let batch_id = db.last_insert_rowid();
        db.execute("INSERT INTO semantic_raw_captures(batch_id,ordinal,language,producer_id,artifact_hash,source_set_id,captured_revision,binary_bytes,decoder_version) SELECT ?1,0,'javascript','fixture',?3,source_set_id,native_revision_id,x'01','fixture' FROM native_revisions WHERE published_index_revision=?2", rusqlite::params![batch_id, r1.index_revision as i64, hash])?;
        let capture_id = db.last_insert_rowid();
        db.execute("INSERT INTO semantic_raw_exclusions(batch_id,language,path,reason) VALUES(?1,'javascript','flow.js','failed')", [batch_id])?;
        for i in 0..70_u32 {
            db.execute("INSERT INTO semantic_raw_proofs(capture_id,proof_id,record_kind,fact_digest,language,path,canonical_bytes) VALUES(?1,?2,'occurrence',?3,'javascript','flow.js',x'03')",rusqlite::params![capture_id,format!("unselected-{i}"),format!("{i:064x}")])?;
        }
        db.execute("INSERT INTO semantic_revision_raw_selections(revision_id,producer_id,batch_id,captured_pin) VALUES(?1,'fixture',?2,?3)",rusqlite::params![format!("pin:v1:{}:{}",r2.index_generation,r2.index_revision),batch_id,format!("pin:v1:{}:{}",r1.index_generation,r1.index_revision)])?;
        Ok((batch_id, capture_id))
    }).unwrap();
    // Keep r1 younger than the release horizon while r2 becomes due, so an
    // unrelated r1 release cannot drain leftover proof batches.
    store
        .with_index_sql_for_tests(|db| {
            db.execute(
                "UPDATE native_revision_supersessions SET superseded_at=1901 WHERE revision_id=?1",
                [format!(
                    "pin:v1:{}:{}",
                    r1.index_generation, r1.index_revision
                )],
            )?;
            Ok(())
        })
        .unwrap();
    store.set_retention_clock_for_tests(1_901, 901);
    let (retained_native, retained_sources) = store.retained_native_snapshot_at(r1).unwrap();
    assert!(!retained_native.revision.documents.is_empty());
    assert!(!retained_native.declarations.is_empty());
    assert_eq!(
        retained_sources.len(),
        retained_native.revision.documents.len()
    );
    store
        .with_index_sql_for_tests(|db| {
            let retained: i64 = db.query_row(
                "SELECT count(*) FROM semantic_raw_proofs WHERE capture_id=?1",
                [capture_id],
                |r| r.get(0),
            )?;
            assert_eq!(retained, 70);
            Ok(())
        })
        .unwrap();
    store.set_retention_clock_for_tests(1_902, 902);
    store.release_revision(r2, &leader).unwrap();
    store
        .with_index_sql_for_tests(|db| {
            let selections: i64 = db.query_row(
                "SELECT count(*) FROM semantic_revision_raw_selections WHERE batch_id=?1",
                [batch_id],
                |r| r.get(0),
            )?;
            let proofs: i64 = db.query_row(
                "SELECT count(*) FROM semantic_raw_proofs WHERE capture_id=?1",
                [capture_id],
                |r| r.get(0),
            )?;
            let captures: i64 = db.query_row(
                "SELECT count(*) FROM semantic_raw_captures WHERE capture_id=?1",
                [capture_id],
                |r| r.get(0),
            )?;
            let (batches, exclusions): (i64, i64) = db.query_row(
                "SELECT (SELECT count(*) FROM semantic_raw_batches WHERE batch_id=?1),(SELECT count(*) FROM semantic_raw_exclusions WHERE batch_id=?1)",
                [batch_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            assert_eq!(
                (selections, proofs, captures, batches, exclusions),
                (0, 0, 0, 0, 0)
            );
            Ok(())
        })
        .unwrap();
    store.set_retention_clock_for_tests(2_802, 1_803);
    store.release_revision(r1, &leader).unwrap();
    assert_eq!(store.status().unwrap().revision, r3);
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
    let db = Connection::open(&path).unwrap();
    db.execute_batch("PRAGMA user_version=30").unwrap();
    drop(db);
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
    assert_eq!(version, 30);
    let metadata_version: i64 = db
        .query_row("SELECT schema_version FROM index_metadata", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(metadata_version, 29);
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
        "SELECT count(*) FROM native_version_declarations WHERE vid=(SELECT vid FROM revision_documents WHERE revision_id=?1 AND path='large.js')",
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

#[test]
#[cfg(feature = "test-support")]
fn declaration_roles_round_trip_and_invalid_stored_role_fails_typed_read() {
    use trellis::native_evidence::DeclarationRole;
    let (state, root, store, cancel) = fixture();
    fs::write(
        root.path().join("tests_role.rs"),
        "fn production() {}\n#[cfg(test)] mod tests { fn inline_helper() {} }\n",
    )
    .unwrap();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let test_rows = store
        .native_declarations_at(pin, "rust", "inline_helper")
        .unwrap();
    assert_eq!(test_rows.len(), 1);
    assert_eq!(test_rows[0].role, DeclarationRole::Test);
    let production_rows = store
        .native_declarations_at(pin, "rust", "production")
        .unwrap();
    assert_eq!(production_rows.len(), 1);
    assert_eq!(production_rows[0].role, DeclarationRole::Production);
    let db = Connection::open(published_db(state.path(), root.path())).unwrap();
    db.execute_batch("PRAGMA ignore_check_constraints=ON")
        .unwrap();
    db.execute(
        "UPDATE native_version_declarations SET role='unknown' WHERE syntax_id=unhex(substr(?1,8))",
        [&test_rows[0].syntax_id],
    )
    .unwrap();
    let error = store
        .native_declarations_at(pin, "rust", "inline_helper")
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("invalid declaration role"),
        "{error:#}"
    );
    assert!(store.recovery_pending_for_tests());
    assert!(
        db.execute(
            "UPDATE native_version_declarations SET role=NULL WHERE syntax_id=unhex(substr(?1,8))",
            [&test_rows[0].syntax_id]
        )
        .is_err()
    );
}

#[test]
#[cfg(feature = "test-support")]
fn incoming_call_index_uses_captured_version_and_nullable_measured_keys() {
    let (state, root, store, cancel) = fixture();
    fs::write(
        root.path().join("flow.js"),
        "function go() {} function caller() { go(); obj.go(); (obj.go)(); }
",
    )
    .unwrap();
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
    let columns: Vec<String> = db
        .prepare("PRAGMA index_info(native_version_calls_incoming)")
        .unwrap()
        .query_map([], |row| row.get(2))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(columns, ["language", "callee_lookup_key"]);
    type CallIndexRow = (String, Option<String>, Option<String>, String);
    let rows: Vec<CallIndexRow> = db
        .prepare("SELECT c.language,c.callee_lookup_key,c.spelling,v.path FROM native_version_calls c JOIN document_versions v ON v.vid=c.vid ORDER BY v.path,c.ordinal")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(rows.iter().all(|(language, _, _, path)| {
        language
            == match path.rsplit('.').next().unwrap() {
                "js" => "javascript",
                "rs" => "rust",
                "py" => "python",
                "java" => "java",
                other => panic!("unexpected language: {other}"),
            }
    }));
    let js: Vec<_> = rows.iter().filter(|row| row.3 == "flow.js").collect();
    assert!(
        js.iter()
            .any(|row| row.1.as_deref() == Some("go") && row.2.as_deref() == Some("go"))
    );
    assert!(js.iter().any(|row| {
        row.1.is_none()
            && row
                .2
                .as_deref()
                .is_some_and(|spelling| spelling.contains("obj"))
    }));
    drop(db);
    fs::write(
        root.path().join("flow.js"),
        "function changed() {}
",
    )
    .unwrap();
    let stored = Connection::open(published_db(state.path(), root.path())).unwrap();
    let key: String = stored.query_row(
        "SELECT callee_lookup_key FROM native_version_calls c JOIN document_versions v ON v.vid=c.vid WHERE v.path='flow.js' AND c.callee_lookup_key='go' LIMIT 1",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(key, "go", "published key must not follow live source edits");
    assert_eq!(store.status().unwrap().revision, pin);
}

#[test]
#[cfg(feature = "test-support")]
fn rust_scoped_final_callee_tokens_publish_and_join_incoming_without_resolution() {
    let (state, root, store, cancel) = fixture();
    let source = "fn target() {}
fn caller() { Self::target(); module::target(); Type::target(); Type::<u8>::target(); module::target::<u8>(); (module::target())(); }
";
    fs::write(root.path().join("flow.rs"), source).unwrap();
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
    let stored: Vec<(i64, i64, String)> = db.prepare("SELECT callee_start,callee_end,spelling FROM native_version_calls WHERE vid=?1 AND callee_lookup_key='target' ORDER BY start_byte")
        .unwrap().query_map([live_document_version(&db, "flow.rs")], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap().collect::<rusqlite::Result<_>>().unwrap();
    assert_eq!(
        stored.len(),
        6,
        "five scoped calls and compound inner call only"
    );
    for (start, end, spelling) in &stored {
        assert_eq!(&source[*start as usize..*end as usize], "target");
        assert_eq!(spelling, "target");
    }
    let unmeasured_outer: i64 = db
        .query_row(
            "SELECT count(*) FROM native_version_calls WHERE vid=?1 AND callee_lookup_key IS NULL",
            [live_document_version(&db, "flow.rs")],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        unmeasured_outer, 1,
        "compound outer call has no lone identifier callee token"
    );
    drop(db);
    let read = store.evidence_response().unwrap();
    let selected = read
        .mcp_find("target", 20, Some(pin))
        .unwrap()
        .symbols
        .into_iter()
        .find(|s| {
            s.declaration.document.path == "flow.rs"
                && s.declaration.name.as_deref() == Some("target")
        })
        .unwrap()
        .declaration;
    let incoming = read
        .mcp_incoming_with_lines(&selected.syntax_id, 20, Some(pin))
        .unwrap()
        .unwrap();
    assert_eq!(incoming.calls.len(), 6);
    assert!(
        incoming
            .calls
            .iter()
            .all(|c| c.callee_text == "target" && c.caller.name.as_deref() == Some("caller"))
    );
    assert_eq!(incoming.same_name_declarations, 1);
    read.finish(()).unwrap();
}

#[test]
#[cfg(feature = "test-support")]
fn rust_scoped_non_identifier_name_has_no_verified_token_or_incoming_match() {
    use trellis::native_evidence::verified_callee_lookup;

    let (state, root, store, cancel) = fixture();
    let source = "fn r#super() {}\nfn caller() { module::super(); }\n";
    let expression = "module::super";
    let start = source.find(expression).unwrap();
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    assert!(!tree.root_node().has_error());
    let callee = tree
        .root_node()
        .named_descendant_for_byte_range(start, start + expression.len())
        .unwrap();
    assert_eq!(callee.kind(), "scoped_identifier");
    assert_eq!(callee.parent().unwrap().kind(), "call_expression");
    let name = callee.child_by_field_name("name").unwrap();
    assert_eq!(name.kind(), "super");
    assert_eq!(&source[name.byte_range()], "super");

    fs::write(root.path().join("flow.rs"), source).unwrap();
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let call = native
        .calls
        .iter()
        .find(|call| call.document.path == "flow.rs")
        .unwrap();
    assert_eq!(call.spelling.as_deref(), Some(expression));
    assert!(call.callee_range.is_none());
    assert_eq!(verified_callee_lookup("rust", call, source), None);
    let selected = native
        .declarations
        .iter()
        .find(|declaration| declaration.name.as_deref() == Some("r#super"))
        .unwrap();
    assert_eq!(selected.lookup_key.as_deref(), Some("super"));
    let selected_id = selected.syntax_id.clone();

    let leader = store.publication_for_tests().unwrap();
    let pin = store
        .publish_native(
            &graph,
            &capture,
            &native,
            &leader,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let db = Connection::open(published_db(state.path(), root.path())).unwrap();
    let stored: (Option<i64>, Option<i64>, String, Option<String>) = db
        .query_row(
            "SELECT callee_start,callee_end,spelling,callee_lookup_key FROM native_version_calls WHERE vid=?1",
            [live_document_version(&db, "flow.rs")],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(stored, (None, None, expression.into(), None));
    drop(db);
    let read = store.evidence_response().unwrap();
    let incoming = read
        .mcp_incoming_with_lines(&selected_id, 20, Some(pin))
        .unwrap()
        .unwrap();
    assert_eq!(incoming.same_name_declarations, 1);
    assert!(incoming.calls.is_empty());
    read.finish(()).unwrap();
}

#[test]
#[cfg(feature = "test-support")]
fn incoming_read_keeps_reverse_roles_scope_and_required_lines() {
    use trellis::native_evidence::DeclarationRole;
    let (state, root, store, cancel) = fixture();
    fs::write(root.path().join("flow.js"),
        "function Target() { Target(); }\nfunction Other() { Target(); function Nested() { Target(); } }\n").unwrap();
    fs::write(
        root.path().join("near.js"),
        "function Near() { Target(); }\n",
    )
    .unwrap();
    fs::write(
        root.path().join("near.test.js"),
        "function Tester() { Target(); }\n",
    )
    .unwrap();
    fs::create_dir(root.path().join("far")).unwrap();
    fs::write(
        root.path().join("far/far.js"),
        "function Far() { Target(); }\n",
    )
    .unwrap();
    fs::write(root.path().join("far/target.js"), "function Target() {}\n").unwrap();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let read = store.evidence_response().unwrap();
    let target = read
        .mcp_find("Target", 20, Some(pin))
        .unwrap()
        .symbols
        .into_iter()
        .find(|s| {
            s.declaration.document.path == "flow.js"
                && s.declaration.name.as_deref() == Some("Target")
        })
        .unwrap()
        .declaration;
    assert_eq!(target.role, DeclarationRole::Production);
    let incoming = read
        .mcp_incoming_with_lines(&target.syntax_id, 3, Some(pin))
        .unwrap()
        .unwrap();
    assert_eq!(incoming.same_name_declarations, 2);
    assert!(incoming.has_more);
    assert_eq!(incoming.calls.len(), 3);
    assert_eq!(incoming.calls[0].caller.syntax_id, target.syntax_id);
    assert_eq!(incoming.calls[0].call.document.path, "flow.js");
    assert_eq!(incoming.calls[1].caller.name.as_deref(), Some("Other"));
    assert_eq!(incoming.calls[2].caller.name.as_deref(), Some("Nested"));
    assert!(
        incoming
            .calls
            .iter()
            .all(|call| call.callee_text == "Target"
                && call.source.lines.is_some()
                && call.call.owner_syntax_id == call.caller.syntax_id)
    );
    let all = read
        .mcp_incoming_with_lines(&target.syntax_id, 20, Some(pin))
        .unwrap()
        .unwrap();
    assert!(!all.has_more);
    assert_eq!(
        all.calls
            .iter()
            .map(|c| c.call.document.path.as_str())
            .collect::<Vec<_>>(),
        [
            "flow.js",
            "flow.js",
            "flow.js",
            "near.js",
            "far/far.js",
            "near.test.js"
        ]
    );
    assert_eq!(all.calls.last().unwrap().caller.role, DeclarationRole::Test);
    assert_eq!(all.scope[3][0], 5);
    assert!(all.scope[..3].iter().all(|counts| *counts == [0; 5]));
    let java = read
        .mcp_find("Demo", 20, Some(pin))
        .unwrap()
        .symbols
        .into_iter()
        .find(|s| s.declaration.name.as_deref() == Some("Demo"))
        .unwrap()
        .declaration;
    let no_calls = read
        .mcp_incoming_with_lines(&java.syntax_id, 20, Some(pin))
        .unwrap()
        .unwrap();
    assert!(no_calls.calls.is_empty());
    assert_eq!(no_calls.same_name_declarations, 1);
    assert_eq!(no_calls.scope[0][0], 1);
    assert!(no_calls.scope[1..].iter().all(|counts| *counts == [0; 5]));
    let wrong = IndexPin {
        index_generation: uuid::Uuid::nil(),
        index_revision: pin.index_revision,
    };
    assert!(
        read.mcp_incoming_with_lines("sid:v1:00000000000000000000000000000000", 1, Some(wrong))
            .err()
            .unwrap()
            .to_string()
            .contains("revision conflict")
    );
    read.finish(()).unwrap();
    drop(state);
}

#[test]
#[cfg(feature = "test-support")]
fn incoming_read_scope_counts_and_null_key_are_not_call_hits() {
    let (_state, root, store, cancel) = fixture();
    fs::write(root.path().join("flow.js"), "function Target() {}\n").unwrap();
    fs::write(
        root.path().join("partial.js"),
        "function Partial() { Target(); }\nfunction broken( {\n",
    )
    .unwrap();
    fs::write(
        root.path().join("failed.js"),
        "function Failed() { Target(); }\n",
    )
    .unwrap();
    fs::write(
        root.path().join("unselected.js"),
        "function Unselected() { Target(); }\n",
    )
    .unwrap();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let before = store.evidence_response().unwrap();
    let symbols = before.mcp_find("sid:v1:", 50, Some(pin)).unwrap().symbols;
    let target = symbols
        .iter()
        .find(|s| s.declaration.name.as_deref() == Some("Target"))
        .unwrap()
        .declaration
        .syntax_id
        .clone();
    let anonymous = symbols
        .iter()
        .find(|s| s.declaration.document.path == "flow.js" && s.declaration.lookup_key.is_none())
        .unwrap()
        .declaration
        .syntax_id
        .clone();
    drop(before);
    store
        .mcp_set_coverage_for_tests(&leader, "failed.js", true, "failed")
        .unwrap();
    store
        .mcp_set_coverage_for_tests(&leader, "unselected.js", false, "unsupported")
        .unwrap();
    let read = store.evidence_response().unwrap();
    let incoming = read
        .mcp_incoming_with_lines(&target, 20, Some(pin))
        .unwrap()
        .unwrap();
    assert_eq!(
        incoming
            .calls
            .iter()
            .map(|c| c.call.document.path.as_str())
            .collect::<Vec<_>>(),
        ["partial.js"]
    );
    assert_eq!(incoming.scope[3], [3, 1, 1, 1, 1]);
    assert!(incoming.scope[..3].iter().all(|row| *row == [0; 5]));
    assert_eq!(incoming.same_name_declarations, 1);
    let empty = read
        .mcp_incoming_with_lines(&anonymous, 20, Some(pin))
        .unwrap()
        .unwrap();
    assert_eq!(empty.same_name_declarations, 0);
    assert!(empty.calls.is_empty());
    assert!(!empty.has_more);
    assert_eq!(empty.scope, incoming.scope);
    read.finish(()).unwrap();
}

#[test]
#[cfg(feature = "test-support")]
fn incoming_read_excludes_other_language_and_prior_revision() {
    let (_state, root, store, cancel) = fixture();
    fs::write(
        root.path().join("flow.js"),
        "function Target() {}\nfunction Old() { Target(); }\n",
    )
    .unwrap();
    fs::write(
        root.path().join("flow.rs"),
        "fn Target() {} fn RustCaller() { Target(); }\n",
    )
    .unwrap();
    let leader = store.publication_for_tests().unwrap();
    let old_pin = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let old = store.evidence_response().unwrap();
    let old_target = old
        .mcp_find("Target", 20, Some(old_pin))
        .unwrap()
        .symbols
        .into_iter()
        .find(|s| {
            s.declaration.document.path == "flow.js"
                && s.declaration.name.as_deref() == Some("Target")
        })
        .unwrap()
        .declaration
        .syntax_id;
    assert_eq!(
        old.mcp_incoming_with_lines(&old_target, 20, Some(old_pin))
            .unwrap()
            .unwrap()
            .calls
            .len(),
        1
    );
    drop(old);
    fs::write(root.path().join("flow.js"), "function Target() {}\n").unwrap();
    let pin = publish(&store, root.path(), &cancel, old_pin, &leader).unwrap();
    let read = store.evidence_response().unwrap();
    let current = read
        .mcp_find("Target", 20, Some(pin))
        .unwrap()
        .symbols
        .into_iter()
        .find(|s| {
            s.declaration.document.path == "flow.js"
                && s.declaration.name.as_deref() == Some("Target")
        })
        .unwrap()
        .declaration
        .syntax_id;
    assert!(
        read.mcp_incoming_with_lines(&current, 20, Some(pin))
            .unwrap()
            .unwrap()
            .calls
            .is_empty()
    );
    assert_eq!(
        read.mcp_incoming_with_lines(&current, 20, Some(pin))
            .unwrap()
            .unwrap()
            .same_name_declarations,
        1
    );
    assert!(
        read.mcp_incoming_with_lines(&current, 20, Some(old_pin))
            .err()
            .unwrap()
            .to_string()
            .contains("revision conflict")
    );
    read.finish(()).unwrap();
}

#[test]
#[cfg(feature = "test-support")]
fn incoming_read_rejects_mismatched_indexed_call_language() {
    let (_state, root, store, cancel) = fixture();
    fs::write(root.path().join("flow.js"), "function Target() {}\n").unwrap();
    fs::write(
        root.path().join("foreign.js"),
        "function Caller() { Target(); }\n",
    )
    .unwrap();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let first = store.evidence_response().unwrap();
    let target = first
        .mcp_find("Target", 20, Some(pin))
        .unwrap()
        .symbols
        .into_iter()
        .find(|s| s.declaration.name.as_deref() == Some("Target"))
        .unwrap()
        .declaration
        .syntax_id;
    assert_eq!(
        first
            .mcp_incoming_with_lines(&target, 20, Some(pin))
            .unwrap()
            .unwrap()
            .calls
            .len(),
        1
    );
    drop(first);
    // The call language must match the selected document, even though source-set IDs
    // are now stored once per index.
    // A mismatched indexed call must fail typed, not become a false zero hit.
    store
        .mcp_rekey_call_language_for_tests(&leader, "foreign.js", "java")
        .unwrap();
    let read = store.evidence_response().unwrap();
    let error = read
        .mcp_incoming_with_lines(&target, 20, Some(pin))
        .err()
        .unwrap();
    assert!(
        format!("{error:#}").contains("incoming indexed call scope mismatch"),
        "{error:#}"
    );
    // Direct response helpers detect the fault; the public native read owns
    // recovery reporting for the same malformed stored call.
    let caller = store
        .native_declarations_at(pin, "javascript", "Caller")
        .unwrap()[0]
        .syntax_id
        .clone();
    let reported = store.native_calls_at(pin, &caller).unwrap_err();
    assert!(
        format!("{reported:#}").contains("native call document mismatch"),
        "{reported:#}"
    );
    assert!(store.recovery_pending_for_tests());
    // The fence compares the pin only (#261): the in-body read already
    // returned the typed corruption, and the separate native read above
    // scheduled recovery.
    read.finish(()).unwrap();
}

#[test]
#[cfg(feature = "test-support")]
fn incoming_read_non_root_directory_does_not_rank_descendants_as_siblings() {
    let (_state, root, store, cancel) = fixture();
    fs::create_dir_all(root.path().join("pkg/sub")).unwrap();
    fs::write(root.path().join("pkg/target.js"), "function Target() {}\n").unwrap();
    fs::write(
        root.path().join("pkg/near.js"),
        "function Near() { Target(); }\n",
    )
    .unwrap();
    fs::write(
        root.path().join("pkg/sub/deep.js"),
        "function Deep() { Target(); }\n",
    )
    .unwrap();
    fs::write(
        root.path().join("elsewhere.js"),
        "function Elsewhere() { Target(); }\n",
    )
    .unwrap();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let read = store.evidence_response().unwrap();
    let target = read
        .mcp_find("Target", 20, Some(pin))
        .unwrap()
        .symbols
        .into_iter()
        .find(|s| s.declaration.name.as_deref() == Some("Target"))
        .unwrap()
        .declaration
        .syntax_id;
    let incoming = read
        .mcp_incoming_with_lines(&target, 20, Some(pin))
        .unwrap()
        .unwrap();
    assert_eq!(
        incoming
            .calls
            .iter()
            .map(|c| c.call.document.path.as_str())
            .collect::<Vec<_>>(),
        ["pkg/near.js", "elsewhere.js", "pkg/sub/deep.js"]
    );
    read.finish(()).unwrap();
}

#[test]
#[cfg(feature = "test-support")]
fn incoming_read_invalid_role_after_limited_valid_hits_fails_typed() {
    let (_state, root, store, cancel) = fixture();
    fs::write(root.path().join("target.js"), "function Target() {}\n").unwrap();
    for n in 0..4 {
        fs::write(
            root.path().join(format!("caller{n}.js")),
            format!("function Caller{n}() {{ Target(); }}\n"),
        )
        .unwrap();
    }
    fs::write(
        root.path().join("bad.js"),
        "function BadCaller() { Target(); }\n",
    )
    .unwrap();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let initial = store.evidence_response().unwrap();
    let target = initial
        .mcp_find("Target", 20, Some(pin))
        .unwrap()
        .symbols
        .into_iter()
        .find(|s| s.declaration.name.as_deref() == Some("Target"))
        .unwrap()
        .declaration
        .syntax_id;
    assert_eq!(
        initial
            .mcp_incoming_with_lines(&target, 2, Some(pin))
            .unwrap()
            .unwrap()
            .calls
            .len(),
        2
    );
    drop(initial);
    store
        .mcp_corrupt_caller_role_for_tests(&leader, "BadCaller")
        .unwrap();
    let read = store.evidence_response().unwrap();
    let outcome = read.mcp_incoming_with_lines(&target, 2, Some(pin));
    assert!(
        outcome.is_err(),
        "malformed keyed role must fail despite the limit"
    );
    let error = outcome.err().unwrap();
    assert!(
        format!("{error:#}").contains("invalid declaration role"),
        "{error:#}"
    );
    let reported = store
        .native_declarations_at(pin, "javascript", "BadCaller")
        .unwrap_err();
    assert!(
        format!("{reported:#}").contains("invalid declaration role"),
        "{reported:#}"
    );
    assert!(store.recovery_pending_for_tests());
    // The fence compares the pin only (#261): the in-body read already
    // returned the typed corruption, and the separate native read above
    // scheduled recovery.
    read.finish(()).unwrap();
}

#[test]
#[cfg(feature = "test-support")]
fn incoming_read_separates_two_valid_source_set_roots() {
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    // The native producer assigns one source set per revision. Two real roots
    // give two valid source sets without inventing a multi-set native revision.
    let mut source_sets = Vec::new();
    for (caller_count, name) in [(1, "First"), (2, "Second")] {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut text = "function Target() {}\n".to_owned();
        for n in 0..caller_count {
            text.push_str(&format!("function {name}{n}() {{ Target(); }}\n"));
        }
        fs::write(root.path().join("flow.js"), text).unwrap();
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        let leader = store.publication_for_tests().unwrap();
        let pin = publish(
            &store,
            root.path(),
            &cancel,
            store.index_baseline().unwrap(),
            &leader,
        )
        .unwrap();
        let read = store.evidence_response().unwrap();
        let target = read
            .mcp_find("Target", 20, Some(pin))
            .unwrap()
            .symbols
            .into_iter()
            .find(|s| s.declaration.name.as_deref() == Some("Target"))
            .unwrap()
            .declaration
            .syntax_id;
        let result = read
            .mcp_incoming_with_lines(&target, 20, Some(pin))
            .unwrap()
            .unwrap();
        source_sets.push(
            result
                .selected
                .selected
                .declaration
                .document
                .source_set_id
                .clone(),
        );
        assert_eq!(result.calls.len(), caller_count);
        assert_eq!(result.same_name_declarations, 1);
        assert_eq!(result.scope[3], [1, 1, 0, 0, 0]);
        assert!(result.calls.iter().all(|c| {
            c.caller
                .name
                .as_deref()
                .is_some_and(|caller| caller.starts_with(name))
        }));
        assert!(result.calls.iter().all(|c| c.call.document.source_set_id
            == result.selected.selected.declaration.document.source_set_id));
        read.finish(()).unwrap();
    }
    assert_ne!(source_sets[0], source_sets[1]);
}

#[test]
#[cfg(feature = "test-support")]
fn incoming_four_languages_restrict_to_measured_name_and_order_near_before_test() {
    // Each case exercises the same reverse-query contract on a real published snapshot.
    for (extension, target, near, other, far, test) in [
        (
            "java",
            "class Target { void go() { go(); this.go(); (go())(); } }",
            "class Near { void go() { new Target().go(); } }",
            "class Other { void go() { new Target().go(); } }",
            "class Far { void go() { new Target().go(); } }",
            "class Tester { void go() { new Target().go(); } }",
        ),
        (
            "rs",
            "fn target() { target(); obj.target(); (target())(); }",
            "fn near() { obj.target(); }",
            "fn other() { obj.target(); }",
            "fn far() { obj.target(); }",
            "fn tester() { obj.target(); }",
        ),
        (
            "py",
            "def target():\n    target()\n    obj.target()\n    (target())()\n",
            "def near():\n    obj.target()\n",
            "def other():\n    obj.target()\n",
            "def far():\n    obj.target()\n",
            "def tester():\n    obj.target()\n",
        ),
        (
            "js",
            "function target() { target(); obj.target(); (target())(); }",
            "function near() { obj.target(); }",
            "function other() { obj.target(); }",
            "function far() { obj.target(); }",
            "function tester() { obj.target(); }",
        ),
    ] {
        let (_state, root, store, cancel) = fixture();
        fs::create_dir(root.path().join("far")).unwrap();
        fs::create_dir(root.path().join("tests")).unwrap();
        for (path, body) in [
            (format!("target.{extension}"), target),
            (format!("near.{extension}"), near),
            (format!("other.{extension}"), other),
            (format!("far/far.{extension}"), far),
            (format!("tests/caller.test.{extension}"), test),
        ] {
            fs::write(root.path().join(path), body).unwrap();
        }
        let leader = store.publication_for_tests().unwrap();
        let pin = publish(
            &store,
            root.path(),
            &cancel,
            store.index_baseline().unwrap(),
            &leader,
        )
        .unwrap();
        let read = store.evidence_response().unwrap();
        let selected = read
            .mcp_find(
                if extension == "java" { "go" } else { "target" },
                20,
                Some(pin),
            )
            .unwrap()
            .symbols
            .into_iter()
            .find(|symbol| {
                symbol.declaration.document.path == format!("target.{extension}")
                    && symbol.declaration.name.as_deref()
                        == Some(if extension == "java" { "go" } else { "target" })
            })
            .unwrap()
            .declaration;
        let incoming = read
            .mcp_incoming_with_lines(&selected.syntax_id, 20, Some(pin))
            .unwrap()
            .unwrap();
        let paths: Vec<_> = incoming
            .calls
            .iter()
            .map(|item| item.call.document.path.as_str())
            .collect();
        assert!(
            paths.iter().any(|p| *p == format!("target.{extension}")),
            "{extension}: recursion {paths:?}"
        );
        assert!(
            paths.iter().any(|p| *p == format!("near.{extension}")),
            "{extension}: same-directory {paths:?}"
        );
        assert!(
            paths.iter().any(|p| *p == format!("far/far.{extension}")),
            "{extension}: distant {paths:?}"
        );
        assert!(
            paths
                .iter()
                .any(|p| *p == format!("tests/caller.test.{extension}")),
            "{extension}: test {paths:?}"
        );
        assert_eq!(
            paths.len(),
            7,
            "{extension}: only the seven measured inner/member calls {paths:?}"
        );
        assert_eq!(
            paths
                .iter()
                .filter(|p| **p == format!("target.{extension}"))
                .count(),
            3,
            "{extension}: direct recursion, member token, inner call, not compound outer call"
        );
        assert_eq!(
            incoming.same_name_declarations,
            if extension == "java" { 6 } else { 1 }
        );
        assert!(
            incoming
                .calls
                .iter()
                .all(|item| item.callee_text == if extension == "java" { "go" } else { "target" }),
            "{extension}: {paths:?}"
        );
        assert_eq!(
            incoming.calls.last().unwrap().caller.role,
            trellis::native_evidence::DeclarationRole::Test,
            "{extension}: {paths:?}"
        );
        assert!(!incoming.has_more);
        read.finish(()).unwrap();
    }
}

#[test]
#[cfg(feature = "test-support")]
fn escaped_javascript_declarations_publish_without_losing_later_facts() {
    // Each case uses a fresh real SQLite publication. A rejected declaration used to
    // abort the whole document before either the declaration or later calls reached it.
    for (source, selected_key, selected_name, later_key, later_call_key) in [
        (
            "function \\u{0000054}arget() {}\nfunction Normal() {}\nfunction Caller() { Target(); Normal(); }\n",
            "Target",
            "\\u{0000054}arget",
            "Normal",
            "Target",
        ),
        (
            "class A { d\\u0065lete() {} normal() {} }\nfunction Caller() { normal(); const a = new A(); a.delete(); a.d\\u0065lete(); }\n",
            "d\\u0065lete",
            "d\\u0065lete",
            "normal",
            "normal",
        ),
    ] {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a.js"), source).unwrap();
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
        assert_eq!(store.status().unwrap().revision, pin);
        let db = Connection::open(published_db(state.path(), root.path())).unwrap();
        let version = live_document_version(&db, "a.js");
        let stored: (String, String) = db.query_row(
            "SELECT name, lookup_key FROM native_version_declarations WHERE vid=?1 AND lookup_key=?2",
            rusqlite::params![version, selected_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(stored, (selected_name.to_owned(), selected_key.to_owned()));
        let selected = store
            .native_declarations_at(pin, "javascript", selected_key)
            .unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name.as_deref(), Some(selected_name));
        let later = store
            .native_declarations_at(pin, "javascript", later_key)
            .unwrap();
        assert_eq!(
            later.len(),
            1,
            "declarations after escaped name must be indexed"
        );
        let caller = store
            .native_declarations_at(pin, "javascript", "Caller")
            .unwrap();
        assert_eq!(caller.len(), 1);
        let calls = store.native_calls_at(pin, &caller[0].syntax_id).unwrap();
        assert!(
            calls
                .iter()
                .any(|call| call.spelling.as_deref() == Some(later_call_key))
        );
        let published_calls: i64 = db
            .query_row(
                "SELECT count(*) FROM native_version_calls WHERE vid=?1 AND callee_lookup_key=?2",
                rusqlite::params![version, later_call_key],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            published_calls > 0,
            "calls after escaped name must reach SQLite"
        );
        if selected_key.contains('\\') {
            // Neither an ordinary keyword member nor its escaped spelling can
            // produce the declaration's raw fallback key.
            let measured: Vec<(String, Option<String>)> = db
                .prepare(
                    "SELECT spelling,callee_lookup_key FROM native_version_calls \
                     WHERE vid=?1 AND spelling IN (?2,?3) ORDER BY spelling",
                )
                .unwrap()
                .query_map(rusqlite::params![version, selected_name, "delete"], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .unwrap()
                .map(Result::unwrap)
                .collect();
            assert_eq!(
                measured,
                vec![
                    (selected_name.to_owned(), None),
                    ("delete".to_owned(), Some("delete".to_owned())),
                ]
            );
        }
    }
}

#[test]
#[cfg(feature = "test-support")]
fn native_reference_and_gap_rows_survive_stream_reuse_and_retained_pins() {
    let (state, root, store, cancel) = fixture();
    fs::write(
        root.path().join("flow.rs"),
        "fn main() { run(); opaque!(hidden); }
",
    )
    .unwrap();
    let options = IndexOptions::new(root.path().to_owned());
    let (first, owner) =
        trellis::index_coordinator::reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
    drop(owner);
    let (_, first_native, _) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let rust_report = first_native
        .capability_reports
        .iter()
        .find(|r| r.document.path == "flow.rs")
        .unwrap();
    assert!(rust_report.families.iter().any(|f| !f.gaps.is_empty()));
    assert_retained_native_family_parity(&store, first, &first_native);
    fs::write(
        root.path().join("flow.js"),
        "function hello(x) { if (x) { obj.changed(); } }
",
    )
    .unwrap();
    let (second, owner) =
        trellis::index_coordinator::reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
    let (_, second_native, _) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    assert_retained_native_family_parity(&store, second, &second_native);
    assert_retained_native_family_parity(&store, first, &first_native);
    let db = Connection::open(published_db(state.path(), root.path())).unwrap();
    let versions: i64 = db
        .query_row(
            "SELECT count(*) FROM document_versions WHERE path='flow.rs'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        versions, 1,
        "unchanged Rust rows must reuse one immutable version"
    );
    drop(db);
    drop(owner);
    drop(store);
    let reopened = Store::open_for_tests(state.path(), root.path()).unwrap();
    assert_retained_native_family_parity(&reopened, first, &first_native);
    assert_retained_native_family_parity(&reopened, second, &second_native);
}

#[test]
#[cfg(feature = "test-support")]
fn reference_lookup_overrides_roundtrip_after_reopen() {
    let (state, root, store, cancel) = fixture();
    for (path, source) in [
        (
            "flow.rs",
            "fn main() { let r#type = 1; let e\u{301} = r#type; }\n",
        ),
        ("flow.py", "def go():\n    Ａ = 1\n    return Ａ\n"),
        (
            "flow.js",
            "function first() { let \\u0061 = 1; return a(); } function second() { return first; }\n",
        ),
    ] {
        fs::write(root.path().join(path), source).unwrap();
    }
    let options = IndexOptions::new(root.path().to_owned());
    let (_, measured, _) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let (pin, owner) =
        trellis::index_coordinator::reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
    assert_retained_native_family_parity(&store, pin, &measured);
    let db = Connection::open(published_db(state.path(), root.path())).unwrap();
    let mut overrides = 0;
    let mut equal = 0;
    for reference in &measured.references {
        let stored: Option<String> = db
            .query_row(
                "SELECT lookup_key FROM native_version_references WHERE vid=?1 AND owner_syntax_id=?2 AND ordinal=?3",
                rusqlite::params![
                    live_document_version(&db, &reference.document.path),
                    hex::decode(reference.owner_syntax_id.strip_prefix("sid:v1:").unwrap()).unwrap(),
                    reference.ordinal as i64
                ],
                |row| row.get(0),
            )
            .unwrap();
        if reference.spelling == reference.lookup_key {
            equal += 1;
            assert_eq!(stored, None);
        } else {
            overrides += 1;
            assert_eq!(stored.as_deref(), Some(reference.lookup_key.as_str()));
        }
    }
    assert!(equal > 0 && overrides > 0);
    for language in ["rust", "python", "javascript"] {
        assert!(
            measured.references.iter().any(|reference| {
                reference.document.language == language
                    && reference.spelling != reference.lookup_key
            }),
            "missing normalized reference for {language}"
        );
    }
    drop(db);
    drop(owner);
    drop(store);
    let reopened = Store::open_for_tests(state.path(), root.path()).unwrap();
    assert_retained_native_family_parity(&reopened, pin, &measured);
}

#[test]
#[cfg(feature = "test-support")]
fn duplicate_reference_artifact_does_not_publish() {
    let (_state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(
        &store,
        root.path(),
        &cancel,
        store.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let graph_before = store.graph_at(Some(pin)).unwrap();
    let (graph, mut native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    native.references.push(native.references[0].clone());
    assert!(
        store
            .publish_native(&graph, &capture, &native, &leader, pin, &cancel)
            .is_err()
    );
    assert_eq!(store.status().unwrap().revision, pin);
    assert_eq!(store.graph_at(Some(pin)).unwrap(), graph_before);
}

#[test]
#[cfg(feature = "test-support")]
fn reference_release_resumes_after_primary_key_batch_and_reopen() {
    let (state, root, store, cancel) = fixture();
    let source = format!("function many() {{ {} }}\n", "value;".repeat(300));
    fs::write(root.path().join("flow.js"), source).unwrap();
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
    let path = published_db(state.path(), root.path());
    let db = Connection::open(&path).unwrap();
    let retired = live_document_version(&db, "flow.js");
    let shared = live_document_version(&db, "flow.java");
    let count = |db: &Connection, vid: i64| -> i64 {
        db.query_row(
            "SELECT count(*) FROM native_version_references WHERE vid=?1",
            [vid],
            |r| r.get(0),
        )
        .unwrap()
    };
    let original = count(&db, retired);
    let shared_count = count(&db, shared);
    assert!(original > 256);
    drop(db);
    fs::write(
        root.path().join("flow.js"),
        "function current() { newest(); }\n",
    )
    .unwrap();
    store.set_retention_clock_for_tests(1_001, 1);
    let head = publish(&store, root.path(), &cancel, old, &leader).unwrap();
    store.set_retention_clock_for_tests(1_901, 901);
    let db = Connection::open(&path).unwrap();
    for _ in 0..16 {
        assert!(store.maintain_one_revision_step_for_tests(&leader).unwrap());
        if count(&db, retired) < original {
            break;
        }
    }
    assert_eq!(count(&db, retired), original - 256);
    assert_eq!(count(&db, shared), shared_count);
    assert_eq!(
        db.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r
            .get::<_, i64>(
            0
        ))
        .unwrap(),
        0
    );
    assert!(store.graph_at(Some(head)).is_ok());
    drop(db);
    drop(leader);
    drop(store);
    let reopened = Store::open_for_tests(state.path(), root.path()).unwrap();
    reopened.set_retention_clock_for_tests(1_901, 0);
    let leader = reopened.publication_for_tests().unwrap();
    let next = publish(&reopened, root.path(), &cancel, head, &leader).unwrap();
    reopened.maintain_revisions(&leader).unwrap();
    let db = Connection::open(&path).unwrap();
    assert_eq!(count(&db, retired), 0);
    assert_eq!(count(&db, shared), shared_count);
    assert_eq!(
        db.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r
            .get::<_, i64>(
            0
        ))
        .unwrap(),
        0
    );
    assert!(reopened.graph_at(Some(next)).is_ok());
    assert!(reopened.graph_at(Some(head)).is_ok());
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
fn malformed_native_reference_ids_fail_at_decode_and_schedule_reindex() {
    for column in ["id", "owner_syntax_id"] {
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
        let (_, native, _) = index_workspace_bundle(
            &IndexOptions::new(root.path().to_owned()),
            store.root_id(),
            &cancel,
            |_| {},
        )
        .unwrap();
        let reference = native
            .references
            .iter()
            .find(|r| r.document.path == "flow.js")
            .unwrap();
        let db = Connection::open(published_db(state.path(), root.path())).unwrap();
        db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        assert_eq!(
            db.execute(
                &format!(
                    "UPDATE native_version_references SET {column}=x'01' WHERE vid=?1 AND id=?2"
                ),
                rusqlite::params![
                    live_document_version(&db, "flow.js"),
                    hex::decode(reference.id.strip_prefix("occ:v2:").unwrap()).unwrap()
                ]
            )
            .unwrap(),
            1
        );
        drop(db);
        let error = store
            .native_references_at(pin, &reference.owner_syntax_id)
            .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<rusqlite::Error>(),
                Some(rusqlite::Error::FromSqlConversionFailure(..))
            ),
            "{error:#}"
        );
        assert!(format!("{error:#}").contains("invalid native ID BLOB length"));
        assert!(store.recovery_pending_for_tests());
        assert_current_corruption(store.status().unwrap_err());
    }
}

#[test]
#[cfg(feature = "test-support")]
fn malformed_native_capability_rows_are_typed_corruption() {
    for sql in [
        "UPDATE native_version_capability_families SET kind='unknown' WHERE kind='reference'",
        "UPDATE native_version_capability_gaps SET diagnostic=''",
        "DELETE FROM native_version_capability_families WHERE kind='reference'",
    ] {
        let (state, root, store, cancel) = fixture();
        fs::write(
            root.path().join("flow.rs"),
            "fn main() { run(); opaque!(hidden); }
",
        )
        .unwrap();
        let leader = store.publication_for_tests().unwrap();
        let pin = publish(
            &store,
            root.path(),
            &cancel,
            store.index_baseline().unwrap(),
            &leader,
        )
        .unwrap();
        let (_, native, _) = index_workspace_bundle(
            &IndexOptions::new(root.path().to_owned()),
            store.root_id(),
            &cancel,
            |_| {},
        )
        .unwrap();
        let key = &native
            .revision
            .documents
            .iter()
            .find(|d| d.key.path == "flow.rs")
            .unwrap()
            .key;
        let db = Connection::open(published_db(state.path(), root.path())).unwrap();
        db.execute_batch("PRAGMA foreign_keys=OFF; PRAGMA ignore_check_constraints=ON")
            .unwrap();
        assert!(db.execute(sql, []).unwrap() > 0);
        drop(db);
        let error = store.native_capability_report_at(pin, key).unwrap_err();
        assert!(
            format!("{error:#}").contains("native capability"),
            "{error:#}"
        );
        assert!(store.recovery_pending_for_tests());
        assert_current_corruption(store.status().unwrap_err());
    }
}

#[test]
#[cfg(feature = "test-support")]
fn cold_stream_matches_plain_artifact_publication_rows() {
    let (_plain_state, root, plain, cancel) = fixture();
    let stream_state = tempfile::tempdir().unwrap();
    let stream = Store::open_for_tests(stream_state.path(), root.path()).unwrap();
    let leader = plain.publication_for_tests().unwrap();
    let plain_pin = publish(
        &plain,
        root.path(),
        &cancel,
        plain.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let options = IndexOptions::new(root.path().to_owned());
    let (stream_pin, _owner) =
        trellis::index_coordinator::reconcile_workspace(&stream, &options, &cancel, |_| {})
            .unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let index_path = |state: &std::path::Path| {
        state
            .join("cache/indexes")
            .join(&identity.root_key)
            .join("index.db")
    };
    let plain_db = Connection::open(index_path(_plain_state.path())).unwrap();
    let stream_db = Connection::open(index_path(stream_state.path())).unwrap();
    let values = |db: &Connection, sql: &str| -> Vec<Vec<rusqlite::types::Value>> {
        let mut stmt = db.prepare(sql).unwrap();
        let columns = stmt.column_count();
        stmt.query_map([], |row| {
            (0..columns)
                .map(|index| row.get::<_, rusqlite::types::Value>(index))
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    };
    // Compare all native revision header fields except the selected revision
    // key and publication sequence, which differ by design between stores.
    let revision_header = "SELECT source_set_id,toolchain_hash,config_hash,dependency_hash,native_revision_id,source_inventory,dependency_observations,reconcile_options,class_warnings,class_truncated,graph_stats,graph_diagnostics FROM native_revisions";
    assert_eq!(
        values(&plain_db, revision_header),
        values(&stream_db, revision_header)
    );
    for table in [
        "document_versions",
        "graph_projections",
        "class_projections",
        "graph_nodes",
        "graph_display_labels",
        "classes",
        "class_relations",
        "native_version_coverage_roles",
        "native_version_declarations",
        "native_version_declaration_ancestors",
        "native_version_ancestor_signature_types",
        "native_version_headers",
        "native_version_header_items",
        "native_version_parameters",
        "native_version_own_signature_types",
        "native_version_control_regions",
        "native_version_references",
        "native_version_capability_families",
        "native_version_capability_gaps",
        "native_version_calls",
        "native_version_call_regions",
    ] {
        let sql = format!("SELECT * FROM {table} ORDER BY 1,2");
        assert_eq!(values(&plain_db, &sql), values(&stream_db, &sql), "{table}");
    }
    // Compare the selected manifest without its intentionally different revision key.
    let selected = "SELECT language,path,vid,gid,cid,capture_stat,coverage_requested,coverage_selected,coverage_state,coverage_diagnostic,ordinal FROM revision_documents ORDER BY ordinal";
    assert_eq!(values(&plain_db, selected), values(&stream_db, selected));
    assert_eq!(
        values(&plain_db, "SELECT stats,diagnostics FROM index_metadata"),
        values(&stream_db, "SELECT stats,diagnostics FROM index_metadata"),
    );
    for name in ["flow.js", "flow.java", "flow.rs", "flow.py"] {
        assert_eq!(
            plain.source_at(name, Some(plain_pin)).unwrap().unwrap().1,
            stream.source_at(name, Some(stream_pin)).unwrap().unwrap().1,
            "{name}",
        );
    }
}

#[test]
#[cfg(feature = "test-support")]
fn empty_cold_stream_preserves_plain_native_header() {
    let state_plain = tempfile::tempdir().unwrap();
    let state_stream = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let plain = Store::open_for_tests(state_plain.path(), root.path()).unwrap();
    let stream = Store::open_for_tests(state_stream.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let leader = plain.publication_for_tests().unwrap();
    let _plain_pin = publish(
        &plain,
        root.path(),
        &cancel,
        plain.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    let (stream_pin, _owner) = trellis::index_coordinator::reconcile_workspace(
        &stream,
        &IndexOptions::new(root.path().to_owned()),
        &cancel,
        |_| {},
    )
    .unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let index_path = |state: &std::path::Path| {
        state
            .join("cache/indexes")
            .join(&identity.root_key)
            .join("index.db")
    };
    let header = |state: &std::path::Path| -> Vec<rusqlite::types::Value> {
        let db = Connection::open(index_path(state)).unwrap();
        let mut stmt = db.prepare("SELECT source_set_id,toolchain_hash,config_hash,dependency_hash,native_revision_id,source_inventory,dependency_observations,reconcile_options,class_warnings,class_truncated,graph_stats,graph_diagnostics FROM native_revisions").unwrap();
        let count = stmt.column_count();
        stmt.query_row([], |row| (0..count).map(|i| row.get(i)).collect())
            .unwrap()
    };
    assert_eq!(header(state_plain.path()), header(state_stream.path()));
    assert_eq!(stream.status().unwrap().revision, stream_pin);
}

#[test]
#[cfg(feature = "test-support")]
fn capped_cold_stream_preserves_plain_revision_header_and_catalog_warning() {
    // Cross the production 20,000-class catalog cap with normal-size source
    // files, not with a test-only limit or a huge individual document.
    let root = tempfile::tempdir().unwrap();
    for file in 0..101 {
        let source: String = (0..200)
            .map(|class| format!("class C{file}_{class} {{}}\n"))
            .collect();
        fs::write(root.path().join(format!("classes_{file:03}.java")), source).unwrap();
    }
    let plain_state = tempfile::tempdir().unwrap();
    let stream_state = tempfile::tempdir().unwrap();
    let plain = Store::open_for_tests(plain_state.path(), root.path()).unwrap();
    let stream = Store::open_for_tests(stream_state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let leader = plain.publication_for_tests().unwrap();
    publish(
        &plain,
        root.path(),
        &cancel,
        plain.index_baseline().unwrap(),
        &leader,
    )
    .unwrap();
    trellis::index_coordinator::reconcile_workspace(
        &stream,
        &IndexOptions::new(root.path().to_owned()),
        &cancel,
        |_| {},
    )
    .unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let read_header = |state: &std::path::Path| {
        let db = Connection::open(
            state
                .join("cache/indexes")
                .join(&identity.root_key)
                .join("index.db"),
        )
        .unwrap();
        let mut stmt = db.prepare("SELECT source_set_id,toolchain_hash,config_hash,dependency_hash,native_revision_id,source_inventory,dependency_observations,reconcile_options,class_warnings,class_truncated,graph_stats,graph_diagnostics FROM native_revisions").unwrap();
        let columns = stmt.column_count();
        stmt.query_row([], |row| {
            (0..columns)
                .map(|i| row.get::<_, rusqlite::types::Value>(i))
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap()
    };
    let header = read_header(plain_state.path());
    assert_eq!(header, read_header(stream_state.path()));
    assert_eq!(header[9], rusqlite::types::Value::Integer(1));
    let warnings = match &header[8] {
        rusqlite::types::Value::Text(text) => serde_json::from_str::<Vec<String>>(text).unwrap(),
        other => panic!("expected class warning JSON, got {other:?}"),
    };
    assert_eq!(
        warnings,
        vec![
            "Class catalog declaration limit reached (20000)",
            "Declared types are terminal source text, not class relationships. Direct Java/Python declarations only; no function-body, conditional/anonymous class, generated member, wildcard-import, type-alias or dependency discovery.",
            "Incomplete class declaration registry: some declarations may be absent.",
        ]
    );
}

#[test]
#[cfg(feature = "test-support")]
fn streamed_precommit_cancel_rolls_back_every_document() {
    let (_state, root, store, cancel) = fixture();
    let options = IndexOptions::new(root.path().to_owned());
    let (prior, initial_owner) =
        trellis::index_coordinator::reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
    drop(initial_owner);
    fs::write(
        root.path().join("flow.js"),
        "function after() { return 1; }\n",
    )
    .unwrap();
    fs::write(root.path().join("flow.py"), "def after():\n    return 2\n").unwrap();
    let cancel_at_commit = cancel.clone();
    store.set_publication_before_commit_hook_for_tests(move || {
        cancel_at_commit.store(true, std::sync::atomic::Ordering::Release);
    });
    let attempt =
        trellis::index_coordinator::reconcile_workspace(&store, &options, &cancel, |_| {});
    assert!(attempt.is_err(), "cancelled revision must not commit");
    assert!(
        cancel.load(std::sync::atomic::Ordering::Acquire),
        "precommit hook not reached: {attempt:?}"
    );
    cancel.store(false, std::sync::atomic::Ordering::Release);
    assert!(attempt.unwrap_err().to_string().contains("cancelled"));
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let db = Connection::open(
        _state
            .path()
            .join("cache/indexes")
            .join(&identity.root_key)
            .join("index.db"),
    )
    .unwrap();
    let count: i64 = db
        .query_row("SELECT count(*) FROM native_revisions", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 1, "rollback must not leave an unpublished revision");
    let selected: (String, i64) = db
        .query_row(
            "SELECT index_generation,index_revision FROM index_metadata WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        selected,
        (
            prior.index_generation.to_string(),
            prior.index_revision as i64
        )
    );
    drop(db);
    let (next, _owner) =
        trellis::index_coordinator::reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
    assert_eq!(next.index_revision, prior.index_revision + 1);
}
