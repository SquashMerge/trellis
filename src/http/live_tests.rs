use super::*;
use crate::indexer::index_workspace_bundle;
use tower::ServiceExt;

#[test]
fn outer_http_fence_covers_native_composition_not_durable_payloads() {
    for path in [
        "/api/status",
        "/api/tree",
        "/api/files",
        "/api/methods",
        "/api/sequence",
        "/api/classes",
        "/api/class-diagram",
        "/api/navigation",
        "/api/symbols",
        "/api/symbol",
        "/api/source",
        "/api/query",
        "/api/dependencies",
        "/api/dependencies/symbols",
        "/api/dependencies/source",
        "/api/rust-sources/tree",
    ] {
        assert!(native_response_route(path), "{path}");
    }
    for path in [
        "/api/views",
        "/api/views/saved",
        "/api/annotations",
        "/api/rust-sources/file",
        "/api/healthz",
        "/api/jobs/current",
    ] {
        assert!(!native_response_route(path), "{path}");
    }
}
#[test]
fn question_error_keeps_typed_refusals_and_selection_errors() {
    for (message, code) in [
        ("root_changed: fixture", "root_changed"),
        ("recovery_required: fixture", "recovery_required"),
        ("unsafe_index: fixture", "unsafe_index"),
        ("index_not_ready: fixture", "index_not_ready"),
    ] {
        assert_eq!(question_error(anyhow::anyhow!("{message}")).1, code);
    }
    assert_eq!(
        question_error(anyhow::anyhow!("revision conflict")).1,
        "revision_conflict"
    );
    assert_eq!(
        question_error(anyhow::anyhow!("invalid selection")).1,
        "invalid_question_selection"
    );
    assert_eq!(
        question_error(anyhow::anyhow!("complete question packet exceeds 1 MiB")).1,
        "evidence_too_large"
    );
    let busy =
        rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY), None);
    assert_eq!(question_error(busy.into()).1, "storage_busy");
}

#[test]
fn api_error_walks_context_but_never_masks_sqlite_ioerr_rdlock() {
    let busy = anyhow::anyhow!("storage_busy: SQLite lock contention").context("durable enqueue");
    let mapped = ApiError::from(busy);
    assert_eq!((mapped.0, mapped.1), (StatusCode::CONFLICT, "storage_busy"));
    let root = anyhow::anyhow!("root_changed: pathname replaced").context("durable enqueue");
    let mapped = ApiError::from(root);
    assert_eq!((mapped.0, mapped.1), (StatusCode::CONFLICT, "root_changed"));
    let ioerr = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR_RDLOCK),
        None,
    );
    let mapped = ApiError::from(
        anyhow::Error::new(ioerr).context("storage_busy: wrapped database I/O failure"),
    );
    assert_eq!(
        (mapped.0, mapped.1),
        (StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
    );
    let rebuild = anyhow::Error::new(crate::store::RebuildInProgress {
        since: std::time::SystemTime::now(),
        documents: 10_000,
    })
    .context("selected evidence");
    let mapped = ApiError::from(rebuild);
    assert_eq!(
        (mapped.0, mapped.1),
        (StatusCode::SERVICE_UNAVAILABLE, "rebuild_in_progress")
    );
}

