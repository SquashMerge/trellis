#![cfg(feature = "test-support")]
mod common;
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
