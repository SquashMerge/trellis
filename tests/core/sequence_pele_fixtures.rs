use std::sync::{Arc, atomic::AtomicBool};
use trellis::{
    behavior::{SequenceStep, build_sequence},
    indexer::{IndexOptions, index_workspace},
    model::*,
};

fn flatten<'a>(steps: &'a [SequenceStep], out: &mut Vec<&'a SequenceStep>) {
    for step in steps {
        out.push(step);
        flatten(&step.children, out);
        flatten(&step.alternate, out);
    }
}
#[test]
fn four_native_producer_snapshots() {
    let cases = [
        (
            "javascript",
            "fixture.js",
            "function run() { if (check()) { yes(); } else { no(); } after(); }",
        ),
        (
            "rust",
            "fixture.rs",
            "fn run() { if check() { yes(); } else { no(); } after(); }",
        ),
        (
            "java",
            "Fixture.java",
            "class Fixture { void run() { if (check()) { yes(); } else { no(); } after(); } }",
        ),
        (
            "python",
            "fixture.py",
            "def run():\n    if check():\n        yes()\n    else:\n        no()\n    after()\n",
        ),
    ];
    let pin = IndexPin {
        index_generation: uuid::Uuid::from_u128(0x00000000000040008000000000000001),
        index_revision: 7,
    };
    for (lang, file, source) in cases {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(file), source).unwrap();
        let graph = index_workspace(
            &IndexOptions::new(dir.path().to_owned()),
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let seed = graph.nodes.iter().find(|n| n.name == "run").unwrap();
        let view = build_sequence(pin, seed, &graph.files[0], &graph.calls, false).unwrap();
        let mut steps = vec![];
        flatten(&view.steps, &mut steps);
        for step in steps.iter().filter(|s| s.call_id.is_some()) {
            let call = graph
                .calls
                .iter()
                .find(|c| Some(&c.id) == step.call_id.as_ref())
                .unwrap();
            assert_eq!((&step.path, &step.range), (&call.path, &call.range));
        }
        // Index identity hashes include the temporary checkout root. Preserve
        // all measured structure/ranges/order but use fixed snapshot aliases
        // for only those root-dependent symbol and occurrence IDs.
        let mut snapshot = serde_json::to_value(&view).unwrap();
        snapshot["seed"]["id"] = serde_json::json!("fixture-seed");
        if !snapshot["seed"]["parent"].is_null() {
            snapshot["seed"]["parent"] = serde_json::json!("fixture-parent");
        }
        for participant in snapshot["participants"].as_array_mut().unwrap() {
            if participant["kind"] == "method" {
                participant["id"] = serde_json::json!("fixture-seed");
            }
        }
        fn stable_calls(steps: &mut [serde_json::Value], ordinal: &mut usize) {
            for step in steps {
                if !step["callId"].is_null() {
                    *ordinal += 1;
                    step["callId"] = serde_json::json!(format!("fixture-call-{ordinal}"));
                }
                stable_calls(step["children"].as_array_mut().unwrap(), ordinal);
                stable_calls(step["alternate"].as_array_mut().unwrap(), ordinal);
            }
        }
        stable_calls(snapshot["steps"].as_array_mut().unwrap(), &mut 0);
        let path = format!("tests/fixtures/sequence-pele/{lang}.json");
        let actual = serde_json::to_string_pretty(&snapshot).unwrap() + "\n";
        if std::env::var_os("TRELLIS_UPDATE_PELE_FIXTURES").is_some() {
            std::fs::write(&path, &actual).unwrap();
        }
        assert_eq!(
            actual,
            std::fs::read_to_string(path).unwrap(),
            "{lang} DTO changed"
        );
        assert!(steps.iter().any(|s| s.call_id.is_some()), "{lang}");
    }
}