#[test]
fn preview_fence_failure_restores_evicted_and_same_id_packets() {
    let revision = IndexPin {
        index_generation: uuid::Uuid::new_v4(),
        index_revision: 1,
    };
    let packet = QuestionPacket {
        packet_id: "original".into(),
        revision,
        request: QuestionRequest {
            seed: "seed".into(),
            question: "what?".into(),
            expected_revision: revision,
            evidence_depth: 0,
            max_visible: 1,
            allow_deeper_display: false,
            focus_terms: vec![],
        },
        context: ViewResult {
            revision,
            query: ViewQuery {
                seed: "seed".into(),
                depth: 0,
                max_nodes: 1,
                max_calls: 1,
                include_callbacks: false,
                exclude_paths: vec![],
            },
            nodes: vec![],
            calls: vec![],
            regions: vec![],
            truncated: false,
            omitted_nodes: 0,
            warnings: vec![],
        },
        source_files: vec![],
        warnings: vec![],
    };
    let mut cache = PacketCache::default();
    for i in 0..MAX_PACKETS {
        let mut item = packet.clone();
        item.packet_id = format!("packet-{i}");
        cache.remember(Arc::new(item), MAX_PACKET_BYTES);
    }
    let original: Vec<_> = cache
        .packets
        .iter()
        .map(|(p, bytes)| (p.clone(), *bytes))
        .collect();
    let original_bytes = cache.bytes;
    let mut incoming = packet.clone();
    incoming.packet_id = "new".into();
    let failed = cache.remember_fenced(Arc::new(incoming), MAX_PACKET_BYTES, || {
        anyhow::bail!("index_not_ready: incarnation lost")
    });
    assert!(
        failed
            .unwrap_err()
            .to_string()
            .starts_with("index_not_ready")
    );
    assert_eq!(cache.bytes, original_bytes);
    assert_eq!(cache.packets.len(), MAX_PACKETS);
    for ((actual, size), (before, expected)) in cache.packets.iter().zip(&original) {
        assert!(Arc::ptr_eq(actual, before));
        assert_eq!(size, expected);
    }
    let mut replacement = packet;
    replacement.packet_id = "packet-3".into();
    assert!(
        cache
            .remember_fenced(Arc::new(replacement), MAX_PACKET_BYTES + 1, || {
                anyhow::bail!("root_changed: root replaced")
            })
            .is_err()
    );
    assert_eq!(cache.bytes, original_bytes);
    assert_eq!(cache.packets.len(), MAX_PACKETS);
    for ((actual, size), (before, expected)) in cache.packets.iter().zip(&original) {
        assert!(Arc::ptr_eq(actual, before));
        assert_eq!(size, expected);
    }
}

