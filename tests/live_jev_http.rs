#![cfg(feature = "test-support")]
//! No credentials or outbound calls. Enabled fixtures use an already exhausted local ledger.
mod common;
mod offline {

    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use serde_json::{Value, json};
    use std::sync::{Arc, atomic::AtomicBool};
    use tower::ServiceExt;
    use trellis::{
        http,
        indexer::{IndexOptions, index_workspace_bundle},
        model::*,
        store::Store,
    };
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    pub(super) fn setup(
        padding: usize,
    ) -> (
        tempfile::TempDir,
        Store,
        Graph,
        Router,
        Value,
        Arc<trellis::store::topology::LeaderSession>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("a.js"), format!("function leaf() {{}}\nfunction helper() {{ leaf(); }}\nfunction seed(flag) {{ if (flag) helper(); console.log(flag); }}\n//{}", "x".repeat(padding))).unwrap();
        let options = IndexOptions::new(workspace.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        let store = crate::common::open_store(&dir.path().join("state"), &workspace).unwrap();
        let (graph, native, capture) =
            index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
        // Named calls remain measured terminal sites; they do not link to sibling declarations.
        let seed = graph
            .nodes
            .iter()
            .find(|n| n.name == "seed")
            .unwrap()
            .id
            .clone();
        let session = store.leader_session().unwrap();
        store
            .publish_native(
                &graph,
                &capture,
                &native,
                session.leader_guard().unwrap(),
                store.index_baseline().unwrap(),
                &cancel,
            )
            .unwrap();
        let state = http::new(
            store.clone(),
            options,
            TOKEN.into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        state.retain_serving_session(session.clone());
        let pin = store.status().unwrap().revision;
        (
            dir,
            store,
            graph,
            http::router(state),
            json!({"seed":seed,"question":"helper leaf", "expectedRevision":pin}),
            session,
        )
    }
    pub(super) async fn call(
        app: &Router,
        method: &str,
        path: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "127.0.0.1:7331")
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(
            response.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("connect-src 'self'")
        );
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
}
use axum::{body::Body, http::Request};
use offline::{call, setup};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};
use tower::ServiceExt;
use trellis::{http, indexer::IndexOptions, live_jev::LiveJev};
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
#[tokio::test]
async fn offline_constructor_exposes_disabled_status_and_never_runs() {
    let (_d, _, _, app, _, _session) = setup(0);
    assert_eq!(
        call(&app, "GET", "/api/jev/status", Value::Null).await,
        (
            axum::http::StatusCode::OK,
            json!({"enabled":false,"budget":null})
        )
    );
    assert_eq!(
        call(&app, "POST", "/api/questions/absent/jev-run", json!({}))
            .await
            .0,
        503
    );
}
#[tokio::test]
async fn new_routes_preserve_security_guards_and_limits() {
    let (_d, _, _, app, _, _session) = setup(0);
    for (method, url) in [
        ("GET", "/api/jev/status"),
        ("POST", "/api/questions/p/jev-run"),
    ] {
        for (host, origin, auth, size, want) in [
            ("127.0.0.1:7331", None, None, 0, 401),
            ("evil.invalid", None, Some(TOKEN), 0, 403),
            ("127.0.0.1:7331", Some("null"), Some(TOKEN), 0, 403),
            ("127.0.0.1:7331", None, Some(TOKEN), 1024 * 1024 + 1, 413),
        ] {
            let mut req = Request::builder()
                .method(method)
                .uri(url)
                .header("host", host);
            if let Some(origin) = origin {
                req = req.header("origin", origin);
            }
            if let Some(auth) = auth {
                req = req.header("authorization", format!("Bearer {auth}"));
            }
            let response = app
                .clone()
                .oneshot(req.body(Body::from(vec![b'x'; size])).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), want);
            assert_eq!(response.headers()["cache-control"], "no-store");
        }
    }
}
#[tokio::test]
async fn enabled_status_exhaustion_and_stale_preflight_are_offline() {
    let (dir, store, graph, _, request, session) = setup(0);
    let workspace = dir.path().join("workspace");
    let ledger = dir.path().join("budget");
    let provider = Arc::new(
        LiveJev::open(&ledger, "synthetic-not-a-credential".into(), 10, &workspace).unwrap(),
    );
    // Seed a retained reservation directly. run() must reject before transport.
    let db = rusqlite::Connection::open(ledger.join("budget.sqlite3")).unwrap();
    db.execute(
        "INSERT INTO attempts(id,reserved,request,status) VALUES('synthetic',10,x'7b7d','failed')",
        [],
    )
    .unwrap();
    drop(db);
    let state = http::new_with_jev(
        store.clone(),
        IndexOptions::new(workspace),
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
        Some(provider),
    )
    .unwrap();
    state.retain_serving_session(session.clone());
    let app = http::router(state);
    let (code, status) = call(&app, "GET", "/api/jev/status", Value::Null).await;
    assert_eq!(code, 200);
    assert_eq!(status["budget"]["reservedCents"], 10);
    assert_eq!(status["budget"]["remainingCents"], 0);
    assert!(!status.to_string().contains("synthetic-not-a-credential"));
    let leaf = graph.nodes.iter().find(|node| node.name == "leaf").unwrap();
    let (code, empty) = call(
        &app,
        "POST",
        "/api/questions/preview",
        json!({"seed":leaf.id,"question":"leaf", "expectedRevision":store.status().unwrap().revision}),
    )
    .await;
    assert_eq!(code, 200);
    let empty_url = format!(
        "/api/questions/{}/jev-run",
        empty["packet"]["packetId"].as_str().unwrap()
    );
    let (code, error) = call(&app, "POST", &empty_url, json!({})).await;
    assert_eq!(code, 422);
    assert_eq!(error["error"]["code"], "jev_no_candidates");
    let (_, preview) = call(&app, "POST", "/api/questions/preview", request).await;
    let url = format!(
        "/api/questions/{}/jev-run",
        preview["packet"]["packetId"].as_str().unwrap()
    );
    assert_eq!(
        call(&app, "POST", &url, json!({"providerKey":"override"}))
            .await
            .0,
        400
    );
    assert_eq!(call(&app, "POST", &url, json!({})).await.0, 429);
    let cancel = Arc::new(AtomicBool::new(false));
    let (_, native, capture) = trellis::indexer::index_workspace_bundle(
        &IndexOptions::new(dir.path().join("workspace")),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let expected = store.status().unwrap().revision;
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            session.leader_guard().unwrap(),
            expected,
            &cancel,
        )
        .unwrap();
    assert_eq!(call(&app, "POST", &url, json!({})).await.0, 409);
    assert_eq!(
        call(&app, "GET", "/api/jev/status", Value::Null).await.1["budget"]["attempts"],
        1
    );
}

