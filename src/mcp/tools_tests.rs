use super::*;
use crate::mcp::wire::{RESPONSE_BYTES, write_response};
use crate::store::topology::WorkspaceIdentity;
fn context() -> (tempfile::TempDir, OpenedWorkspace) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("selected");
    std::fs::create_dir(&root).unwrap();
    let opened =
        OpenedWorkspace::new(WorkspaceIdentity::discover(Some(&root), dir.path()).unwrap());
    (dir, opened)
}
fn pin() -> Value {
    json!({"indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":9007199254740991_u64})
}
fn sid() -> String {
    format!("sid:v1:{}", "a".repeat(32))
}
fn good(name: &str) -> Value {
    match name {
        "trellis_workspace_describe" => json!({"schemaVersion":1}),
        "trellis_find_symbols" | "trellis_search_symbols" => {
            json!({"schemaVersion":1,"query":"a"})
        }
        "trellis_references" | "trellis_rules_check" | "trellis_endpoints" => {
            json!({"schemaVersion":1})
        }
        "trellis_rules_explain" => json!({"schemaVersion":1,"rule":"rule-a",
            "target":{"sourceSetId":"app","language":"rust","path":"src/a.rs","sid":sid()}}),
        "trellis_declaration"
        | "trellis_outgoing_calls"
        | "trellis_incoming_calls"
        | "trellis_call_graph" => {
            json!({"schemaVersion":1,"symbolId":sid()})
        }
        "trellis_show" => json!({"schemaVersion":1,"view":"sequence","symbolId":sid()}),
        _ => json!({"schemaVersion":1,"path":"src/a.rs","startLine":1,"endLine":1}),
    }
}
fn check(name: &str, v: &Value, code: &str) {
    assert_eq!(validate(name, Some(v)), Err(code), "{name} {v}");
}
/// #769: every accepted field of every tool has its own static message, short enough
/// for the 512-byte summary, and a bad value of it names it. Graph bounds name the
/// constants they check. Other codes keep their generic message.
#[test]
fn invalid_fields_name_themselves_in_static_messages() {
    let message = |name: &str, v: &Value| super::check(name, Some(v)).unwrap_err().message;
    for name in TOOLS {
        let (accepted, required) = fields(name);
        for field in accepted {
            let text = field_message(name, field);
            assert_ne!(text, UNKNOWN_FIELD, "{name} {field}");
            assert!(text.contains(&format!("`{field}`")), "{text}");
            let summary = format!("{name} failed: invalid_request: {text}");
            assert!(summary.len() <= 512);
            if *field == "schemaVersion" {
                continue;
            }
            // A value of the wrong type is rejected and names the field
            // (or the exclusive pair it belongs to).
            let mut bad = good(name);
            bad[*field] = json!({"wrong":"type"});
            if *field == "lookupKey" {
                bad.as_object_mut().unwrap().remove("symbolId");
                bad.as_object_mut().unwrap().remove("query");
            }
            let named = message(name, &bad).expect("invalid_request message");
            assert!(
                named.contains(&format!("`{field}`")),
                "{name} {field}: {named}"
            );
            if required.contains(field) {
                let mut missing = good(name);
                missing.as_object_mut().unwrap().remove(*field);
                assert_eq!(message(name, &missing), Some(text), "{name} {field}");
            }
        }
        let mut unknown = good(name);
        unknown["unexpected"] = json!(true);
        assert_eq!(message(name, &unknown), Some(UNKNOWN_FIELD));
        assert_eq!(
            message(name, &json!([])),
            Some("Tool arguments must be a JSON object")
        );
    }
    use crate::graph_query::{MAX_CALLS, MAX_DEPTH, MAX_NODES};
    for (field, range) in [
        ("depth", format!("from 0 to {MAX_DEPTH}")),
        ("maxNodes", format!("from 1 to {MAX_NODES}")),
        ("maxCalls", format!("from 0 to {MAX_CALLS}")),
    ] {
        assert!(field_message("trellis_call_graph", field).ends_with(&range));
    }
    // An object field sent as a string (a stringifying client) says so.
    for (name, field) in [
        ("trellis_rules_explain", "target"),
        ("trellis_rules_explain", "destination"),
        ("trellis_rules_explain", "expectedBasis"),
        ("trellis_find_symbols", "expectedBasis"),
    ] {
        let mut stringified = good(name);
        let object = json!({"indexGeneration":"x"}).to_string();
        stringified[field] = json!(object);
        if field == "destination" {
            stringified["rule"] = json!("$layers");
        }
        let named = message(name, &stringified).unwrap();
        assert!(
            named.starts_with(&format!("`{field}` must be a JSON object (got a string)")),
            "{named}"
        );
    }
    // A numeric cap keeps its own code and generic message.
    let over = super::check(
        "trellis_rules_check",
        Some(&json!({"schemaVersion":1,"limit":51})),
    );
    assert_eq!(
        over,
        Err(Rejection {
            code: "range_too_large",
            message: None
        })
    );
}
#[test]
fn closed_application_objects_and_pins() {
    let n = "trellis_workspace_describe";
    assert_eq!(validate(n, Some(&good(n))), Ok(()));
    for v in [
        json!({}),
        json!({"schemaVersion":0}),
        json!({"schemaVersion":1,"other":true}),
        json!({"schemaVersion":1.1}),
        json!({"schemaVersion":null}),
    ] {
        check(n, &v, "invalid_request")
    }
    for n in TOOLS {
        assert_eq!(validate(n, None), Err("invalid_request"));
        for v in [json!(null), json!([]), json!(true), json!("x")] {
            assert_eq!(validate(n, Some(&v)), Err("invalid_request"));
        }
        let mut v = good(n);
        v["schemaVersion"] = json!(2);
        check(n, &v, "invalid_request");
        let mut v = good(n);
        v["unknown"] = json!(true);
        check(n, &v, "invalid_request");
        if n == "trellis_workspace_describe" {
            continue;
        }
        for wrong in [
            json!(null),
            json!({"indexGeneration":"123e4567-e89b-42d3-a456-426614174000"}),
            json!({"indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":1,"extra":0}),
            json!({"indexGeneration":"00000000-0000-0000-0000-000000000000","indexRevision":1}),
            json!({"indexGeneration":"123E4567-e89b-42d3-a456-426614174000","indexRevision":1}),
            json!({"indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":-1}),
            json!({"indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":9007199254740992_u64}),
        ] {
            let mut v = good(n);
            v["expectedBasis"] = wrong;
            check(n, &v, "invalid_request");
        }
        let mut v = good(n);
        v["expectedBasis"] = pin();
        assert_eq!(validate(n, Some(&v)), Ok(()));
    }
}
#[test]
fn stable_lookup_selectors_are_exclusive_and_nonempty() {
    for name in [
        "trellis_find_symbols",
        "trellis_declaration",
        "trellis_outgoing_calls",
        "trellis_incoming_calls",
        "trellis_call_graph",
    ] {
        let other = if name == "trellis_find_symbols" {
            "query"
        } else {
            "symbolId"
        };
        let mut args = json!({"schemaVersion":1,"lookupKey":"src/store.rs::Store::read"});
        assert_eq!(validate(name, Some(&args)), Ok(()));
        // No new per-field cap: only the existing transport request bound applies.
        args["lookupKey"] = json!("x".repeat(9000));
        assert_eq!(validate(name, Some(&args)), Ok(()));
        for invalid in [json!(""), json!(null), json!(1), json!([])] {
            args["lookupKey"] = invalid;
            check(name, &args, "invalid_request");
        }
        args["lookupKey"] = json!("src/store.rs::Store::read");
        args[other] = good(name)[other].clone();
        check(name, &args, "invalid_request");
        check(name, &json!({"schemaVersion":1}), "invalid_request");
    }
    check(
        "trellis_search_symbols",
        &json!({"schemaVersion":1,"lookupKey":"src/a.rs::a"}),
        "invalid_request",
    );
}

