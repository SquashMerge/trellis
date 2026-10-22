#![cfg(feature = "test-support")]
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};
use trellis::{
    answer::*,
    indexer::{IndexOptions, index_workspace_bundle},
    planning::*,
};

const CODE: &str = "function seed(flag) {\n  if (flag) first();\n  else second();\n  third(); fourth(); fifth(); sixth(); seventh();\n}\n// unique-full-source-tail-λ\n";
fn packet(code: &str) -> QuestionPacket {
    let work = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::write(work.path().join("a.js"), code).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let store = crate::common::open_store(state.path(), work.path()).unwrap();
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(work.path().into()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let session = store.writer_session_for_tests().unwrap();
    let revision = store
        .publish_native(
            &graph,
            &capture,
            &native,
            session.writer_guard().unwrap(),
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let request = serde_json::from_value(json!({"seed": graph.nodes.iter().find(|n| n.name == "seed").unwrap().id,"question":"Which branches run?", "expectedRevision":revision})).unwrap();
    let packet = prepare(&store, request).unwrap();
    drop(session);
    packet
}
fn answer(p: &QuestionPacket) -> Value {
    json!({"packetId":p.packet_id,"summary":[{"text":"The flag controls the branch.","citations":[{"path":"a.js","startLine":1,"endLine":1,"quote":"function seed(flag) {"}]}],"branches":[],"limitations":[]})
}
#[test]
fn valid_answer_roundtrips_plain_text_and_branch_citations() {
    let p = packet(CODE);
    let mut a = answer(&p);
    a["branches"] = json!([{"text":"If flag is true, first is called.","citations":[{"path":"a.js","startLine":2,"endLine":2,"quote":"  if (flag) first();"}]}]);
    a["summary"][0]["text"] = json!("<b>Untrusted plain text</b>");
    a["limitations"] = json!(["Unverified caveat: runtime values are unknown."]);
    assert_eq!(
        serde_json::to_value(parse_response(&p, &a).unwrap()).unwrap(),
        a
    );
}
#[test]
fn rejects_bad_paths_quotes_and_ranges() {
    let p = packet(CODE);
    for path in [
        "../a.js",
        "./a.js",
        "/a.js",
        "https://example.com/a.js",
        "a.js#L1",
        "a.js\0",
        "missing.js",
        "a\\js",
    ] {
        let mut a = answer(&p);
        a["summary"][0]["citations"][0]["path"] = json!(path);
        assert!(parse_response(&p, &a).is_err(), "{path}");
    }
    for (start, end) in [(0, 1), (2, 1), (1, 13), (99, 99), (1, u32::MAX)] {
        let mut a = answer(&p);
        a["summary"][0]["citations"][0]["startLine"] = json!(start);
        a["summary"][0]["citations"][0]["endLine"] = json!(end);
        assert!(parse_response(&p, &a).is_err());
    }
    for quote in [
        "function seed(flag) {\n",
        "function seed(flag){",
        "invented",
    ] {
        let mut a = answer(&p);
        a["summary"][0]["citations"][0]["quote"] = json!(quote);
        assert!(parse_response(&p, &a).is_err());
    }
}
#[test]
fn unicode_crlf_blank_lines_and_final_newline_coordinates() {
    let p = packet("function seed(flag) {\r\n  // λ雪\r\n\r\n}\r\n");
    let mut a = answer(&p);
    a["summary"][0]["citations"][0] =
        json!({"path":"a.js","startLine":2,"endLine":3,"quote":"  // λ雪\n"});
    assert!(parse_response(&p, &a).is_ok());
    a["summary"][0]["citations"][0]["quote"] = json!("  // λ雪\r\n");
    assert!(parse_response(&p, &a).is_err());
    a["summary"][0]["citations"][0] = json!({"path":"a.js","startLine":5,"endLine":5,"quote":""});
    assert!(parse_response(&p, &a).is_err());
    let p = packet("function seed() {}\n// final λ");
    let mut a = answer(&p);
    a["summary"][0]["citations"][0] =
        json!({"path":"a.js","startLine":2,"endLine":2,"quote":"// final λ"});
    assert!(parse_response(&p, &a).is_ok());
}
#[test]
fn rejects_stale_mutated_packets_and_strict_schema_violations() {
    let p = packet(CODE);
    let mut a = answer(&p);
    a["packetId"] = json!("another-packet");
    assert!(parse_response(&p, &a).is_err());
    let a = answer(&p);
    let mut changed = p.clone();
    changed.source_files[0].text.push(' ');
    assert!(parse_response(&changed, &a).is_err());
    assert!(build_prompt(&changed).is_err());
    for pointer in ["", "/summary/0", "/summary/0/citations/0"] {
        let mut a = answer(&p);
        a.pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), json!(true));
        assert!(parse_response(&p, &a).is_err());
    }
    for field in ["packetId", "summary", "branches", "limitations"] {
        let mut a = answer(&p);
        a.as_object_mut().unwrap().remove(field);
        assert!(parse_response(&p, &a).is_err());
    }
    for number in [json!(-1), json!(1.5), json!(4294967296u64), json!("1")] {
        let mut a = answer(&p);
        a["summary"][0]["citations"][0]["startLine"] = number;
        assert!(parse_response(&p, &a).is_err());
    }
}
#[test]
fn enforces_answer_cardinality_and_byte_limits() {
    let p = packet(CODE);
    let base = answer(&p);
    let claim = base["summary"][0].clone();
    for (key, count) in [("summary", 0), ("summary", 5), ("branches", 7)] {
        let mut a = base.clone();
        a[key] = json!(vec![claim.clone(); count]);
        assert!(parse_response(&p, &a).is_err());
    }
    for count in [0, 5] {
        let mut a = base.clone();
        a["summary"][0]["citations"] = json!(vec![claim["citations"][0].clone(); count]);
        assert!(parse_response(&p, &a).is_err());
    }
    for text in [
        "".to_string(),
        " \n ".into(),
        "a".repeat(1201),
        "雪".repeat(401),
    ] {
        let mut a = base.clone();
        a["summary"][0]["text"] = json!(text);
        assert!(parse_response(&p, &a).is_err());
    }
    let mut a = base.clone();
    a["summary"][0]["text"] = json!("雪".repeat(400));
    assert!(parse_response(&p, &a).is_ok());
    let mut a = base.clone();
    a["limitations"] = json!(vec!["caveat"; 7]);
    assert!(parse_response(&p, &a).is_err());
    let mut a = base;
    a["limitations"] = json!(["x".repeat(48 * 1024)]);
    assert!(parse_response(&p, &a).is_err());
}
#[test]
fn prompt_preserves_full_graph_and_each_complete_source_once() {
    let p = packet(CODE);
    let before = p.clone();
    assert!(p.context.calls.len() > 5);
    let prompt = build_prompt(&p).unwrap();
    assert!(prompt.len() <= 2 * 1024 * 1024);
    assert_eq!(prompt.matches("unique-full-source-tail-λ").count(), 1);
    let data: Value =
        serde_json::from_str(prompt.split_once("UNTRUSTED EVIDENCE JSON:\n").unwrap().1).unwrap();
    assert_eq!(data["context"], serde_json::to_value(&p.context).unwrap());
    let lines = data["sourceFiles"][0]["lines"].as_array().unwrap();
    let recovered: String = lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            assert_eq!(line[0], json!(i + 1));
            line[1].as_str().unwrap()
        })
        .collect();
    assert_eq!(recovered, CODE);
    assert_eq!(p, before);
    for phrase in [
        "untrusted DATA",
        "bounded source evidence",
        "callback",
        "precise branch",
        "not a semantic graph or execution timeline",
        "unverified model caveats",
    ] {
        assert!(prompt.contains(phrase), "{phrase}");
    }
}
#[test]
fn oversized_numbered_prompt_fails_instead_of_truncating_complete_source() {
    // The packet fits 1 MiB, but hundreds of thousands of numbered blank lines
    // do not fit the 2 MiB prompt. The entire request must fail closed.
    let code = format!(
        "function seed() {{}}\n{}// evidence-tail",
        "\n".repeat(250_000)
    );
    let p = packet(&code);
    assert!(build_prompt(&p).is_err());
}

