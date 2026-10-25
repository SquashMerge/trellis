use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use std::{
    fs,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tower::ServiceExt;
use trellis::{
    daemon::{
        BrowserProvisioner,
        registry::{BrowserOptions, CheckoutOptions, CheckoutRegistry},
    },
    store::topology::WorkspaceIdentity,
};

fn checkout(root: &Path) -> WorkspaceIdentity {
    fs::create_dir_all(root.join(".git")).unwrap();
    WorkspaceIdentity::discover(Some(root), root)
        .unwrap()
        .attach_marker()
        .unwrap()
}
fn options() -> CheckoutOptions {
    CheckoutOptions(serde_json::to_value(BrowserOptions::default()).unwrap())
}
async fn response(
    router: axum::Router,
    address: std::net::SocketAddr,
    route: &str,
    token: Option<&str>,
) -> axum::response::Response {
    let mut builder = Request::builder()
        .uri(route)
        .header("host", address.to_string());
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    router
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn socket_first_then_serve_and_serve_first_then_mcp_are_inert() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let a = checkout(&root.join("a"));
    let b = checkout(&root.join("b"));
    for socket_first in [true, false] {
        let registry = Arc::new(Mutex::new(CheckoutRegistry::new()));
        let mut browser = BrowserProvisioner::new();
        assert!(browser.address().is_none());
        let token_file = root.join(format!("token-{socket_first}"));
        assert!(!token_file.exists());
        if socket_first {
            registry.lock().await.attach_launch(1, &a).unwrap();
        }
        assert!(
            browser
                .register_serve(
                    &registry,
                    &a,
                    options(),
                    "127.0.0.1:0".parse().unwrap(),
                    &token_file
                )
                .await
                .is_ok()
        );
        let address = browser.address().unwrap();
        assert_ne!(address.port(), 0);
        if !socket_first {
            registry.lock().await.attach_launch(1, &a).unwrap();
        }
        let registered = registry.lock().await;
        assert_eq!(registered.active_count(), 1);
        assert!(registered.runtime(&a.root_key).is_none());
        drop(registered);
        let token = fs::read_to_string(&token_file).unwrap();
        let router = browser.router().unwrap();
        assert_eq!(
            response(router.clone(), address, "/api/checkouts", None)
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            response(router.clone(), address, "/api/checkouts", Some(&token))
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            response(router.clone(), address, "/api/daemon/status", Some(&token))
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            response(router.clone(), address, "/api/status", None)
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            response(router.clone(), address, "/api/status", Some(&token))
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            response(router.clone(), address, "/healthz", None)
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            response(router.clone(), address, "/", None).await.status(),
            StatusCode::OK
        );
        assert_eq!(
            response(router.clone(), address, "/app.js", None)
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(registry.lock().await.active_count(), 1);
        // Exact repeat is idempotent, including requested port zero.
        assert_eq!(
            browser
                .register_serve(
                    &registry,
                    &a,
                    options(),
                    "127.0.0.1:0".parse().unwrap(),
                    &token_file
                )
                .await
                .unwrap(),
            address
        );
        assert!(
            browser
                .register_serve(
                    &registry,
                    &b,
                    options(),
                    "0.0.0.0:0".parse().unwrap(),
                    &token_file
                )
                .await
                .is_err()
        );
        assert!(
            browser
                .register_serve(
                    &registry,
                    &b,
                    options(),
                    "127.0.0.1:1".parse().unwrap(),
                    &token_file
                )
                .await
                .is_err()
        );
        assert!(registry.lock().await.registration(&b.root_key).is_none());
        let alternate = root.join(format!("alternate-{socket_first}"));
        assert!(
            browser
                .register_serve(
                    &registry,
                    &b,
                    options(),
                    "127.0.0.1:0".parse().unwrap(),
                    &alternate
                )
                .await
                .is_err()
        );
        assert!(!alternate.exists());
        fs::write(&token_file, "0".repeat(64)).unwrap();
        assert!(
            browser
                .register_serve(
                    &registry,
                    &b,
                    options(),
                    "127.0.0.1:0".parse().unwrap(),
                    &token_file
                )
                .await
                .is_err()
        );
        assert!(registry.lock().await.registration(&b.root_key).is_none());
        fs::write(&token_file, &token).unwrap();
        assert_eq!(
            browser
                .register_serve(
                    &registry,
                    &b,
                    options(),
                    "127.0.0.1:0".parse().unwrap(),
                    &token_file
                )
                .await
                .unwrap(),
            address
        );
        assert_eq!(registry.lock().await.active_count(), 1);
        registry.lock().await.disconnect(1);
        assert_eq!(registry.lock().await.active_count(), 1); // release has its own timer
    }
}

#[tokio::test]
async fn listener_and_control_do_not_postpone_idle_exit() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let checkout = checkout(&root.join("checkout"));
    let registry = Arc::new(Mutex::new(CheckoutRegistry::starting_at(
        Instant::now() - Duration::from_secs(31 * 60),
    )));
    let mut browser = BrowserProvisioner::new();
    let address = browser
        .register_serve(
            &registry,
            &checkout,
            options(),
            "127.0.0.1:0".parse().unwrap(),
            &root.join("token"),
        )
        .await
        .unwrap();
    assert_eq!(registry.lock().await.active_count(), 0);
    let (_control, _daemon_control) = std::os::unix::net::UnixStream::pair().unwrap();
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let serving = browser.spawn_with_shutdown(shutdown_rx).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        trellis::daemon::run_idle_lifecycle(registry).await.unwrap();
        shutdown.send(true).unwrap();
        serving.await.unwrap().unwrap();
    })
    .await
    .expect("idle exit did not close the browser listener");
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
}