#[test]
fn candidate_summaries_precede_operation_fields() {
    for name in [
        "trellis_declaration",
        "trellis_outgoing_calls",
        "trellis_incoming_calls",
        "trellis_call_graph",
    ] {
        let envelope = json!({"schemaVersion":1,"requestId":1,"data":{"lookupKey":"src/a.rs::a","candidates":[{},{}]},"truncated":false});
        let summary = result(name, envelope, true);
        assert_eq!(summary["isError"], false);
        assert_eq!(
            summary["content"][0]["text"],
            format!("{name}: 2 candidates; full result in structuredContent.")
        );
    }
}

#[test]
fn find_and_inspect_boundaries() {
    let n = "trellis_find_symbols";
    for q in ["", &"é".repeat(129), &"x".repeat(257)] {
        let mut v = good(n);
        v["query"] = json!(q);
        check(n, &v, "invalid_request");
    }
    for q in [&"x".repeat(256), &"é".repeat(128), "  a  "] {
        let mut v = good(n);
        v["query"] = json!(q);
        assert_eq!(validate(n, Some(&v)), Ok(()));
    }
    let mut v = good(n);
    v["query"] = json!(null);
    check(n, &v, "invalid_request");
    for name in [
        "trellis_find_symbols",
        "trellis_outgoing_calls",
        "trellis_incoming_calls",
    ] {
        let original = good(name);
        for (number, expected) in [
            (json!(0), Some("range_too_large")),
            (json!(1), None),
            (json!(50), None),
            (json!(51), Some("range_too_large")),
            (json!(null), Some("invalid_request")),
            (json!(1.5), Some("invalid_request")),
            (json!(-1), Some("invalid_request")),
            (json!(9007199254740992_u64), Some("invalid_request")),
            (json!("1"), Some("invalid_request")),
        ] {
            let mut v = original.clone();
            v["limit"] = number;
            assert_eq!(validate(name, Some(&v)).err(), expected, "{name} {v}");
        }
    }
    let n = "trellis_declaration";
    for view in [
        "incoming_calls",
        "call_paths",
        "usages",
        "type_hierarchy",
        "implementations",
        "coverage",
        "other",
    ] {
        let mut v = good(n);
        v["view"] = json!(view);
        check(n, &v, "invalid_request");
    }
    let mut v = good(n);
    v["limit"] = json!(1);
    check(n, &v, "invalid_request");
    let mut v = good("trellis_outgoing_calls");
    v["view"] = json!("outgoing_calls");
    check("trellis_outgoing_calls", &v, "invalid_request");
    for wrong in [
        "sid:v1:ABCDEF0123456789abcdef0123456789",
        "sid:v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "sid:v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaag",
        "occ:v2:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "occ:v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ] {
        let mut v = good(n);
        v["symbolId"] = json!(wrong);
        check(n, &v, "invalid_request");
    }
}
#[test]
fn search_is_closed_and_bounds_decoded_utf8_bytes() {
    let name = "trellis_search_symbols";
    assert_eq!(super::super::search::DEFAULT_LIMIT, 10);
    for query in [
        "x".repeat(1024),
        "é".repeat(512),
        "  a  ".into(),
        "\n".into(),
    ] {
        assert_eq!(
            validate(name, Some(&json!({"schemaVersion":1,"query":query}))),
            Ok(())
        );
    }
    for query in [
        json!(""),
        json!("x".repeat(1025)),
        json!("é".repeat(513)),
        json!(null),
        json!(1),
    ] {
        check(
            name,
            &json!({"schemaVersion":1,"query":query}),
            "invalid_request",
        );
    }
    for field in [
        "filter", "filters", "backend", "score", "scores", "debug", "cursor", "index", "view",
    ] {
        let mut args = good(name);
        args[field] = json!(true);
        check(name, &args, "invalid_request");
    }
    for (limit, expected) in [
        (json!(1), Ok(())),
        (json!(50), Ok(())),
        (json!(0), Err("range_too_large")),
        (json!(51), Err("range_too_large")),
        (json!(-1), Err("invalid_request")),
        (json!(1.5), Err("invalid_request")),
        (json!(null), Err("invalid_request")),
        (json!("10"), Err("invalid_request")),
    ] {
        assert_eq!(
            validate(
                name,
                Some(&json!({"schemaVersion":1,"query":"q","limit":limit}))
            ),
            expected
        );
    }
}