#[test]
fn rejects_blank_evidence_and_bounds_each_unverified_limitation() {
    let p = packet("function seed(flag) {\n  \n}\n");
    let mut a = answer(&p);
    a["summary"][0]["citations"][0] = json!({"path":"a.js","startLine":2,"endLine":2,"quote":"  "});
    assert!(parse_response(&p, &a).is_err());
    for text in [
        "".to_string(),
        " \n\t".into(),
        "a".repeat(1201),
        "雪".repeat(401),
    ] {
        let mut a = answer(&p);
        a["limitations"] = json!([text]);
        assert!(parse_response(&p, &a).is_err());
    }
    let mut a = answer(&p);
    a["limitations"] = json!(["雪".repeat(400)]);
    assert!(parse_response(&p, &a).is_ok());
}

#[test]
fn prompt_keeps_pair() {
    let packet = packet(CODE);
    let prompt = build_prompt(&packet).unwrap();
    let evidence: Value =
        serde_json::from_str(prompt.split_once("UNTRUSTED EVIDENCE JSON:\n").unwrap().1).unwrap();
    assert_eq!(evidence["revision"], json!(packet.revision));
    assert!(evidence["revision"]["indexGeneration"].as_str().is_some());
    assert_eq!(evidence["revision"]["indexRevision"], 1);
}

