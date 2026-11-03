//! Discovery suite: set METRICS_ARBORIST and METRICS_RCA to the two pinned binaries.
//! The generated common subset has straight-line assignments and independent ifs.
//! RCA uses different Halstead boundaries; its measurements are reported, not equated.
use proptest::prelude::*;
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs,
    path::Path,
    process::Command,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    capture::Capture, indexer::IndexOptions, model::CancelFlag, native_evidence::from_capture,
};

const TMP: &str = env!("CARGO_MANIFEST_DIR");

fn source(lang: &str, branches: &[bool]) -> (String, Vec<&'static str>, Vec<&'static str>) {
    let mut operators = Vec::new();
    let mut operands = Vec::new();
    let (prefix, suffix) = match lang {
        "rust" => ("fn calc(mut x: i32) -> i32 {\n", "    x\n}\n"),
        "javascript" => ("function calc(x) {\n", "    return x;\n}\n"),
        "python" => ("def calc(x):\n", "    return x\n"),
        "java" => (
            "class C {\n  int calc(int x) {\n",
            "    return x;\n  }\n}\n",
        ),
        _ => unreachable!(),
    };
    let mut out = prefix.to_string();
    for &branch in branches {
        if branch {
            if lang == "python" {
                out.push_str("    if x > 0:\n        x = x + 1\n");
            } else if lang == "rust" {
                out.push_str("    if x > 0 { x = x + 1; }\n");
            } else {
                out.push_str("    if (x > 0) { x = x + 1; }\n");
            }
            operators.extend(["if", ">", "=", "+"]);
            operands.extend(["x", "0", "x", "x", "1"]);
        } else {
            out.push_str(if lang == "python" {
                "    x = x + 1\n"
            } else {
                "    x = x + 1;\n"
            });
            operators.extend(["=", "+"]);
            operands.extend(["x", "x", "1"]);
        }
    }
    if lang != "rust" {
        operators.push("return");
    }
    operands.push("x");
    out.push_str(suffix);
    (out, operators, operands)
}

fn facts(
    root: &Path,
    language: &str,
    path: &str,
    text: &str,
) -> (
    Vec<trellis::native_evidence::Declaration>,
    Vec<trellis::metrics::DeclarationMetrics>,
) {
    let grammar = match language {
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        _ => unreachable!(),
    };
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&grammar).unwrap();
    assert!(
        !parser.parse(text, None).unwrap().root_node().has_error(),
        "invalid generated {language}: {text}"
    );
    fs::write(root.join(path), text).unwrap();
    let identity = trellis::store::topology::WorkspaceIdentity::discover(Some(root), root).unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(root.into()), &cancel, &|_| {}).unwrap();
    let artifact = from_capture(&capture, root, &identity.record_id, &cancel).unwrap();
    let declarations: Vec<_> = artifact
        .declarations
        .into_iter()
        .filter(|d| d.document.path == path && trellis::metrics::callable(&d.kind))
        .collect();
    let calls: Vec<_> = artifact
        .calls
        .into_iter()
        .filter(|d| d.document.path == path)
        .collect();
    let regions: Vec<_> = artifact
        .control_regions
        .into_iter()
        .filter(|d| d.document.path == path)
        .collect();
    let scores =
        trellis::metrics::compute(language, text, &declarations, &calls, &regions, &mut || {
            Ok(())
        })
        .unwrap();
    (declarations, scores)
}

