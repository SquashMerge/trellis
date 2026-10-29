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

fn indexed_catalog_fixture() -> (
    tempfile::TempDir,
    Arc<DaemonState>,
    IndexPin,
    Arc<crate::store::topology::WriterSession>,
) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("vendor/local-library/src")).unwrap();
    for (path, text) in [
        (
            "Cargo.toml",
            "[package]\nname='catalog-app'\nversion='0.1.0'\n[dependencies]\nlocal-library={path='vendor/local-library'}\n",
        ),
        (
            "Cargo.lock",
            "version=4\n[[package]]\nname='catalog-app'\nversion='0.1.0'\ndependencies=['local-library']\n[[package]]\nname='local-library'\nversion='0.1.0'\n",
        ),
        (
            "vendor/local-library/Cargo.toml",
            "[package]\nname='local-library'\nversion='0.1.0'\n",
        ),
        (
            "vendor/local-library/src/lib.rs",
            "pub fn candidate_only() {}\n",
        ),
    ] {
        std::fs::write(workspace.join(path), text).unwrap();
    }
    let cargo_home = temp.path().join("cargo-home");
    std::fs::create_dir(&cargo_home).unwrap();
    let store = Store::open_for_tests(&temp.path().join("state"), &workspace).unwrap();
    let options = IndexOptions::new(workspace.clone());
    let (pin, session) = crate::index_coordinator::reconcile_workspace(
        &store,
        &options,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert!(store.earliest_unfinished_request().unwrap().is_none());
    let state = new_with_dependency_options(
        store,
        options,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
        workspace,
        vec![],
        Some(CatalogOptions {
            cargo_home: Some(cargo_home),
            rust_library: None,
        }),
    )
    .unwrap();
    assert_eq!(state.store.status().unwrap().revision, pin);
    (temp, state, pin, session)
}

async fn assert_catalog_generation_finished(state: &DaemonState, generation: u64, pin: IndexPin) {
    tokio::time::timeout(
        crate::testing::HANG_BACKSTOP,
        state.dependency_idle.notified(),
    )
    .await
    .expect("dependency driver did not publish completion");
    let index = state.dependencies.lock().unwrap();
    assert_eq!(index.generation, generation);
    assert_eq!(index.state, "ready");
    let catalog = index.catalog.as_ref().unwrap();
    assert_eq!(catalog.workspace_revision, pin);
    assert_eq!(catalog.packages.len(), 1);
    assert_eq!(catalog.packages[0].name, "local-library");
    assert_eq!(catalog.packages[0].source_state, "present");
    assert_eq!(catalog.packages[0].index_state, "complete");
    assert_eq!(catalog.symbols.len(), 1);
    assert_eq!(catalog.symbols[0].name, "candidate_only");
}

#[tokio::test]
async fn enabled_config_replacement_admits_catalog_without_fifo_or_refresh() {
    let (_temp, state, pin, session) = indexed_catalog_fixture();
    state.retain_serving_session_without_tick_for_tests(session);
    // Establish a completed driver, not merely an observed ready publication.
    state.start_dependency_index();
    let previous_generation = state.dependencies.lock().unwrap().generation;
    assert_catalog_generation_finished(&state, previous_generation, pin).await;
    assert!(state.store.earliest_unfinished_request().unwrap().is_none());
    let old_config = state.config();
    let replacement = BrowserConfig::build(
        old_config.options.clone(),
        false,
        &old_config.options.workspace_root,
        vec![],
        old_config.dependency_options.clone(),
    )
    .unwrap();
    state.apply_config(replacement);
    let generation = {
        let index = state.dependencies.lock().unwrap();
        assert!(
            index.worker_running,
            "enabled configuration publication must admit a catalog driver without FIFO work"
        );
        assert!(index.generation > previous_generation);
        index.generation
    };
    assert_catalog_generation_finished(&state, generation, pin).await;
    assert_eq!(state.store.status().unwrap().revision, pin);
    assert!(state.store.earliest_unfinished_request().unwrap().is_none());
}

