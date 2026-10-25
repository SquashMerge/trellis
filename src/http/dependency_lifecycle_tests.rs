use super::*;
fn catalog(id: &str, revision: IndexPin) -> Catalog {
    Catalog {
        id: id.into(),
        workspace_revision: revision,
        packages: vec![],
        symbols: vec![],
        warnings: vec![],
        sources: Default::default(),
    }
}
#[tokio::test]
#[cfg(feature = "test-support")]
async fn dependency_status_whole_response_fence() {
    async fn call(app: &Router, path: &str) -> (StatusCode, Value) {
        use tower::ServiceExt;
        let request = axum::http::Request::builder()
            .uri(path)
            .header("host", "127.0.0.1:7331")
            .header(
                "authorization",
                "Bearer 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            )
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        (status, body)
    }
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let store = Store::open_for_tests(&temp.path().join("state"), &workspace).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(workspace.clone());
    let (pin, session) =
        crate::index_coordinator::reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
    let state = new(
        store.clone(),
        options.clone(),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    state.retain_serving_session(session);
    {
        let mut deps = state.dependencies.lock().unwrap();
        deps.state = "ready";
        deps.catalog = Some(Arc::new(catalog("matching", pin)));
    }
    let app = router(state.clone());
    let (valid_status, valid) = call(&app, "/api/dependencies").await;
    assert_eq!(valid_status, StatusCode::OK);
    assert_eq!(valid["workspaceRevision"], json!(pin));
    assert_eq!(valid["catalogId"], "matching");
    let mismatched = IndexPin {
        index_revision: pin.index_revision + 1,
        ..pin
    };
    state.dependencies.lock().unwrap().catalog = Some(Arc::new(catalog("old", mismatched)));
    let (failed_status, failed) = call(&app, "/api/dependencies").await;
    assert_eq!(failed_status, StatusCode::OK);
    assert_eq!(failed["state"], "failed");
    assert_eq!(failed["catalogId"], Value::Null);
    assert_eq!(failed["packages"], json!([]));
    assert_eq!(failed["symbolCount"], 0);
    assert_eq!(
        failed["warnings"],
        json!(["Workspace changed; refresh the dependency catalog"])
    );
    let (browse_status, browse) = call(&app, "/api/files").await;
    assert_eq!(browse_status, StatusCode::OK);
    assert_eq!(browse["revision"], json!(pin));
    let publishing_store = store.clone();
    let publishing_options = options.clone();
    let publishing_session = state.retained_serving_session().unwrap();
    *state.outer_fence_hook.lock().unwrap() = Some(Arc::new(move || {
        let cancel = Arc::new(AtomicBool::new(false));
        let (graph, native, capture) = crate::indexer::index_workspace_bundle(
            &publishing_options,
            publishing_store.root_id(),
            &cancel,
            |_| {},
        )
        .unwrap();
        publishing_store
            .publish_native(
                &graph,
                &capture,
                &native,
                publishing_session.writer_guard().unwrap(),
                pin,
                &cancel,
            )
            .unwrap();
    }));
    let (published_status, old_response) = call(&app, "/api/files").await;
    assert_eq!(published_status, StatusCode::OK);
    assert_eq!(old_response["revision"], json!(pin));
    *state.outer_fence_hook.lock().unwrap() = None;
    let current = store.status().unwrap().revision;
    assert_eq!(current.index_revision, pin.index_revision + 1);
}
#[test]
fn refresh_bursts_admit_one_worker_and_keep_only_latest_generation() {
    let mut index = DependencyIndex {
        generation: 0,
        cancel: Arc::new(AtomicBool::new(false)),
        stopped: false,
        worker_running: false,
        state: "loading",
        catalog: None,
        warnings: vec![],
    };
    assert!(index.request_build());
    let first_cancel = index.cancel.clone();
    for _ in 0..1000 {
        assert!(!index.request_build());
    }
    assert!(first_cancel.load(Ordering::Acquire));
    assert!(!index.cancel.load(Ordering::Acquire));
    assert_eq!(index.generation, 1001);
    // A finished driver relinquishes the slot; the next request can restart it.
    index.worker_running = false;
    assert!(index.request_build());
    index.stopped = true;
    assert!(!index.request_build());
    assert_eq!(index.generation, 1002);
}
#[test]
fn stale_generation_cancel_and_revision_never_publish() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let store = Store::open_for_tests(&temp.path().join("state"), &workspace).unwrap();
    crate::store::topology::assert_topology_fixture(&store, &temp.path().join("state"));
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = crate::indexer::index_workspace_bundle(
        &IndexOptions::new(workspace.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let session = store.writer_session().unwrap();
    let pin0 = store
        .publish_native(
            &graph,
            &capture,
            &native,
            session.writer_guard().unwrap(),
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let state = new_with_dependency_options(
        store.clone(),
        IndexOptions::new(workspace.clone()),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
        workspace.clone(),
        vec![],
        Some(CatalogOptions::default()),
    )
    .unwrap();
    state.retain_serving_session(session.clone());
    let active = AtomicBool::new(false);
    state.dependencies.lock().unwrap().generation = 2;
    state.publish_dependency_index(2, &active, Ok(catalog("new", pin0)));
    state.publish_dependency_index(1, &active, Ok(catalog("old", pin0)));
    state.publish_dependency_index(1, &active, Err(anyhow::anyhow!("late failure")));
    assert_eq!(state.catalog_snapshot(pin0).unwrap().id, "new");
    state.publish_dependency_index(2, &AtomicBool::new(true), Ok(catalog("cancelled", pin0)));
    assert_eq!(state.catalog_snapshot(pin0).unwrap().id, "new");
    let (updated, native, captured) = crate::indexer::index_workspace_bundle(
        &IndexOptions::new(workspace),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    store
        .publish_native(
            &updated,
            &captured,
            &native,
            session.writer_guard().unwrap(),
            pin0,
            &cancel,
        )
        .unwrap();
    let pin1 = store.status().unwrap().revision;
    state.publish_dependency_index(2, &active, Ok(catalog("stale-revision", pin0)));
    assert!(state.catalog_snapshot(pin0).is_none());
    assert_eq!(state.dependencies.lock().unwrap().state, "failed");
    state.cancel_active();
    let generation = state.dependencies.lock().unwrap().generation;
    state.publish_dependency_index(generation, &active, Ok(catalog("after-shutdown", pin1)));
    assert!(state.catalog_snapshot(pin1).is_none());
}
