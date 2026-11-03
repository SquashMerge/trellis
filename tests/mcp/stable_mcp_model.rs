//! Small independent source model checked through actual MCP requests and replies.
use super::*;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug)]
enum Form {
    Inherent,
    Generic,
    Trait,
}

#[derive(Clone, Debug)]
struct State {
    forms: Vec<Form>,
    noise: bool,
}

fn render(state: &State) -> (String, BTreeSet<(u64, u64)>) {
    let mut text = String::from("struct Foo<T>(T);\ntrait Action { fn run(); }\n");
    if state.noise {
        text.push_str("struct Other;\nimpl Other { fn extra() {} }\n");
    }
    let mut ranges = BTreeSet::new();
    for form in &state.forms {
        text.push_str(match form {
            Form::Inherent => "impl Foo<u8> { ",
            Form::Generic => "impl<T> Foo<T> { ",
            Form::Trait => "impl Action for Foo<u8> { ",
        });
        let start = text.len() as u64;
        text.push_str("fn run() { helper(); }");
        let end = text.len() as u64;
        ranges.insert((start, end));
        text.push_str(" }\n");
    }
    text.push_str("fn helper() {}\n");
    (text, ranges)
}

fn symbol_ranges(data: &Value, field: &str, path: &str, key: &str) -> BTreeSet<(u64, u64)> {
    let symbols = data[field].as_array().unwrap();
    let mut ranges = BTreeSet::new();
    for symbol in symbols {
        assert_eq!(symbol["stableLookupKey"], key);
        assert_eq!(emitted_document(data, symbol)["document"]["path"], path);
        assert_eq!(symbol["name"], "run");
        assert!(ranges.insert((
            symbol["range"]["start"].as_u64().unwrap(),
            symbol["range"]["end"].as_u64().unwrap(),
        )));
    }
    assert_eq!(ranges.len(), symbols.len());
    ranges
}

async fn observe(session: u64, path: &str, history: &[State]) -> tempfile::TempDir {
    let rendered: Vec<_> = history.iter().map(render).collect();
    let specs: Vec<_> = rendered
        .iter()
        .map(|(text, _)| graph_fixtures::Spec::syntax_only(&[(path, text)]))
        .collect();
    let mut mcp = GraphMcp::start(session, &specs, Default::default()).await;
    let expected = &rendered.last().unwrap().1;
    let key = format!("{path}::Foo::run");
    let found = mcp.call_tool(
        "trellis_find_symbols",
        json!({"schemaVersion":1,"lookupKey":key,"limit":1}),
    );
    assert_eq!(
        symbol_ranges(&found["data"], "symbols", path, &key),
        *expected,
        "history={history:?}"
    );
    assert_eq!(found["truncated"], false);
    for tool_name in LOOKUP_ROOT_TOOLS {
        let args = json!({"schemaVersion":1,"lookupKey":key});
        if expected.is_empty() {
            lookup_reply(&mut mcp, tool_name, args, Some("not_found"));
        } else {
            let by_key = mcp.call_tool(tool_name, args);
            if expected.len() == 1 {
                let sid = &found["data"]["symbols"][0]["symbolId"];
                let by_id = mcp.call_tool(tool_name, json!({"schemaVersion":1,"symbolId":sid}));
                assert_eq!(
                    by_key["data"], by_id["data"],
                    "{tool_name} history={history:?}"
                );
                assert_eq!(by_key["data"]["stableLookupKey"], key);
            } else {
                let data = &by_key["data"];
                assert_eq!(data["lookupKey"], key);
                assert_eq!(symbol_ranges(data, "candidates", path, &key), *expected);
                assert_eq!(data["candidates"], found["data"]["symbols"]);
                let fields: BTreeSet<_> = data
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect();
                assert_eq!(
                    fields,
                    [
                        "lookupKey",
                        "candidates",
                        "documents",
                        "coverage",
                        "scopeCoverage",
                        "provenance"
                    ]
                    .into_iter()
                    .collect()
                );
                assert_eq!(by_key["truncated"], false);
            }
        }
    }
    // Finish without the optional external Node schema checker: all assertions above
    // inspect actual JSON-RPC replies; no subprocess is needed for this investigation.
    let _ = mcp.release.send(());
    let thread = mcp.session.take().unwrap();
    drop(mcp.reader);
    drop(mcp.client);
    thread.join().unwrap().unwrap();
    mcp.temp
}

#[test]
fn generated_stable_lookup_protocol_histories() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut roots = Vec::new();
    let cases: usize = std::env::var("TRELLIS_STABLE_MCP_CASES")
        .ok()
        .map(|s| s.parse().unwrap())
        .unwrap_or(4);
    let mut seed: u64 = 20261102;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let forms = [Form::Inherent, Form::Generic, Form::Trait];
    let mut form_counts = [0usize; 3];
    let mut delimited_paths = 0;
    for case in 0..cases {
        let ai = (next() % 3) as usize;
        let bi = (next() % 3) as usize;
        form_counts[ai] += 1;
        form_counts[bi] += 1;
        let a = forms[ai];
        let b = forms[bi];
        let noise = next() % 2 == 0;
        let path = if next() % 2 == 0 {
            "src/lib.rs"
        } else {
            delimited_paths += 1;
            "src::lib.rs"
        };
        let history = [
            State {
                forms: vec![a],
                noise,
            },
            State {
                forms: vec![a, b],
                noise: !noise,
            },
            State {
                forms: vec![b],
                noise,
            },
            State {
                forms: vec![],
                noise: !noise,
            },
            State {
                forms: vec![a],
                noise,
            },
        ];
        for end in 1..=history.len() {
            roots.push(runtime.block_on(observe(
                10000 + (case * 5 + end) as u64,
                path,
                &history[..end],
            )));
        }
    }
    eprintln!(
        "stable MCP cases={cases} observations={} forms={form_counts:?} delimited_paths={delimited_paths} seed=20261102",
        cases * 5,
    );
    drop(runtime);
    drop(roots);
}
