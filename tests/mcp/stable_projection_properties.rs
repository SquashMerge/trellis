//! Exploratory #593 projection histories. The model is the rendered byte ranges and written
//! paths, independently of the index's IDs or key-derivation implementation.
use super::*;
use std::collections::BTreeSet;

#[derive(Debug)]
struct Case {
    path: &'static str,
    ty: &'static str,
    key_ty: &'static str,
    container: u8,
    method: &'static str,
    shift: bool,
}

fn render(c: &Case, duplicate: bool, edit: bool) -> (String, Vec<(u64, u64)>, String) {
    let mut text = String::new();
    if c.shift && edit {
        text.push_str("impl Other { fn unrelated() {} }\n");
    }
    let container = match c.container {
        0 => "",
        1 => {
            text.push_str("mod inner {\n");
            "inner::"
        }
        _ => {
            text.push_str("fn outer() { assert!({\n");
            "outer::"
        }
    };
    let mut ranges = Vec::new();
    for i in 0..=usize::from(duplicate) {
        let header = if i == 0 {
            format!("impl {} {{ ", c.ty)
        } else {
            format!("impl Trait for {} {{ ", c.ty)
        };
        text.push_str(&header);
        let start = text.len() as u64;
        text.push_str(&format!(
            "fn {}() {{ let _ = {}; }}",
            c.method,
            usize::from(edit)
        ));
        ranges.push((start, text.len() as u64));
        text.push_str(" }\n");
    }
    match c.container {
        1 => text.push_str("}\n"),
        2 => text.push_str("true }); }\n"),
        _ => {}
    }
    (
        text,
        ranges,
        format!("{}::{container}{}::{}", c.path, c.key_ty, c.method),
    )
}

async fn history(c: &Case, serial: u64) {
    let mut sources = Vec::new();
    // The three observations each serve the same progressively published history,
    // including the intermediate ambiguity, in a private in-process MCP session.
    for (phase, (duplicate, edit)) in [(false, false), (true, true), (false, true)]
        .into_iter()
        .enumerate()
    {
        let (source, expected, key) = render(c, duplicate, edit);
        sources.push(source);
        let specs: Vec<_> = sources
            .iter()
            .map(|s| graph_fixtures::Spec::syntax_only(&[(c.path, s)]))
            .collect();
        let mut mcp = GraphMcp::start(
            810000 + serial * 3 + phase as u64,
            &specs,
            Default::default(),
        )
        .await;
        let found = mcp.call_tool(
            "trellis_find_symbols",
            json!({"schemaVersion":1,"lookupKey":key,"limit":1}),
        );
        let data = &found["data"];
        let symbols = data["symbols"].as_array().unwrap();
        assert_eq!(
            symbols.len(),
            expected.len(),
            "case={c:?} phase={phase} source={}",
            sources.last().unwrap()
        );
        let actual: BTreeSet<_> = symbols
            .iter()
            .map(|s| {
                assert_eq!(s["stableLookupKey"], key);
                assert_eq!(emitted_document(data, s)["document"]["path"], c.path);
                (
                    s["range"]["start"].as_u64().unwrap(),
                    s["range"]["end"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            actual,
            expected.iter().copied().collect(),
            "case={c:?} phase={phase}"
        );
        assert_eq!(found["truncated"], false);
        for tool_name in LOOKUP_ROOT_TOOLS {
            let by_key = mcp.call_tool(tool_name, json!({"schemaVersion":1,"lookupKey":key}));
            if duplicate {
                let candidate = &by_key["data"];
                assert_eq!(candidate["lookupKey"], key);
                assert_eq!(
                    candidate["candidates"], data["symbols"],
                    "{tool_name} {c:?}"
                );
                for field in ["documents", "coverage", "scopeCoverage", "provenance"] {
                    assert_eq!(candidate[field], data[field], "{tool_name} {field} {c:?}");
                }
                for field in [
                    "declaration",
                    "calls",
                    "callers",
                    "result",
                    "symbolId",
                    "semanticBases",
                ] {
                    assert!(candidate.get(field).is_none(), "{tool_name} {field} {c:?}");
                }
                assert_eq!(by_key["truncated"], false);
            } else {
                let by_id = mcp.call_tool(
                    tool_name,
                    json!({"schemaVersion":1,"symbolId":symbols[0]["symbolId"]}),
                );
                assert_eq!(by_key["data"], by_id["data"], "{tool_name} {c:?}");
                assert_eq!(by_key["data"]["stableLookupKey"], key);
            }
        }
        mcp.finish();
    }
}

// SplitMix64: explicit seed and trace make even exploratory failures reproducible.
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e3779b97f4a7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_stable_projection_histories() {
    let count: u64 = std::env::var("TRELLIS_LOOKUP_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let seed: u64 = std::env::var("TRELLIS_LOOKUP_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20261102);
    let mut rng = seed;
    let types = [
        ("Store", "Store"),
        ("crate::data::Store<u8>", "crate::data::Store"),
        ("&'a mut data::Store<u8>", "data::Store"),
        ("::data::Store<u16>", "::data::Store"),
    ];
    let paths = ["src/lib.rs", "src::lib.rs"];
    let mut counts = [[0u64; 3]; 4];
    for serial in 0..count {
        let ti = next(&mut rng) as usize % types.len();
        let container = (next(&mut rng) % 3) as u8;
        let case = Case {
            path: paths[next(&mut rng) as usize % paths.len()],
            ty: types[ti].0,
            key_ty: types[ti].1,
            container,
            method: if next(&mut rng) % 2 == 0 {
                "run"
            } else {
                "save"
            },
            shift: next(&mut rng) % 2 == 0,
        };
        counts[ti][container as usize] += 1;
        history(&case, serial).await;
    }
    eprintln!("seed={seed} histories={count} type_by_container={counts:?}");
}
