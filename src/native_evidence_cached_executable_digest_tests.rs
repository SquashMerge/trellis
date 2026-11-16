use super::*;
use crate::{
    indexer::{IndexOptions, index_workspace_bundle},
    store::Store,
};
use std::{
    fs,
    sync::{Arc, atomic::AtomicBool},
};

#[test]
fn admitted_executable_digest_survives_projection_validation_and_paired_publish() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(
        workspace.path().join("Types.java"),
        "class A { void go() { helper(); } void helper() {} }\n",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let root = fs::canonicalize(workspace.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(workspace.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let exe = std::env::current_exe().unwrap();
    assert_eq!(
        native.producer.executable_hash,
        capture.executable_digest(&exe).unwrap()
    );
    assert_eq!(capture.graph_projection_count(), 1);
    assert!(
        capture
            .source_operations
            .values()
            .all(|ops| ops.opens == 1 && ops.complete_reads == 1 && ops.hashes == 1)
    );
    native
        .validate(&capture, &root, store.root_id(), &cancel)
        .unwrap();
    assert_eq!(
        from_capture(&capture, &root, store.root_id(), &cancel).unwrap(),
        native
    );
    let session = store.writer_session().unwrap();
    let pin = store
        .publish_native(
            &graph,
            &capture,
            &native,
            session.writer_guard().unwrap(),
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    assert_eq!(store.status().unwrap().revision, pin);
    let key = &native.revision.documents[0].key;
    let (document, bytes) = store.native_source_at(pin, key).unwrap().unwrap();
    assert_eq!(
        document.content_hash,
        native.revision.documents[0].content_hash
    );
    assert_eq!(bytes.as_slice(), graph.files[0].text.as_bytes());
    let go = store.native_declarations_at(pin, "java", "go").unwrap();
    assert!(!go.is_empty());
    assert!(go.iter().all(|row| {
        native
            .declarations
            .iter()
            .any(|expected| row.syntax_id == expected.syntax_id)
    }));
    drop(session);
}

fn all_index_rows(db_path: &Path) -> Vec<(String, Vec<String>)> {
    use rusqlite::{Connection, types::Value};
    let db = Connection::open(db_path).unwrap();
    let tables = db
        // Decision 0013's trigram prefilter tables are derived search state, not evidence.
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '%trigrams%' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    tables
        .into_iter()
        .map(|table| {
            let mut statement = db.prepare(&format!("SELECT * FROM \"{table}\"")).unwrap();
            let count = statement.column_count();
            let mut rows = statement
                .query_map([], |row| {
                    (0..count)
                        .map(|column| row.get::<_, Value>(column).map(|v| format!("{v:?}")))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
                .into_iter()
                .map(|row| format!("{row:?}"))
                .collect::<Vec<_>>();
            rows.sort();
            (table, rows)
        })
        .collect()
}

#[test]
fn rejected_extraction_components_cannot_publish_or_mint_occurrences() {
    use crate::store::topology::WorkspaceIdentity;
    use rusqlite::Connection;
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(
        workspace.path().join("Types.java"),
        "class A { int go() { return helper(); } int helper() { return 1; } }\n",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let session = store.writer_session().unwrap();
    let leader = session.writer_guard().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(workspace.path().to_owned());
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let first = store
        .publish_native(
            &graph,
            &capture,
            &native,
            leader,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let db_path = state
        .path()
        .join("cache/indexes")
        .join(
            WorkspaceIdentity::discover(Some(workspace.path()), workspace.path())
                .unwrap()
                .root_key,
        )
        .join("index.db");
    let old_rows = all_index_rows(&db_path);
    let old_source = store
        .native_source_at(first, &native.revision.documents[0].key)
        .unwrap();
    let old_graph = store.graph_at(Some(first)).unwrap();
    let old_class =
        serde_json::to_value(store.classes_at(None, "", Some(first), 0, 100).unwrap()).unwrap();
    let old_occurrences: i64 = Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT (SELECT count(*) FROM native_version_calls WHERE length(id)=16) +
                    (SELECT count(*) FROM native_version_control_regions WHERE length(id)=16)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(old_occurrences > 0);

    fs::write(
        workspace.path().join("Types.java"),
        "class A { int go() { return helper() + 1; } int helper() { return 1; } }\n",
    )
    .unwrap();
    // Admission and production measurement complete before the injected
    // component fault. Only the publication boundary receives the fault.
    let (next_graph, next_native, next_capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    assert_ne!(next_native.revision.id, native.revision.id);
    for (fault, diagnostic) in [
        (
            ExtractionAuthFault::Undeclared,
            "undeclared native extraction-input inventory",
        ),
        (
            ExtractionAuthFault::Mismatched,
            "mismatched native extraction component: config",
        ),
    ] {
        let guard = inject_extraction_auth_fault(&next_native.revision.id, "java", fault);
        let error = store
            .publish_native(
                &next_graph,
                &next_capture,
                &next_native,
                leader,
                first,
                &cancel,
            )
            .unwrap_err();
        drop(guard);
        assert!(format!("{error:#}").contains(diagnostic), "{error:#}");
        assert_eq!(store.status().unwrap().revision, first);
        assert_eq!(
            store
                .native_source_at(first, &native.revision.documents[0].key)
                .unwrap(),
            old_source
        );
        assert_eq!(store.graph_at(Some(first)).unwrap(), old_graph);
        assert_eq!(
            serde_json::to_value(store.classes_at(None, "", Some(first), 0, 100).unwrap()).unwrap(),
            old_class
        );
        assert_eq!(
            all_index_rows(&db_path),
            old_rows,
            "failed auth changed old or added a new native revision"
        );
        let db = Connection::open(&db_path).unwrap();
        let published: i64 = db
            .query_row("SELECT count(*) FROM native_revisions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(published, 1, "new revision escaped component refusal");
        let occurrence_count: i64 = db
            .query_row(
                "SELECT (SELECT count(*) FROM native_version_calls WHERE length(id)=16) +
                        (SELECT count(*) FROM native_version_control_regions WHERE length(id)=16)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            occurrence_count, old_occurrences,
            "new occ:v2 record escaped component refusal"
        );
    }
}
