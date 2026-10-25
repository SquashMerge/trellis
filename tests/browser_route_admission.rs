use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
};
use std::{fs, sync::Arc, time::Instant};
use tokio::sync::Mutex;
use tower::ServiceExt;
use trellis::{
    daemon::{
        BrowserProvisioner,
        registry::{
            BROWSER_IDLE_DELAY, BrowserOptions, CHECKOUT_RELEASE_DELAY, CheckoutOptions,
            CheckoutRegistry,
        },
    },
    store::topology::WorkspaceIdentity,
};

#[tokio::test]
async fn unknown_checkout_routes_do_not_attach_activate_or_renew() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    fs::create_dir(root.join(".git")).unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let registry = Arc::new(Mutex::new(CheckoutRegistry::new()));
    let mut browser = BrowserProvisioner::new();
    let token_path = root.join("token");
    let address = browser
        .register_serve(
            &registry,
            &identity,
            CheckoutOptions(serde_json::to_value(BrowserOptions::default()).unwrap()),
            "127.0.0.1:0".parse().unwrap(),
            &token_path,
        )
        .await
        .unwrap();
    let token = fs::read_to_string(token_path).unwrap();
    let router = browser.router().unwrap();
    let now = Instant::now();
    // A retained registration alone is not an active checkout.
    assert_eq!(registry.lock().await.active_count(), 0);
    let invalid = [
        (Method::GET, "unknown"),
        (Method::POST, "git"),
        (Method::PUT, "git/diff"),
        (Method::DELETE, "git"),
        (Method::GET, "git/extra"),
        (Method::GET, "git/diff/extra"),
        (Method::POST, "status"),
        (Method::GET, "views/a/extra"),
        (Method::GET, "annotations/a"),
        (Method::POST, "questions/a/unknown"),
        (Method::POST, "questions/a/extra/jev-run"),
        (Method::POST, "jobs/a/extra/cancel"),
        (Method::POST, "jobs//cancel"),
        (Method::GET, "symbols"),
        (Method::GET, "symbol"),
        (Method::POST, "query"),
        (Method::GET, "views"),
        (Method::GET, "views/id"),
        (Method::PUT, "views/id"),
        (Method::DELETE, "views/id"),
        (Method::GET, "annotations"),
        (Method::PUT, "annotations/id"),
        (Method::DELETE, "annotations/id"),
        (Method::POST, "questions/preview"),
        (Method::GET, "questions/id/jev-request"),
        (Method::POST, "questions/id/jev-response"),
        (Method::POST, "questions/id/selection"),
        (Method::POST, "questions/id/jev-run"),
        (Method::POST, "questions/id/acp-answer"),
        (Method::GET, "jev/status"),
        (Method::GET, "acp/status"),
    ];
    for active in [false, true] {
        if active {
            registry
                .lock()
                .await
                .browser_request_at(&identity, now)
                .unwrap();
        }
        for (method, suffix) in &invalid {
            // Exercise the production listener router, including its auth boundary.
            // Retired routes are indistinguishable from any ordinary unknown route,
            // with or without an explicit checkout selector.
            for selected in [false, true] {
                let prefix = if selected {
                    format!("/api/checkouts/{}", identity.root_key)
                } else {
                    "/api".to_owned()
                };
                for (host, credential, origin, expected) in [
                    (
                        address.to_string(),
                        Some(token.as_str()),
                        None,
                        StatusCode::NOT_FOUND,
                    ),
                    (address.to_string(), None, None, StatusCode::UNAUTHORIZED),
                    (
                        address.to_string(),
                        Some("wrong"),
                        None,
                        StatusCode::UNAUTHORIZED,
                    ),
                    (
                        "example.invalid".to_owned(),
                        Some(token.as_str()),
                        None,
                        StatusCode::FORBIDDEN,
                    ),
                    (
                        address.to_string(),
                        Some(token.as_str()),
                        Some("https://example.invalid"),
                        StatusCode::FORBIDDEN,
                    ),
                ] {
                    let mut results = Vec::new();
                    for path in [
                        format!("{prefix}/ordinary-unknown"),
                        format!("{prefix}/{suffix}"),
                    ] {
                        let mut request = Request::builder()
                            .method(method.clone())
                            .uri(&path)
                            .header("host", &host);
                        if let Some(credential) = credential {
                            request =
                                request.header("authorization", format!("Bearer {credential}"));
                        }
                        if let Some(origin) = origin {
                            request = request.header("origin", origin);
                        }
                        let response = router
                            .clone()
                            .oneshot(request.body(Body::empty()).unwrap())
                            .await
                            .unwrap();
                        assert_eq!(response.status(), expected, "{method} {path}");
                        assert_eq!(response.headers()["cache-control"], "no-store");
                        assert!(!response.headers().contains_key("X-Trellis-Workspace"));
                        assert!(!response.headers().contains_key("X-Trellis-Catching-Up"));
                        results.push(
                            axum::body::to_bytes(response.into_body(), 4096)
                                .await
                                .unwrap(),
                        );
                    }
                    assert_eq!(results[0], results[1], "{method} {prefix}/{suffix}");
                }
            }
            assert!(registry.lock().await.runtime(&identity.root_key).is_none());
            assert_eq!(registry.lock().await.active_count(), usize::from(active));
        }
    }
    let tick = registry
        .lock()
        .await
        .advance(now + BROWSER_IDLE_DELAY + CHECKOUT_RELEASE_DELAY)
        .unwrap();
    assert_eq!(tick.released, vec![identity.root_key.clone()]);
    assert_eq!(registry.lock().await.active_count(), 0);
}