#[test]
fn search_role_is_optional_closed_and_does_not_change_find() {
    let name = "trellis_search_symbols";
    assert_eq!(validate(name, Some(&good(name))), Ok(()));
    for role in ["any", "production", "test"] {
        let mut args = good(name);
        args["role"] = json!(role);
        assert_eq!(validate(name, Some(&args)), Ok(()));
        check("trellis_find_symbols", &args, "invalid_request");
    }
    for role in [
        json!(""),
        json!("Any"),
        json!("tests"),
        json!(" production"),
        json!("test "),
        json!(null),
        json!(true),
        json!(1),
        json!([]),
        json!({}),
    ] {
        let mut args = good(name);
        args["role"] = role;
        check(name, &args, "invalid_request");
    }
}

#[test]
fn source_path_hash_and_line_matrix() {
    let n = "trellis_read_source";
    for path in [
        "",
        "/a",
        "../a",
        "a/../b",
        "a/./b",
        "./a",
        "a//b",
        "a/",
        "a\\b",
        "a:b",
        "a\0b",
        &"x".repeat(4097),
    ] {
        let mut v = good(n);
        v["path"] = json!(path);
        check(n, &v, "invalid_request");
    }
    for path in [&"x".repeat(4096), ".hidden/file", "a/é"] {
        let mut v = good(n);
        v["path"] = json!(path);
        assert_eq!(validate(n, Some(&v)), Ok(()));
    }
    for hash in ["A".repeat(64), "z".repeat(64), "a".repeat(63)] {
        let mut v = good(n);
        v["expectedContentHash"] = json!(hash);
        check(n, &v, "invalid_request");
    }
    let mut v = good(n);
    v["expectedContentHash"] = json!("a".repeat(64));
    v["expectedBasis"] = pin();
    assert_eq!(validate(n, Some(&v)), Ok(()));
    for field in ["startLine", "endLine"] {
        for wrong in [
            json!(0),
            json!(-1),
            json!(null),
            json!(1.1),
            json!(9007199254740992_u64),
            json!("1"),
        ] {
            let mut v = good(n);
            v[field] = wrong;
            check(n, &v, "invalid_request");
        }
    }
    let mut v = good(n);
    v["endLine"] = json!(200);
    assert_eq!(validate(n, Some(&v)), Ok(()));
    v["endLine"] = json!(201);
    check(n, &v, "range_too_large");
    v["startLine"] = json!(202);
    check(n, &v, "invalid_request");
}
#[test]
fn exact_envelopes_identity_precedence_and_complete_wire_bound() {
    let (dir, workspace) = context();
    let max_ids = [
        json!("\u{0000}".repeat(42) + "aa"),
        json!(9007199254740991_i64),
        json!(-9007199254740991_i64),
    ];
    let describe_expected = json!({"schemaVersion":1,"requestId":7,"evidenceBasis":null,"data":{
        "workspaceLabel":"selected","indexState":"unavailable","progress":null,"watcherDegraded":false,"languages":[],"toolVersion":30,"schemaVersion":1,
        "searchIndex":{"backend":null,"unavailableReason":null},
        "apps":{"state":"unavailable","items":[],"noApp":null},
        "limits":{"requestBytes":16384,"responseBytes":65536,"sourceBytes":16384,"sourceLines":200,"defaultLimit":20,"callDefaultLimit":6,"maxLimit":50,"pageBytes":24576,"calleeTextBytes":1024,"hintWorkMs":1000,"deadlineMs":5000,"concurrency":4}},
        "warnings":[],"partial":true,"truncated":false,"truncationReason":null});
    assert_eq!(describe(&json!(7), workspace.label()), describe_expected);
    for modern in [false, true] {
        for id in &max_ids {
            for name in TOOLS {
                let good = good(name);
                let prepared = prepare(name, Some(&good), id, modern, &workspace);
                let result = &prepared.response;
                assert_eq!(result.get("resultType").is_some(), modern);
                assert_eq!(result["isError"], name != "trellis_workspace_describe");
                assert_eq!(
                    result["content"][0]["text"],
                    if name == "trellis_workspace_describe" {
                        format!("{name}: 1 workspace; full result in structuredContent.")
                    } else {
                        format!("{name} failed: index_not_ready: Index is not ready")
                    }
                );
                assert_eq!(result["structuredContent"]["requestId"], *id);
                if name != "trellis_workspace_describe" {
                    assert_eq!(
                        result["structuredContent"]["error"]["code"],
                        "index_not_ready"
                    );
                }
                let mut response = Vec::new();
                write_response(
                    &mut response,
                    &json!({"jsonrpc":"2.0","id":id,"result":result}),
                )
                .unwrap();
                assert!(response.len() <= RESPONSE_BYTES);
                let invalid = prepare(
                    name,
                    Some(&json!({"schemaVersion":1,"unknown":0})),
                    id,
                    modern,
                    &workspace,
                );
                assert_eq!(
                    invalid.response["structuredContent"]["error"]["code"],
                    "invalid_request"
                );
                for code in [
                    "invalid_request",
                    "range_too_large",
                    "unsupported_encoding",
                    "revision_conflict",
                    "index_not_ready",
                    "not_found",
                    "too_many_requests",
                    "deadline_exceeded",
                    "rebuild_in_progress",
                    "root_changed",
                    "store_unavailable",
                ] {
                    let tool = super::result(name, super::failure(id, code), modern);
                    let failure = &tool["structuredContent"]["error"];
                    assert_eq!(
                        tool["content"][0]["text"],
                        format!(
                            "{name} failed: {}: {}",
                            failure["code"].as_str().unwrap(),
                            failure["message"].as_str().unwrap()
                        )
                    );
                    assert!(tool["content"][0]["text"].as_str().unwrap().len() <= 512);
                    let mut wire = Vec::new();
                    write_response(&mut wire, &json!({"jsonrpc":"2.0","id":id,"result":tool}))
                        .unwrap();
                    assert!(wire.len() <= RESPONSE_BYTES);
                    assert_eq!(
                        tool["structuredContent"]["error"]["currentBasis"],
                        Value::Null
                    );
                    assert_eq!(
                        tool["structuredContent"]["error"]["currentContentHash"],
                        Value::Null
                    );
                }
            }
        }
    }
    let mut unsent = prepare(
        "trellis_workspace_describe",
        Some(&good("trellis_workspace_describe")),
        &json!("pending"),
        true,
        &workspace,
    );
    assert_eq!(unsent.response["isError"], false);
    let root = dir.path().join("selected");
    std::fs::rename(&root, dir.path().join("moved")).unwrap();
    final_check(&mut unsent, &workspace);
    assert_eq!(
        unsent.response["structuredContent"]["error"]["code"],
        "root_changed"
    );
    for name in TOOLS {
        let valid = good(name);
        let mut prepared = prepare(name, Some(&valid), &json!(2), true, &workspace);
        assert_eq!(
            prepared.response["structuredContent"]["error"]["code"],
            "root_changed"
        );
        final_check(&mut prepared, &workspace);
        assert_eq!(
            prepared.response["structuredContent"]["error"]["code"],
            "root_changed"
        );
        let mut invalid = prepare(
            name,
            Some(&json!({"schemaVersion":1,"unknown":0})),
            &json!(2),
            true,
            &workspace,
        );
        final_check(&mut invalid, &workspace);
        assert_eq!(
            invalid.response["structuredContent"]["error"]["code"],
            "invalid_request"
        );
    }
}
#[test]
fn marker_verification_error_is_sanitized_and_does_not_repair() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("git-selected");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(root.join(".git")).unwrap();
    let workspace =
        OpenedWorkspace::new(WorkspaceIdentity::discover(Some(&root), temp.path()).unwrap());
    let mut prepared = prepare(
        "trellis_workspace_describe",
        Some(&good("trellis_workspace_describe")),
        &json!(5),
        false,
        &workspace,
    );
    assert_eq!(prepared.response["isError"], false);
    let marker = root.join(".git/trellis/workspace-id");
    std::fs::remove_file(&marker).unwrap();
    final_check(&mut prepared, &workspace);
    assert_eq!(
        prepared.response["structuredContent"],
        attributed(failure(&json!(5), "store_unavailable"), &workspace, false)
    );
    assert!(!marker.exists());
    let valid = prepare(
        "trellis_find_symbols",
        Some(&good("trellis_find_symbols")),
        &json!(6),
        true,
        &workspace,
    );
    assert_eq!(
        valid.response["structuredContent"]["error"]["code"],
        "store_unavailable"
    );
    let invalid = prepare(
        "trellis_find_symbols",
        Some(&json!({"schemaVersion":1,"query":""})),
        &json!(7),
        true,
        &workspace,
    );
    assert_eq!(
        invalid.response["structuredContent"]["error"]["code"],
        "invalid_request"
    );
}
#[test]
fn call_graph_bounds_are_exact_and_unclamped() {
    let name = "trellis_call_graph";
    for (field, valid, invalid) in [
        ("depth", vec![0, 2, 5], vec![json!(6), json!(-1)]),
        ("maxNodes", vec![1, 150], vec![json!(0), json!(151)]),
        ("maxCalls", vec![0, 500], vec![json!(501), json!(-1)]),
    ] {
        for n in valid {
            let mut v = good(name);
            v[field] = json!(n);
            assert_eq!(validate(name, Some(&v)), Ok(()), "{field} {n}");
        }
        for n in invalid.into_iter().chain([
            json!(null),
            json!(1.5),
            json!("2"),
            json!(9007199254740992_u64),
        ]) {
            let mut v = good(name);
            v[field] = n;
            check(name, &v, "invalid_request");
        }
    }
    for producer in [json!(""), json!(1), json!(true), json!([])] {
        let mut v = good(name);
        v["semanticProducerId"] = producer;
        check(name, &v, "invalid_request");
    }
    for producer in [json!(null), json!("p")] {
        let mut v = good(name);
        v["semanticProducerId"] = producer;
        assert_eq!(validate(name, Some(&v)), Ok(()));
    }
    for field in ["view", "limit", "revisionId", "sourceSetId"] {
        let mut v = good(name);
        v[field] = json!(1);
        check(name, &v, "invalid_request");
    }
    let mut v = good(name);
    v["workspace"] = json!("x".repeat(4097));
    check(name, &v, "invalid_request");
    for field in ["includeExternal", "includeTests"] {
        for flag in [false, true] {
            let mut v = good(name);
            v[field] = json!(flag);
            assert_eq!(validate(name, Some(&v)), Ok(()));
            let query = call_graph_query(&v);
            assert_eq!(
                if field == "includeExternal" {
                    query.include_external
                } else {
                    query.include_tests
                },
                flag
            );
        }
        for invalid in [json!(null), json!(0), json!("true"), json!([]), json!({})] {
            let mut v = good(name);
            v[field] = invalid;
            check(name, &v, "invalid_request");
        }
    }
    let defaults = call_graph_query(&good(name));
    assert!(!defaults.include_external && !defaults.include_tests);
    assert_eq!(
        (
            defaults.depth,
            defaults.max_nodes,
            defaults.max_calls,
            defaults.semantic_producer_id
        ),
        (2, 8, 10, None)
    );
    // #665: compact answers are smaller, so their default limits are larger.
    let mut v = good(name);
    v["compact"] = json!(true);
    let compact = call_graph_query(&v);
    assert_eq!((compact.max_nodes, compact.max_calls), (50, 60));
    v["maxNodes"] = json!(150);
    v["maxCalls"] = json!(500);
    let compact = call_graph_query(&v);
    assert_eq!((compact.max_nodes, compact.max_calls), (150, 500));
    let mut v = good(name);
    v["semanticProducerId"] = json!("p");
    v["depth"] = json!(0);
    v["maxNodes"] = json!(1);
    v["maxCalls"] = json!(0);
    let query = call_graph_query(&v);
    assert_eq!((query.depth, query.max_nodes, query.max_calls), (0, 1, 0));
    assert_eq!(query.semantic_producer_id.as_deref(), Some("p"));
    assert_eq!(query.root_syntax_id, sid());
}
#[test]
fn incoming_is_prepared_and_catalogued() {
    let name = "trellis_incoming_calls";
    assert!(TOOLS.contains(&name));
    let mut valid = json!({"schemaVersion":1,"symbolId":sid(),"limit":20,
        "expectedBasis":pin()});
    assert_eq!(validate(name, Some(&valid)), Ok(()));
    for field in ["view", "other"] {
        valid[field] = json!("invalid");
        check(name, &valid, "invalid_request");
        valid.as_object_mut().unwrap().remove(field);
    }
    for field in ["workspace", "expectedBasis", "limit"] {
        valid[field] = Value::Null;
        check(name, &valid, "invalid_request");
        valid.as_object_mut().unwrap().remove(field);
    }
    for limit_value in [json!(0), json!(51), json!(51_u64)] {
        valid["limit"] = limit_value;
        check(name, &valid, "range_too_large");
    }
    for limit_value in [json!(-1), json!(1.5), json!("1")] {
        valid["limit"] = limit_value;
        check(name, &valid, "invalid_request");
    }
    valid.as_object_mut().unwrap().remove("limit");
    valid["symbolId"] = json!("sid:v1:BAD");
    check(name, &valid, "invalid_request");
    valid["symbolId"] = json!("x".repeat(8193));
    check(name, &valid, "invalid_request");
    valid["symbolId"] = json!(sid());
    let (_dir, workspace) = context();
    let prepared = prepare(name, Some(&valid), &json!(1), true, &workspace);
    assert!(prepared.valid_arguments);
}
/// #467: the call tools take an optional explicit producer of 1..=512 bytes; anything
/// else, including an explicit null, is `invalid_request` before any read.
#[test]
fn call_tools_bound_the_explicit_semantic_producer() {
    for name in ["trellis_outgoing_calls", "trellis_incoming_calls"] {
        let mut valid = good(name);
        for producer in [json!("scip-typescript"), json!("p".repeat(512))] {
            valid["semanticProducerId"] = producer;
            assert_eq!(validate(name, Some(&valid)), Ok(()), "{name}");
        }
        for producer in [
            Value::Null,
            json!(""),
            json!("p".repeat(513)),
            json!("a\u{0}b"),
            json!(7),
            json!(["scip-typescript"]),
        ] {
            valid["semanticProducerId"] = producer;
            check(name, &valid, "invalid_request");
        }
    }
}
/// #528, #715: `compact` is an optional boolean on the call tools and on find and
/// search (query and lookup-key find alike); other tools reject it.
#[test]
fn compact_is_a_closed_boolean_on_the_call_and_symbol_tools() {
    let lookup = json!({"schemaVersion":1,"lookupKey":"src/a.rs::a"});
    for (name, base) in [
        ("trellis_outgoing_calls", good("trellis_outgoing_calls")),
        ("trellis_incoming_calls", good("trellis_incoming_calls")),
        ("trellis_call_graph", good("trellis_call_graph")),
        ("trellis_find_symbols", good("trellis_find_symbols")),
        ("trellis_find_symbols", lookup),
        ("trellis_search_symbols", good("trellis_search_symbols")),
    ] {
        let mut valid = base;
        for compact in [json!(true), json!(false)] {
            valid["compact"] = compact;
            assert_eq!(validate(name, Some(&valid)), Ok(()), "{name}");
        }
        for compact in [Value::Null, json!(1), json!("true"), json!([true])] {
            valid["compact"] = compact;
            check(name, &valid, "invalid_request");
        }
    }
    for name in ["trellis_declaration", "trellis_read_source"] {
        let mut other = good(name);
        other["compact"] = json!(true);
        check(name, &other, "invalid_request");
    }
    // #665: outgoing calls also take a closed `includeExternal`; incoming does not.
    let name = "trellis_outgoing_calls";
    let mut valid = good(name);
    for include in [json!(true), json!(false)] {
        valid["includeExternal"] = include;
        assert_eq!(validate(name, Some(&valid)), Ok(()));
    }
    for include in [Value::Null, json!(1), json!("true")] {
        valid["includeExternal"] = include;
        check(name, &valid, "invalid_request");
    }
    let mut other = good("trellis_incoming_calls");
    other["includeExternal"] = json!(true);
    check("trellis_incoming_calls", &other, "invalid_request");
}
#[test]
fn summary_uses_the_singular_for_one_item() {
    let success = |data: Value| json!({"schemaVersion":1,"requestId":1,"data":data,"truncated":false,"truncationReason":null});
    for (name, data, text) in [
        ("trellis_find_symbols", json!({"symbols":[{}]}), "1 symbol"),
        ("trellis_find_symbols", json!({"symbols":[]}), "0 symbols"),
        ("trellis_search_symbols", json!({"hits":[{}]}), "1 hit"),
        ("trellis_search_symbols", json!({"hits":[]}), "0 hits"),
        ("trellis_outgoing_calls", json!({"calls":[{}]}), "1 call"),
        ("trellis_incoming_calls", json!({"callers":[{}]}), "1 call"),
        ("trellis_incoming_calls", json!({"callers":[]}), "0 calls"),
        (
            "trellis_outgoing_calls",
            json!({"calls":[{},{}]}),
            "2 calls",
        ),
        (
            "trellis_read_source",
            json!({"startLine":4,"endLine":4}),
            "1 line",
        ),
        (
            "trellis_read_source",
            json!({"startLine":4,"endLine":5}),
            "2 lines",
        ),
        ("trellis_declaration", json!({}), "1 declaration"),
        ("trellis_workspace_describe", json!({}), "1 workspace"),
    ] {
        assert_eq!(
            result(name, success(data), true)["content"][0]["text"],
            format!("{name}: {text}; full result in structuredContent.")
        );
    }
}
#[test]
fn architecture_inputs_are_closed_and_prompt_is_private() {
    let identity = json!({"sourceSetId":"app","language":"rust","path":"src/a.rs","sid":sid()});
    for name in ["trellis_references", "trellis_rules_check"] {
        assert_eq!(
            validate(name, Some(&json!({"schemaVersion":1,"paths":["src/**"]}))),
            Ok(())
        );
        for paths in [
            json!(null),
            json!("src/**"),
            json!(["src/[ab].rs"]),
            json!(["src/**bad"]),
            json!(["../src"]),
            json!(["/src"]),
            json!([true]),
        ] {
            assert_eq!(
                validate(name, Some(&json!({"schemaVersion":1,"paths":paths}))),
                Err("invalid_request")
            );
        }
    }
    assert_eq!(
        validate(
            "trellis_references",
            Some(&json!({"schemaVersion":1,"limit":51}))
        ),
        Err("range_too_large")
    );
    // #448: every architecture tool pages with `offset` and bounds samples with `limit`.
    for name in ["trellis_references", "trellis_rules_check"] {
        assert_eq!(
            validate(
                name,
                Some(&json!({"schemaVersion":1,"limit":1,"offset":9007199254740991_u64}))
            ),
            Ok(())
        );
        assert_eq!(
            validate(name, Some(&json!({"schemaVersion":1,"limit":0}))),
            Err("range_too_large")
        );
        for offset in [json!(-1), json!(1.5), json!("1"), json!(null)] {
            assert_eq!(
                validate(name, Some(&json!({"schemaVersion":1,"offset":offset}))),
                Err("invalid_request")
            );
        }
    }
    // #725: check alone takes a nonempty, duplicate-free status filter.
    for status in [
        json!(["deviates"]),
        json!(["deviates", "unknown"]),
        json!(["conforms", "deviates", "unknown"]),
    ] {
        assert_eq!(
            validate(
                "trellis_rules_check",
                Some(&json!({"schemaVersion":1,"status":status}))
            ),
            Ok(())
        );
    }
    for status in [
        json!(null),
        json!("deviates"),
        json!([]),
        json!(["violates"]),
        json!(["deviates", "deviates"]),
        json!([1]),
    ] {
        assert_eq!(
            validate(
                "trellis_rules_check",
                Some(&json!({"schemaVersion":1,"status":status}))
            ),
            Err("invalid_request"),
            "{status}"
        );
    }
    for name in ["trellis_references", "trellis_rules_explain"] {
        let mut arguments = json!({"schemaVersion":1,"status":["deviates"]});
        if name == "trellis_rules_explain" {
            arguments["rule"] = json!("$layers");
            arguments["target"] = identity.clone();
        }
        assert_eq!(validate(name, Some(&arguments)), Err("invalid_request"));
    }
    assert_eq!(
        validate(
            "trellis_rules_explain",
            Some(
                &json!({"schemaVersion":1,"rule":"$layers","target":identity,"limit":50,"offset":2})
            )
        ),
        Ok(())
    );
    assert_eq!(
        validate(
            "trellis_rules_explain",
            Some(&json!({"schemaVersion":1,"rule":"$layers","target":identity,"limit":51}))
        ),
        Err("range_too_large")
    );
    assert_eq!(
        validate(
            "trellis_rules_explain",
            Some(&json!({"schemaVersion":1,"rule":"$layers","target":identity}))
        ),
        Ok(())
    );
    assert_eq!(
        validate(
            "trellis_rules_explain",
            Some(
                &json!({"schemaVersion":1,"rule":"named","target":identity,"destination":identity})
            )
        ),
        Err("invalid_request")
    );
    assert_eq!(
        validate(
            super::super::architecture::PROMPT_NAME,
            Some(&json!({"schemaVersion":1}))
        ),
        Err("invalid_request")
    );
}