#[tokio::test]
async fn conflicting_checkout_settings_and_token_inode_leave_other_entries_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let first = checkout(&root.join("first"));
    let second = checkout(&root.join("second"));
    let token_file = root.join("token");
    let registry = Arc::new(Mutex::new(CheckoutRegistry::new()));
    let mut browser = BrowserProvisioner::new();
    let original = options();
    browser
        .register_serve(
            &registry,
            &first,
            original.clone(),
            "127.0.0.1:0".parse().unwrap(),
            &token_file,
        )
        .await
        .unwrap();
    let address = browser.address();
    let different = CheckoutOptions(
        serde_json::to_value(BrowserOptions {
            max_file_bytes: 4096,
            ..BrowserOptions::default()
        })
        .unwrap(),
    );
    registry.lock().await.attach_launch(7, &first).unwrap();
    assert!(
        browser
            .register_serve(
                &registry,
                &first,
                different.clone(),
                "127.0.0.1:0".parse().unwrap(),
                &token_file
            )
            .await
            .is_err()
    );
    assert_eq!(
        registry.lock().await.registration(&first.root_key),
        Some(&original)
    );
    assert!(
        registry
            .lock()
            .await
            .registration(&second.root_key)
            .is_none()
    );
    assert_eq!(browser.address(), address);
    registry.lock().await.disconnect(7);
    assert!(registry.lock().await.release(&first.root_key).unwrap());
    assert!(
        browser
            .register_serve(
                &registry,
                &first,
                different.clone(),
                "127.0.0.1:0".parse().unwrap(),
                &token_file
            )
            .await
            .is_ok()
    );
    browser
        .register_serve(
            &registry,
            &second,
            original,
            "127.0.0.1:0".parse().unwrap(),
            &token_file,
        )
        .await
        .unwrap();
    let saved = registry
        .lock()
        .await
        .registration(&second.root_key)
        .cloned();
    fs::rename(&token_file, root.join("old-token")).unwrap();
    fs::write(&token_file, fs::read(root.join("old-token")).unwrap()).unwrap();
    assert!(
        browser
            .register_serve(
                &registry,
                &second,
                different,
                "127.0.0.1:0".parse().unwrap(),
                &token_file
            )
            .await
            .is_err()
    );
    assert_eq!(
        registry
            .lock()
            .await
            .registration(&second.root_key)
            .cloned(),
        saved
    );
    assert_eq!(browser.address(), address);
}

