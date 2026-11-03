use proptest::prelude::*;
use serde_json::{Value, json};
use std::io::{BufReader, Cursor};
use trellis::mcp::{
    catalog,
    session::{Action, Event, Session},
    tools,
    wire::{self, Frame},
};

fn cases() -> u32 {
    std::env::var("TRELLIS_INPUT_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(48)
}

fn metadata() -> Value {
    json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}})
}

#[test]
fn find_arguments_through_wire_and_session() {
    let observed = std::cell::RefCell::new([0usize; 2]);
    let mut runner = proptest::test_runner::TestRunner::new(proptest::test_runner::Config {
        cases: cases(),
        source_file: Some(file!()),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/mcp_input_properties.txt",
            ),
        )),
        ..Default::default()
    });
    runner.run(&(0u8..7, 0u8..7, 0u8..4, any::<bool>()), |(query,limit,selector,unknown)| {
        let mut args = json!({"schemaVersion":1});
        let q = match query { 0 => json!("a"), 1 => json!(""), 2 => json!("x".repeat(256)), 3 => json!("x".repeat(257)), 4 => json!("é".repeat(128)), 5 => json!(null), _ => json!(42) };
        if selector != 1 { args["query"] = q; }
        if selector == 1 || selector == 2 { args["lookupKey"] = json!("src/a::f"); }
        if selector == 3 { args.as_object_mut().unwrap().remove("query"); }
        match limit { 0 => {}, 1 => args["limit"] = json!(1), 2 => args["limit"] = json!(50), 3 => args["limit"] = json!(0), 4 => args["limit"] = json!(51), 5 => args["limit"] = json!(null), _ => args["limit"] = json!(1.5) }
        if unknown { args["unexpected"] = json!(true); }
        let message = json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"trellis_find_symbols","arguments":args,"_meta":metadata()}});
        let request = wire::decode(Frame::Line(serde_json::to_vec(&message).unwrap())).unwrap().unwrap();
        let event = Session::new().accept(request, catalog::known);
        let Event::Admitted(admission) = event else { return Err(TestCaseError::fail(format!("unexpected event {event:?}"))); };
        let Action::Call { name, arguments } = admission.action else { return Err(TestCaseError::fail("not a call")); };
        let good_selector = selector == 1 || (selector == 0 && matches!(query,0|2|4));
        let expected = good_selector && limit <= 2 && !unknown;
        observed.borrow_mut()[usize::from(expected)] += 1;
        prop_assert_eq!(tools::validate(&name, arguments.as_ref()).is_ok(), expected, "args={}", message["params"]["arguments"]);
        Ok(())
    }).unwrap();
    eprintln!("MCP observed [invalid, valid]: {:?}", observed.into_inner());
}

#[test]
fn generated_id_and_metadata_admission() {
    for count in [253, 254, 255] {
        let message = json!({"jsonrpc":"2.0", "id":"x".repeat(count), "method":"tools/list"});
        let result = wire::decode(Frame::Line(serde_json::to_vec(&message).unwrap()));
        assert_eq!(result.is_ok(), count <= 254, "string ID length {count}");
    }
    let mut runner = proptest::test_runner::TestRunner::new(proptest::test_runner::Config {
        cases: cases(),
        source_file: Some(file!()),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/mcp_input_properties.txt",
            ),
        )),
        ..Default::default()
    });
    let observed = std::cell::RefCell::new([0usize; 3]);
    runner.run(&(0u8..7, 0usize..280, 0u8..4), |(kind, count, meta)| {
        let ch = match kind { 0 => 'x', 1 => 'é', 2 => '🦀', 3 => '"', 4 => '\\', 5 => '\n', _ => '\u{1}' };
        let id = ch.to_string().repeat(count);
        // serde_json independently forms the canonical token specified by the contract.
        let token_len = serde_json::to_vec(&id).unwrap().len();
        let params = match meta { 0 => json!({"_meta":metadata()}), 1 => json!({}), 2 => json!({"_meta":metadata(),"cursor":"x"}), _ => json!({"_meta":{"io.modelcontextprotocol/protocolVersion":"wrong","io.modelcontextprotocol/clientCapabilities":{}}}) };
        let bytes = serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":"tools/list","params":params})).unwrap();
        let decoded = wire::decode(Frame::Line(bytes));
        if token_len > 256 {
            observed.borrow_mut()[0] += 1;
            let error = decoded.unwrap_err();
            prop_assert_eq!(error.code, -32600);
            prop_assert_eq!(error.id, Value::Null);
        } else {
            let mut session = Session::new();
            let event = session.accept(decoded.unwrap().unwrap(), catalog::known);
            if meta == 0 {
                observed.borrow_mut()[1] += 1;
                prop_assert!(matches!(event, Event::Admitted(_)));
            } else {
                observed.borrow_mut()[2] += 1;
                let Event::Error(error) = event else { return Err(TestCaseError::fail("invalid metadata admitted")); };
                prop_assert_eq!(error.code, if meta == 3 {-32022} else {-32602});
                prop_assert_eq!(session.pending_count(), 0);
                prop_assert_eq!(session.mode(), trellis::mcp::session::Mode::Unselected);
            }
        }
        Ok(())
    }).unwrap();
    eprintln!(
        "MCP ID/metadata observed [oversize ID, valid, invalid params]: {:?}",
        observed.into_inner()
    );
}

#[test]
fn frame_boundaries_and_drain() {
    for n in [16_383, 16_384, 16_385] {
        for capacity in [1, 7, 4096] {
            let mut bytes = vec![b' '; n];
            bytes
                .extend_from_slice(b"\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n");
            let mut reader = BufReader::with_capacity(capacity, Cursor::new(bytes));
            let frame = wire::read_frame(&mut reader).unwrap();
            if n <= 16_384 {
                assert_eq!(frame, Frame::Line(vec![b' '; n]));
            } else {
                assert_eq!(frame, Frame::Oversized);
                assert_eq!(wire::decode(frame).unwrap_err().code, -32600);
            }
            assert_eq!(
                wire::decode(wire::read_frame(&mut reader).unwrap())
                    .unwrap()
                    .unwrap()
                    .id,
                Some(json!(1))
            );
        }
    }
}
