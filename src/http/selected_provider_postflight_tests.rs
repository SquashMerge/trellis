use super::*;
use crate::{
    daemon::registry::{CheckoutOptions, CheckoutRegistry},
    store::topology::{TopologyRoots, WorkspaceIdentity},
};
use std::os::unix::fs::PermissionsExt;
use tower::ServiceExt;

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const SOURCE: &str = "function seed() { console.log('synthetic'); }";

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method(method)
                .uri(path)
                .header("host", "127.0.0.1:7331")
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, headers, body)
}

async fn selected_provider_case(kind: &'static str, outcome: &'static str, corrupt: bool) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("a.js"), SOURCE).unwrap();
    let identity = WorkspaceIdentity::discover(Some(&workspace), &workspace).unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(dir.path().join("cache"), dir.path().join("data"));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let mut mock_server = None;
    let registration = if kind == "jev" {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let entered_mock = entered.clone();
        let release_mock = release.clone();
        let mock = Router::new().route("/", post(move |Json(request): Json<Value>| {
                let entered = entered_mock.clone();
                let release = release_mock.clone();
                async move {
                    entered.notify_one();
                    release.notified().await;
                    if outcome == "failure" {
                        return (StatusCode::BAD_REQUEST, Json(json!({"detail":{"error_type":"max_tokens_exceeded"}}))).into_response();
                    }
                    let answers: serde_json::Map<String, Value> = request["questions"].as_object().unwrap()
                        .keys().map(|key| (key.clone(), json!({"type":"choice","choice":"essential","confidence":0.6,
                            "probabilities":{"essential":0.7,"supporting":0.2,"incidental":0.1,"uncertain":0.0}}))).collect();
                    Json(json!({"model":"jev-1.13.0","answers":answers,
                        "usage":{"input_tokens":1,"output_tokens":1}})).into_response()
                }
            }));
        mock_server = Some(tokio::spawn(async move {
            axum::serve(listener, mock).await.unwrap();
        }));
        let mut registry = CheckoutRegistry::with_roots(roots);
        registry.set_jev_endpoint_for_tests(endpoint);
        let options = json!({"jevBudgetDir":dir.path().join("budget"),"jevBudgetCents":10,"jevKey":"synthetic"});
        registry
            .register(&identity, CheckoutOptions(options))
            .unwrap();
        registry
    } else {
        let runner = dir.path().join("runner");
        std::fs::write(&runner, format!(
                "#!/bin/sh\ncat >/dev/null\n: > '{}'\nsteps=0\nwhile [ ! -e '{}' ] && [ \"$steps\" -lt 600 ]; do sleep 0.02; steps=$((steps+1)); done\n[ -e '{}' ] || exit 2\ncat '{}'\n",
                dir.path().join("entered").display(), dir.path().join("release").display(),
                dir.path().join("release").display(), dir.path().join("response.json").display(),
            )).unwrap();
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut registry = CheckoutRegistry::with_roots(roots);
        let options = json!({"acpRunner":runner,"acpStateDir":dir.path().join("acp-state"),"acpMaxAttempts":1});
        registry
            .register(&identity, CheckoutOptions(options))
            .unwrap();
        registry
    };
    let registry = Arc::new(tokio::sync::Mutex::new(registration));
    let app = ProvisionedBrowser::new(
        registry.clone(),
        TOKEN.to_owned(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap()
    .router();
    let prefix = format!("/api/checkouts/{}", identity.root_key);
    let status = tokio::time::timeout(crate::testing::HANG_BACKSTOP, async {
        loop {
            let (code, _, status) =
                call(&app, "GET", &format!("{prefix}/status"), Value::Null).await;
            if code == StatusCode::OK && status["catchingUp"] == false {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let (code, _, symbols) = call(
        &app,
        "GET",
        &format!("{prefix}/symbols?q=seed"),
        Value::Null,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{kind}: {symbols}");
    let seed = symbols["items"][0]["id"].as_str().unwrap();
    let (code, _, preview) = call(
        &app,
        "POST",
        &format!("{prefix}/questions/preview"),
        json!({"seed":seed,"question":"What is logged?","expectedRevision":status["revision"]}),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{kind}: {preview}");
    let packet = preview["packet"]["packetId"].as_str().unwrap();
    let pin: IndexPin = serde_json::from_value(status["revision"].clone()).unwrap();
    if kind == "acp" {
        let response = if outcome == "success" {
            json!({"answer":{"packetId":packet,"summary":[{"text":"Logs synthetic.","citations":[{"path":"a.js","startLine":1,"endLine":1,"quote":SOURCE}]}],"branches":[],"limitations":[]},"estimatedUsd":0.01}).to_string()
        } else {
            "invalid provider result".to_owned()
        };
        std::fs::write(dir.path().join("response.json"), response).unwrap();
    }
    let action = if kind == "jev" {
        "jev-run"
    } else {
        "acp-answer"
    };
    let task_app = app.clone();
    let route = format!("{prefix}/questions/{packet}/{action}");
    let request = tokio::spawn(async move { call(&task_app, "POST", &route, json!({})).await });
    if kind == "jev" {
        tokio::time::timeout(crate::testing::HANG_BACKSTOP, entered.notified())
            .await
            .unwrap();
    } else {
        let marker = dir.path().join("entered");
        tokio::time::timeout(crate::testing::HANG_BACKSTOP, async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }
    if corrupt {
        let runtime = registry.lock().await.runtime(&identity.root_key).unwrap();
        let state = runtime.browser_scheduler().unwrap();
        state
            .store
            .corrupt_selected_source_for_tests(pin, "a.js")
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.join("a.js")).unwrap(),
            SOURCE,
            "workspace bytes remain untouched and no new startup reconciliation is required"
        );
    }
    if kind == "jev" {
        release.notify_one();
    } else {
        std::fs::write(dir.path().join("release"), b"go\n").unwrap();
    }
    let (code, headers, body) = tokio::time::timeout(crate::testing::HANG_BACKSTOP, request)
        .await
        .unwrap()
        .unwrap();
    if corrupt {
        assert_eq!(
            code,
            StatusCode::SERVICE_UNAVAILABLE,
            "{kind} {outcome}: {body}"
        );
        assert_eq!(
            body["error"]["code"], "incompatible_index",
            "{kind} {outcome}: {body}"
        );
        assert_eq!(
            headers["X-Trellis-Mutation-Outcome"], "unknown",
            "{kind} {outcome}"
        );
        assert!(
            body.get("answer").is_none(),
            "no stale selected answer may escape"
        );
    } else {
        assert_eq!(
            code,
            StatusCode::OK,
            "{kind} clean provider control: {body}"
        );
        assert_eq!(headers["X-Trellis-Mutation-Outcome"], "committed");
        if kind == "jev" {
            assert_eq!(body["view"]["selectionSource"], "liveJev", "{body}");
        } else {
            assert_eq!(body["answer"]["packetId"], packet, "{body}");
        }
    }
    if let Some(server) = mock_server {
        server.abort();
    }
}

#[tokio::test]
async fn selected_jev_and_acp_postflight_reattest_stored_packet_on_success_and_failure() {
    for kind in ["jev", "acp"] {
        // A real successful provider result is the positive control for
        // both post-await branches before testing the same-pin corruption.
        selected_provider_case(kind, "success", false).await;
        for outcome in ["success", "failure"] {
            selected_provider_case(kind, outcome, true).await;
        }
    }
}
