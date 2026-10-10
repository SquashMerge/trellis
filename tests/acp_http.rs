#![cfg(feature = "test-support")]
//! Synthetic executable fixtures only. No model, Jev, or network calls.
#![cfg(unix)]
mod common;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::{Value, json};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, atomic::AtomicBool},
};
use tower::ServiceExt;
use trellis::{
    acp::{Acp, AcpConfig},
    http,
    indexer::{IndexOptions, index_workspace},
    model::Graph,
    store::{Store, topology::LeaderSession},
};
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const CODE: &str = "function seed() { console.log('synthetic'); }";
struct Fixture {
    dir: tempfile::TempDir,
    store: Store,
    graph: Graph,
    app: Router,
    session: Arc<LeaderSession>,
    provider: Option<Arc<Acp>>,
    seed: String,
}
impl Fixture {
    fn new(enabled: bool, gated: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("a.js"), CODE).unwrap();
        let options = IndexOptions::new(workspace.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        let graph = index_workspace(&options, &cancel, |_| {}).unwrap();
        let seed = graph
            .nodes
            .iter()
            .find(|n| n.name == "seed")
            .unwrap()
            .id
            .clone();
        let store = crate::common::open_store(&dir.path().join("index"), &workspace).unwrap();
        let session = store.leader_session().unwrap();
        publish_bundle(
            &store,
            &graph,
            &workspace,
            session.leader_guard().unwrap(),
            trellis::model::IndexPin {
                index_generation: store.index_baseline().unwrap().index_generation,
                index_revision: 0,
            },
            &cancel,
        )
        .unwrap();
        let runner = dir.path().join("runner");
        let gate = if gated {
            for name in ["entered", "release"] {
                assert!(
                    std::process::Command::new("mkfifo")
                        .arg(dir.path().join(name))
                        .status()
                        .unwrap()
                        .success()
                );
            }
            format!(
                "printf ready > '{}'/entered\nread gate < '{}'/release\n",
                dir.path().display(),
                dir.path().display()
            )
        } else {
            String::new()
        };
        std::fs::write(
            &runner,
            format!(
                "#!/bin/sh\ncat > /dev/null\n{gate}cat '{}'/response.json\n",
                dir.path().display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
        let provider = enabled.then(|| {
            Arc::new(
                Acp::open(AcpConfig {
                    runner,
                    state_dir: dir.path().join("private-acp"),
                    max_attempts: 1,
                    workspace,
                })
                .unwrap(),
            )
        });
        let state = http::new_with_providers(
            store.clone(),
            options,
            TOKEN.into(),
            "127.0.0.1:7331".parse().unwrap(),
            None,
            provider.clone(),
        )
        .unwrap();
        state.retain_serving_session(session.clone());
        let app = http::router(state);
        Self {
            dir,
            store,
            graph,
            app,
            session,
            provider,
            seed,
        }
    }
    async fn preview(&self) -> String {
        let (status, body) = call(
            &self.app,
            "POST",
            "/api/questions/preview",
            json!({"seed":self.seed,"question":"What is logged?","expectedRevision":self.store.status().unwrap().revision}),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        body["packet"]["packetId"].as_str().unwrap().into()
    }
    fn response(&self, packet: &str, valid: bool) {
        let value = json!({"answer":{"packetId":packet,"summary":[{"text":"Logs synthetic.","citations":[{
            "path":"a.js","startLine":1,"endLine":1,"quote":if valid {CODE} else {"private invalid quote"}
        }]}],"branches":[],"limitations":[]},"estimatedUsd":0.01});
        std::fs::write(self.dir.path().join("response.json"), value.to_string()).unwrap();
    }
    fn advance(&self) {
        let index_generation = self.store.status().unwrap().revision.index_generation;
        publish_bundle(
            &self.store,
            &self.graph,
            &self.dir.path().join("workspace"),
            self.session.leader_guard().unwrap(),
            trellis::model::IndexPin {
                index_generation,
                index_revision: 1,
            },
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    }
}
async fn call(app: &Router, method: &str, path: &str, body: Value) -> (u16, Value) {
    raw(app, method, path, body.to_string()).await
}
async fn raw(app: &Router, method: &str, path: &str, body: String) -> (u16, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:7331")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
#[tokio::test]
async fn disabled_and_missing_are_safe() {
    let f = Fixture::new(false, false);
    assert_eq!(
        call(&f.app, "GET", "/api/acp/status", Value::Null).await,
        (200, json!({"enabled":false,"status":null}))
    );
    assert_eq!(
        call(
            &f.app,
            "POST",
            "/api/questions/absent/acp-answer",
            json!({})
        )
        .await
        .0,
        503
    );
    let f = Fixture::new(true, false);
    assert_eq!(
        call(
            &f.app,
            "POST",
            "/api/questions/absent/acp-answer",
            json!({})
        )
        .await
        .0,
        404
    );
    assert_eq!(f.provider.unwrap().status().unwrap().attempts, 0);
}
#[tokio::test]
async fn success_is_labeled_and_allowance_is_separate_and_retained() {
    let f = Fixture::new(true, false);
    let id = f.preview().await;
    f.response(&id, true);
    let path = format!("/api/questions/{id}/acp-answer");
    let (status, body) = call(&f.app, "POST", &path, json!({})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["packetId"], id);
    assert_eq!(body["answer"]["packetId"], id);
    assert_eq!(body["revision"], json!(f.store.status().unwrap().revision));
    assert_eq!(body["source"], "liveAcp");
    assert!(body["attemptId"].is_string());
    assert!(body["latencyMs"].is_number());
    assert_eq!(body["estimatedUsd"], 0.01);
    let (_, status) = call(&f.app, "GET", "/api/acp/status", Value::Null).await;
    assert_eq!(status["status"]["attempts"], 1);
    assert_eq!(status["status"]["remainingAttempts"], 0);
    assert_eq!(status["status"]["model"], "sonnet");
    assert_eq!(status["status"]["maxEstimatedUsdPerAttempt"], 1.0);
    assert_eq!(call(&f.app, "POST", &path, json!({})).await.0, 429);
    assert_eq!(
        call(&f.app, "GET", "/api/jev/status", Value::Null).await.1,
        json!({"enabled":false,"budget":null})
    );
}
#[tokio::test]
async fn invalid_answer_and_provider_errors_are_sanitized() {
    for valid_json in [true, false] {
        let f = Fixture::new(true, false);
        let id = f.preview().await;
        f.response(&id, false);
        if !valid_json {
            std::fs::write(
                f.dir.path().join("runner"),
                "#!/bin/sh\necho 'private provider output' >&2\nexit 1\n",
            )
            .unwrap();
        }
        let (status, body) = call(
            &f.app,
            "POST",
            &format!("/api/questions/{id}/acp-answer"),
            json!({}),
        )
        .await;
        assert_eq!(status, if valid_json { 422 } else { 502 }, "{body}");
        assert!(!body.to_string().contains("private"));
        assert_eq!(f.provider.as_ref().unwrap().status().unwrap().attempts, 1);
    }
}
#[tokio::test]
async fn stale_preflight_does_not_reserve() {
    let f = Fixture::new(true, false);
    let id = f.preview().await;
    f.advance();
    assert_eq!(
        call(
            &f.app,
            "POST",
            &format!("/api/questions/{id}/acp-answer"),
            json!({})
        )
        .await
        .0,
        409
    );
    assert_eq!(f.provider.unwrap().status().unwrap().attempts, 0);
}
#[tokio::test]
async fn stale_success_and_failure_are_rejected_after_process() {
    for valid in [true, false] {
        let f = Fixture::new(true, true);
        let id = f.preview().await;
        f.response(&id, valid);
        let app = f.app.clone();
        let path = format!("/api/questions/{id}/acp-answer");
        let run = tokio::spawn(async move { call(&app, "POST", &path, json!({})).await });
        let entered = f.dir.path().join("entered");
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::task::spawn_blocking(move || std::fs::read(entered).unwrap()),
        )
        .await
        .unwrap()
        .unwrap();
        f.advance();
        let release = f.dir.path().join("release");
        tokio::task::spawn_blocking(move || std::fs::write(release, b"go\n").unwrap())
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), run)
                .await
                .unwrap()
                .unwrap()
                .0,
            409
        );
        assert_eq!(f.provider.as_ref().unwrap().status().unwrap().attempts, 1);
    }
}
#[tokio::test]
async fn body_overrides_invalid_shapes_and_size_are_rejected_without_reservation() {
    let f = Fixture::new(true, false);
    let id = f.preview().await;
    let path = format!("/api/questions/{id}/acp-answer");
    for body in [
        json!({"prompt":"override"}),
        json!({"source":"override"}),
        json!({"runner":"override"}),
        json!({"maxAttempts":20}),
        json!({"packet":{}}),
        json!(null),
        json!([]),
    ] {
        assert_eq!(call(&f.app, "POST", &path, body).await.0, 400);
    }
    assert_eq!(raw(&f.app, "POST", &path, String::new()).await.0, 400);
    assert_eq!(
        raw(&f.app, "POST", &path, "x".repeat(1024 * 1024 + 1))
            .await
            .0,
        413
    );
    assert_eq!(f.provider.unwrap().status().unwrap().attempts, 0);
}

#[tokio::test]
async fn routes_keep_auth_host_and_origin_guards() {
    let f = Fixture::new(true, false);
    for (method, path) in [
        ("GET", "/api/acp/status"),
        ("POST", "/api/questions/p/acp-answer"),
    ] {
        for (host, origin, auth, expected) in [
            ("127.0.0.1:7331", None, None, 401),
            ("evil.invalid", None, Some(TOKEN), 403),
            ("127.0.0.1:7331", Some("null"), Some(TOKEN), 403),
        ] {
            let mut req = Request::builder()
                .method(method)
                .uri(path)
                .header("host", host);
            if let Some(origin) = origin {
                req = req.header("origin", origin);
            }
            if let Some(auth) = auth {
                req = req.header("authorization", format!("Bearer {auth}"));
            }
            let response = f
                .app
                .clone()
                .oneshot(req.body(Body::from("{}")).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), expected);
            assert_eq!(response.headers()["cache-control"], "no-store");
        }
    }
    assert_eq!(f.provider.unwrap().status().unwrap().attempts, 0);
}