/// #818: each view takes its own target fields and nothing else.
#[test]
fn show_arguments_are_closed_per_view() {
    let n = "trellis_show";
    let ok = |v: Value| assert_eq!(validate(n, Some(&v)), Ok(()), "{v}");
    for view in ["sequence", "class"] {
        ok(json!({"schemaVersion":1,"view":view,"symbolId":sid()}));
        ok(json!({"schemaVersion":1,"view":view,"lookupKey":"src/a.rs::A","expectedBasis":pin()}));
        for v in [
            json!({"schemaVersion":1,"view":view}),
            json!({"schemaVersion":1,"view":view,"symbolId":sid(),"lookupKey":"k"}),
            json!({"schemaVersion":1,"view":view,"symbolId":sid(),"line":1}),
            json!({"schemaVersion":1,"view":view,"symbolId":sid(),"path":"a.rs"}),
            json!({"schemaVersion":1,"view":view,"symbolId":"sid:v1:short"}),
            json!({"schemaVersion":1,"view":view,"lookupKey":""}),
        ] {
            check(n, &v, "invalid_request");
        }
    }
    ok(json!({"schemaVersion":1,"view":"source","path":"src/a.rs","line":1}));
    ok(json!({"schemaVersion":1,"view":"source","path":"src/a.rs","line":9007199254740991_u64}));
    for v in [
        json!({"schemaVersion":1,"view":"source","path":"src/a.rs"}),
        json!({"schemaVersion":1,"view":"source","line":3}),
        json!({"schemaVersion":1,"view":"source","path":"src/a.rs","line":0}),
        json!({"schemaVersion":1,"view":"source","path":"src/a.rs","line":1.5}),
        json!({"schemaVersion":1,"view":"source","path":"/abs.rs","line":1}),
        json!({"schemaVersion":1,"view":"source","path":"../a.rs","line":1}),
        json!({"schemaVersion":1,"view":"source","path":"a.rs","line":1,"symbolId":sid()}),
        json!({"schemaVersion":1,"view":"source","path":"a.rs","line":1,"lookupKey":"k"}),
        json!({"schemaVersion":1,"view":"architecture","symbolId":sid()}),
        json!({"schemaVersion":1,"view":null,"symbolId":sid()}),
    ] {
        check(n, &v, "invalid_request");
    }
    let envelope = json!({"schemaVersion":1,"requestId":1,"truncated":false,
        "data":{"outcome":"no_viewer","view":"class","generation":null,"message":"m"}});
    assert_eq!(
        result(n, envelope, false)["content"][0]["text"],
        "trellis_show: no_viewer class; full result in structuredContent."
    );
}

