use proptest::prelude::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    fs,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    indexer::{IndexOptions, index_workspace_with_native},
    model::CancelFlag,
    native_ids::{IdentityRegistry, extraction_context},
    store::topology::WorkspaceIdentity,
};

// Contract-v1 canonical profile, independent of native_ids' encoder.
fn encode(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(x) => x.to_string(),
        Value::Number(x) => x.to_string(),
        Value::String(s) => {
            let mut out = String::from("\"");
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\0'..='\u{1f}' => out.push_str(&format!("\\u{:04x}", c as u32)),
                    _ => out.push(c),
                }
            }
            out.push('"');
            out
        }
        Value::Array(a) => format!("[{}]", a.iter().map(encode).collect::<Vec<_>>().join(",")),
        Value::Object(m) => {
            let sorted: BTreeMap<_, _> = m.iter().collect();
            format!(
                "{{{}}}",
                sorted
                    .into_iter()
                    .map(|(k, v)| format!("{}:{}", encode(&json!(k)), encode(v)))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
    }
}

fn full(domain: &str, value: &Value) -> String {
    let mut h = Sha256::new();
    h.update(domain.as_bytes());
    h.update(encode(value).as_bytes());
    hex::encode(h.finalize())
}
fn handle(prefix: &str, domain: &str, value: &Value) -> String {
    format!("{prefix}{}", &full(domain, value)[..32])
}
fn key(name: &str, ordinal: usize) -> Value {
    json!({"kind":"function", "name":name, "signature":null, "ordinal":ordinal})
}
fn syntax(set: &str, path: &str, name: &str, ordinal: usize) -> String {
    handle(
        "sid:v1:",
        "trellis.syntax.v1\0",
        &json!({"sourceSet":set,"path":path,"language":"javascript","ancestors":[],"declaration":key(name,ordinal)}),
    )
}
fn occurrence(
    hash: &str,
    context: &str,
    producer: &str,
    version: &str,
    owner: &str,
    kind: &str,
    ordinal: usize,
) -> String {
    handle(
        "occ:v2:",
        "trellis.occurrence.v2\0",
        &json!({"contentHash":hash,"extractionContext":context,"nativeProducerId":producer,"nativeProducerVersion":version,"ownerSyntaxId":owner,"kind":kind,"ordinal":ordinal}),
    )
}

#[derive(Clone, Debug)]
struct Decl {
    name: usize,
    body: usize,
}

type DocumentIds = Vec<(String, Vec<String>)>;

// A small repeated-name pool deliberately causes ordinal shifts. Each entry has
// one to three calls; source offsets, not IDs or names alone, identify its row.
fn render(ds: &[Decl]) -> (String, Vec<usize>) {
    let mut text = String::new();
    let mut starts = vec![];
    for d in ds {
        starts.push(text.len());
        text.push_str(&format!(
            "function {}() {{ {}return {}; }}\n",
            ["alpha", "beta"][d.name],
            "ping(); ".repeat(1 + d.body),
            d.body
        ));
    }
    (text, starts)
}

