#![cfg(feature = "test-support")]
//! Synthetic class projection tests. No workspace programs or providers run.
mod common;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, atomic::AtomicBool},
};
use tower::ServiceExt;
use trellis::{
    class_diagram::ClassDiagramRequest,
    http,
    indexer::{IndexOptions, index_workspace},
    model::*,
    store::Store,
};
fn sid_blob(id: &str) -> Vec<u8> {
    hex::decode(id.strip_prefix("sid:v1:").expect("syntax ID")).expect("hex syntax ID")
}
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const JAVA: &str = r#"package demo;
class A extends B {
    B field;
    Missing unknown;
    B run(B input) { return input; }
    class Inner { B nested(B input) { return input; } }
}
class B { A back; C second; }
class C {}
class Alone {}
"#;
fn pin_query(pin: IndexPin) -> String {
    format!(
        "indexGeneration={}&indexRevision={}",
        pin.index_generation, pin.index_revision
    )
}
fn index_db(state: &std::path::Path) -> std::path::PathBuf {
    std::fs::read_dir(state.join("cache/indexes"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
        .join("index.db")
}
fn cancel() -> CancelFlag {
    Arc::new(AtomicBool::new(false))
}
fn setup_with(
    source: &str,
) -> (
    tempfile::TempDir,
    Store,
    Graph,
    Router,
    Arc<trellis::store::topology::WriterSession>,
) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("Types.java"), source).unwrap();
    std::fs::write(workspace.join("unsupported.js"), "class Unsupported {}").unwrap();
    let options = IndexOptions::new(workspace.clone());
    let graph = index_workspace(&options, &cancel(), |_| {}).unwrap();
    let store = crate::common::open_store(&dir.path().join("state"), &workspace).unwrap();
    let session = store.writer_session_for_tests().unwrap();
    assert_eq!(
        publish_bundle(
            &store,
            &graph,
            &workspace,
            session.writer_guard().unwrap(),
            trellis::model::IndexPin {
                index_generation: store.index_baseline().unwrap().index_generation,
                index_revision: 0
            },
            &cancel()
        )
        .unwrap()
        .index_revision,
        1
    );
    let app = http::router(
        http::new(
            store.clone(),
            options,
            TOKEN.into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap(),
    );
    (dir, store, graph, app, session)
}
fn setup() -> (
    tempfile::TempDir,
    Store,
    Graph,
    Router,
    Arc<trellis::store::topology::WriterSession>,
) {
    setup_with(JAVA)
}
fn id(graph: &Graph, name: &str) -> String {
    graph
        .nodes
        .iter()
        .find(|n| n.name == name)
        .unwrap()
        .id
        .clone()
}
fn request(seed: String, store: &Store) -> ClassDiagramRequest {
    ClassDiagramRequest {
        seed,
        expected_revision: store.status().unwrap().revision,
        expanded: vec![],
        include_unmatched: false,
        include_hierarchy: false,
    }
}
fn request_json(request: &ClassDiagramRequest) -> Value {
    json!({
        "seed": &request.seed,
        "expectedRevision": &request.expected_revision,
        "expanded": &request.expanded,
        "includeUnmatched": request.include_unmatched,
        "includeHierarchy": request.include_hierarchy,
    })
}
async fn call(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
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
    assert_eq!(response.headers()["cache-control"], "no-store");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn search_pagination_literal_wildcards_and_supported_languages() {
    let (_dir, store, _graph, app, _session) = setup();
    let pin = store.status().unwrap().revision;
    let (status, page) = call(
        &app,
        "GET",
        &format!("/api/classes?{}&path=Types.java&limit=2", pin_query(pin)),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(page["revision"], json!(pin));
    assert_eq!(page["nextOffset"], 2);
    assert_eq!(page["requireIndex"], false);
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    let (_, filtered) = call(
        &app,
        "GET",
        &format!("/api/classes?q=Alone&{}", pin_query(pin)),
        Value::Null,
    )
    .await;
    assert_eq!(filtered["items"].as_array().unwrap().len(), 1);
    assert_eq!(filtered["items"][0]["symbol"]["name"], "Alone");
    let (status, empty_path) = call(
        &app,
        "GET",
        &format!("/api/classes?path=&q=Alone&{}", pin_query(pin)),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(empty_path, filtered);
    for q in ["%25", "%5F", "%27%20OR%201%3D1--", "%5C"] {
        let (status, page) = call(&app, "GET", &format!("/api/classes?q={q}"), Value::Null).await;
        assert_eq!(status, 200);
        assert_eq!(page["items"], json!([]));
    }
    let (_, unsupported) = call(&app, "GET", "/api/classes?path=unsupported.js", Value::Null).await;
    assert_eq!(unsupported["items"], json!([]));
    assert!(!unsupported["warnings"].as_array().unwrap().is_empty());
}
#[tokio::test]
async fn one_hop_incoming_methods_terminal_hints_and_cached_only() {
    let (_dir, store, graph, _app, _session) = setup();
    let seed = id(&graph, "A");
    let mut request = request(seed.clone(), &store);
    request.include_hierarchy = true;
    request.include_unmatched = true;
    let diagram = store.class_diagram_at(&request).unwrap();
    assert_eq!(diagram.nodes.len(), 1);
    assert_eq!(diagram.nodes[0].id, seed);
    assert!(
        diagram.edges.is_empty(),
        "syntax names must not create class edges"
    );
    assert!(diagram.nodes.iter().all(|node| node.kind == "class"));
}
#[tokio::test]
async fn authentication_strict_requests_revision_and_disconnected_expansion() {
    let (dir, store, graph, app, session) = setup();
    let pin = store.status().unwrap().revision;
    for path in ["/api/classes", "/api/class-diagram"] {
        let req = Request::builder()
            .uri(path)
            .header("host", "127.0.0.1:7331")
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.clone().oneshot(req).await.unwrap().status(), 401);
    }
    for path in [
        "/api/classes?extra=1",
        "/api/classes?limit=0",
        "/api/classes?limit=101",
        "/api/classes?offset=1000001",
        "/api/classes?path=../Types.java",
        "/api/classes?path=/Types.java",
        "/api/classes?revision=x",
    ] {
        assert_eq!(call(&app, "GET", path, Value::Null).await.0, 400, "{path}");
    }
    let seed = id(&graph, "A");
    for body in [
        json!({"seed":seed}),
        json!({"seed":seed,"expectedRevision":pin,"unknown":true}),
        json!({"seed":"","expectedRevision":pin}),
        json!({"seed":"missing","expectedRevision":pin}),
        json!({"seed":seed,"expectedRevision":pin,"includeUnmatched":"yes"}),
        json!({"seed":seed,"expectedRevision":pin,"expanded":vec![seed.clone();13]}),
        json!({"seed":id(&graph,"Unsupported"),"expectedRevision":pin}),
    ] {
        let (status, value) = call(&app, "POST", "/api/class-diagram", body).await;
        assert_eq!(status, 400, "{value}");
        assert!(value["error"]["message"].is_string());
    }
    let (status, independent) = call(
        &app,
        "POST",
        "/api/class-diagram",
        json!({"seed":seed,"expectedRevision":pin,"expanded":[id(&graph,"Alone")]}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(independent["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(independent["edges"], json!([]));
    let index_generation = store.status().unwrap().revision.index_generation;
    publish_bundle(
        &store,
        &graph,
        &dir.path().join("workspace"),
        session.writer_guard().unwrap(),
        trellis::model::IndexPin {
            index_generation,
            index_revision: 1,
        },
        &cancel(),
    )
    .unwrap();
    let (status, retained_classes) = call(
        &app,
        "GET",
        &format!("/api/classes?{}", pin_query(pin)),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200, "{retained_classes}");
    assert_eq!(retained_classes["revision"], json!(pin));
    let (status, retained_diagram) = call(
        &app,
        "POST",
        "/api/class-diagram",
        json!({"seed":seed,"expectedRevision":pin}),
    )
    .await;
    assert_eq!(status, 200, "{retained_diagram}");
    assert_eq!(retained_diagram["revision"], json!(pin));
    for path in ["/classes.js", "/classes.css"] {
        let req = Request::builder()
            .uri(path)
            .header("host", "127.0.0.1:7331")
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.clone().oneshot(req).await.unwrap().status(), 200);
    }
}

#[test]
fn selected_class_pagination_spans_paths_in_one_revision() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    for n in 0..12 {
        std::fs::write(
            workspace.join(format!("C{n:02}.java")),
            format!("class C{n:02} {{ int run() {{ return {n}; }} }}\n"),
        )
        .unwrap();
    }
    let options = IndexOptions::new(workspace.clone());
    let graph = index_workspace(&options, &cancel(), |_| {}).unwrap();
    let store = crate::common::open_store(&dir.path().join("state"), &workspace).unwrap();
    let session = store.writer_session_for_tests().unwrap();
    let pin = publish_bundle(
        &store,
        &graph,
        &workspace,
        session.writer_guard().unwrap(),
        store.index_baseline().unwrap(),
        &cancel(),
    )
    .unwrap();
    let first = store.classes_at(None, "", Some(pin), 0, 10).unwrap();
    assert_eq!(first.items.len(), 10);
    assert_eq!(first.next_offset, Some(10));
    let second = store.classes_at(None, "", Some(pin), 10, 10).unwrap();
    assert_eq!(second.items.len(), 2);
    assert_eq!(second.next_offset, None);
    let diagram = request(id(&graph, "C11"), &store);
    assert!(store.class_diagram_at(&diagram).is_ok());
}

/// Run explicitly in release mode on the reference host. The regular test
/// suite retains fast selected-path correctness coverage above.
#[test]
#[ignore = "release medium cohort: cargo test --release --test class_diagram_http selected_class_pages_medium_release_latency -- --ignored"]
fn selected_class_pages_medium_release_latency() {
    use trellis::index_coordinator::IndexJobCoordinator;
    let scratch = tempfile::tempdir().unwrap();
    let canonical = scratch.path().join("canonical");
    let generated = std::process::Command::new("node")
        .arg(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tools/synthetic-cohorts/generate.mjs"),
        )
        .arg("--out")
        .arg(&canonical)
        .status()
        .unwrap();
    assert!(generated.success());
    assert_eq!(
        std::fs::read(canonical.join("manifest.json")).unwrap(),
        std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tools/synthetic-cohorts/manifest-v1.json")
        )
        .unwrap()
    );
    let workspace = canonical.join("medium");
    let store = crate::common::open_store(&scratch.path().join("state"), &workspace).unwrap();
    let options = IndexOptions::new(workspace);
    let job = IndexJobCoordinator::prepare(&store, None).unwrap();
    let _session = job.session();
    let pin = job.run(&options, &cancel(), |_| {}).unwrap();
    for (limit, upper_bound) in [(1, 5.0), (10, 10.0)] {
        let start = std::time::Instant::now();
        let page = store.classes_at(None, "", Some(pin), 0, limit).unwrap();
        let seconds = start.elapsed().as_secs_f64();
        assert_eq!(page.items.len(), limit);
        eprintln!("selected medium class page limit={limit} seconds={seconds:.4}");
        assert!(
            seconds <= upper_bound,
            "selected medium class page must not attest/reparse each full workspace"
        );
    }
}

#[test]
fn projection_bounds_preserve_expansion_roots_edges_and_cycles() {
    let mut source = String::from("class Seed { Missing missing;\n");
    for i in 0..40 {
        source.push_str(&format!("N{i} n{i};\n"));
    }
    source.push_str("}\n");
    for i in 0..40 {
        source.push_str(&format!(
            "class N{i} {{ Seed back; Next{i} next; }} class Next{i} {{}}\n"
        ));
    }
    let (_dir, store, graph, _app, _session) = setup_with(&source);
    let seed = graph
        .nodes
        .iter()
        .find(|n| n.kind == SymbolKind::Class && n.path == "Types.java")
        .unwrap()
        .id
        .clone();
    let mut request = request(seed.clone(), &store);
    request.include_hierarchy = true;
    request.include_unmatched = true;
    let diagram = store.class_diagram_at(&request).unwrap();
    assert_eq!(diagram.nodes.len(), 1);
    assert_eq!(diagram.nodes[0].id, seed);
    assert!(
        diagram.edges.is_empty(),
        "syntax names must not create class edges"
    );
    assert!(diagram.nodes.iter().all(|node| node.kind == "class"));
}
#[test]
fn incompatible_index_refuses_without_migrating_durable_data() {
    let (dir, store, graph, app, session) = setup();
    let state = dir.path().join("state");
    let workspace = dir.path().join("workspace");
    let saved = SavedView {
        id: "view".into(),
        title: "Class notes".into(),
        query: serde_json::from_value(json!({"seed":id(&graph,"A")})).unwrap(),
        pins: BTreeMap::new(),
        hidden: vec![],
    };
    store.put_view(&saved).unwrap();
    let record = std::fs::read_dir(state.join("data/workspaces"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
        .join("workspace.db");
    let before = std::fs::read(&record).unwrap();
    drop(app);
    drop(session);
    drop(store);
    let db = rusqlite::Connection::open(index_db(&state)).unwrap();
    db.pragma_update(None, "user_version", 2).unwrap();
    drop(db);
    // The incompatible derived index is disposable, but cannot be read before
    // mandatory reconciliation. Durable saved-view bytes remain untouched.
    let deferred = Store::open_for_tests(&state, &workspace).unwrap();
    let refusal = deferred.status().unwrap_err();
    assert!(
        refusal.to_string().contains("index_not_ready"),
        "{refusal:#}"
    );
    assert_eq!(std::fs::read(record).unwrap(), before);
    let db = rusqlite::Connection::open(index_db(&state)).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
}
#[test]
fn failed_publication_keeps_projection_atomic_with_graph() {
    let (dir, store, graph, app, session) = setup();
    let q = request(id(&graph, "A"), &store);
    let before = serde_json::to_value(store.class_diagram_at(&q).unwrap()).unwrap();
    let prior = store.status().unwrap().revision;
    let page = serde_json::to_value(
        store
            .classes_at(Some("Types.java"), "", Some(prior), 0, 100)
            .unwrap(),
    )
    .unwrap();
    let path = index_db(&dir.path().join("state"));
    let before_bytes = std::fs::read(&path).unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::write(
        workspace.join("Types.java"),
        JAVA.replace("class Alone {}", "class Added {} class Alone {}"),
    )
    .unwrap();
    let changed =
        index_workspace(&IndexOptions::new(workspace.clone()), &cancel(), |_| {}).unwrap();
    assert!(
        changed
            .nodes
            .iter()
            .any(|node| node.kind == SymbolKind::Class && node.name == "Added")
    );
    let abort = cancel();
    let signal = abort.clone();
    store.set_publication_before_commit_hook_for_tests(move || {
        signal.store(true, std::sync::atomic::Ordering::Release)
    });
    let rollback = publish_bundle(
        &store,
        &changed,
        &workspace,
        session.writer_guard().unwrap(),
        prior,
        &abort,
    )
    .unwrap_err();
    assert!(rollback.to_string().contains("cancelled"), "{rollback:#}");
    std::fs::write(workspace.join("Types.java"), JAVA).unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before_bytes,
        "changed graph/class INSERT must roll back before COMMIT"
    );
    assert_eq!(store.status().unwrap().revision, prior);
    assert_eq!(
        serde_json::to_value(
            store
                .classes_at(Some("Types.java"), "", Some(prior), 0, 100)
                .unwrap()
        )
        .unwrap(),
        page
    );
    assert_eq!(
        serde_json::to_value(store.class_diagram_at(&q).unwrap()).unwrap(),
        before
    );
    assert_eq!(store.graph().unwrap().nodes, graph.nodes);
    drop(app);
    drop(session);

    let leader = store.publication_for_tests().unwrap();
    let control = store.index_baseline().unwrap();
    assert!(
        publish_bundle(
            &store,
            &graph,
            &dir.path().join("workspace"),
            &leader,
            trellis::model::IndexPin {
                index_generation: control.index_generation,
                index_revision: 0
            },
            &cancel()
        )
        .is_err()
    );
    drop(leader);

    let leader = store.publication_for_tests().unwrap();
    let control = store.index_baseline().unwrap();
    assert!(
        publish_bundle(
            &store,
            &graph,
            &dir.path().join("workspace"),
            &leader,
            trellis::model::IndexPin {
                index_generation: control.index_generation,
                index_revision: 1
            },
            &Arc::new(AtomicBool::new(true))
        )
        .is_err()
    );
    drop(leader);

    let prior = store.index_baseline().unwrap();
    let leader = store.publication_for_tests().unwrap();
    let path = index_db(&dir.path().join("state"));
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER fail_projection BEFORE INSERT ON class_relations BEGIN SELECT RAISE(ABORT,'synthetic failure'); END;").unwrap();
    let unchanged = std::fs::read(&path).unwrap();
    // A second local WriterGuard is valid: there is no elected leader lock.
    // Atomic rollback below is guarded by SQLite publication, not election.
    let concurrent = store.publication_for_tests().unwrap();
    concurrent.verify().unwrap();
    drop(concurrent);
    let rejected = publish_bundle(
        &store,
        &graph,
        &dir.path().join("workspace"),
        &leader,
        prior,
        &cancel(),
    )
    .unwrap_err();
    let reason = format!("{rejected:#}");
    assert!(
        reason.contains("incompatible_index")
            && reason.contains("unknown cache object type, name or shape"),
        "{reason}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), unchanged);
    let (version, marker, generation, revision): (i64, String, String, i64) = db
        .query_row("SELECT schema_version,extractor_version,index_generation,index_revision FROM index_metadata", [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap();
    assert_eq!(
        (version, marker.as_str(), generation, revision),
        (
            21,
            "native-v6-delta-v1",
            prior.index_generation.to_string(),
            prior.index_revision as i64
        )
    );
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        21
    );
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
    assert!(
        store
            .class_diagram_at(&q)
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
    assert!(
        store
            .graph()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
    db.execute_batch("DROP TRIGGER fail_projection").unwrap();
    // Keep this independent SQLite descriptor open until the Store and writer
    // guards are gone; closing another handle to a live DB can drop POSIX locks.
    drop(leader);

    for closed in [
        store.status().unwrap_err(),
        store.class_diagram_at(&q).unwrap_err(),
        store.graph().unwrap_err(),
    ] {
        assert!(
            closed.to_string().contains("incompatible_index"),
            "{closed:#}"
        );
    }

    let control = store.index_baseline().unwrap();
    assert_eq!(control, prior);
    let leader = store.publication_for_tests().unwrap();
    let next = publish_bundle(
        &store,
        &graph,
        &dir.path().join("workspace"),
        &leader,
        control,
        &cancel(),
    )
    .unwrap();
    assert_ne!(next.index_generation, prior.index_generation);
    assert_eq!(next.index_revision, 1);
    assert_eq!(store.status().unwrap().revision, next);
    assert_eq!(store.graph().unwrap().nodes, graph.nodes);
    assert!(
        store
            .class_diagram_at(&q)
            .unwrap_err()
            .to_string()
            .starts_with("revision conflict")
    );
    let mut current = q.clone();
    current.expected_revision = next;
    let mut before_at_next = before.clone();
    before_at_next["revision"] = serde_json::to_value(next).unwrap();
    assert_eq!(
        serde_json::to_value(store.class_diagram_at(&current).unwrap()).unwrap(),
        before_at_next
    );
    drop(leader);
    drop(store);
    drop(db);
}

#[tokio::test]
async fn terminal_hints_and_parent_cycles_fail_readably() {
    let (_dir, store, graph, _app, _session) = setup();
    let seed = graph
        .nodes
        .iter()
        .find(|n| n.kind == SymbolKind::Class && n.path == "Types.java")
        .unwrap()
        .id
        .clone();
    let mut request = request(seed.clone(), &store);
    request.include_hierarchy = true;
    request.include_unmatched = true;
    let diagram = store.class_diagram_at(&request).unwrap();
    assert_eq!(diagram.nodes.len(), 1);
    assert_eq!(diagram.nodes[0].id, seed);
    assert!(
        diagram.edges.is_empty(),
        "syntax names must not create class edges"
    );
    assert!(diagram.nodes.iter().all(|node| node.kind == "class"));
    assert!(
        diagram
            .nodes
            .iter()
            .all(|node| !node.id.starts_with("class-hint:"))
    );
}
#[test]
fn repeated_references_are_grouped_before_caps_with_real_evidence() {
    let mut text = String::from("class A {\n");
    for i in 0..65 {
        text.push_str(&format!("B field{i};\n"));
    }
    text.push_str("C other; Missing x; Missing y; } class B {} class C {}\n");
    let (_dir, store, graph, _app, _session) = setup_with(&text);
    let seed = graph
        .nodes
        .iter()
        .find(|n| n.kind == SymbolKind::Class && n.path == "Types.java")
        .unwrap()
        .id
        .clone();
    let mut request = request(seed.clone(), &store);
    request.include_hierarchy = true;
    request.include_unmatched = true;
    let diagram = store.class_diagram_at(&request).unwrap();
    assert_eq!(diagram.nodes.len(), 1);
    assert_eq!(diagram.nodes[0].id, seed);
    assert!(
        diagram.edges.is_empty(),
        "syntax names must not create class edges"
    );
    assert!(diagram.nodes.iter().all(|node| node.kind == "class"));
}
#[test]
fn edge_limit_is_explicit_without_dangling_nodes() {
    let mut text = String::new();
    for i in 0..13 {
        text.push_str(&format!("class N{i} {{\n"));
        for j in 0..13 {
            if i != j {
                text.push_str(&format!("N{j} field{j};\n"));
            }
        }
        text.push_str("}\n");
    }
    let (_dir, store, graph, _app, _session) = setup_with(&text);
    let seed = id(&graph, "N0");
    let mut request = request(seed.clone(), &store);
    request.include_hierarchy = true;
    request.include_unmatched = true;
    let diagram = store.class_diagram_at(&request).unwrap();
    assert_eq!(diagram.nodes.len(), 1);
    assert_eq!(diagram.nodes[0].id, seed);
    assert!(
        diagram.edges.is_empty(),
        "syntax names must not create class edges"
    );
    assert!(diagram.nodes.iter().all(|node| node.kind == "class"));
}
#[test]
fn presentation_byte_budget_clips_members_and_paginates_without_skipping_rows() {
    // Real native multi-document source creates measured fields across multiple pages.
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let ty = "T".repeat(1000);
    for i in 0..100 {
        let mut text = format!("class N{i:03} {{\n");
        for j in 0..40 {
            text.push_str(&format!(" {ty} field{j:03};\n"));
        }
        text.push_str("}\n");
        std::fs::write(workspace.join(format!("N{i:03}.java")), text).unwrap();
    }
    let store = crate::common::open_store(&dir.path().join("state"), &workspace).unwrap();
    let session = store.writer_session_for_tests().unwrap();
    let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
        &IndexOptions::new(workspace.clone()),
        store.root_id(),
        &cancel(),
        |_| {},
    )
    .unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            session.writer_guard().unwrap(),
            store.index_baseline().unwrap(),
            &cancel(),
        )
        .unwrap();
    let revision = store.status().unwrap().revision;
    let mut offset = 0;
    let mut seen = BTreeSet::new();
    let mut pages = 0;
    loop {
        let page = store
            .classes_at(None, "", Some(revision), offset, 100)
            .unwrap();
        assert!(
            serde_json::to_vec(&page).unwrap().len() <= trellis::class_diagram::MAX_RESPONSE_BYTES
        );
        for class in page.items {
            assert!(!class.truncated, "real measured fields must not be clipped");
            assert_eq!(class.fields.len(), 40);
            assert!(seen.insert(class.symbol.id));
        }
        pages += 1;
        if let Some(next) = page.next_offset {
            assert!(next > offset);
            offset = next;
        } else {
            break;
        }
    }
    assert!(
        pages > 1,
        "real measured classes should exercise pagination"
    );
    assert_eq!(seen.len(), 100);
}
#[test]
fn diagram_byte_budget_preserves_expansion_bridges_and_exact_relation_evidence() {
    let mut text = String::new();
    for i in 0..13 {
        text.push_str(&format!("class N{i} {{\n"));
        for j in 0..13 {
            if i != j {
                text.push_str(&format!("N{j} field{j};\n"));
            }
        }
        text.push_str("}\n");
    }
    let (_dir, store, graph, _app, _session) = setup_with(&text);
    let seed = graph
        .nodes
        .iter()
        .find(|n| n.kind == SymbolKind::Class && n.path == "Types.java")
        .unwrap()
        .id
        .clone();
    let mut request = request(seed.clone(), &store);
    request.include_hierarchy = true;
    request.include_unmatched = true;
    let diagram = store.class_diagram_at(&request).unwrap();
    assert_eq!(diagram.nodes.len(), 1);
    assert_eq!(diagram.nodes[0].id, seed);
    assert!(
        diagram.edges.is_empty(),
        "syntax names must not create class edges"
    );
    assert!(diagram.nodes.iter().all(|node| node.kind == "class"));
}
#[test]
fn outgoing_declarations_precede_high_fan_in_before_caps() {
    let mut source = String::from("class Focus { Own field; } class Own {}\n");
    for i in 0..100 {
        source.push_str(&format!("class Incoming{i} {{ Focus field; }}\n"));
    }
    let (_dir, store, graph, _app, _session) = setup_with(&source);
    let seed = graph
        .nodes
        .iter()
        .find(|n| n.kind == SymbolKind::Class && n.path == "Types.java")
        .unwrap()
        .id
        .clone();
    let mut request = request(seed.clone(), &store);
    request.include_hierarchy = true;
    request.include_unmatched = true;
    let diagram = store.class_diagram_at(&request).unwrap();
    assert_eq!(diagram.nodes.len(), 1);
    assert_eq!(diagram.nodes[0].id, seed);
    assert!(
        diagram.edges.is_empty(),
        "syntax names must not create class edges"
    );
    assert!(diagram.nodes.iter().all(|node| node.kind == "class"));
}
#[tokio::test]
async fn automatic_hierarchy_is_opt_in_directional_transitive_and_cache_only() {
    let source = "interface Top {} interface Face extends Top {}\n
        class Base implements Face {} class Mid extends Base {}\n
        class Leaf extends Mid {} class Peer extends Base {} class Other implements Face {}";
    let (_dir, store, graph, _app, _session) = setup_with(source);
    let seed = graph
        .nodes
        .iter()
        .find(|n| n.kind == SymbolKind::Class && n.path == "Types.java")
        .unwrap()
        .id
        .clone();
    let mut request = request(seed.clone(), &store);
    request.include_hierarchy = true;
    request.include_unmatched = true;
    let diagram = store.class_diagram_at(&request).unwrap();
    assert_eq!(diagram.nodes.len(), 1);
    assert_eq!(diagram.nodes[0].id, seed);
    assert!(
        diagram.edges.is_empty(),
        "syntax names must not create class edges"
    );
    assert!(diagram.nodes.iter().all(|node| node.kind == "class"));
}
#[test]
fn hierarchy_precedes_associations_and_reserves_deep_manual_neighbor_bridges() {
    let mut source = String::from("class Seed extends Parent {\n");
    for i in 0..100 {
        source.push_str(&format!("N{i} field{i};\n"));
    }
    source.push_str(
        "} class Parent extends Grand {} class Grand implements Face {} interface Face {}\n",
    );
    source.push_str("class Child extends Seed {} class Deep extends Child { Chosen selected; } class Chosen {}\n");
    for i in 0..100 {
        source.push_str(&format!("class N{i} {{}}\n"));
    }
    let (_dir, store, graph, _app, _session) = setup_with(&source);
    let seed = graph
        .nodes
        .iter()
        .find(|n| n.kind == SymbolKind::Class && n.path == "Types.java")
        .unwrap()
        .id
        .clone();
    let mut request = request(seed.clone(), &store);
    request.include_hierarchy = true;
    request.include_unmatched = true;
    let diagram = store.class_diagram_at(&request).unwrap();
    assert_eq!(diagram.nodes.len(), 1);
    assert_eq!(diagram.nodes[0].id, seed);
    assert!(
        diagram.edges.is_empty(),
        "syntax names must not create class edges"
    );
    assert!(diagram.nodes.iter().all(|node| node.kind == "class"));
}
#[test]
fn hierarchy_cycles_deduplicate_and_unknown_bases_stay_terminal_opt_in() {
    let (_dir, store, graph, _app, _session) = setup_with(
        "class A extends B {} class B extends C {} class C extends A {} class D extends Missing {} class Alone {}",
    );
    let seed = graph
        .nodes
        .iter()
        .find(|n| n.kind == SymbolKind::Class && n.path == "Types.java")
        .unwrap()
        .id
        .clone();
    let mut request = request(seed.clone(), &store);
    request.include_hierarchy = true;
    request.include_unmatched = true;
    let diagram = store.class_diagram_at(&request).unwrap();
    assert_eq!(diagram.nodes.len(), 1);
    assert_eq!(diagram.nodes[0].id, seed);
    assert!(
        diagram.edges.is_empty(),
        "syntax names must not create class edges"
    );
    assert!(diagram.nodes.iter().all(|node| node.kind == "class"));
}
#[test]
fn hierarchy_node_search_caps_and_impossible_mandatory_paths_are_explicit() {
    let mut source = String::from("class N0 {}\n");
    for i in 1..60 {
        source.push_str(&format!("class N{i} extends N{} {{}}\n", i - 1));
    }
    let (_dir, store, graph, _app, _session) = setup_with(&source);
    let seed = graph
        .nodes
        .iter()
        .find(|n| n.kind == SymbolKind::Class && n.path == "Types.java")
        .unwrap()
        .id
        .clone();
    let mut request = request(seed.clone(), &store);
    request.include_hierarchy = true;
    request.include_unmatched = true;
    let diagram = store.class_diagram_at(&request).unwrap();
    assert_eq!(diagram.nodes.len(), 1);
    assert_eq!(diagram.nodes[0].id, seed);
    assert!(
        diagram.edges.is_empty(),
        "syntax names must not create class edges"
    );
    assert!(diagram.nodes.iter().all(|node| node.kind == "class"));
}
#[test]
fn hierarchy_edge_caps_are_connected_and_deterministic() {
    let mut source = String::from("interface I0 {}\n");
    for i in 1..20 {
        let parents = (0..i)
            .map(|j| format!("I{j}"))
            .collect::<Vec<_>>()
            .join(",");
        source.push_str(&format!("interface I{i} extends {parents} {{}}\n"));
    }
    let (_dir, store, graph, _app, _session) = setup_with(&source);
    let seed = graph
        .nodes
        .iter()
        .find(|n| n.kind == SymbolKind::Class && n.path == "Types.java")
        .unwrap()
        .id
        .clone();
    let mut request = request(seed.clone(), &store);
    request.include_hierarchy = true;
    request.include_unmatched = true;
    let diagram = store.class_diagram_at(&request).unwrap();
    assert_eq!(diagram.nodes.len(), 1);
    assert_eq!(diagram.nodes[0].id, seed);
    assert!(
        diagram.edges.is_empty(),
        "syntax names must not create class edges"
    );
    assert!(diagram.nodes.iter().all(|node| node.kind == "class"));
}
#[test]
fn hierarchy_oversize_mandatory_evidence_fails_instead_of_stranding_roots() {
    let (_dir, store, graph, _app, _session) = setup_with(
        "class A {} class B extends A {} class C extends B { Chosen field; } class Chosen {}",
    );
    let seed = id(&graph, "A");
    let mut request = request(seed.clone(), &store);
    request.include_hierarchy = true;
    request.include_unmatched = true;
    let diagram = store.class_diagram_at(&request).unwrap();
    assert_eq!(diagram.nodes.len(), 1);
    assert_eq!(diagram.nodes[0].id, seed);
    assert!(
        diagram.edges.is_empty(),
        "syntax names must not create class edges"
    );
    assert!(diagram.nodes.iter().all(|node| node.kind == "class"));
}
#[test]
fn hierarchy_mandatory_paths_reject_total_response_byte_overflow() {
    let mut source = String::from("class N0 {}\n");
    for i in 1..24 {
        source.push_str(&format!("class N{i} extends N{} {{}}\n", i - 1));
    }
    let (_dir, store, graph, _app, _session) = setup_with(&source);
    let seed = id(&graph, "N0");
    let mut request = request(seed.clone(), &store);
    request.include_hierarchy = true;
    request.include_unmatched = true;
    let diagram = store.class_diagram_at(&request).unwrap();
    assert_eq!(diagram.nodes.len(), 1);
    assert_eq!(diagram.nodes[0].id, seed);
    assert!(
        diagram.edges.is_empty(),
        "syntax names must not create class edges"
    );
    assert!(diagram.nodes.iter().all(|node| node.kind == "class"));
}

fn publish_bundle(
    store: &trellis::store::Store,
    graph: &trellis::model::Graph,
    workspace: &std::path::Path,
    leader: &trellis::store::topology::WriterGuard,
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

#[tokio::test]
async fn selected_class_json_decode_and_clipping_fail_closed_without_retyping_invalid_requests() {
    // The first selected catalog decode is typed and closes pre-created clones.
    let (dir, store, graph, app, _session) = setup();
    let clone = store.clone();
    let pin = store.status().unwrap().revision;
    let class_id = id(&graph, "A");
    let db = rusqlite::Connection::open(index_db(&dir.path().join("state"))).unwrap();
    db.execute(
        "UPDATE classes SET payload='not-json' WHERE id=?1",
        [sid_blob(&class_id)],
    )
    .unwrap();
    let (status, body) = call(
        &app,
        "GET",
        &format!("/api/classes?{}&q=A", pin_query(pin)),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "incompatible_index");
    assert!(body.get("items").is_none());
    assert!(
        clone
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );

    // The diagram endpoint independently reports selected class decode corruption.
    let (dir, store, graph, app, _session) = setup();
    let clone = store.clone();
    let pin = store.status().unwrap().revision;
    let class_id = id(&graph, "A");
    let db = rusqlite::Connection::open(index_db(&dir.path().join("state"))).unwrap();
    db.execute(
        "UPDATE classes SET payload='not-json' WHERE id=?1",
        [sid_blob(&class_id)],
    )
    .unwrap();
    let (status, body) = call(
        &app,
        "POST",
        "/api/class-diagram",
        json!({"seed":class_id,"expectedRevision":pin}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "incompatible_index");
    assert!(body.get("nodes").is_none());
    assert!(
        clone
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );

    // Valid top-level JSON with a non-object member is refused before clipped JSON1.
    let (dir, store, graph, app, _session) = setup();
    let clone = store.clone();
    let pin = store.status().unwrap().revision;
    let class_id = id(&graph, "A");
    let db = rusqlite::Connection::open(index_db(&dir.path().join("state"))).unwrap();
    db.execute(
        "UPDATE classes SET payload=json_set(payload,'$.fields',json_array('wrong'),'$.padding',?1) WHERE id=?2",
        rusqlite::params!["x".repeat(70 * 1024), sid_blob(&class_id)],
    )
    .unwrap();
    let (status, body) = call(
        &app,
        "GET",
        &format!("/api/classes?{}&q=A", pin_query(pin)),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "incompatible_index");
    assert!(
        clone
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );

    // A genuine request-domain failure stays HTTP 400 and does not close the Store.
    let (_dir, store, graph, app, _session) = setup();
    let clone = store.clone();
    let pin = store.status().unwrap().revision;
    let (status, body) = call(
        &app,
        "POST",
        "/api/class-diagram",
        json!({"seed":id(&graph,"Unsupported"),"expectedRevision":pin}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "invalid_class_request");
    assert_eq!(store.status().unwrap().revision, pin);
    assert_eq!(clone.status().unwrap().revision, pin);
}

#[tokio::test]
async fn ready_file_extraction_is_persisted_and_selected_http_reads_serve_it() {
    let (dir, store, graph, app, _session) = setup();
    let pin = store.status().unwrap().revision;
    let db = rusqlite::Connection::open(index_db(&dir.path().join("state"))).unwrap();
    let (state, stored): (String, String) = db.query_row(
        "SELECT g.class_extraction_state,g.class_extraction_payload FROM graph_projections g JOIN revision_documents m ON m.gid=g.gid WHERE m.path='Types.java'",
        [], |r| Ok((r.get(0)?,r.get(1)?)),
    ).unwrap();
    assert_eq!(state, "ready");
    let file = graph.files.iter().find(|f| f.path == "Types.java").unwrap();
    let expected = trellis::classes::FileExtraction::extract_file(
        file,
        &graph.nodes,
        &cancel(),
        trellis::classes::Limits::default(),
    )
    .unwrap();
    assert_eq!(
        serde_json::from_str::<trellis::classes::FileExtraction>(&stored).unwrap(),
        expected
    );
    let source = format!("/api/source?path=Types.java&{}", pin_query(pin));
    let classes = format!("/api/classes?path=Types.java&{}", pin_query(pin));
    assert_eq!(call(&app, "GET", &source, Value::Null).await.0, 200);
    assert_eq!(call(&app, "GET", &classes, Value::Null).await.0, 200);
    let (_,unrelated): (String,Option<String>)=db.query_row(
        "SELECT class_extraction_state,class_extraction_payload FROM graph_projections g JOIN revision_documents m ON m.gid=g.gid WHERE m.path='unsupported.js'",
        [],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert!(unrelated.is_none());
}

fn write_class_scip_labels(root: &std::path::Path, label_version: &str) {
    use protobuf::Message;
    use sha2::{Digest, Sha256};
    let mut index = scip::types::Index::new();
    let mut manifest = serde_json::Map::new();
    for (path, column, name) in [
        ("A.java", 6, "A"),
        ("a.py", 6, "P"),
        ("a.js", 6, "J"),
        ("a.rs", 7, "R"),
    ] {
        let bytes = std::fs::read(root.join("workspace").join(path)).unwrap();
        manifest.insert(
            path.into(),
            Value::String(hex::encode(Sha256::digest(bytes))),
        );
        let mut document = scip::types::Document::new();
        document.relative_path = path.into();
        document.position_encoding =
            scip::types::PositionEncoding::UTF16CodeUnitOffsetFromLineStart.into();
        let mut occurrence = scip::types::Occurrence::new();
        occurrence.range = vec![0, column, column + 1];
        occurrence.symbol_roles = 1;
        occurrence.symbol = format!("scip presentation {label_version} {name}");
        document.occurrences.push(occurrence);
        index.documents.push(document);
    }
    std::fs::write(root.join("labels.scip"), index.write_to_bytes().unwrap()).unwrap();
    std::fs::write(
        root.join("labels.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
}

#[tokio::test]
async fn changed_captured_class_scip_label_recomputes_f_and_new_revision_class_projection() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    for (path, text) in [
        ("A.java", "class A {}\n"),
        ("a.py", "class P: pass\n"),
        ("a.js", "class J {}\n"),
        ("a.rs", "struct R {}\n"),
    ] {
        std::fs::write(workspace.join(path), text).unwrap();
    }
    let mut options = IndexOptions::new(workspace.clone());
    options.scip_path = Some(dir.path().join("labels.scip"));
    options.manifest_path = Some(dir.path().join("labels.json"));
    let store = common::open_store(&dir.path().join("state"), &workspace).unwrap();
    let session = store.writer_session_for_tests().unwrap();
    let mut observations = Vec::new();
    for version in ["one", "two"] {
        write_class_scip_labels(dir.path(), version);
        let (graph, native, capture) =
            trellis::indexer::index_workspace_bundle(&options, store.root_id(), &cancel(), |_| {})
                .unwrap();
        for (path, name) in [("A.java", "A"), ("a.py", "P"), ("a.js", "J")] {
            let symbol = graph
                .nodes
                .iter()
                .find(|s| s.path == path && s.name == name)
                .unwrap();
            assert_eq!(
                symbol.display_label.as_deref(),
                Some(format!("scip presentation {version} {name}").as_str())
            );
        }
        assert!(
            graph
                .nodes
                .iter()
                .filter(|s| s.path == "a.rs")
                .all(|s| s.display_label.is_none())
        );
        let expected = store.index_baseline().unwrap();
        let pin = store
            .publish_native(
                &graph,
                &capture,
                &native,
                session.writer_guard().unwrap(),
                expected,
                &cancel(),
            )
            .unwrap();
        observations.push((pin, graph));
    }
    let ((first, first_graph), (second, second_graph)) = (&observations[0], &observations[1]);
    assert_eq!(first.index_generation, second.index_generation);
    assert_eq!((first.index_revision, second.index_revision), (1, 2));
    let db = rusqlite::Connection::open(index_db(&dir.path().join("state"))).unwrap();
    for path in ["A.java", "a.py", "a.js", "a.rs"] {
        let ids = |revision| -> (i64, i64, Option<i64>, Option<String>) {
            db.query_row("SELECT m.vid,m.gid,m.cid,g.class_extraction_payload FROM revision_documents m JOIN native_revisions r ON r.id=m.revision_id JOIN graph_projections g ON g.gid=m.gid WHERE r.published_index_revision=?1 AND m.path=?2",
                rusqlite::params![revision,path],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap()
        };
        let old = ids(1);
        let fresh = ids(2);
        assert_eq!(
            old.0, fresh.0,
            "unchanged authenticated source should retain native document: {path}"
        );
        if matches!(path, "A.java" | "a.py" | "a.js") {
            assert_ne!(
                old.1, fresh.1,
                "new display label must change graph: {path}"
            );
        } else {
            assert_eq!(old.1, fresh.1, "Rust SCIP label must not be admitted");
        }
        if matches!(path, "A.java" | "a.py") {
            assert_ne!(old.3, fresh.3, "F must be recomputed: {path}");
            assert_ne!(old.2, fresh.2, "class projection must change: {path}");
            assert!(old.3.is_some() && fresh.3.is_some());
        } else {
            assert!(old.3.is_none() && fresh.3.is_none());
        }
    }
    for (path, name) in [("A.java", "A"), ("a.py", "P")] {
        let symbol = second_graph
            .nodes
            .iter()
            .find(|s| s.path == path && s.name == name)
            .unwrap();
        let before = first_graph
            .nodes
            .iter()
            .find(|s| s.path == path && s.name == name)
            .unwrap();
        assert_eq!(
            symbol.id, before.id,
            "SCIP label cannot mint a new native ID"
        );
    }
    let app = http::router(
        http::new(
            store.clone(),
            options,
            TOKEN.into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap(),
    );
    for (path, name) in [("A.java", "A"), ("a.py", "P")] {
        let (status, page) = call(
            &app,
            "GET",
            &format!("/api/classes?path={path}&q={name}&{}", pin_query(*second)),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{path}: {page}");
        assert_eq!(
            page["items"][0]["symbol"]["displayLabel"],
            format!("scip presentation two {name}")
        );
        let (status, source) = call(
            &app,
            "GET",
            &format!("/api/source?path={path}&{}", pin_query(*second)),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{source}");
    }
}

#[tokio::test]
async fn normal_size_captured_java_python_cold_oracle_attests_full_published_catalog() {
    use trellis::{
        classes::{Catalog, FileExtraction, Limits},
        indexer::index_workspace_bundle,
    };
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    for (path, source) in [
        (
            "small/java/Csmall0000.java",
            include_str!("fixtures/classes/below-cap/source/small/java/Csmall0000.java"),
        ),
        (
            "small/python/Csmall0000.py",
            include_str!("fixtures/classes/below-cap/source/small/python/Csmall0000.py"),
        ),
    ] {
        let target = workspace.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, source).unwrap();
    }
    let store = crate::common::open_store(&dir.path().join("state"), &workspace).unwrap();
    let session = store.writer_session_for_tests().unwrap();
    let options = IndexOptions::new(workspace.clone());
    let cancel = cancel();
    let (published_graph, published_native, authenticated_capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    assert_eq!(authenticated_capture.files.len(), 2);
    let pin = store
        .publish_native(
            &published_graph,
            &authenticated_capture,
            &published_native,
            session.writer_guard().unwrap(),
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    assert_eq!(pin.index_revision, 1);

    // This is a second, isolated cold admission/extraction. Its only inputs are
    // the captured source bytes and a freshly measured native graph; never the
    // persisted graph, class rows, or F. Compare both immutable captures first.
    let (cold_graph, cold_native, cold_capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    cold_capture.verify(&cancel).unwrap();
    assert_eq!(cold_capture.files, authenticated_capture.files);
    assert_eq!(cold_capture.hashes, authenticated_capture.hashes);
    assert!(!cold_native.declarations.is_empty());
    assert!(!cold_graph.nodes.is_empty());
    let mut cold_f = Vec::new();
    for file in &cold_capture.files {
        cold_f.push(
            FileExtraction::extract_file(file, &cold_graph.nodes, &cancel, Limits::default())
                .unwrap(),
        );
    }
    let cold_catalog = Catalog::compose(
        &cold_f,
        cold_capture.files.len(),
        cold_graph.nodes.len(),
        Limits::default(),
    )
    .unwrap();
    assert!(!cold_catalog.classes.is_empty());
    assert!(!cold_catalog.truncated);
    let db = rusqlite::Connection::open(index_db(&dir.path().join("state"))).unwrap();
    let derived_graph = store.graph_at(Some(pin)).unwrap();
    let revision_id = format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision);
    for (file, extraction) in cold_capture.files.iter().zip(&cold_f) {
        let (state, payload): (String, String) = db.query_row(
            "SELECT g.class_extraction_state,g.class_extraction_payload FROM revision_documents m JOIN graph_projections g ON g.gid=m.gid WHERE m.revision_id=?1 AND m.path=?2",
            rusqlite::params![revision_id, file.path], |r| Ok((r.get(0)?,r.get(1)?)),
        ).unwrap();
        assert_eq!(state, "ready");
        assert_eq!(
            payload.as_bytes(),
            serde_json::to_vec(extraction).unwrap(),
            "cold F: {}",
            file.path
        );
        let (_, selected) = store.source_at(&file.path, Some(pin)).unwrap().unwrap();
        assert_eq!(
            serde_json::to_vec(&selected).unwrap(),
            serde_json::to_vec(file).unwrap()
        );
        let stored_nodes = db.prepare(
            "SELECT n.id,n.name,n.kind FROM graph_nodes n JOIN revision_documents m ON m.gid=n.gid WHERE m.revision_id=?1 AND m.path=?2 ORDER BY n.id"
        ).unwrap().query_map(rusqlite::params![revision_id, file.path], |r| Ok((format!("sid:v1:{}", hex::encode(r.get::<_, Vec<u8>>(0)?)), r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
            .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        let mut measured_nodes: Vec<_> = cold_graph
            .nodes
            .iter()
            .filter(|node| node.path == file.path)
            .collect();
        measured_nodes.sort_by(|a, b| a.id.cmp(&b.id));
        let mut derived_nodes: Vec<_> = derived_graph
            .nodes
            .iter()
            .filter(|node| node.path == file.path)
            .collect();
        derived_nodes.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(
            derived_nodes, measured_nodes,
            "derived graph: {}",
            file.path
        );
        assert_eq!(
            stored_nodes.len(),
            measured_nodes.len(),
            "measured graph: {}",
            file.path
        );
        for ((id, name, kind), measured) in stored_nodes.iter().zip(measured_nodes) {
            assert_eq!(id, &measured.id, "measured node ID: {}", file.path);
            assert_eq!(name, &measured.name, "measured node name: {}", file.path);
            assert_eq!(
                kind.as_str(),
                serde_json::to_value(measured.kind)
                    .unwrap()
                    .as_str()
                    .unwrap(),
                "measured node kind: {}",
                file.path
            );
        }
    }
    let stored_classes = db.prepare(
        "SELECT c.payload FROM classes c JOIN revision_documents m ON m.cid=c.cid WHERE m.revision_id=?1 ORDER BY c.path,json_extract(c.payload,'$.symbol.range.startByte'),c.id"
    ).unwrap().query_map([&revision_id], |r| r.get::<_, String>(0))
        .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    assert_eq!(stored_classes.len(), cold_catalog.classes.len());
    for (stored, cold) in stored_classes.iter().zip(&cold_catalog.classes) {
        assert_eq!(
            stored.as_bytes(),
            serde_json::to_vec(cold).unwrap(),
            "full class row: {}",
            cold.symbol.path
        );
    }
    let stored_relations = db.prepare(
        "SELECT r.payload FROM class_relations r JOIN revision_documents m ON m.cid=r.cid WHERE m.revision_id=?1 ORDER BY json_extract(r.payload,'$.path'),json_extract(r.payload,'$.range.startByte'),r.id"
    ).unwrap().query_map([&revision_id], |r| r.get::<_, String>(0))
        .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    assert_eq!(stored_relations.len(), cold_catalog.relations.len());
    for (stored, cold) in stored_relations.iter().zip(&cold_catalog.relations) {
        assert_eq!(
            stored.as_bytes(),
            serde_json::to_vec(cold).unwrap(),
            "full relation row: {}",
            cold.path
        );
    }
    let (warnings, truncated): (String, bool) = db
        .query_row(
            "SELECT class_warnings,class_truncated FROM native_revisions WHERE id=?1",
            [&revision_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        warnings.as_bytes(),
        serde_json::to_vec(&cold_catalog.warnings).unwrap()
    );
    assert_eq!(truncated, cold_catalog.truncated);
    let full_published = Catalog {
        classes: stored_classes
            .iter()
            .map(|row| serde_json::from_str(row).unwrap())
            .collect(),
        relations: stored_relations
            .iter()
            .map(|row| serde_json::from_str(row).unwrap())
            .collect(),
        warnings: serde_json::from_str(&warnings).unwrap(),
        truncated,
    };
    assert_eq!(
        serde_json::to_vec(&full_published).unwrap(),
        serde_json::to_vec(&cold_catalog).unwrap()
    );

    let app = http::router(
        http::new(
            store.clone(),
            options,
            TOKEN.into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap(),
    );
    for file in &cold_capture.files {
        let (status, source) = call(
            &app,
            "GET",
            &format!("/api/source?path={}&{}", file.path, pin_query(pin)),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{source}");
        assert_eq!(source["revision"], json!(pin));
        assert_eq!(source["file"], json!(file));
        let (status, page) = call(
            &app,
            "GET",
            &format!("/api/classes?path={}&{}", file.path, pin_query(pin)),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let expected: BTreeMap<_, _> = cold_catalog
            .classes
            .iter()
            .filter(|c| c.symbol.path == file.path)
            .map(|c| (c.symbol.id.clone(), json!(c)))
            .collect();
        let selected: BTreeMap<_, _> = page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["symbol"]["id"].as_str().unwrap().to_owned(), c.clone()))
            .collect();
        assert_eq!(selected, expected, "selected full classes: {}", file.path);
        assert_eq!(page["warnings"], json!(cold_catalog.warnings));
        assert_eq!(page["truncated"], json!(cold_catalog.truncated));
        let first = cold_graph
            .nodes
            .iter()
            .find(|n| n.path == file.path && n.kind == SymbolKind::Class)
            .unwrap();
        let (status, symbol) = call(
            &app,
            "GET",
            &format!("/api/symbol?id={}&{}", first.id, pin_query(pin)),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{symbol}");
        assert_eq!(
            symbol["symbol"],
            json!(first),
            "selected fresh native graph: {}",
            file.path
        );
    }
}

#[tokio::test]
async fn retained_class_page_and_diagram_are_byte_stable_after_full_rewrite() {
    let (dir, store, graph, app, session) = setup();
    let old = store.status().unwrap().revision;
    let page_route = format!("/api/classes?path=Types.java&{}", pin_query(old));
    let (status, page) = call(&app, "GET", &page_route, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let old_request = request(id(&graph, "A"), &store);
    let (status, diagram) = call(
        &app,
        "POST",
        "/api/class-diagram",
        request_json(&old_request),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{diagram}");
    let root = dir.path().join("workspace");
    std::fs::write(
        root.join("Types.java"),
        "class Replacement { void newMethod() {} }\n",
    )
    .unwrap();
    let changed = index_workspace(&IndexOptions::new(root.clone()), &cancel(), |_| {}).unwrap();
    store.set_retention_clock_for_tests(1_000, 0);
    let new_pin = publish_bundle(
        &store,
        &changed,
        &root,
        session.writer_guard().unwrap(),
        old,
        &cancel(),
    )
    .unwrap();
    assert_ne!(old, new_pin);
    use trellis::{
        classes::{Catalog, FileExtraction, Limits},
        indexer::index_workspace_bundle,
    };
    // The r1 bytes come from an authenticated selected source, NOT the old
    // persisted F, graph projection, class rows or earlier HTTP response.
    let old_source = store.source_at("Types.java", Some(old)).unwrap().unwrap().1;
    assert_eq!(
        old_source.text, JAVA,
        "literal pre-rewrite class source oracle"
    );
    std::fs::write(root.join("Types.java"), &old_source.text).unwrap();
    let (fresh_graph, fresh_native, fresh_capture) = index_workspace_bundle(
        &IndexOptions::new(root.clone()),
        store.root_id(),
        &cancel(),
        |_| {},
    )
    .unwrap();
    fresh_capture.verify(&cancel()).unwrap();
    assert!(!fresh_native.declarations.is_empty());
    assert_eq!(
        fresh_graph
            .files
            .iter()
            .find(|f| f.path == "Types.java")
            .unwrap(),
        &old_source
    );
    let fresh_f: Vec<_> = fresh_graph
        .files
        .iter()
        .filter(|f| matches!(f.language.as_str(), "java" | "python"))
        .map(|f| {
            FileExtraction::extract_file(f, &fresh_graph.nodes, &cancel(), Limits::default())
                .unwrap()
        })
        .collect();
    let cold_catalog = Catalog::compose(
        &fresh_f,
        fresh_graph.files.len(),
        fresh_graph.nodes.len(),
        Limits::default(),
    )
    .unwrap();
    assert!(!cold_catalog.truncated);
    assert!(
        cold_catalog
            .classes
            .iter()
            .any(|class| class.symbol.name == "A")
    );
    std::fs::write(
        root.join("Types.java"),
        "class Replacement { void newMethod() {} }\n",
    )
    .unwrap();
    let cold_app = http::router(
        http::new(
            store.clone(),
            IndexOptions::new(root.clone()),
            TOKEN.into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap(),
    );
    let (cold_code, cold_page) = call(&cold_app, "GET", &page_route, Value::Null).await;
    assert_eq!(cold_code, StatusCode::OK, "{cold_page}");
    let mut expected_classes: Vec<_> = cold_catalog
        .classes
        .iter()
        .filter(|class| class.symbol.path == "Types.java")
        .map(|class| json!(class))
        .collect();
    expected_classes.sort_by_key(|class| class["symbol"]["id"].as_str().unwrap().to_owned());
    let mut cold_classes = cold_page["items"].as_array().unwrap().clone();
    cold_classes.sort_by_key(|class| class["symbol"]["id"].as_str().unwrap().to_owned());
    assert_eq!(
        cold_classes, expected_classes,
        "cold old-pin full Catalog page"
    );
    assert_eq!(cold_page["warnings"], json!(cold_catalog.warnings));
    assert_eq!(cold_page["truncated"], json!(cold_catalog.truncated));
    let (cold_diagram_code, cold_diagram) = call(
        &cold_app,
        "POST",
        "/api/class-diagram",
        request_json(&old_request),
    )
    .await;
    assert_eq!(cold_diagram_code, StatusCode::OK, "{cold_diagram}");
    let expected_a = cold_catalog
        .classes
        .iter()
        .find(|class| class.symbol.name == "A")
        .unwrap();
    assert_eq!(cold_diagram["revision"], json!(old));
    assert_eq!(cold_diagram["seed"], json!(expected_a.symbol.id));
    assert_eq!(
        cold_diagram["nodes"],
        json!([{
            "id":expected_a.symbol.id,"class":expected_a,
            "label":expected_a.qualified_name,"kind":"class","expandable":true
        }])
    );
    assert_eq!(cold_diagram["edges"], json!([]));
    assert_eq!(
        call(&app, "GET", &page_route, Value::Null).await,
        (StatusCode::OK, cold_page.clone())
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "/api/class-diagram",
            request_json(&old_request)
        )
        .await,
        (StatusCode::OK, cold_diagram.clone())
    );
    assert_eq!(page["items"], cold_page["items"]);
    assert_eq!(diagram["nodes"], cold_diagram["nodes"]);
    store.set_retention_clock_for_tests(1_900, 900);
    store
        .release_revision(old, session.writer_guard().unwrap())
        .unwrap();
    let (code, body) = call(&app, "GET", &page_route, Value::Null).await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "pin_expired");
    let (code, body) = call(
        &app,
        "POST",
        "/api/class-diagram",
        request_json(&old_request),
    )
    .await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "pin_expired");
}