/// #780: describe's `coverageDocuments` is an optional closed object; its bounds are
/// typed, and a bad request never falls back to a plain describe.
#[test]
fn describe_coverage_documents_request_is_closed_and_bounded() {
    let n = "trellis_workspace_describe";
    let with = |request: Value| json!({"schemaVersion":1,"coverageDocuments":request});
    for request in [
        json!({}),
        json!({"language":"rust"}),
        json!({"language":"typescript","limit":50,"offset":9007199254740991_u64}),
        json!({"limit":1,"offset":0}),
    ] {
        assert_eq!(
            validate(n, Some(&with(request.clone()))),
            Ok(()),
            "{request}"
        );
    }
    for (request, code) in [
        (json!(null), "invalid_request"),
        (json!(true), "invalid_request"),
        (json!("rust"), "invalid_request"),
        (json!({"language":"go"}), "invalid_request"),
        (json!({"language":null}), "invalid_request"),
        (json!({"state":"failed"}), "invalid_request"),
        (json!({"limit":"5"}), "invalid_request"),
        (json!({"offset":-1}), "invalid_request"),
        (json!({"limit":0}), "range_too_large"),
        (json!({"limit":51}), "range_too_large"),
    ] {
        assert_eq!(
            validate(n, Some(&with(request.clone()))),
            Err(code),
            "{request}"
        );
    }
    assert!(
        super::check(n, Some(&with(json!({"language":"go"}))))
            .unwrap_err()
            .message
            .unwrap()
            .contains("coverageDocuments")
    );
}

