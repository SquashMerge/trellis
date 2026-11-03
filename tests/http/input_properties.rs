#![cfg(feature = "test-support")]

use axum::{body::Body, http::Request};
use proptest::prelude::*;
use tower::ServiceExt;
use trellis::{http, indexer::IndexOptions};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn app() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let store = crate::common::open_store(&dir.path().join("state"), &root).unwrap();
    let state = http::new(
        store,
        IndexOptions::new(root),
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    let router = http::router(state);
    (dir, router)
}

fn cases() -> u32 {
    std::env::var("TRELLIS_INPUT_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(48)
}

#[test]
fn http_guard_combinations() {
    let (_dir, app) = app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let observed = std::cell::RefCell::new([0usize; 4]);
    let mut runner = proptest::test_runner::TestRunner::new(proptest::test_runner::Config {
        cases: cases(),
        source_file: Some(file!()),
        ..Default::default()
    });
    // Each class has a valid control and near-valid alternatives, including duplicate headers.
    runner
        .run(
            &(0u8..6, 0u8..6, 0u8..6, any::<bool>()),
            |(h, o, a, public)| {
                let hosts: Vec<&str> = match h {
                    0 => vec!["127.0.0.1:7331"],
                    1 => vec!["localhost:7331"],
                    2 => vec![],
                    3 => vec!["localhost:7332"],
                    4 => vec!["localhost:7331", "localhost:7331"],
                    _ => vec!["evil.example:7331"],
                };
                let origins: Vec<&str> = match o {
                    0 => vec![],
                    1 => vec!["http://localhost:7331"],
                    2 => vec!["http://127.0.0.1:7331"],
                    3 => vec!["null"],
                    4 => vec!["http://localhost:7331", "http://localhost:7331"],
                    _ => vec!["https://localhost:7331"],
                };
                let good = format!("Bearer {TOKEN}");
                let wrong = format!("Bearer {}", "0".repeat(64));
                let auth: Vec<&str> = match a {
                    0 => vec![&good],
                    1 => vec![],
                    2 => vec![&wrong],
                    3 => vec![&good, &good],
                    4 => vec![TOKEN],
                    _ => vec!["Bearer"],
                };
                let mut req =
                    Request::builder().uri(if public { "/healthz" } else { "/api/status" });
                for x in hosts {
                    req = req.header("host", x);
                }
                for x in origins {
                    req = req.header("origin", x);
                }
                for x in auth {
                    req = req.header("authorization", x);
                }
                let response = rt
                    .block_on(app.clone().oneshot(req.body(Body::empty()).unwrap()))
                    .unwrap();
                let expected = if h > 1 || o > 2 {
                    403
                } else if !public && a != 0 {
                    401
                } else if public {
                    200
                } else {
                    503
                };
                observed.borrow_mut()[match expected {
                    200 => 0,
                    401 => 1,
                    403 => 2,
                    _ => 3,
                }] += 1;
                prop_assert_eq!(
                    response.status().as_u16(),
                    expected,
                    "h={} o={} a={} public={}",
                    h,
                    o,
                    a,
                    public
                );
                Ok(())
            },
        )
        .unwrap();
    eprintln!(
        "HTTP observed [public, unauthorized, forbidden, authenticated]: {:?}",
        observed.into_inner()
    );
}

#[test]
fn http_body_boundary() {
    let (_dir, app) = app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    for len in [1_048_575, 1_048_576, 1_048_577] {
        let mut body = b"{\"unknown\":1}".to_vec();
        body.resize(len, b' ');
        let req = Request::builder()
            .method("POST")
            .uri("/api/index")
            .header("host", "localhost:7331")
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let status = rt
            .block_on(app.clone().oneshot(req))
            .unwrap()
            .status()
            .as_u16();
        assert_eq!(status, if len > 1_048_576 { 413 } else { 400 }, "{len}");
    }
}