fn check_history(ops: &[(u8, u8, u8)]) -> [usize; 10] {
    // inserted, deleted, body edits, name edits, swaps, no-ops, repeated
    // document observations, modeled declarations, modeled calls, restorations
    let mut counts = [0; 10];
    let dir = tempfile::tempdir().unwrap();
    let identity = WorkspaceIdentity::discover(Some(dir.path()), dir.path()).unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let mut docs = [
        vec![
            Decl { name: 0, body: 0 },
            Decl { name: 0, body: 1 },
            Decl { name: 1, body: 0 },
        ],
        vec![Decl { name: 1, body: 0 }],
    ];
    let mut seen: BTreeMap<(usize, String), DocumentIds> = BTreeMap::new();
    let mut last = [String::new(), String::new()];
    for step in 0..=ops.len() {
        if step > 0 {
            let (op, doc, operand) = ops[step - 1];
            let v = &mut docs[usize::from(doc) % 2];
            let i = usize::from(operand) % v.len();
            match op % 5 {
                0 if v.len() < 6 => {
                    counts[0] += 1;
                    v.insert(
                        i,
                        Decl {
                            name: usize::from(operand / 2) % 2,
                            body: usize::from(operand) % 3,
                        },
                    );
                }
                1 if v.len() > 1 => {
                    counts[1] += 1;
                    v.remove(i);
                }
                2 => {
                    counts[2] += 1;
                    v[i].body = (v[i].body + 1) % 3;
                }
                3 => {
                    counts[3] += 1;
                    v[i].name = 1 - v[i].name;
                }
                4 => {
                    counts[4] += 1;
                    let j = (i + 1) % v.len();
                    v.swap(i, j);
                }
                _ => {
                    counts[5] += 1;
                }
            }
        }
        let rendered: Vec<_> = docs.iter().map(|d| render(d)).collect();
        for (i, (text, _)) in rendered.iter().enumerate() {
            fs::write(dir.path().join(format!("{i}.js")), text).unwrap();
        }
        let (_, artifact) = index_workspace_with_native(
            &IndexOptions::new(dir.path().into()),
            &identity.record_id,
            &cancel,
            |_| {},
        )
        .unwrap();
        assert_eq!(artifact.coverage.len(), 2);
        assert!(artifact.coverage.iter().all(|c| c.state == "complete"));
        let context = full(
            "trellis.extraction-context.v1\0",
            &json!({"language":"javascript","components":[]}),
        );
        for (i, (text, starts)) in rendered.iter().enumerate() {
            let path = format!("{i}.js");
            let hash = hex::encode(Sha256::digest(text.as_bytes()));
            let mut ordinals = [0usize; 2];
            let mut observed = vec![];
            for (d, start) in docs[i].iter().zip(starts) {
                counts[7] += 1;
                let rows: Vec<_> = artifact
                    .declarations
                    .iter()
                    .filter(|r| {
                        r.document.path == path && r.kind == "function" && r.range.start == *start
                    })
                    .collect();
                assert_eq!(rows.len(), 1, "step {step} ops {ops:?} source {text:?}");
                let row = rows[0];
                let name = ["alpha", "beta"][d.name];
                let expected = syntax(&artifact.source_set.id, &path, name, ordinals[d.name]);
                ordinals[d.name] += 1;
                assert_eq!(
                    row.syntax_id, expected,
                    "step {step} ops {ops:?} source {text:?}"
                );
                let mut calls: Vec<_> = artifact
                    .calls
                    .iter()
                    .filter(|c| c.document.path == path && c.owner_syntax_id == expected)
                    .collect();
                calls.sort_by_key(|c| (c.range.start, c.range.end));
                assert_eq!(calls.len(), 1 + d.body, "step {step} ops {ops:?}");
                let mut call_ids = Vec::new();
                for (ordinal, call) in calls.into_iter().enumerate() {
                    counts[8] += 1;
                    assert_eq!(call.ordinal, ordinal);
                    assert_eq!(
                        call.id,
                        occurrence(
                            &hash,
                            &context,
                            &artifact.producer.id,
                            &artifact.producer.version,
                            &expected,
                            "call",
                            ordinal
                        ),
                        "step {step} ops {ops:?}"
                    );
                    call_ids.push(call.id.clone());
                }
                observed.push((row.syntax_id.clone(), call_ids));
            }
            if let Some(previous) = seen.insert((i, text.clone()), observed.clone()) {
                counts[6] += 1;
                if last[i] != *text {
                    counts[9] += 1;
                }
                assert_eq!(
                    previous, observed,
                    "restored identical document, step {step}"
                );
            }
            last[i] = text.clone();
        }
    }
    counts
}

#[test]
fn generated_edit_histories() {
    let counts = RefCell::new([0usize; 10]);
    let config = ProptestConfig {
        cases: std::env::var("TRELLIS_NATIVE_ID_CASES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(24),
        source_file: Some(file!()),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/native-id-properties.txt",
            ),
        )),
        ..ProptestConfig::default()
    };
    let mut runner = proptest::test_runner::TestRunner::new(config);
    runner
        .run(
            &prop::collection::vec((0u8..5, 0u8..2, 0u8..12), 1..12),
            |ops| {
                for (total, count) in counts.borrow_mut().iter_mut().zip(check_history(&ops)) {
                    *total += count;
                }
                Ok(())
            },
        )
        .unwrap();
    println!(
        "history counts [insert, delete, body, name, swap, skipped, repeated document, declarations, calls, restorations]: {:?}",
        counts.borrow()
    );
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("TRELLIS_NATIVE_ID_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(24), failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::Direct("proptest-regressions/native-id-canonical.txt"))), ..ProptestConfig::default() })]
    #[test]
    fn generated_canonical_and_identity_inputs(chars in prop::collection::vec(prop_oneof![Just('a'), Just('é'), Just('\u{301}'), Just('\n'), Just('\0'), Just('"'), Just('\\'), Just('\u{2028}')], 1..12), ord in prop_oneof![0u64..10, Just(9_007_199_254_740_991)]) {
        let s: String = chars.into_iter().collect();
        let stable = json!({"sourceSet":s,"path":"src/A.java","language":"java","ancestors":[{"kind":"type","name":"A","signature":null,"ordinal":0}],"declaration":{"kind":"method","name":"run","signature":{"parameterTypes":[s],"typeParameterCount":ord,"variadic":true},"ordinal":ord}});
        let context_input = json!({"language":"java","components":[{"name":s,"hash":"a".repeat(64)}]});
        let context = full("trellis.extraction-context.v1\0", &context_input);
        prop_assert_eq!(extraction_context("java", &[(s.clone(), "a".repeat(64))]).unwrap(), context.clone());
        let mut registry = IdentityRegistry::default();
        let sid = registry.stable(&stable).unwrap();
        prop_assert_eq!(&sid, &handle("sid:v1:", "trellis.syntax.v1\0", &stable));
        let occ = json!({"contentHash":"b".repeat(64),"extractionContext":context,"nativeProducerId":s,"nativeProducerVersion":"v1","ownerSyntaxId":sid,"kind":"reference","ordinal":ord});
        prop_assert_eq!(registry.occurrence(&occ).unwrap(), handle("occ:v2:", "trellis.occurrence.v2\0", &occ));
    }
}

#[test]
fn ordinal_shift_and_restore_control() {
    let _ = check_history(&[(0, 0, 0), (1, 0, 0), (2, 1, 0), (2, 1, 0), (2, 1, 0)]);
}