/// #775: references `groupBy` is closed, and `depth` is bounded and only for
/// `directory`. Check takes neither.
#[test]
fn references_group_by_is_closed_and_depth_needs_directory() {
    let n = "trellis_references";
    let with = |extra: Value| {
        let mut arguments = json!({"schemaVersion":1});
        for (key, value) in extra.as_object().unwrap() {
            arguments[key] = value.clone();
        }
        arguments
    };
    for ok in [
        json!({"groupBy":"file"}),
        json!({"groupBy":"directory"}),
        json!({"groupBy":"directory","depth":1}),
        json!({"groupBy":"directory","depth":32,"limit":50,"offset":3}),
        json!({"groupBy":"package","paths":["src/**"]}),
    ] {
        assert_eq!(validate(n, Some(&with(ok.clone()))), Ok(()), "{ok}");
    }
    for (bad, code) in [
        (json!({"groupBy":"module"}), "invalid_request"),
        (json!({"groupBy":null}), "invalid_request"),
        (json!({"groupBy":["directory"]}), "invalid_request"),
        (json!({"depth":2}), "invalid_request"),
        (json!({"groupBy":"file","depth":2}), "invalid_request"),
        (json!({"groupBy":"package","depth":2}), "invalid_request"),
        (
            json!({"groupBy":"directory","depth":"2"}),
            "invalid_request",
        ),
        (
            json!({"groupBy":"directory","depth":1.5}),
            "invalid_request",
        ),
        (json!({"groupBy":"directory","depth":0}), "range_too_large"),
        (json!({"groupBy":"directory","depth":33}), "range_too_large"),
    ] {
        assert_eq!(validate(n, Some(&with(bad.clone()))), Err(code), "{bad}");
    }
    assert_eq!(
        validate(
            "trellis_rules_check",
            Some(&json!({"schemaVersion":1,"groupBy":"directory"}))
        ),
        Err("invalid_request")
    );
}