#[tokio::test]
async fn initial_enabled_startup_admits_catalog_after_reconciliation_without_fifo() {
    let (_temp, state, _indexed_pin, session) = indexed_catalog_fixture();
    drop(session);
    // Drive the production startup transition and worker step by hand, so the
    // assertion runs after H without racing a background checkout worker.
    state.worker_started.store(true, Ordering::Release);
    state.begin_startup_reconciliation();
    state.worker_step().unwrap();
    assert_eq!(state.runtime_phase(), RuntimePhase::Ready);
    assert!(state.store.earliest_unfinished_request().unwrap().is_none());
    let pin = state.store.status().unwrap().revision;
    let generation = {
        let index = state.dependencies.lock().unwrap();
        assert!(
            index.worker_running,
            "enabled startup must admit a catalog driver after reconciliation without FIFO work"
        );
        index.generation
    };
    assert_catalog_generation_finished(&state, generation, pin).await;
    state.release_checkout_runtime_for_fixture();
}

#[tokio::test]
async fn disabled_config_cancels_the_prior_generation_without_admitting_a_driver() {
    let (_temp, state, pin, session) = indexed_catalog_fixture();
    state.retain_serving_session_without_tick_for_tests(session);
    state.start_dependency_index();
    let generation = state.dependencies.lock().unwrap().generation;
    assert_catalog_generation_finished(&state, generation, pin).await;
    let prior_cancel = state.dependencies.lock().unwrap().cancel.clone();
    let old_config = state.config();
    let disabled = BrowserConfig::build(
        old_config.options.clone(),
        false,
        &old_config.options.workspace_root,
        vec![],
        None,
    )
    .unwrap();
    state.apply_config(disabled);
    assert!(prior_cancel.load(Ordering::Acquire));
    state.publish_dependency_index(generation, &prior_cancel, Ok(catalog("late", pin)));
    let index = state.dependencies.lock().unwrap();
    assert_eq!(index.generation, generation + 1);
    assert_eq!(index.state, "disabled");
    assert!(!index.worker_running);
    assert!(index.catalog.is_none());
}

#[tokio::test]
async fn disabling_an_active_catalog_build_keeps_the_successor_disabled() {
    let (_temp, state, _pin, session) = indexed_catalog_fixture();
    state.retain_serving_session_without_tick_for_tests(session);
    let (entered, wait_entered) = tokio::sync::oneshot::channel();
    let (release, wait_release) = std::sync::mpsc::channel::<()>();
    *state.dependency_build_hook.lock().unwrap() = Some(Box::new(move || {
        let _ = entered.send(());
        let _ = wait_release.recv();
    }));
    state.start_dependency_index();
    let entered = tokio::time::timeout(crate::testing::HANG_BACKSTOP, wait_entered).await;
    if !matches!(entered, Ok(Ok(()))) {
        drop(release);
        panic!("catalog build did not reach its configuration snapshot");
    }
    let prior_cancel = state.dependencies.lock().unwrap().cancel.clone();
    let old_config = state.config();
    let disabled = BrowserConfig::build(
        old_config.options.clone(),
        false,
        &old_config.options.workspace_root,
        vec![],
        None,
    )
    .unwrap();
    state.apply_config(disabled);
    let disabled_generation = state.dependencies.lock().unwrap().generation;
    // Release the blocking worker before assertions, including failure paths.
    drop(release);
    tokio::time::timeout(
        crate::testing::HANG_BACKSTOP,
        state.dependency_idle.notified(),
    )
    .await
    .expect("cancelled catalog driver did not finish");
    assert!(prior_cancel.load(Ordering::Acquire));
    let index = state.dependencies.lock().unwrap();
    assert_eq!(index.generation, disabled_generation);
    assert_eq!(index.state, "disabled");
    assert!(!index.worker_running);
    assert!(index.catalog.is_none());
}