#[tokio::test]
async fn preview_root_change_after_serialization_discards_packet() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("a.js"), "function go() { measured(); }\n").unwrap();
    let options = IndexOptions::new(workspace.clone());
    let cancel = Arc::new(AtomicBool::new(false));
    let store = Store::open_for_tests(&dir.path().join("state"), &workspace).unwrap();
    let (graph, _, _) = index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let seed = graph
        .nodes
        .iter()
        .find(|node| node.name == "go")
        .unwrap()
        .id
        .clone();
    let (pin, session) =
        crate::index_coordinator::reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
    let state = new(
        store,
        options,
        "0123456789abcdef".repeat(4),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    state.retain_serving_session(session);
    let root = workspace.clone();
    *state.preview_finish_hook.lock().unwrap() = Some(Arc::new(move || {
        std::fs::rename(&root, root.with_file_name("replaced-workspace")).unwrap();
        std::fs::create_dir(&root).unwrap();
    }));
    let app = router(state.clone());
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/api/questions/preview")
        .header("host", "127.0.0.1:7331")
        .header(
            "authorization",
            "Bearer 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"seed":seed,"question":"what does go do?","expectedRevision":pin}).to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(body["error"]["code"], "root_changed");
    assert!(body.get("packet").is_none());
    assert!(state.packets.lock().unwrap().packets.is_empty());
}

#[test]
fn acp_controlled_errors_are_exact_and_sanitized() {
    for (message, code) in [
        ("ACP authentication required", "acp_auth_required"),
        ("ACP model unavailable", "acp_model_unavailable"),
        ("ACP model mismatch", "acp_model_mismatch"),
    ] {
        let known = acp_error(anyhow::anyhow!("{message}"));
        assert_eq!(known.0, StatusCode::BAD_GATEWAY);
        assert_eq!(known.1, code);
        assert!(known.2.contains("no automatic retry"));
        let untrusted = acp_error(anyhow::anyhow!("{message} private output"));
        assert_eq!(untrusted.1, "acp_failed");
        assert!(!untrusted.2.contains("private output"));
    }
}

#[tokio::test]
async fn live_response_is_rejected_if_snapshot_changes_during_call() {
    mock_run(true, "success").await;
}

#[tokio::test]
async fn pinned_packet_conflicts_even_when_provider_fails() {
    mock_run(true, "context_exceeded").await;
}

#[tokio::test]
async fn live_response_is_labeled_and_accounted() {
    mock_run(false, "success").await;
}

#[tokio::test]
async fn corrupt_selected_cached_source_prevents_live_provider_call() {
    mock_run(false, "selected_corruption").await;
}

#[tokio::test]
async fn context_exceeded_is_actionable_and_retains_reservation() {
    mock_run(false, "context_exceeded").await;
}

#[test]
fn live_error_classification_is_allowlisted_and_redacted() {
    for (message, status, code) in [
        (
            "invalid Jev request: private source",
            422,
            "invalid_jev_request",
        ),
        (
            "Jev requires at least one candidate",
            422,
            "jev_no_candidates",
        ),
        (
            "Jev attempt failed: context_exceeded",
            422,
            "jev_context_exceeded",
        ),
        (
            "Jev attempt failed: context_exceeded private source",
            502,
            "jev_failed",
        ),
        (
            "Jev budget exhausted: private source",
            429,
            "jev_budget_exhausted",
        ),
        (
            "Jev attempt failed: invalid_selection",
            502,
            "jev_invalid_response",
        ),
        (
            "Jev attempt failed: invalid_selection private source",
            502,
            "jev_failed",
        ),
        ("private source", 502, "jev_failed"),
    ] {
        let error = live_jev_error(anyhow::anyhow!(message));
        assert_eq!(error.0.as_u16(), status);
        assert_eq!(error.1, code);
        assert!(!error.2.contains("private source"));
    }
}

#[tokio::test]
async fn rounded_live_response_warns_and_invalid_response_is_actionable() {
    mock_run(false, "rounded").await;
    mock_run(false, "invalid_selection").await;
}

async fn mock_run(change_snapshot: bool, scenario: &'static str) {
    // Loopback mock only: no environment credentials or external endpoint.
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let notify = entered.clone();
    let gate = release.clone();
    let mock = Router::new().route("/", post(move |Json(request): Json<Value>| {
            let notify = notify.clone();
            let gate = gate.clone();
            async move {
                notify.notify_one();
                gate.notified().await;
                if scenario == "context_exceeded" {
                    return (StatusCode::BAD_REQUEST, Json(json!({"detail":{"error_type":"max_tokens_exceeded"}}))).into_response();
                }
                let answers: serde_json::Map<String, Value> = request["questions"].as_object().unwrap()
                    .keys().map(|key| (key.clone(), json!({"type":"choice","choice":"essential","confidence":1.0,
                    "probabilities":{"essential":if scenario == "rounded" {0.69} else if scenario == "invalid_selection" {0.6} else {1.0},"supporting":if scenario == "success" {0.0} else {0.2},"incidental":if scenario == "success" {0.0} else {0.1},"uncertain":0.0}}))).collect();
                Json(json!({"model":"jev-1.13.0","answers":answers})).into_response()
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, mock).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(
        workspace.join("a.js"),
        "function seed() { console.log('synthetic'); }",
    )
    .unwrap();
    let options = IndexOptions::new(workspace.clone());
    let cancel = Arc::new(AtomicBool::new(false));
    let store = Store::open_for_tests(&dir.path().join("state"), &workspace).unwrap();
    store.settle_foreign_root_requests().unwrap();
    crate::store::topology::assert_topology_fixture(&store, &dir.path().join("state"));
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let seed = graph
        .nodes
        .iter()
        .find(|node| node.name == "seed")
        .unwrap()
        .id
        .clone();
    let session = store.writer_session().unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            session.writer_guard().unwrap(),
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let provider = Arc::new(
        LiveJev::open(
            &dir.path().join("budget"),
            "synthetic".into(),
            10,
            &workspace,
        )
        .unwrap()
        .with_test_endpoint(endpoint),
    );
    let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let state = new_with_jev(
        store.clone(),
        options,
        token.into(),
        "127.0.0.1:7331".parse().unwrap(),
        Some(provider.clone()),
    )
    .unwrap();
    // The provider fixture drives its own startup reconciliation explicitly; do not race an
    // unrelated timer watcher for the selected source change.
    state.retain_serving_session_without_tick_for_tests(session.clone());
    let app = router(state.clone());
    let request = |path: &str, body: Value| {
        axum::http::Request::builder()
            .method("POST")
            .uri(path)
            .header("host", "127.0.0.1:7331")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let preview = app
            .clone()
            .oneshot(request(
                "/api/questions/preview",
                json!({"seed":seed,"question":"logging", "expectedRevision":store.status().unwrap().revision}),
            ))
            .await
            .unwrap();
    assert_eq!(preview.status(), StatusCode::OK);
    let bytes = to_bytes(preview.into_body(), 1024 * 1024).await.unwrap();
    let preview: Value = serde_json::from_slice(&bytes).unwrap();
    let path = format!(
        "/api/questions/{}/jev-run",
        preview["packet"]["packetId"].as_str().unwrap()
    );
    if scenario == "selected_corruption" {
        let index_root = dir.path().join("state/cache/indexes");
        let db_path = std::fs::read_dir(index_root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.is_dir())
            .unwrap()
            .join("index.db");
        let db = rusqlite::Connection::open(db_path).unwrap();
        let mut bytes: Vec<u8> = db
                .query_row(
                    "SELECT v.source_bytes FROM document_versions v JOIN revision_documents m ON m.document_version_id=v.id JOIN native_revisions r ON r.id=m.revision_id JOIN index_metadata current ON current.index_revision=r.published_index_revision WHERE m.path='a.js'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
        bytes[0] ^= 1;
        assert_eq!(
                db.execute(
                    "UPDATE document_versions SET source_bytes=?1 WHERE id=(SELECT m.document_version_id FROM revision_documents m JOIN native_revisions r ON r.id=m.revision_id JOIN index_metadata current ON current.index_revision=r.published_index_revision WHERE m.path='a.js')",
                    [bytes],
                )
                .unwrap(),
                1
            );
        // The background leader watcher may discover the corrupted selected
        // bytes before this read; both a still-pinned status and a typed
        // fail-closed index are valid. Neither may call the live provider.
        match store.status() {
            Ok(status) => assert_eq!(
                status.revision,
                serde_json::from_value(preview["packet"]["revision"].clone()).unwrap()
            ),
            Err(error) => assert!(
                format!("{error:#}").contains("incompatible_index"),
                "unexpected selected corruption classification: {error:#}"
            ),
        }
        let attempts = provider.budget().unwrap().attempts;
        let response = app.oneshot(request(&path, json!({}))).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(body["error"]["code"], "incompatible_index");
        assert_eq!(provider.budget().unwrap().attempts, attempts);
        server.abort();
        return;
    }
    let run = tokio::spawn(app.oneshot(request(&path, json!({}))));
    tokio::time::timeout(crate::testing::HANG_BACKSTOP, entered.notified())
        .await
        .unwrap();
    if change_snapshot {
        // The provider has admitted the old packet and is paused. Change
        // captured source so the same daemon-local writer must publish startup reconciliation,
        // rather than taking the unchanged-head fast path.
        std::fs::write(
            workspace.join("a.js"),
            "function seed() { console.log('changed while provider waits'); }",
        )
        .unwrap();
        let publisher = store.clone();
        let owner = session.clone();
        let selected = IndexOptions::new(workspace.clone());
        let prior = store.status().unwrap().revision;
        let publishing = tokio::task::spawn_blocking(move || {
            crate::index_coordinator::IndexJobCoordinator::prepare_with_session(
                &publisher, None, owner,
            )?
            .run_serving(&selected, &cancel, |_| {})
        });
        // startup reconciliation commits while the provider is blocked. A leaked SQLite read
        // snapshot would prevent this publication within the timeout.
        let committed = tokio::time::timeout(crate::testing::HANG_BACKSTOP, publishing)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(committed.index_generation, prior.index_generation);
        assert!(committed.index_revision > prior.index_revision);
    }
    release.notify_one();
    let response = tokio::time::timeout(crate::testing::HANG_BACKSTOP, run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    if change_snapshot {
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("liveJev"));
    } else if scenario == "invalid_selection" {
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "jev_invalid_response");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("saved for inspection"));
        assert!(message.contains("no automatic retry"));
        assert!(!message.contains("probabilities"));
    } else if scenario == "context_exceeded" {
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "jev_context_exceeded");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("reservation is retained"));
        assert!(message.contains("cannot be truncated"));
        assert!(!message.contains("private source"));
    } else {
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["view"]["selectionSource"], "liveJev");
        assert_eq!(
            body["view"]["revision"],
            json!(store.status().unwrap().revision)
        );
        assert!(body["view"]["calls"].as_array().unwrap().len() <= 5);
        assert!(body["attemptId"].as_str().is_some());
        if scenario == "rounded" {
            let warnings = body["warnings"].as_array().unwrap();
            assert!(!warnings.is_empty());
            for warning in warnings {
                assert!(
                    body["view"]["warnings"]
                        .as_array()
                        .unwrap()
                        .contains(warning)
                );
            }
        } else {
            assert_eq!(body["warnings"], json!([]));
        }
    }
    assert_eq!(provider.budget().unwrap().reserved_cents, 10);
    assert_eq!(provider.budget().unwrap().attempts, 1);
    server.abort();
}