/// #837: references `prefix` is a checkout-relative directory with every `groupBy`;
/// anything else is a typed `invalid_request` that names it. Check takes no prefix.
#[test]
fn references_prefix_is_a_relative_directory() {
    let n = "trellis_references";
    let with = |extra: Value| {
        let mut arguments = json!({"schemaVersion":1});
        for (key, value) in extra.as_object().unwrap() {
            arguments[key] = value.clone();
        }
        arguments
    };
    let longest = "a/".repeat(2047) + "b";
    assert_eq!(longest.len(), 4095);
    for ok in [
        json!({"prefix":"src"}),
        json!({"prefix":"src/store/"}),
        json!({"prefix":"src/store","groupBy":"file"}),
        json!({"prefix":"src/","groupBy":"directory","depth":2}),
        json!({"prefix":"src/store","groupBy":"package","paths":["src/**"]}),
        json!({"prefix":"a b/$x.y"}),
        json!({"prefix":longest.clone() + "/"}),
    ] {
        assert_eq!(validate(n, Some(&with(ok.clone()))), Ok(()), "{ok}");
    }
    for bad in [
        json!(""),
        json!("/"),
        json!("/src"),
        json!("src//store"),
        json!("src/../etc"),
        json!(".."),
        json!("./src"),
        json!("."),
        json!("src\\store"),
        json!("c:/src"),
        json!("src\u{0}"),
        json!("src//"),
        json!(longest.clone() + "cc"),
        json!(null),
        json!(1),
        json!(["src"]),
    ] {
        let rejection = super::check(n, Some(&with(json!({"prefix":bad.clone()})))).unwrap_err();
        assert_eq!(rejection.code, "invalid_request", "{bad}");
        assert!(rejection.message.unwrap().contains("`prefix`"), "{bad}");
    }
    assert_eq!(
        validate(
            "trellis_rules_check",
            Some(&json!({"schemaVersion":1,"prefix":"src"}))
        ),
        Err("invalid_request")
    );
}