fn oracle(binary: &str, args: &[&str], root: &Path) -> Value {
    let output = Command::new(binary)
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn rca_spaces<'a>(v: &'a Value, rows: &mut Vec<&'a Value>) {
    for row in v["spaces"].as_array().unwrap() {
        if row["kind"] == "function" {
            rows.push(row);
        }
        rca_spaces(row, rows);
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("PROPTEST_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(32), .. ProptestConfig::default() })]
    #[test]
    #[ignore = "requires independently built pinned oracle binaries"]
    fn generated_named_functions(branches in prop::collection::vec(any::<bool>(), 1..8)) {
        // Every case contains both classes; the suffix is independently shrinkable.
        let branches = [vec![true, false], branches].concat();
        let arborist = std::env::var("METRICS_ARBORIST").unwrap();
        let rca = std::env::var("METRICS_RCA").unwrap();
        for (lang, path) in [("rust", "input.rs"), ("javascript", "input.js"), ("python", "input.py"), ("java", "C.java")] {
            let dir = tempfile::tempdir_in(TMP).unwrap();
            let (text, operators, operands) = source(lang, &branches);
            let (declarations, scores) = facts(dir.path(), lang, path, &text);
            let ds: Vec<_> = declarations.iter().filter(|d| d.name.as_deref() == Some("calc")).collect();
            prop_assert_eq!(ds.len(), 1, "{}: {}", lang, text);
            let d = ds[0];
            let span = &text[d.range.start..d.range.end];
            let start = text[..d.range.start].bytes().filter(|b| *b == b'\n').count() + 1;
            let end = text[..d.range.end].bytes().filter(|b| *b == b'\n').count() + usize::from(!text[..d.range.end].ends_with('\n'));
            prop_assert!(span.contains("calc"));
            let score = &scores.iter().find(|s| s.sid == d.syntax_id).unwrap().metrics;
            let n = branches.iter().filter(|b| **b).count();
            prop_assert_eq!(score.cyclomatic, 1 + n, "{}: {}", lang, text);
            prop_assert_eq!(score.cognitive, n, "{}: {}", lang, text);
            prop_assert_eq!(score.sloc, span.lines().filter(|s| !s.trim().is_empty()).count(), "{}: {}", lang, text);
            let s = serde_json::to_value(score).unwrap();
            for (key, value) in [("n1", operators.iter().collect::<BTreeSet<_>>().len()), ("n2", operands.iter().collect::<BTreeSet<_>>().len()), ("N1", operators.len()), ("N2", operands.len())] {
                prop_assert_eq!(s["halstead"][key].as_u64().unwrap() as usize, value, "{}: {}: {}", lang, key, text);
            }
            let a = oracle(&arborist, &[lang, path], dir.path());
            prop_assert_eq!(a["path"].as_str(), Some(path));
            let ar: Vec<_> = a["functions"].as_array().unwrap().iter().filter(|r| r["name"] == "calc" && r["start_line"] == start && r["end_line"] == end).collect();
            prop_assert_eq!(ar.len(), 1, "arborist tuple {} {}:{}: {}", lang, start, end, a);
            for metric in ["cyclomatic", "cognitive", "sloc"] {
                prop_assert_eq!(&ar[0][metric], &s[metric], "arborist {} {}: {}", lang, metric, text);
            }
            let r = oracle(&rca, &["--metrics", "--output-format", "json", "--paths", path], dir.path());
            prop_assert_eq!(r["name"].as_str(), Some(path));
            let mut spaces = Vec::new(); rca_spaces(&r, &mut spaces);
            let rr: Vec<_> = spaces.into_iter().filter(|r| r["name"] == "calc" && r["start_line"] == start && r["end_line"] == end && r["spaces"].as_array().unwrap().is_empty()).collect();
            prop_assert_eq!(rr.len(), 1, "RCA tuple {} {}:{}: {}", lang, start, end, r);
            // RCA counts different lexical regions; record real values and their deltas,
            // while the independent generated-token model checks the v1 contract.
            for key in ["n1", "n2", "N1", "N2"] {
                let observed = rr[0]["metrics"]["halstead"][key].as_f64().unwrap();
                if std::env::var_os("METRICS_TRACE_RCA").is_some() {
                    eprintln!("RCA {lang} {key}: oracle={observed} native={}", s["halstead"][key]);
                }
                // RCA includes the signature's function and parameter identifiers;
                // v1 excludes signatures. Their names are distinct in this grammar.
                let signature_delta = match key { "n2" => Some(1.0), "N2" => Some(2.0), _ => None };
                if let Some(delta) = signature_delta {
                    prop_assert_eq!(observed, s["halstead"][key].as_f64().unwrap() + delta,
                        "RCA signature-normalized {} {}: {}", lang, key, text);
                }
            }
        }
    }

    #[test]
    fn generated_rust_macro_nested_closures(threshold in 0u8..8, add in 1u8..8) {
        // The identical nested closure occurs once normally and once in each allowlisted macro.
        // Its body has one if and one else; the enclosing closure must exclude it.
        let inner = format!("|y: i32| if y > {threshold} {{ y + {add} }} else {{ 0 }}");
        let outer = format!("|x: i32| {{ let g = {inner}; g(x) }}");
        for invocation in [format!("assert!(({outer})(1) > 0)"), format!("ensure!(({outer})(1) > 0)"), format!("vec![({outer})(1)]"), format!("format!(\"{{}}\", ({outer})(1))")] {
        let text = format!("fn calc() {{\n    let f = {outer};\n    {invocation};\n}}\n");
        let dir = tempfile::tempdir_in(TMP).unwrap();
        let (declarations, scores) = facts(dir.path(), "rust", "input.rs", &text);
        for (needle, cyclomatic, cognitive) in [(&outer, 1, 0), (&inner, 2, 4)] {
            let ranges: Vec<_> = text.match_indices(needle).map(|(i, _)| (i, i + needle.len())).collect();
            prop_assert_eq!(ranges.len(), 2);
            let mut found = Vec::new();
            for (start, end) in ranges {
                let matches: Vec<_> = declarations.iter().filter(|d| d.range.start == start && d.range.end == end && d.kind == "anonymousFunction").collect();
                prop_assert_eq!(matches.len(), 1, "missing exact closure span {}:{} in {}", start, end, text);
                let s = &scores.iter().find(|s| s.sid == matches[0].syntax_id).unwrap().metrics;
                prop_assert_eq!(s.cyclomatic, cyclomatic, "{}", text);
                prop_assert_eq!(s.cognitive, cognitive, "{}", text);
                found.push(serde_json::to_value(s).unwrap());
            }
            prop_assert_eq!(&found[0], &found[1], "macro context changed score: {}", text);
        }
        }
    }

    #[test]
    fn generated_rust_macro_named_function(threshold in 0u8..8, add in 1u8..8) {
        let function = format!("fn helper(n: i32) -> i32 {{ if n > {threshold} {{ n + {add} }} else {{ 0 }} }}");
        let text = format!("{function}\nfn calc() {{ vec![{{ {function} helper(1) }}]; }}\n");
        let dir = tempfile::tempdir_in(TMP).unwrap();
        let (decls, scores) = facts(dir.path(), "rust", "input.rs", &text);
        let mut measured = Vec::new();
        for (start, _) in text.match_indices(&function) {
            let found: Vec<_> = decls.iter().filter(|d| d.name.as_deref() == Some("helper") && d.range.start == start && d.range.end == start + function.len()).collect();
            prop_assert_eq!(found.len(), 1, "{}", text);
            let score = &scores.iter().find(|s| s.sid == found[0].syntax_id).unwrap().metrics;
            prop_assert_eq!(score.cyclomatic, 2);
            prop_assert_eq!(score.cognitive, 2);
            measured.push(serde_json::to_value(score).unwrap());
        }
        prop_assert_eq!(measured.len(), 2);
        prop_assert_eq!(&measured[0], &measured[1]);
    }

    #[test]
    fn generated_nested_lambda_ownership(threshold in 0u8..8) {
        for (lang, path) in [("javascript", "input.js"), ("python", "input.py"), ("java", "C.java")] {
            let make = |branch: bool| {
                let expr = if branch { format!("y > {threshold} ? 1 : 0") } else { "1".to_string() };
                match lang {
                    "javascript" => format!("function calc() {{ const f = x => (y => {expr})(x); }}\n"),
                    "python" => {
                        let expr = if branch { format!("1 if y > {threshold} else 0") } else { "1".to_string() };
                        format!("def calc():\n    f = lambda x: (lambda y: {expr})(x)\n")
                    },
                    "java" => format!("class C {{ void calc() {{ java.util.function.Function<Integer,Integer> f = x -> ((java.util.function.Function<Integer,Integer>)(y -> {expr})).apply(x); }} }}\n"),
                    _ => unreachable!(),
                }
            };
            let mut parents = Vec::new();
            let mut inners = Vec::new();
            for branch in [false, true] {
                let text = make(branch);
                let dir = tempfile::tempdir_in(TMP).unwrap();
                let (decls, scores) = facts(dir.path(), lang, path, &text);
                let parent: Vec<_> = decls.iter().filter(|d| d.name.as_deref() == Some("calc")).collect();
                prop_assert_eq!(parent.len(), 1, "{} {}", lang, text);
                let anonymous: Vec<_> = decls.iter().filter(|d| d.kind == "anonymousFunction").collect();
                prop_assert_eq!(anonymous.len(), 2, "{} {}", lang, text);
                let outer = anonymous.iter().min_by_key(|d| d.range.start).unwrap();
                let inner = anonymous.iter().max_by_key(|d| d.range.start).unwrap();
                prop_assert!(outer.range.start < inner.range.start && outer.range.end >= inner.range.end);
                prop_assert!(text[inner.range.start..inner.range.end].contains('y'));
                let lookup = |id: &str| &scores.iter().find(|s| s.sid == id).unwrap().metrics;
                let p = lookup(&parent[0].syntax_id);
                prop_assert_eq!(p.cyclomatic, 1);
                prop_assert_eq!(p.cognitive, 0);
                parents.push(serde_json::to_value(p).unwrap());
                let i = lookup(&inner.syntax_id);
                inners.push((i.cyclomatic, i.cognitive));
            }
            prop_assert_eq!(&parents[0], &parents[1], "nested body changed parent {}", lang);
            prop_assert_eq!(inners[0].0, 1, "{}", lang);
            prop_assert_eq!(inners[1].0, 2, "{}", lang);
            prop_assert!(inners[1].1 > inners[0].1, "{}", lang);
        }
    }
}