#[tokio::test]
async fn imported_rounded_response_adds_warning_only_after_validation() {
    let (_dir, _, _, app, request, _session) = setup(0);
    let (status, preview) = call(&app, "POST", "/api/questions/preview", request).await;
    assert_eq!(status, 200);
    let packet: trellis::planning::QuestionPacket =
        serde_json::from_value(preview["packet"].clone()).unwrap();
    let path = format!("/api/questions/{}/jev-response", packet.packet_id);
    for essential in [0.69, 0.71, 0.60, 0.70] {
        let answers: serde_json::Map<String, Value> = packet.context.calls.iter().enumerate().map(|(i, _)| (
            format!("c{i}_{}", packet.packet_id),
            json!({"type":"choice","choice":"essential","confidence":0.6,
                "probabilities":{"essential":essential,"supporting":0.2,"incidental":0.1,"uncertain":0.0}}),
        )).collect();
        let response = json!({"model":"jev-1.13.0","answers":answers});
        let warnings = trellis::jev::response_warnings(&response);
        let (status, body) = call(&app, "POST", &path, response).await;
        if essential == 0.60 {
            assert!(!status.is_success());
            assert!(body.get("view").is_none());
        } else {
            assert_eq!(status, 200);
            assert_eq!(body["view"]["selectionSource"], "importedJev");
            if essential == 0.70 {
                assert!(warnings.is_empty());
            } else {
                assert!(!warnings.is_empty());
                for warning in warnings {
                    assert!(
                        body["view"]["warnings"]
                            .as_array()
                            .unwrap()
                            .contains(&json!(warning))
                    );
                }
            }
        }
    }
}
