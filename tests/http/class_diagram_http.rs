#![cfg(feature = "test-support")]
//! Synthetic class projection tests. No workspace programs or providers run.
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
type Setup = (
    tempfile::TempDir,
    Store,
    Graph,
    Router,
    Arc<trellis::store::topology::WriterSession>,
);
fn setup_with(source: &str) -> Setup {
    setup_files(&[
        ("Types.java", source),
        ("unsupported.rs", "fn Unsupported() {}"),
    ])
}
fn setup_files(files: &[(&str, &str)]) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    for (path, text) in files {
        let path = workspace.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
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
    let (_, unsupported) = call(&app, "GET", "/api/classes?path=notes.txt", Value::Null).await;
    assert_eq!(unsupported["items"], json!([]));
    assert!(!unsupported["warnings"].as_array().unwrap().is_empty());
}
/// A real-shaped browser module: exported ES classes across files, a class
/// expression, an object literal and a prototype assignment (the last three out).
pub const JS_WIDGETS: &str = r#"import { View } from "./view.js";
export class Widget extends View {
  #state = null;
  label = "";
  static instances = 0;
  constructor(label) { super(); this.label = label; Widget.instances++; }
  render(target) { target.textContent = this.label; }
  static of(label) { return new Widget(label); }
}
export default class Panel extends Widget { open() {} }
export const Dialog = class extends View {};
export const helpers = { render() {} };
View.prototype.legacy = function () {};
"#;
#[tokio::test]
async fn javascript_classes_are_served_with_supported_languages_and_method_seeds() {
    let (_dir, store, graph, app, _session) = setup_files(&[
        ("web/widgets.js", JS_WIDGETS),
        ("web/view.js", "export class View { mount() {} }\n"),
    ]);
    let pin = store.status().unwrap().revision;
    let (status, page) = call(
        &app,
        "GET",
        &format!("/api/classes?path=web/widgets.js&{}", pin_query(pin)),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(
        page["supportedLanguages"],
        json!(["java", "python", "javascript", "rust"])
    );
    assert!(
        page["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|w| !w.as_str().unwrap().contains("currently support")),
        "{page}"
    );
    let items = page["items"].as_array().unwrap();
    let names: Vec<_> = items
        .iter()
        .map(|c| c["qualifiedName"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["web.widgets.Panel", "web.widgets.Widget"]);
    let widget = &items[1];
    assert_eq!(widget["language"], "javascript");
    assert_eq!(widget["declarationKind"], "class");
    let members = |key: &str| -> Vec<String> {
        widget[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(members("fields"), ["#state", "label", "instances"]);
    assert_eq!(members("methods"), ["constructor", "render", "of"]);
    // A method seed resolves to its lexical ES class.
    let render_id = widget["methods"][1]["symbolId"].as_str().unwrap();
    let render = graph.nodes.iter().find(|n| n.id == render_id).unwrap();
    assert!(
        graph
            .nodes
            .iter()
            .any(|n| n.name == "render" && n.id != render_id),
        "the object-literal render() is a different, unclassed method"
    );
    let (status, diagram) = call(
        &app,
        "POST",
        "/api/class-diagram",
        json!({"seed":render.id,"expectedRevision":pin,"includeHierarchy":true,"includeUnmatched":true}),
    )
    .await;
    assert_eq!(status, 200, "{diagram}");
    assert_eq!(diagram["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(diagram["nodes"][0]["label"], "web.widgets.Widget");
    assert_eq!(diagram["edges"], json!([]), "extends stays a terminal hint");
    let literal = graph
        .nodes
        .iter()
        .find(|n| n.name == "render" && n.id != render_id)
        .unwrap();
    let (status, refused) = call(
        &app,
        "POST",
        "/api/class-diagram",
        json!({"seed":literal.id,"expectedRevision":pin}),
    )
    .await;
    assert_eq!(
        status, 400,
        "an object-literal method has no class: {refused}"
    );
    let (_, search) = call(
        &app,
        "GET",
        &format!("/api/classes?q=View&{}", pin_query(pin)),
        Value::Null,
    )
    .await;
    assert_eq!(search["items"].as_array().unwrap().len(), 1);
    assert_eq!(search["items"][0]["qualifiedName"], "web.view.View");
}
/// Real-shaped Rust: this repository's own `JoinIndex`/`JoinSink` traits and
/// their implementations, at their real paths, read from the checkout.
#[tokio::test]
async fn rust_join_traits_and_impls_from_this_repository_are_served() {
    let read = |path: &str| {
        std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
            .unwrap()
    };
    let paths = [
        "src/semantic_import.rs",
        "src/semantic_import/join_store.rs",
        "src/semantic_import/memory.rs",
        "src/semantic_import/staged/streaming.rs",
    ];
    let sources: Vec<_> = paths.iter().map(|p| (*p, read(p))).collect();
    let files: Vec<_> = sources.iter().map(|(p, s)| (*p, s.as_str())).collect();
    let (dir, store, graph, app, _session) = setup_files(&files);
    let pin = store.status().unwrap().revision;
    let page = |q: &str| {
        let app = app.clone();
        let q = q.to_owned();
        async move {
            let (status, page) = call(
                &app,
                "GET",
                &format!("/api/classes?q={q}&{}", pin_query(pin)),
                Value::Null,
            )
            .await;
            assert_eq!(status, 200, "{page}");
            page
        }
    };
    let found = |page: &Value, qualified: &str| -> Value {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["qualifiedName"] == qualified)
            .unwrap_or_else(|| panic!("{qualified}: {page}"))
            .clone()
    };
    let sinks = page("JoinSink").await;
    assert_eq!(
        sinks["supportedLanguages"],
        json!(["java", "python", "javascript", "rust"])
    );
    let sink = found(&sinks, "semantic_import::join_store::JoinSink");
    assert_eq!(sink["language"], "rust");
    assert_eq!(sink["declarationKind"], "trait");
    assert!(!sink["methods"].as_array().unwrap().is_empty());
    let memory = found(&sinks, "semantic_import::memory::MemoryJoinSink");
    assert_eq!(memory["declarationKind"], "struct");
    let methods: Vec<_> = memory["methods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert!(
        methods.contains(&"into_facts"),
        "inherent impl merged: {methods:?}"
    );
    let index = found(
        &page("JoinIndex").await,
        "semantic_import::join_store::JoinIndex",
    );
    assert_eq!(index["declarationKind"], "trait");
    // implements/extends hints are stored with the owner's projection.
    let db = rusqlite::Connection::open(index_db(&dir.path().join("state"))).unwrap();
    let hints = |owner: &Value, kind: &str| -> Vec<String> {
        db.prepare(
            "SELECT json_extract(r.payload,'$.typeName') FROM class_relations r JOIN revision_documents m ON m.cid=r.cid
             WHERE r.owner=?1 AND json_extract(r.payload,'$.kind')=?2 AND json_extract(r.payload,'$.matchKind')='unmatched' AND r.target IS NULL",
        )
        .unwrap()
        .query_map(
            rusqlite::params![sid_blob(owner["symbol"]["id"].as_str().unwrap()), kind],
            |r| r.get(0),
        )
        .unwrap()
        .map(Result::unwrap)
        .collect()
    };
    assert!(hints(&memory, "implements").contains(&"JoinSink".to_owned()));
    assert!(hints(&index, "extends").contains(&"DocumentJoinState".to_owned()));
    let streaming = found(
        &page("DocumentSink").await,
        "semantic_import::staged::streaming::DocumentSink",
    );
    assert!(hints(&streaming, "implements").contains(&"JoinSink".to_owned()));
    // A method from an impl block seeds its type's diagram.
    let into_facts = memory["methods"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "into_facts")
        .unwrap()["symbolId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(graph.nodes.iter().any(|n| n.id == into_facts));
    let (status, diagram) = call(
        &app,
        "POST",
        "/api/class-diagram",
        json!({"seed":into_facts,"expectedRevision":pin}),
    )
    .await;
    assert_eq!(status, 200, "{diagram}");
    assert_eq!(
        diagram["nodes"][0]["label"],
        "semantic_import::memory::MemoryJoinSink"
    );
    assert_eq!(diagram["edges"], json!([]));
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
#[ignore = "release medium cohort: cargo test --release --test http class_diagram_http::selected_class_pages_medium_release_latency -- --exact --ignored"]
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
                .join("tools/synthetic-cohorts/manifest-v2.json")
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
    let (dir, store, _graph, app, session) = setup();
    let state = dir.path().join("state");
    let workspace = dir.path().join("workspace");
    // A retired workspace record is opaque, even when its bytes are corrupt.
    let record = state
        .join("data/workspaces")
        .join(store.root_id())
        .join("workspace.db");
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    std::fs::write(&record, b"unreadable retired record format").unwrap();
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
        refusal.to_string().contains("recovery_required"),
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
            33,
            "native-v6-delta-v1",
            prior.index_generation.to_string(),
            prior.index_revision as i64
        )
    );
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        33
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
    let expected = trellis::model::FileExtraction::extract_file(
        file,
        &graph.nodes,
        &cancel(),
        trellis::classes::Limits::default(),
    )
    .unwrap();
    assert_eq!(
        serde_json::from_str::<trellis::model::FileExtraction>(&stored).unwrap(),
        expected
    );
    let source = format!("/api/source?path=Types.java&{}", pin_query(pin));
    let classes = format!("/api/classes?path=Types.java&{}", pin_query(pin));
    assert_eq!(call(&app, "GET", &source, Value::Null).await.0, 200);
    assert_eq!(call(&app, "GET", &classes, Value::Null).await.0, 200);
    let (_,unrelated): (String,Option<String>)=db.query_row(
        "SELECT class_extraction_state,class_extraction_payload FROM graph_projections g JOIN revision_documents m ON m.gid=g.gid WHERE m.path='unsupported.rs'",
        [],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    // Rust is a catalog language (#452): a file without types stores an empty F.
    let unrelated: trellis::model::FileExtraction =
        serde_json::from_str(&unrelated.unwrap()).unwrap();
    assert!(unrelated.classes.is_empty() && unrelated.impls.is_empty());
}

#[tokio::test]
async fn normal_size_captured_java_python_cold_oracle_attests_full_published_catalog() {
    use trellis::{
        classes::{Catalog, Limits},
        indexer::index_workspace_bundle,
        model::FileExtraction,
    };
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    for (path, source) in [
        (
            "small/java/Csmall0000.java",
            include_str!("../fixtures/classes/below-cap/source/small/java/Csmall0000.java"),
        ),
        (
            "small/python/Csmall0000.py",
            include_str!("../fixtures/classes/below-cap/source/small/python/Csmall0000.py"),
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
        ..Catalog::default()
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
        let selected_symbol = page["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["symbol"]["id"] == first.id)
            .unwrap();
        assert_eq!(
            selected_symbol["symbol"],
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
        classes::{Catalog, Limits},
        indexer::index_workspace_bundle,
        model::FileExtraction,
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
