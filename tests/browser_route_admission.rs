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
            let request = Request::builder()
                .method(method.clone())
                .uri(format!("/api/checkouts/{}/{}", identity.root_key, suffix))
                .header("host", address.to_string())
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                router.clone().oneshot(request).await.unwrap().status(),
                StatusCode::NOT_FOUND,
                "{method} {suffix}"
            );
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