#[tokio::test]
async fn later_serve_compares_configuration_while_http_is_running() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let first = checkout(&root.join("first"));
    let second = checkout(&root.join("second"));
    let registry = Arc::new(Mutex::new(CheckoutRegistry::new()));
    registry.lock().await.attach_launch(1, &first).unwrap();
    let mut browser = BrowserProvisioner::new();
    let token_file = root.join("token");
    let address = browser
        .register_serve(
            &registry,
            &first,
            options(),
            "127.0.0.1:0".parse().unwrap(),
            &token_file,
        )
        .await
        .unwrap();
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let serving = browser.spawn_with_shutdown(shutdown_rx).unwrap();
    assert_eq!(browser.address(), Some(address));
    assert_eq!(
        browser
            .register_serve(
                &registry,
                &second,
                options(),
                "127.0.0.1:0".parse().unwrap(),
                &token_file
            )
            .await
            .unwrap(),
        address
    );
    assert!(
        browser
            .register_serve(
                &registry,
                &second,
                options(),
                "127.0.0.1:1".parse().unwrap(),
                &token_file
            )
            .await
            .is_err()
    );
    assert_eq!(registry.lock().await.active_count(), 1);
    shutdown.send(true).unwrap();
    serving.await.unwrap().unwrap();
}

#[tokio::test]
async fn invalid_options_fail_before_listener_or_token_and_valid_options_are_retained() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let identity = checkout(&root.join("checkout"));
    let outside = root.join("outside");
    fs::create_dir(&outside).unwrap();
    let runner = outside.join("runner");
    fs::write(&runner, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&runner, fs::Permissions::from_mode(0o700)).unwrap();
    let registry = Arc::new(Mutex::new(CheckoutRegistry::new()));
    let mut browser = BrowserProvisioner::new();
    let token_file = root.join("token");
    let mut variants = Vec::new();
    variants.push(BrowserOptions {
        rust_source_roots: (0..9)
            .map(|i| (format!("root{i}"), outside.clone()))
            .collect(),
        ..BrowserOptions::default()
    });
    for labels in [["bad label", "good"], ["same", "same"]] {
        variants.push(BrowserOptions {
            rust_source_roots: labels
                .into_iter()
                .map(|label| (label.into(), outside.clone()))
                .collect(),
            ..BrowserOptions::default()
        });
    }
    for config in variants {
        assert!(
            browser
                .register_serve(
                    &registry,
                    &identity,
                    CheckoutOptions(serde_json::to_value(config).unwrap()),
                    "127.0.0.1:0".parse().unwrap(),
                    &token_file
                )
                .await
                .is_err()
        );
        assert!(browser.address().is_none());
        assert!(!token_file.exists());
    }
    let config = BrowserOptions {
        trusted_rustc: Some(runner),
        rust_source_roots: vec![("source_1".into(), outside)],
        ..BrowserOptions::default()
    };
    let saved = CheckoutOptions(serde_json::to_value(config).unwrap());
    browser
        .register_serve(
            &registry,
            &identity,
            saved.clone(),
            "127.0.0.1:0".parse().unwrap(),
            &token_file,
        )
        .await
        .unwrap();
    assert_eq!(
        registry.lock().await.registration(&identity.root_key),
        Some(&saved)
    );
}

#[tokio::test]
async fn provisioned_page_serves_all_referenced_assets() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let identity = checkout(&root.join("checkout"));
    let registry = Arc::new(Mutex::new(CheckoutRegistry::new()));
    let mut browser = BrowserProvisioner::new();
    let address = browser
        .register_serve(
            &registry,
            &identity,
            options(),
            "127.0.0.1:0".parse().unwrap(),
            &root.join("token"),
        )
        .await
        .unwrap();
    let html = include_str!("../web/index.html");
    for asset in [
        "style.css",
        "classes.css",
        "navigation.css",
        "sequence.js",
        "shell.js",
        "classes.js",
        "navigation.js",
        "app.js",
    ] {
        assert!(html.contains(&format!("/{asset}")));
        let reply = response(
            browser.router().unwrap(),
            address,
            &format!("/{asset}"),
            None,
        )
        .await;
        assert_eq!(reply.status(), StatusCode::OK, "{asset}");
        assert_eq!(
            reply.headers()["content-type"],
            if asset.ends_with(".css") {
                "text/css; charset=utf-8"
            } else {
                "text/javascript; charset=utf-8"
            }
        );
    }
    for asset in ["sequence-pele.js", "vendor/pele-1.10.0-trellis.2.js"] {
        let reply = response(
            browser.router().unwrap(),
            address,
            &format!("/{asset}"),
            None,
        )
        .await;
        assert_eq!(reply.status(), StatusCode::OK, "{asset}");
        assert_eq!(
            reply.headers()["content-type"],
            "text/javascript; charset=utf-8"
        );
        assert!(
            !reply.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("unsafe-inline")
        );
    }
}

