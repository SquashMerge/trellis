#![cfg(feature = "test-support")]
mod common;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::Value;
use std::sync::{Arc, atomic::AtomicBool};
use tower::ServiceExt;
use trellis::{
    http,
    indexer::{IndexOptions, index_workspace_bundle},
    store::Store,
};
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
fn setup() -> (
    tempfile::TempDir,
    Store,
    Router,
    Arc<trellis::store::topology::LeaderSession>,
) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let source = temp.path().join("library");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(source.join("std/src")).unwrap();
    std::fs::write(
        source.join("std/src/fs.rs"),
        "// café\npub struct OpenOptions;\nimpl OpenOptions { pub fn open(&self) {} }\n",
    )
    .unwrap();
    std::fs::write(source.join("secret"), "SECRET_MUST_NOT_LEAK").unwrap();
    let store = crate::common::open_store(&temp.path().join("state"), &workspace).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(workspace.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
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
    let state = http::new_with_source_roots(
        store.clone(),
        IndexOptions::new(workspace.clone()),
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
        None,
        None,
        workspace,
        vec![("rust".into(), source)],
    )
    .unwrap();
    state.retain_serving_session(session.clone());
    let app = http::router(state);
    (temp, store, app, session)
}
async fn call(app: &Router, path: &str) -> (u16, Value) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header("host", "127.0.0.1:7331")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.headers()["cache-control"], "no-store");
    let status = res.status().as_u16();
    let bytes = to_bytes(res.into_body(), 8 * 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
#[tokio::test]
async fn candidates_are_separate_and_ranges_match_returned_snapshot() {
    let (temp, store, app, _session) = setup();
    let before = serde_json::to_value(store.status().unwrap()).unwrap();
    let (status, roots) = call(&app, "/api/rust-sources").await;
    assert_eq!(status, 200);
    assert_eq!(roots["roots"][0]["id"], "rust");
    assert!(
        roots["roots"][0]["path"]
            .as_str()
            .unwrap()
            .ends_with("library")
    );
    let (_, tree) = call(
        &app,
        "/api/rust-sources/tree?root=rust&path=std/src&limit=1",
    )
    .await;
    assert_eq!(tree["indexedWorkspace"], "");
    assert_eq!(
        tree["revision"],
        serde_json::json!(store.status().unwrap().revision)
    );
    assert!(tree["items"][0]["indexedPath"].is_null());
    let endpoint = "/api/rust-sources/file?root=rust&path=std/src/fs.rs";
    let (status, snap) = call(&app, endpoint).await;
    assert_eq!(status, 200);
    assert_eq!(snap["rootId"], "rust");
    assert_eq!(snap["hash"], snap["file"]["hash"]);
    let defs = snap["definitions"].as_array().unwrap();
    let open = defs.iter().find(|d| d["name"] == "open").unwrap();
    assert_eq!(open["kind"], "method");
    assert_eq!(open["provenance"]["semantic"], "unavailable");
    let text = snap["file"]["text"].as_str().unwrap();
    assert_eq!(
        &text[open["range"]["startByte"].as_u64().unwrap() as usize
            ..open["range"]["endByte"].as_u64().unwrap() as usize],
        "pub fn open(&self) {}"
    );
    assert_eq!(open["range"]["startLine"], 3);
    assert!(
        snap["warnings"]
            .to_string()
            .contains("not confirmed callees")
    );
    assert_eq!(call(&app, endpoint).await.1["id"], snap["id"]);
    std::fs::write(temp.path().join("library/std/src/copy.rs"), text).unwrap();
    let (_, copy) = call(
        &app,
        "/api/rust-sources/file?root=rust&path=std/src/copy.rs",
    )
    .await;
    assert_eq!(copy["hash"], snap["hash"]);
    assert_ne!(copy["id"], snap["id"]);
    std::fs::write(temp.path().join("library/std/src/fs.rs"), "fn changed() {}").unwrap();
    assert_ne!(call(&app, endpoint).await.1["id"], snap["id"]);
    assert!(text.contains("OpenOptions")); // response remains the original source snapshot
    assert_eq!(
        serde_json::to_value(store.status().unwrap()).unwrap(),
        before
    );
    assert_eq!(call(&app, "/api/source?path=std/src/fs.rs").await.0, 404);
}
#[tokio::test]
async fn invalid_paths_unknown_roots_limits_and_parse_warnings() {
    let (temp, _, app, _session) = setup();
    for path in [
        "..",
        "%2Fetc%2Fpasswd.rs",
        "std/../secret.rs",
        "std//fs.rs",
        "std/./fs.rs",
        "a%5Cb.rs",
        "a%00.rs",
        "C:foo.rs",
        "secret",
        "",
    ] {
        let (status, body) = call(
            &app,
            &format!("/api/rust-sources/file?root=rust&path={path}"),
        )
        .await;
        assert_eq!(status, 400, "{path}: {body}");
        assert!(!body.to_string().contains("SECRET_MUST_NOT_LEAK"));
    }
    for suffix in ["path=..", "limit=201", "limit=0", "offset=10001", "extra=1"] {
        assert_eq!(
            call(&app, &format!("/api/rust-sources/tree?root=rust&{suffix}"))
                .await
                .0,
            400
        );
    }
    assert_eq!(
        call(&app, "/api/rust-sources/file?root=other&path=fs.rs")
            .await
            .0,
        404
    );
    assert_eq!(call(&app, "/api/rust-sources/tree?root=other").await.0, 404);
    assert_eq!(
        call(&app, "/api/rust-sources/file?root=rust&path=missing.rs")
            .await
            .0,
        404
    );
    let root = temp.path().join("library");
    std::fs::write(root.join("large.rs"), vec![b' '; 2 * 1024 * 1024 + 1]).unwrap();
    assert_eq!(
        call(&app, "/api/rust-sources/file?root=rust&path=large.rs")
            .await
            .0,
        413
    );
    std::fs::write(root.join("invalid.rs"), [255]).unwrap();
    assert_eq!(
        call(&app, "/api/rust-sources/file?root=rust&path=invalid.rs")
            .await
            .0,
        400
    );
    std::fs::write(root.join("broken.rs"), "fn broken( {").unwrap();
    let (status, snap) = call(&app, "/api/rust-sources/file?root=rust&path=broken.rs").await;
    assert_eq!(status, 200);
    assert!(snap["warnings"].to_string().contains("invalid Rust"));
    let many = (0..2001)
        .map(|i| format!("fn f{i}() {{}}\n"))
        .collect::<String>();
    std::fs::write(root.join("many.rs"), many).unwrap();
    let (_, snap) = call(&app, "/api/rust-sources/file?root=rust&path=many.rs").await;
    assert_eq!(snap["definitions"].as_array().unwrap().len(), 2000);
    assert!(snap["warnings"].to_string().contains("truncated"));
}
#[cfg(unix)]
#[tokio::test]
async fn symlinks_and_non_regular_files_are_never_read() {
    use std::os::unix::fs::symlink;
    let (temp, _, app, _session) = setup();
    let root = temp.path().join("library");
    std::fs::write(temp.path().join("outside.rs"), "SECRET_MUST_NOT_LEAK").unwrap();
    symlink(temp.path().join("outside.rs"), root.join("escape.rs")).unwrap();
    symlink(temp.path(), root.join("alias")).unwrap();
    symlink(root.join("std/src/fs.rs"), root.join("internal.rs")).unwrap();
    std::fs::create_dir(root.join("directory.rs")).unwrap();
    let fifo = std::ffi::CString::new(root.join("fifo.rs").as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    for path in [
        "escape.rs",
        "internal.rs",
        "alias/outside.rs",
        "directory.rs",
        "fifo.rs",
    ] {
        let (status, body) = call(
            &app,
            &format!("/api/rust-sources/file?root=rust&path={path}"),
        )
        .await;
        assert!([403, 400].contains(&status), "{path}: {body}");
        assert!(!body.to_string().contains("SECRET_MUST_NOT_LEAK"));
    }
    assert!(
        [403, 400].contains(
            &call(&app, "/api/rust-sources/tree?root=rust&path=alias")
                .await
                .0
        )
    );
}
#[tokio::test]
async fn guards_apply_to_every_external_endpoint() {
    let (_, _, app, _session) = setup();
    for path in [
        "/api/rust-sources",
        "/api/rust-sources/tree?root=rust",
        "/api/rust-sources/file?root=rust&path=std/src/fs.rs",
    ] {
        for (host, origin, token, expected) in [
            ("127.0.0.1:7331", None, None, 401),
            ("evil.test", None, Some(TOKEN), 403),
            ("127.0.0.1:7331", Some("http://evil.test"), Some(TOKEN), 403),
        ] {
            let mut req = Request::builder().uri(path).header("host", host);
            if let Some(origin) = origin {
                req = req.header("origin", origin);
            }
            if let Some(token) = token {
                req = req.header("authorization", format!("Bearer {token}"));
            }
            assert_eq!(
                app.clone()
                    .oneshot(req.body(Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status(),
                expected
            );
        }
        let req = Request::builder()
            .uri(path)
            .header("host", "127.0.0.1:7331")
            .header("authorization", format!("Bearer {TOKEN}"))
            .body(Body::from(vec![0; 1024 * 1024 + 1]))
            .unwrap();
        assert_eq!(app.clone().oneshot(req).await.unwrap().status(), 413);
    }
}
#[tokio::test]
async fn old_constructors_have_no_roots_and_configuration_is_validated() {
    let (temp, store, _, session) = setup();
    let workspace = temp.path().join("workspace");
    let opts = IndexOptions::new(workspace.clone());
    let state = http::new(
        store.clone(),
        opts.clone(),
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    state.retain_serving_session(session.clone());
    assert_eq!(
        call(&http::router(state), "/api/rust-sources").await.1,
        serde_json::json!({"roots": []})
    );
    for roots in [
        vec![("../bad".into(), workspace.clone())],
        vec![("a".into(), workspace.clone()); 2],
        vec![("a".into(), workspace.clone()); 9],
        vec![("a".into(), temp.path().join("missing"))],
    ] {
        assert!(
            http::new_with_source_roots(
                store.clone(),
                opts.clone(),
                TOKEN.into(),
                "127.0.0.1:7331".parse().unwrap(),
                None,
                None,
                workspace.clone(),
                roots
            )
            .is_err()
        );
    }
}

#[cfg(unix)]
#[test]
fn descriptor_is_pinned_and_exact_size_bound_is_accepted() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("library");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("ok.rs"), vec![b' '; 2 * 1024 * 1024]).unwrap();
    let directory = trellis::file_tree::SourceDir::open(&root).unwrap();
    let moved = temp.path().join("moved");
    std::fs::rename(&root, &moved).unwrap();
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("ok.rs"), "replacement must not be read").unwrap();
    assert_eq!(directory.read_file("ok.rs").unwrap().len(), 2 * 1024 * 1024);
    std::fs::remove_file(moved.join("ok.rs")).unwrap();
    std::os::unix::fs::symlink(root.join("ok.rs"), moved.join("ok.rs")).unwrap();
    assert!(directory.read_file("ok.rs").is_err());
}

#[tokio::test]
async fn extraction_budget_returns_explicit_partial_candidates() {
    let (temp, _, app, _session) = setup();
    let text = (0..8100)
        .map(|i| format!("fn f{i}() {{}}\n"))
        .collect::<String>();
    std::fs::write(temp.path().join("library/budget.rs"), &text).unwrap();
    let (status, snap) = call(&app, "/api/rust-sources/file?root=rust&path=budget.rs").await;
    assert_eq!(status, 200);
    assert_eq!(snap["file"]["text"], text);
    assert_eq!(snap["definitions"].as_array().unwrap().len(), 2000);
    assert!(
        snap["warnings"]
            .to_string()
            .contains("50000 AST visits or 8000 records")
    );
    assert!(snap["warnings"].to_string().contains("partial candidates"));
}