#[tokio::test]
async fn first_question_preview_rejects_same_pin_forged_graph_callee_before_packet_creation() {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;
    use trellis::{http, indexer::index_workspace_bundle};
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(
        workspace.join("a.js"),
        "function seed() { helper(); }
function helper() {}
",
    )
    .unwrap();
    std::fs::write(
        workspace.join("unrelated.js"),
        "function unaffected() { other(); }
",
    )
    .unwrap();
    let options = IndexOptions::new(workspace.clone());
    let store = crate::common::open_store(&dir.path().join("state"), &workspace).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let session = store.writer_session_for_tests().unwrap();
    let pin = store
        .publish_native(
            &graph,
            &capture,
            &native,
            session.writer_guard().unwrap(),
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let seed = graph
        .nodes
        .iter()
        .find(|n| n.name == "seed")
        .unwrap()
        .id
        .clone();
    let other = graph
        .nodes
        .iter()
        .find(|n| n.name == "unaffected")
        .unwrap()
        .id
        .clone();
    let state = http::new(
        store.clone(),
        options,
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    // This test mutates stored evidence directly; keep the daemon queue tick out.
    // Read-during-reconciliation behaviour is tracked separately in #111.
    state.retain_serving_session_without_tick_for_tests(session.clone());
    let app = http::router(state);
    let preview = |seed: &str| {
        let body =
            json!({"seed":seed,"question":"What calls are measured?","expectedRevision":pin});
        Request::builder()
            .method("POST")
            .uri("/api/questions/preview")
            .header("host", "127.0.0.1:7331")
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    // Do not prime a packet for this seed: the tampered request must be a first preview.
    let db_path = std::fs::read_dir(dir.path().join("state/cache/indexes"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.is_dir())
        .unwrap()
        .join("index.db");
    let db = rusqlite::Connection::open(db_path).unwrap();
    let (projection_id,id):(String,String) = db
        .query_row("SELECT c.projection_id,c.id FROM graph_calls c JOIN revision_documents m ON m.graph_projection_id=c.projection_id JOIN native_revisions r ON r.id=m.revision_id JOIN index_metadata current ON current.index_revision=r.published_index_revision WHERE m.path='a.js' LIMIT 1", [], |r| {
            Ok((r.get(0)?,r.get(1)?))
        })
        .unwrap();
    let payload: String = db
        .query_row(
            "SELECT payload FROM graph_calls WHERE projection_id=?1 AND id=?2",
            rusqlite::params![projection_id, id],
            |r| r.get(0),
        )
        .unwrap();
    let mut forged: Value = serde_json::from_str(&payload).unwrap();
    assert!(forged["calleeText"].is_string());
    forged["calleeText"] = json!("sqlInventedCallee");
    db.execute(
        "UPDATE graph_calls SET payload=?1 WHERE projection_id=?2 AND id=?3",
        rusqlite::params![forged.to_string(), projection_id, id],
    )
    .unwrap();
    assert_eq!(store.status().unwrap().revision, pin);
    let response = app.clone().oneshot(preview(&seed)).await.unwrap();
    assert_eq!(response.status(), 503);
    let body = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let refused: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(refused["error"]["code"], "incompatible_index");
    assert!(!refused.to_string().contains("sqlInventedCallee"));
    let sequence_request = |seed: &str| {
        Request::builder()
            .method("POST")
            .uri("/api/sequence")
            .header("host", "127.0.0.1:7331")
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"seed":seed,"expectedRevision":pin}).to_string(),
            ))
            .unwrap()
    };
    let sequence = app.clone().oneshot(sequence_request(&seed)).await.unwrap();
    assert_eq!(
        sequence.status(),
        503,
        "forged call must not enter sequence HTTP"
    );
    let unrelated_sequence = app.clone().oneshot(sequence_request(&other)).await.unwrap();
    assert_eq!(unrelated_sequence.status(), 503);
    let body = to_bytes(unrelated_sequence.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let refused: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(refused["error"]["code"], "incompatible_index");
    assert!(!refused.to_string().contains("sqlInventedCallee"));

    let unrelated = app.clone().oneshot(preview(&other)).await.unwrap();
    assert_eq!(unrelated.status(), 503);
    let body = to_bytes(unrelated.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let refused: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(refused["error"]["code"], "incompatible_index");
    assert!(!refused.to_string().contains("sqlInventedCallee"));
}