#[tokio::test]
async fn bare_relative_token_file_is_anchored_to_working_directory() {
    let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let identity = checkout(&temp.path().join("checkout"));
    let token = temp.path().join("token");
    let relative = token
        .strip_prefix(std::env::current_dir().unwrap())
        .unwrap();
    let registry = Arc::new(Mutex::new(CheckoutRegistry::new()));
    let mut browser = BrowserProvisioner::new();
    browser
        .register_serve(
            &registry,
            &identity,
            options(),
            "127.0.0.1:0".parse().unwrap(),
            relative,
        )
        .await
        .unwrap();
    assert_eq!(fs::read_to_string(&token).unwrap().len(), 64);
    // A bare filename resolves against the current working directory too.
    let bare = format!("trellis-token-{}", uuid::Uuid::new_v4());
    let mut second = BrowserProvisioner::new();
    second
        .register_serve(
            &registry,
            &identity,
            options(),
            "127.0.0.1:0".parse().unwrap(),
            Path::new(&bare),
        )
        .await
        .unwrap();
    assert_eq!(fs::read_to_string(&bare).unwrap().len(), 64);
    fs::remove_file(bare).unwrap();
}

#[tokio::test]
async fn registry_lock_covers_validation_through_commit() {
    let temp = tempfile::tempdir().unwrap();
    let identity = checkout(&temp.path().join("checkout"));
    let registry = Arc::new(Mutex::new(CheckoutRegistry::with_roots(
        trellis::store::topology::TopologyRoots::isolated_for_tests(
            temp.path().join("cache"),
            temp.path().join("data"),
        ),
    )));
    let mut browser = BrowserProvisioner::new();
    let mut attempted = false;
    let candidate = registry.clone();
    let checkout_root = identity.root.clone();
    let mut attach = None;
    let mut hook = |stage| {
        if stage == "validated" {
            attempted = true;
            assert!(
                candidate.try_lock().is_err(),
                "activation cannot interleave commit"
            );
            let candidate = candidate.clone();
            let checkout_root = checkout_root.clone();
            attach = Some(tokio::spawn(async move {
                let selected = WorkspaceIdentity::discover(Some(&checkout_root), &checkout_root)
                    .unwrap()
                    .attach_existing_marker_readonly()
                    .unwrap();
                let mut guard = candidate.lock().await;
                guard.attach_launch(7, &selected).unwrap();
                // The activation attempt occurs only after registration commits.
                guard.activate(&selected.root_key).unwrap();
            }));
        }
    };
    browser
        .register_serve_with_hook(
            &registry,
            &identity,
            options(),
            "127.0.0.1:0".parse().unwrap(),
            &temp.path().join("token"),
            Some(&mut hook),
        )
        .await
        .unwrap();
    assert!(attempted);
    attach.take().unwrap().await.unwrap();
    assert!(
        registry
            .lock()
            .await
            .registration(&identity.root_key)
            .is_some()
    );
}

#[tokio::test]
async fn replacing_token_after_descriptor_read_rejects_same_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let identity = checkout(&temp.path().join("checkout"));
    let token_file = temp.path().join("token");
    let registry = Arc::new(Mutex::new(CheckoutRegistry::new()));
    let mut browser = BrowserProvisioner::new();
    browser
        .register_serve(
            &registry,
            &identity,
            options(),
            "127.0.0.1:0".parse().unwrap(),
            &token_file,
        )
        .await
        .unwrap();
    let original = fs::read(&token_file).unwrap();
    let mut hook = |stage| {
        if stage == "token_observed" {
            fs::rename(&token_file, temp.path().join("old")).unwrap();
            fs::write(&token_file, &original).unwrap();
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).unwrap();
        }
    };
    let result = browser
        .register_serve_with_hook(
            &registry,
            &identity,
            options(),
            "127.0.0.1:0".parse().unwrap(),
            &token_file,
            Some(&mut hook),
        )
        .await;
    assert!(result.unwrap_err().to_string().contains("identity changed"));
}
