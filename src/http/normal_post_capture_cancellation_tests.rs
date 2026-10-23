use super::*;
#[cfg(feature = "test-support")]
use crate::store::topology::{TopologyRoots, WorkspaceIdentity};
use rusqlite::{OpenFlags, types::Value as SqlValue};
#[cfg(feature = "test-support")]
use std::{fs, time::Duration};
use tower::ServiceExt;

#[derive(Debug, PartialEq)]
struct PairSnapshot {
    pin: IndexPin,
    rows: BTreeMap<String, Vec<String>>,
}

// Read all derived graph, native, class and source rows in one SQLite snapshot.
fn pair_snapshot(index: &std::path::Path) -> PairSnapshot {
    let db =
        rusqlite::Connection::open_with_flags(index, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    db.execute_batch("BEGIN DEFERRED").unwrap();
    let (generation, revision): (String, i64) = db
        .query_row(
            "SELECT index_generation,index_revision FROM index_metadata WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let pin = IndexPin {
        index_generation: uuid::Uuid::parse_str(&generation).unwrap(),
        index_revision: u64::try_from(revision).unwrap(),
    };
    let tables: Vec<String> = db
            // Decision 0013's trigram prefilter tables are derived search state, not evidence.
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name!='index_metadata' AND name NOT LIKE '%trigrams%' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
    assert_eq!(
        tables.len(),
        54,
        "compare exact schema-15 derived evidence inventory"
    );
    let expected = vec![
        "class_projections",
        "class_relations",
        "classes",
        "document_versions",
        "graph_display_labels",
        "graph_nodes",
        "graph_projections",
        "native_binding_epoch",
        "native_producer_inputs",
        "native_producer_languages",
        "native_producers",
        "native_release_candidate_classes",
        "native_release_candidate_graphs",
        "native_release_candidate_versions",
        "native_revision_release_debt",
        "native_revision_supersessions",
        "native_revisions",
        "native_source_set_dependencies",
        "native_source_set_languages",
        "native_source_sets",
        "native_version_ancestor_signature_types",
        "native_version_call_regions",
        "native_version_calls",
        "native_version_control_regions",
        "native_version_coverage_roles",
        "native_version_declaration_ancestors",
        "native_version_declarations",
        "native_version_header_items",
        "native_version_headers",
        "native_version_own_signature_types",
        "native_version_parameters",
        "revision_capture_inputs",
        "revision_documents",
        "revision_producer_bindings",
        "semantic_basis",
        "semantic_basis_lookup_dependencies",
        "semantic_call_bindings",
        "semantic_call_candidates",
        "semantic_call_join_candidates",
        "semantic_call_possible_dispatch",
        "semantic_coverage",
        "semantic_coverage_roles",
        "semantic_declaration_binding_symbols",
        "semantic_declaration_bindings",
        "semantic_declaration_join_candidates",
        "semantic_producer_languages",
        "semantic_producers",
        "semantic_provenance",
        "semantic_reference_candidates",
        "semantic_reference_roles",
        "semantic_references",
        "semantic_symbol_targets",
        "semantic_symbols",
        "semantic_type_relationships",
    ];
    assert_eq!(
        tables.iter().map(String::as_str).collect::<Vec<_>>(),
        expected,
        "exact schema-15 table names: complete paired native and semantic evidence"
    );
    for required in [
        "document_versions",
        "revision_documents",
        "graph_nodes",
        "native_version_declarations",
        "class_projections",
    ] {
        assert!(
            tables.iter().any(|table| table == required),
            "missing {required}"
        );
    }
    let mut rows = BTreeMap::new();
    for table in tables {
        assert!(
            table
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        );
        let mut stmt = db.prepare(&format!("SELECT * FROM {table}")).unwrap();
        let columns = stmt.column_count();
        let mut values = stmt
            .query_map([], |row| {
                let cells = (0..columns)
                    .map(|column| row.get::<_, SqlValue>(column))
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(format!("{cells:?}"))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        values.sort();
        rows.insert(table, values);
    }
    db.execute_batch("ROLLBACK").unwrap();
    PairSnapshot { pin, rows }
}

async fn api(
    app: &Router,
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:7331")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(
            body.map(|value| value.to_string()).unwrap_or_default(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
#[cfg(feature = "test-support")]
async fn accepted_job_cannot_be_cancelled_and_full_pair_changes_only_after_leader_drains() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("one.js"),
        "function seed() { old_step(); } function old_step() {}\n",
    )
    .unwrap();
    let state_root = tmp.path().join("state");
    let roots =
        TopologyRoots::isolated_for_tests(state_root.join("cache"), state_root.join("data"));
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let index = roots.index_db(&identity);
    let store = Store::open_for_tests(&state_root, &root).unwrap();
    let options = IndexOptions::new(root.clone());
    let (_, owner) = crate::index_coordinator::reconcile_workspace(
        &store,
        &options,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let state = new(
        store.clone(),
        options,
        token.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    state.retain_serving_session(owner.clone());
    tokio::time::timeout(crate::testing::HANG_BACKSTOP, async {
        while state.checkout_watch_pending() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("initial watcher inventory did not quiesce");
    let (held_tx, held_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let held_state = state.clone();
    let holder = std::thread::spawn(move || {
        held_state.hold_worker_step_for_tests(|| {
            held_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
    });
    tokio::time::timeout(crate::testing::HANG_BACKSTOP, held_rx)
        .await
        .expect("worker step was not held")
        .unwrap();
    let before = pair_snapshot(&index);
    fs::write(
        root.join("one.js"),
        "function seed() { fresh(); } function fresh() {}\n",
    )
    .unwrap();
    fs::write(root.join("two.js"), "function extra() {}\n").unwrap();
    let app = router(state.clone());
    let (code, accepted) = api(&app, token, "POST", "/api/index", Some(json!({}))).await;
    assert_eq!(code, StatusCode::ACCEPTED, "{accepted}");
    assert_eq!(accepted["state"], "queued");
    let id = accepted["id"].as_str().unwrap();
    let (code, rejected) = api(&app, token, "POST", &format!("/api/jobs/{id}/cancel"), None).await;
    assert_eq!(code, StatusCode::CONFLICT, "{rejected}");
    assert_eq!(rejected["error"]["code"], "request_not_cancellable");
    assert_eq!(pair_snapshot(&index), before);
    assert_eq!(store.request_by_id(id).unwrap().unwrap().state, "queued");
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    let completed = tokio::time::timeout(crate::testing::HANG_BACKSTOP, async {
        loop {
            let (code, row) = api(&app, token, "GET", &format!("/api/jobs/{id}"), None).await;
            assert_eq!(code, StatusCode::OK, "{row}");
            if !row["finishedAt"].is_null() {
                break row;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(completed["state"], "done", "{completed}");
    let after = pair_snapshot(&index);
    assert!(after.pin.index_revision > before.pin.index_revision);
    assert_ne!(
        after.rows["revision_documents"],
        before.rows["revision_documents"]
    );
    assert_ne!(
        after.rows["document_versions"],
        before.rows["document_versions"]
    );
    assert!(
        after.rows["document_versions"]
            .iter()
            .any(|row| row.contains("two.js"))
    );
    assert_eq!(
        after.pin,
        serde_json::from_value(completed["revision"].clone()).unwrap()
    );
}