#[tokio::test]
async fn controlled_process_diagnostics_are_actionable_but_never_echo_output() {
    for (category, expected_code) in [
        ("auth_required", "acp_auth_required"),
        ("model_unavailable", "acp_model_unavailable"),
        ("model_mismatch", "acp_model_mismatch"),
        ("auth_required private details", "acp_failed"),
    ] {
        let f = Fixture::new(true, false);
        let id = f.preview().await;
        std::fs::write(
            f.dir.path().join("response.json"),
            json!({
                "error":category,"partialAnswer":"private model output"
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(f.dir.path().join("runner"), format!(
            "#!/bin/sh\ncat > /dev/null\ncat '{}'/response.json\necho 'private stderr' >&2\nexit 1\n", f.dir.path().display()
        )).unwrap();
        let (status, body) = call(
            &f.app,
            "POST",
            &format!("/api/questions/{id}/acp-answer"),
            json!({}),
        )
        .await;
        assert_eq!(status, 502);
        assert_eq!(body["error"]["code"], expected_code);
        assert!(!body.to_string().contains("private"));
        assert!(!body.to_string().contains("partialAnswer"));
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no automatic retry")
        );
        assert_eq!(f.provider.as_ref().unwrap().status().unwrap().attempts, 1);
    }
}

fn publish_bundle(
    store: &trellis::store::Store,
    graph: &trellis::model::Graph,
    workspace: &std::path::Path,
    leader: &trellis::store::topology::LeaderGuard,
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
