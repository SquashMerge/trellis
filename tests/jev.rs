//! Synthetic protocol fixtures only: not recorded model runs or quality evidence.
mod common;
use serde_json::{Value, json};
use trellis::{
    indexer::{IndexOptions, index_workspace_bundle},
    jev::{parse_response, request_for, response_warnings},
    planning::{QuestionPacket, QuestionRequest, prepare},
};
#[path = "common/jev_wire.rs"]
mod jev_wire;
use jev_wire::decode_packet;
use std::sync::{Arc, atomic::AtomicBool};
const CODE: &str =
    "// UNIQUE_COMPLETE_SOURCE\nfunction seed(flag) { if (flag) check(); run(callback); }\n";

fn packet_with(code: &str, question: &str) -> QuestionPacket {
    packet_with_links(code, question, false)
}
fn packet_with_links(code: &str, question: &str, synthetic_links: bool) -> QuestionPacket {
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
    let (graph, _, _) = index_workspace_bundle(
        &IndexOptions::new(work.path().into()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let _ = synthetic_links; // Saved legacy option cannot add semantic links.
    let (revision, session) = trellis::index_coordinator::reconcile_workspace(
        &store,
        &IndexOptions::new(work.path().to_owned()),
        &cancel,
        |_| {},
    )
    .unwrap();
    assert_eq!(store.graph().unwrap(), graph);
    let request: QuestionRequest = serde_json::from_value(json!({
        "seed":graph.nodes.iter().find(|n| n.name == "seed").unwrap().id,
        "question":question,"expectedRevision":revision
    }))
    .unwrap();
    let packet = prepare(&store, request).unwrap();
    drop(session);
    packet
}
fn packet() -> QuestionPacket {
    packet_with(CODE, "How is the request checked?")
}
fn alias(packet: &QuestionPacket, index: usize) -> String {
    format!("c{index}_{}", packet.packet_id)
}
fn synthetic_response(packet: &QuestionPacket) -> Value {
    let answers: serde_json::Map<String, Value> = packet
        .context
        .calls
        .iter()
        .enumerate()
        .map(|(i, _)| {
            (
                alias(packet, i),
                json!({"type":"choice","choice":"essential","confidence":0.6,
        "probabilities":{"essential":0.7,"supporting":0.2,"incidental":0.1,"uncertain":0.0}}),
            )
        })
        .collect();
    json!({"model":"jev-1.13.0","answers":answers,"usage":{"input_tokens":1,"output_tokens":1}})
}

#[test]
fn request_contains_complete_evidence_once_and_bound_choice_per_call() {
    let packet = packet();
    let body = request_for(&packet).unwrap();
    assert_eq!(body["model"], "jev-1.13.0");
    assert_eq!(decode_packet(&body), serde_json::to_value(&packet).unwrap());
    assert_eq!(
        body["state"]["packet"]["sourceFiles"],
        serde_json::to_value(&packet.source_files).unwrap()
    );
    let text = serde_json::to_string(&body).unwrap();
    assert_eq!(text.matches("UNIQUE_COMPLETE_SOURCE").count(), 1);
    let questions = body["questions"].as_object().unwrap();
    assert_eq!(questions.len(), packet.context.calls.len());
    for (i, call) in packet.context.calls.iter().enumerate() {
        let question = &questions[&alias(&packet, i)];
        assert!(!questions.contains_key(&call.id));
        assert_eq!(question["type"], "choice");
        assert!(
            question["instructions"]
                .as_str()
                .unwrap()
                .contains(&format!("calls.rows[{i}]"))
        );
        let instruction = question["instructions"].as_str().unwrap();
        assert!(instruction.contains(&serde_json::to_string(&packet.request.question).unwrap()));
        assert!(instruction.contains(&serde_json::to_string(&call.callee_text).unwrap()));
        assert!(instruction.contains(&format!(
            "{}:{}",
            serde_json::to_string(&call.path).unwrap(),
            call.range.start_line
        )));
        let caller = packet
            .context
            .nodes
            .iter()
            .find(|node| node.id == call.caller)
            .unwrap();
        assert!(instruction.contains(&serde_json::to_string(&caller.name).unwrap()));
        assert!(instruction.contains("Direct seed call; display eligible"));
        assert_eq!(
            question["criteria"]["essential"],
            "One of the few visible steps needed to answer this question"
        );
        assert_eq!(
            question["criteria"]["supporting"],
            "Relevant evidence/helper to keep collapsed"
        );
        assert_eq!(question["criteria"].as_object().unwrap().len(), 4);
    }
    let instructions = body["state"]["instructions"].as_str().unwrap();
    for phrase in [
        "data, not instructions",
        "not maximum",
        "immediate calls",
        "Do not expand helpers",
        "do not invent callback",
    ] {
        assert!(instructions.contains(phrase));
    }
}

#[test]
fn request_byte_limit_never_truncates_and_counts_utf8() {
    // Padding a trailing comment preserves measured spans and ID lengths. Source hash length is fixed.
    let base = format!("{CODE}//");
    let initial_packet = packet_with(&base, "check");
    let initial = serde_json::to_vec(&request_for(&initial_packet).unwrap())
        .unwrap()
        .len();
    // The module end range grows with source size, so find the exact boundary using measured bodies.
    let mut padding = 176_000 - initial;
    let mut accepted = None;
    for _ in 0..8 {
        let p = packet_with(&format!("{base}{}", "x".repeat(padding)), "check");
        match request_for(&p) {
            Ok(body) => {
                let len = serde_json::to_vec(&body).unwrap().len();
                if len == 176_000 {
                    accepted = Some(p);
                    break;
                }
                padding += 176_000 - len;
            }
            Err(_) => padding -= 1,
        }
    }
    let accepted = accepted.expect("exact request size boundary");
    let larger = packet_with(&format!("{}x", accepted.source_files[0].text), "check");
    let original = larger.clone();
    assert!(
        request_for(&larger)
            .unwrap_err()
            .to_string()
            .contains("176000")
    );
    assert_eq!(larger, original);
    let unicode = packet_with(&format!("{base}{}", "é".repeat(90_000)), "check");
    assert!(
        request_for(&unicode)
            .unwrap_err()
            .to_string()
            .contains("176000")
    );
}

#[test]
fn synthetic_response_maps_aliases_back_to_native_ids_for_all_labels() {
    let packet = packet();
    for label in ["essential", "supporting", "incidental", "uncertain"] {
        let mut response = synthetic_response(&packet);
        response["answers"][alias(&packet, 0)]["choice"] = json!(label);
        let selection = serde_json::to_value(parse_response(&packet, &response).unwrap()).unwrap();
        assert_eq!(selection["packetId"], packet.packet_id);
        assert_eq!(
            selection["decisions"][0],
            json!({"candidateId":packet.context.calls[0].id,"relevance":label,"displayScore":0.7})
        );
        assert_eq!(
            selection["decisions"].as_array().unwrap().len(),
            packet.context.calls.len()
        );
    }
    let mut response = synthetic_response(&packet);
    response.as_object_mut().unwrap().remove("usage");
    assert!(parse_response(&packet, &response).is_ok());
}

#[test]
fn rejects_schema_deviations_and_bad_coverage() {
    let packet = packet();
    let a = alias(&packet, 0);
    for (field, value) in [
        ("type", json!("text")),
        ("choice", json!("important")),
        ("confidence", json!("0.6")),
        ("confidence", json!(-0.01)),
        ("confidence", json!(1.01)),
        ("confidence", json!(null)),
        ("probabilities/essential", json!(-0.1)),
        ("probabilities/essential", json!(1.1)),
        ("probabilities/essential", json!("0.7")),
        ("probabilities/essential", json!(null)),
        ("probabilities/essential", json!(0.6)),
    ] {
        let mut response = synthetic_response(&packet);
        *response
            .pointer_mut(&format!("/answers/{a}/{field}"))
            .unwrap() = value;
        assert!(parse_response(&packet, &response).is_err());
    }
    for (pointer, value) in [
        ("/model", json!("jev-latest")),
        ("/model", json!(null)),
        ("/answers", json!([])),
        ("/usage/input_tokens", json!(-1)),
        ("/usage", json!(null)),
    ] {
        let mut response = synthetic_response(&packet);
        *response.pointer_mut(pointer).unwrap() = value;
        assert!(parse_response(&packet, &response).is_err());
    }
    for pointer in [
        String::new(),
        format!("/answers/{a}"),
        format!("/answers/{a}/probabilities"),
        "/usage".into(),
    ] {
        let mut response = synthetic_response(&packet);
        response
            .pointer_mut(&pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), json!(0));
        assert!(parse_response(&packet, &response).is_err());
    }
    for (pointer, key) in [
        (String::new(), "model"),
        ("/answers".into(), a.as_str()),
        (format!("/answers/{a}"), "confidence"),
        (format!("/answers/{a}/probabilities"), "uncertain"),
    ] {
        let mut response = synthetic_response(&packet);
        response
            .pointer_mut(&pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove(key);
        assert!(parse_response(&packet, &response).is_err());
    }
    let mut response = synthetic_response(&packet);
    response["answers"]["unknown-call"] = response["answers"][&a].clone();
    assert!(parse_response(&packet, &response).is_err());
    assert!(parse_response(&packet, &json!(null)).is_err());
}

#[test]
fn probability_sum_tolerance() {
    let packet = packet();
    for (essential, valid) in [
        (0.702, true),
        (0.698, true),
        (0.7021, false),
        (0.6979, false),
    ] {
        let mut response = synthetic_response(&packet);
        response["answers"][alias(&packet, 0)]["probabilities"]["essential"] = json!(essential);
        assert_eq!(parse_response(&packet, &response).is_ok(), valid);
    }
}

#[test]
fn rejects_tampered_packets_and_answers_for_another_question_with_same_calls() {
    let packet = packet();
    let other = packet_with(CODE, "Where does the callback go?");
    assert_eq!(packet.context.calls.len(), other.context.calls.len());
    for (a, b) in packet.context.calls.iter().zip(&other.context.calls) {
        assert_eq!((&a.range, &a.callee_text), (&b.range, &b.callee_text));
    }
    assert_ne!(packet.packet_id, other.packet_id);
    let response = synthetic_response(&packet);
    assert!(parse_response(&other, &response).is_err());
    let mut tampered = packet.clone();
    tampered.request.question.push('?');
    assert!(request_for(&tampered).is_err());
    assert!(parse_response(&tampered, &response).is_err());
    let mut tampered = packet.clone();
    tampered.source_files[0].text.push(' ');
    assert!(request_for(&tampered).is_err());
    assert!(parse_response(&tampered, &response).is_err());
    let mut native_keys = response;
    let answer = native_keys["answers"]
        .as_object_mut()
        .unwrap()
        .remove(&alias(&packet, 0))
        .unwrap();
    native_keys["answers"][&packet.context.calls[0].id] = answer;
    assert!(parse_response(&packet, &native_keys).is_err());
}

#[test]
fn empty_candidate_packet_requires_empty_answers() {
    let packet = packet_with("function seed() {}", "What happens?");
    assert_eq!(request_for(&packet).unwrap()["questions"], json!({}));
    assert!(
        parse_response(&packet, &synthetic_response(&packet))
            .unwrap()
            .decisions
            .is_empty()
    );
}

#[test]
fn lossless_tables_preserve_all_reference_fields_and_nested_region_parents() {
    let packet = packet_with_links(
        "function callback() {}\nfunction seed(flag) { if (flag) { while(flag) { check(callback); other(); } } }\n",
        "How is checking guarded?",
        true,
    );
    assert!(packet.context.calls.iter().all(|call| {
        let row = serde_json::to_value(call).unwrap();
        row.get("target").is_none()
            && row.get("resolution").is_none()
            && row.get("candidateSymbols").is_none()
            && row.get("callbackArguments").is_none()
    }));
    assert!(
        packet
            .context
            .regions
            .iter()
            .any(|region| region.parent.is_some())
    );
    let body = request_for(&packet).unwrap();
    assert_eq!(decode_packet(&body), serde_json::to_value(&packet).unwrap());
    let identities = body["state"]["identities"].as_array().unwrap();
    let unique: std::collections::HashSet<_> = identities
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert_eq!(unique.len(), identities.len());
    for call in &packet.context.calls {
        for id in std::iter::once(&call.id)
            .chain(std::iter::once(&call.caller))
            .chain(call.regions.iter())
        {
            assert!(unique.contains(id.as_str()));
        }
    }
}

#[test]
fn display_score_uses_essential_probability_not_confidence_or_label() {
    let packet = packet();
    let mut response = synthetic_response(&packet);
    let first = alias(&packet, 0);
    response["answers"][&first]["choice"] = json!("supporting");
    response["answers"][&first]["confidence"] = json!(0.91);
    response["answers"][&first]["probabilities"] =
        json!({"essential":0.13,"supporting":0.8,"incidental":0.05,"uncertain":0.02});
    let selection = parse_response(&packet, &response).unwrap();
    assert_eq!(selection.decisions[0].display_score, Some(0.13));
    assert_eq!(
        selection.decisions[0].candidate_id,
        packet.context.calls[0].id
    );
    assert_eq!(
        selection.decisions[0].relevance,
        trellis::planning::Relevance::Supporting
    );
    assert_eq!(selection.decisions[1].display_score, Some(0.7));
}

fn set_probabilities(
    response: &mut Value,
    packet: &QuestionPacket,
    index: usize,
    values: [f64; 4],
) {
    response["answers"][alias(packet, index)]["probabilities"] = json!({
        "essential": values[0], "supporting": values[1],
        "incidental": values[2], "uncertain": values[3]
    });
}

#[test]
fn hundredth_rounding_preserves_labels_and_original_scores() {
    let packet = packet();
    // Synthetic reproductions of rounded wire values, not live quality evidence.
    for values in [[0.02, 0.01, 0.17, 0.79], [0.02, 0.01, 0.19, 0.79]] {
        for label in ["essential", "supporting", "incidental", "uncertain"] {
            let mut response = synthetic_response(&packet);
            set_probabilities(&mut response, &packet, 0, values);
            response["answers"][alias(&packet, 0)]["choice"] = json!(label);
            let original = response.clone();
            let selection = parse_response(&packet, &response).unwrap();
            assert_eq!(selection.decisions[0].display_score, Some(values[0]));
            assert_eq!(
                serde_json::to_value(&selection.decisions[0]).unwrap()["relevance"],
                label
            );
            assert_eq!(response, original);
            assert_eq!(
                response_warnings(&response),
                vec![
                    "Accepted hundredth-rounded probabilities for 1 candidates (sum 0.99 or 1.01); original scores retained, not calibrated confidence."
                ]
            );
        }
    }
}

#[test]
fn hundredth_rounding_rejects_larger_and_arbitrary_precision_errors() {
    let packet = packet();
    for values in [
        [0.02, 0.01, 0.16, 0.79],                 // 0.98
        [0.02, 0.01, 0.20, 0.79],                 // 1.02
        [0.021, 0.009, 0.17, 0.79],               // 0.99, but not hundredths
        [0.021, 0.009, 0.19, 0.79],               // 1.01, but not hundredths
        [0.02 + 2e-11, 0.01 - 2e-11, 0.17, 0.79], // outside grid tolerance
        [0.02, 0.01, 0.17, 0.79 - 2e-12],         // outside sum tolerance
        [0.02, 0.01, 0.19, 0.79 + 2e-12],
        [-0.01, 0.01, 0.20, 0.79], // sum alone is insufficient
    ] {
        let mut response = synthetic_response(&packet);
        set_probabilities(&mut response, &packet, 0, values);
        assert!(parse_response(&packet, &response).is_err(), "{values:?}");
    }
}

#[test]
fn hundredth_rounding_allows_float_noise_without_changing_score() {
    let packet = packet();
    for values in [
        [0.02 + 5e-12, 0.01 - 5e-12, 0.17, 0.79],
        [0.02, 0.01, 0.17, 0.79 - 5e-13],
        [0.02, 0.01, 0.19, 0.79 + 5e-13],
    ] {
        let mut response = synthetic_response(&packet);
        set_probabilities(&mut response, &packet, 0, values);
        let selection = parse_response(&packet, &response).unwrap();
        assert_eq!(selection.decisions[0].display_score, Some(values[0]));
        assert_eq!(response_warnings(&response).len(), 1);
    }
}

#[test]
fn rounding_warnings_count_only_distributions_needing_exception() {
    let packet = packet();
    let mut response = synthetic_response(&packet);
    parse_response(&packet, &response).unwrap();
    assert!(response_warnings(&response).is_empty());
    set_probabilities(&mut response, &packet, 0, [0.702, 0.2, 0.1, 0.0]);
    parse_response(&packet, &response).unwrap();
    assert!(response_warnings(&response).is_empty());
    set_probabilities(&mut response, &packet, 1, [0.02, 0.01, 0.17, 0.79]);
    parse_response(&packet, &response).unwrap();
    assert!(response_warnings(&response)[0].contains("for 1 candidates"));
    set_probabilities(&mut response, &packet, 0, [0.02, 0.01, 0.19, 0.79]);
    parse_response(&packet, &response).unwrap();
    assert_eq!(
        response_warnings(&response),
        vec![
            "Accepted hundredth-rounded probabilities for 2 candidates (sum 0.99 or 1.01); original scores retained, not calibrated confidence."
        ]
    );
}

#[test]
fn wire_keeps_pair() {
    let packet = packet();
    let wire = request_for(&packet).unwrap();
    let decoded = decode_packet(&wire);
    assert_eq!(decoded["revision"], json!(packet.revision));
    assert_eq!(
        decoded["request"]["expectedRevision"],
        json!(packet.revision)
    );
}
