use trellis::metrics::{self, Accumulator, MetricsMemo, SourceInput};
use trellis::native_evidence::{Call, Declaration, DocumentKey, Header, Key, Range};

fn declaration(lang: &str, len: usize) -> Declaration {
    Declaration {
        syntax_id: "sid:v1:test".into(),
        document: DocumentKey {
            source_set_id: "set".into(),
            language: lang.into(),
            path: "example".into(),
        },
        revision_id: "rev".into(),
        kind: "function".into(),
        name: Some("calc".into()),
        lookup_key: None,
        ancestors: vec![],
        key: Key {
            kind: "function".into(),
            name: Some("calc".into()),
            signature: None,
            ordinal: 0,
        },
        range: Range { start: 0, end: len },
        name_range: None,
        header: Header {
            kind: "function".into(),
            name: Some("calc".into()),
            modifiers: vec![],
            type_parameters: vec![],
            parameters: vec![],
            result_type: None,
            bases: vec![],
        },
        provenance_id: "test".into(),
    }
}
fn fixture(lang: &str) -> &'static str {
    match lang {
        "java" => include_str!("fixtures/metrics/java/Example.java"),
        "python" => include_str!("fixtures/metrics/python/example.py"),
        "javascript" => include_str!("fixtures/metrics/javascript/example.js"),
        "rust" => include_str!("fixtures/metrics/rust/example.rs"),
        _ => unreachable!(),
    }
}
#[test]
fn four_grammars_hand_checked_branches_and_fan_in() {
    for lang in ["java", "python", "javascript", "rust"] {
        let src = fixture(lang);
        let rows = metrics::compute(
            lang,
            src,
            &[declaration(lang, src.trim_end().len())],
            &[],
            &[],
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(rows.len(), 1, "{lang}");
        assert_eq!(
            rows[0].metrics.cyclomatic,
            if lang == "rust" { 4 } else { 5 },
            "{lang}"
        );
        assert_eq!(rows[0].metrics.fan_in.value, None);
        assert_eq!(rows[0].metrics.fan_out, 0);
        assert_eq!(
            rows,
            metrics::compute(
                lang,
                src,
                &[declaration(lang, src.trim_end().len())],
                &[],
                &[],
                &mut || Ok(())
            )
            .unwrap()
        );
    }
}
#[test]
fn malformed_source_fails_even_outside_callable() {
    let text = "def calc():\n    return 1\ninvalid = (\n";
    let err = metrics::compute(
        "python",
        text,
        &[declaration("python", 24)],
        &[],
        &[],
        &mut || Ok(()),
    )
    .unwrap_err();
    assert!(err.to_string().contains("invalid_source_syntax"));
}
#[test]
fn cancelled_parse_never_enters_memo() {
    let src = fixture("rust");
    let mut memo = MetricsMemo::new(1024);
    let mut first = true;
    let input = || SourceInput {
        language: "rust",
        content_hash: "hash",
        text: src,
    };
    let err = metrics::compute_memoized(
        &mut memo,
        input(),
        &[declaration("rust", src.len())],
        &[],
        &[],
        &mut || {
            if first {
                first = false;
                Ok(())
            } else {
                anyhow::bail!("cancelled")
            }
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("cancelled"));
    assert_eq!(memo.len(), 0);
    metrics::compute_memoized(
        &mut memo,
        input(),
        &[declaration("rust", src.len())],
        &[],
        &[],
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(memo.len(), 1);
}
#[test]
fn checked_serialization_rejects_last_row_atomically() {
    let rows = vec!["a"; 100];
    let full = metrics::serialize_checked(&rows, 4096, &mut || Ok(())).unwrap();
    assert!(
        metrics::serialize_checked(&rows, full.len() - 1, &mut || Ok(()))
            .unwrap_err()
            .to_string()
            .contains("response_too_large")
    );
}

#[test]
fn completed_memo_rebinds_native_identity_and_call_sites() {
    let src = fixture("java");
    let mut memo = MetricsMemo::new(1024 * 1024);
    let mut first = declaration("java", src.trim_end().len());
    let rows = metrics::compute_memoized(
        &mut memo,
        SourceInput {
            language: "java",
            content_hash: "same",
            text: src,
        },
        &[first.clone()],
        &[],
        &[],
        &mut || Ok(()),
    )
    .unwrap();
    first.document.path = "elsewhere.java".into();
    first.syntax_id = "sid:v1:other".into();
    let call = Call {
        id: "occ:v2:test".into(),
        owner_syntax_id: first.syntax_id.clone(),
        ordinal: 0,
        document: first.document.clone(),
        revision_id: "other".into(),
        range: Range { start: 0, end: 1 },
        callee_range: None,
        spelling: Some("calc".into()),
        region_ids: vec![],
        provenance_id: "test".into(),
    };
    let rebound = metrics::compute_memoized(
        &mut memo,
        SourceInput {
            language: "java",
            content_hash: "same",
            text: src,
        },
        &[first],
        &[call],
        &[],
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(memo.len(), 1);
    assert_eq!(rows[0].metrics.fan_out, 0);
    assert_eq!(rebound[0].metrics.fan_out, 1);
    assert_eq!(rebound[0].metrics.cognitive, rows[0].metrics.cognitive);
    assert_eq!(rebound[0].path, "elsewhere.java");
    assert_eq!(rebound[0].sid, "sid:v1:other");
}
#[test]
fn declaration_fold_is_exact_and_empty_leaves_are_null() {
    let src = fixture("python");
    let score = metrics::compute(
        "python",
        src,
        &[declaration("python", src.trim_end().len())],
        &[],
        &[],
        &mut || Ok(()),
    )
    .unwrap()
    .remove(0)
    .metrics;
    let mut fold = Accumulator::default();
    assert_eq!(
        fold.project("mean", &["cyclomatic", "halstead", "fanIn"])
            .unwrap()["cyclomatic"],
        serde_json::Value::Null
    );
    assert_eq!(
        fold.project("sum", &["halstead"]).unwrap()["halstead"]["volume"],
        serde_json::Value::Null
    );
    fold.push(&score).unwrap();
    fold.push(&score).unwrap();
    assert_eq!(fold.count(), 2);
    assert_eq!(
        fold.project("sum", &["cyclomatic"]).unwrap()["cyclomatic"],
        10
    );
    assert_eq!(
        fold.project("mean", &["cyclomatic"]).unwrap()["cyclomatic"],
        5.0
    );
    assert!(fold.project("sum", &["fanIn"]).unwrap()["fanIn"]["value"].is_null());
}

#[test]
fn hand_checked_python_halstead_lines_and_derived_values() {
    let src = "def calc(x):\n    # note\n    return x + 1\n";
    let m = metrics::compute(
        "python",
        src,
        &[declaration("python", src.len())],
        &[],
        &[],
        &mut || Ok(()),
    )
    .unwrap()
    .remove(0)
    .metrics;
    assert_eq!(
        (
            m.loc,
            m.sloc,
            m.comment_lines,
            m.cyclomatic,
            m.exits.returns
        ),
        (3, 2, 1, 1, 1)
    );
    assert_eq!(
        (
            m.halstead.n1,
            m.halstead.n2,
            m.halstead.total_operators,
            m.halstead.total_operands
        ),
        (3, 3, 3, 4)
    );
    assert_eq!((m.halstead.vocabulary, m.halstead.length), (6, 7));
    assert_eq!(m.halstead.volume, 18.094738);
    assert_eq!(m.halstead.difficulty, 2.0);
    assert_eq!(m.halstead.effort, 36.189476);
    assert_eq!(m.halstead.estimated_bugs, 0.006032);
    assert_eq!(m.maintainability_index, 80.652193);
}

#[test]
fn memo_does_not_publish_after_bind_cancellation() {
    let src = fixture("python");
    let mut memo = MetricsMemo::new(1024 * 1024);
    let mut checks = 0;
    let baseline = metrics::compute(
        "python",
        src,
        &[declaration("python", src.len())],
        &[],
        &[],
        &mut || {
            checks += 1;
            Ok(())
        },
    )
    .unwrap();
    assert!(!baseline.is_empty());
    let mut observed = 0;
    let result = metrics::compute_memoized(
        &mut memo,
        SourceInput {
            language: "python",
            content_hash: "h",
            text: src,
        },
        &[declaration("python", src.len())],
        &[],
        &[],
        &mut || {
            observed += 1;
            if observed > checks {
                anyhow::bail!("cancelled")
            }
            Ok(())
        },
    );
    assert!(result.unwrap_err().to_string().contains("cancelled"));
    assert_eq!(memo.len(), 0);
}

#[test]
fn serialization_preserves_deadline_reason_at_last_row() {
    let rows = ["a", "b", "c"];
    let mut checked = 0;
    let err = metrics::serialize_checked(&rows, 1024, &mut || {
        checked += 1;
        if checked >= 3 {
            anyhow::bail!("deadline_exceeded")
        }
        Ok(())
    })
    .unwrap_err();
    assert_eq!(err.to_string(), "deadline_exceeded");
}

#[test]
fn grammar_decisions_defaults_and_exits() {
    let cases = [
        (
            "java",
            "class C { int calc(int x) { if (x > 0) return x; else return 0; } }",
            2,
            2,
            0,
        ),
        (
            "python",
            "def calc(x):\n    match x:\n        case 1:\n            return 1\n        case _:\n            return 0\n",
            2,
            2,
            0,
        ),
        (
            "javascript",
            "function calc(x) { if (x > 0) return 1; else if (x < 0) return -1; else return 0; }",
            3,
            3,
            0,
        ),
        (
            "rust",
            "fn calc(x: i32) -> i32 { if x > 0 { return x; } else { return 0; } }",
            2,
            2,
            0,
        ),
    ];
    for (language, source, cc, returns, throws) in cases {
        let score = metrics::compute(
            language,
            source,
            &[declaration(language, source.len())],
            &[],
            &[],
            &mut || Ok(()),
        )
        .unwrap()
        .remove(0)
        .metrics;
        assert_eq!(score.cyclomatic, cc, "{language}");
        assert_eq!(score.exits.returns, returns, "{language}");
        assert_eq!(score.exits.throws, throws, "{language}");
    }
}

#[test]
fn only_unqualified_calls_are_syntactic_self_recursion() {
    let src = "def calc(x):\n    calc(x)\n    obj.calc(x)\n    calc(x)\n";
    let positions = [
        src.find("    calc(").unwrap() + 4,
        src.find("    obj.calc(").unwrap() + 8,
        src.rfind("    calc(").unwrap() + 4,
    ];
    let calls: Vec<_> = positions
        .into_iter()
        .enumerate()
        .map(|(ordinal, start)| Call {
            id: format!("occ:v2:{ordinal}"),
            owner_syntax_id: "sid:v1:test".into(),
            ordinal,
            document: declaration("python", src.len()).document,
            revision_id: "rev".into(),
            range: Range {
                start,
                end: start + 7,
            },
            callee_range: Some(Range {
                start,
                end: start + 4,
            }),
            spelling: Some("calc".into()),
            region_ids: vec![],
            provenance_id: "test".into(),
        })
        .collect();
    let m = metrics::compute(
        "python",
        src,
        &[declaration("python", src.len())],
        &calls,
        &[],
        &mut || Ok(()),
    )
    .unwrap()
    .remove(0)
    .metrics;
    assert_eq!(m.fan_out, 3);
    assert_eq!(m.cognitive, 2);
}

#[test]
fn boolean_runs_charge_each_operator_but_one_cognitive_per_run() {
    let cases = [
        (
            "java",
            "class C { boolean calc(boolean a, boolean b, boolean c) { return a && b && c; } }",
        ),
        ("python", "def calc(a, b, c):\n    return a and b and c\n"),
        ("javascript", "function calc(a,b,c) { return a && b && c; }"),
        (
            "rust",
            "fn calc(a: bool, b: bool, c: bool) -> bool { a && b && c }",
        ),
    ];
    for (lang, source) in cases {
        let m = metrics::compute(
            lang,
            source,
            &[declaration(lang, source.len())],
            &[],
            &[],
            &mut || Ok(()),
        )
        .unwrap()
        .remove(0)
        .metrics;
        assert_eq!(m.cyclomatic, 3, "{lang}");
        assert_eq!(m.cognitive, 1, "{lang}");
    }
}

#[test]
fn rust_try_and_valued_break_are_not_branches_or_labeled_jumps() {
    let src =
        "fn calc(x: Result<i32, ()>) -> Result<i32, ()> { let y = x?; loop { break y; } Ok(y) }";
    let m = metrics::compute(
        "rust",
        src,
        &[declaration("rust", src.len())],
        &[],
        &[],
        &mut || Ok(()),
    )
    .unwrap()
    .remove(0)
    .metrics;
    assert_eq!(m.cyclomatic, 2);
    assert_eq!(m.cognitive, 1);
    assert_eq!(m.exits.returns, 0);
}

#[test]
fn strings_that_resemble_comments_do_not_add_comment_lines() {
    for (lang, source) in [
        (
            "java",
            r#"class C { String calc() { return "// not comment"; } }"#,
        ),
        (
            "python",
            r##"def calc():
    return "# not comment"
"##,
        ),
        ("javascript", "function calc() { return `// not comment`; }"),
        ("rust", r#"fn calc() -> &'static str { "// not comment" }"#),
    ] {
        let m = metrics::compute(
            lang,
            source,
            &[declaration(lang, source.len())],
            &[],
            &[],
            &mut || Ok(()),
        )
        .unwrap()
        .remove(0)
        .metrics;
        assert_eq!(m.comment_lines, 0, "{lang}");
    }
}

#[test]
fn else_if_continuation_is_flat_not_nested() {
    let source =
        "function calc(x) { if (x > 0) return 1; else if (x < 0) return -1; else return 0; }";
    let score = metrics::compute(
        "javascript",
        source,
        &[declaration("javascript", source.len())],
        &[],
        &[],
        &mut || Ok(()),
    )
    .unwrap()
    .remove(0)
    .metrics;
    assert_eq!(score.cyclomatic, 3);
    assert_eq!(score.cognitive, 4);
}

#[test]
fn production_native_declarations_have_precise_ranges_and_parameters() {
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        capture::Capture, indexer::IndexOptions, model::CancelFlag, native_evidence::from_capture,
    };
    let root = tempfile::tempdir().unwrap();
    for (language, filename) in [
        ("java", "Example.java"),
        ("python", "example.py"),
        ("javascript", "example.js"),
        ("rust", "example.rs"),
    ] {
        fs::write(root.path().join(filename), fixture(language)).unwrap();
    }
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(root.path().into()), &cancel, &|_| {}).unwrap();
    let artifact = from_capture(&capture, root.path(), &identity.record_id, &cancel).unwrap();
    for (language, filename, loc, exits) in [
        ("java", "Example.java", 11, 3),
        ("python", "example.py", 11, 4),
        ("javascript", "example.js", 11, 3),
        ("rust", "example.rs", 7, 1),
    ] {
        let source = fixture(language);
        let declarations: Vec<_> = artifact
            .declarations
            .iter()
            .filter(|d| d.document.path == filename)
            .cloned()
            .collect();
        let calls: Vec<_> = artifact
            .calls
            .iter()
            .filter(|c| c.document.path == filename)
            .cloned()
            .collect();
        let regions: Vec<_> = artifact
            .control_regions
            .iter()
            .filter(|r| r.document.path == filename)
            .cloned()
            .collect();
        let rows = metrics::compute(
            language,
            source,
            &declarations,
            &calls,
            &regions,
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(rows.len(), 1, "{language}");
        let row = &rows[0];
        assert!(row.sid.starts_with("sid:v1:"), "{language}");
        assert_eq!(
            row.range.start,
            source.find("calc").unwrap()
                - if language == "java" {
                    4
                } else if language == "rust" {
                    3
                } else if language == "javascript" {
                    9
                } else {
                    4
                },
            "{language}"
        );
        assert_eq!(row.metrics.parameters, 2, "{language}");
        assert_eq!(row.metrics.loc, loc, "{language}");
        assert_eq!(
            row.metrics.cyclomatic,
            if language == "rust" { 4 } else { 5 },
            "{language}"
        );
        assert_eq!(row.metrics.exits.returns, exits, "{language}");
        assert_eq!(
            row.metrics.max_nesting,
            if language == "rust" {
                1
            } else if language == "javascript" {
                3
            } else {
                2
            },
            "{language}"
        );
    }
}

#[test]
fn native_grammar_control_cases() {
    let cases = [
        (
            "java",
            "class C { int calc(int x) { switch (x) { case 1: return 1; default: return 0; } } }",
            2,
        ),
        (
            "python",
            "def calc(x):\n    try:\n        if x:\n            return 1 if x > 1 else 0\n    except ValueError:\n        return 2\n",
            4,
        ),
        (
            "javascript",
            "function calc(x) { switch (x) { case 1: return 1; default: return 0; } }",
            2,
        ),
        (
            "rust",
            "fn calc(x: i32) -> i32 { match x { 1 => 1, _ => 0 } }",
            2,
        ),
    ];
    for (lang, source, cyclomatic) in cases {
        let row = metrics::compute(
            lang,
            source,
            &[declaration(lang, source.len())],
            &[],
            &[],
            &mut || Ok(()),
        )
        .unwrap()
        .remove(0);
        assert_eq!(row.metrics.cyclomatic, cyclomatic, "{lang}");
    }
}

#[test]
fn represented_nested_callable_is_not_scored_in_parent() {
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        capture::Capture, indexer::IndexOptions, model::CancelFlag, native_evidence::from_capture,
    };
    let source = "def calc(x):\n    def inner(y):\n        if y:\n            return 1\n        return 0\n    return inner(x)\n";
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("example.py"), source).unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(root.path().into()), &cancel, &|_| {}).unwrap();
    let artifact = from_capture(&capture, root.path(), &identity.record_id, &cancel).unwrap();
    let rows = metrics::compute(
        "python",
        source,
        &artifact.declarations,
        &artifact.calls,
        &artifact.control_regions,
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(rows.len(), 2);
    let outer = rows.iter().find(|r| r.range.start == 0).unwrap();
    let inner = rows
        .iter()
        .find(|r| r.range.start == source.find("    def inner").unwrap() + 4)
        .unwrap();
    assert_eq!(outer.metrics.cyclomatic, 1);
    assert_eq!(inner.metrics.cyclomatic, 2);
    assert_eq!(outer.metrics.fan_out, 1);
    assert_eq!(inner.metrics.fan_out, 0);
    assert_eq!(outer.metrics.parameters, 1);
    assert_eq!(inner.metrics.parameters, 1);
    assert_eq!(outer.metrics.exits.returns, 1);
    assert_eq!(inner.metrics.exits.returns, 2);
}

#[test]
fn each_numeric_leaf_aggregates_from_declarations_without_recalculation() {
    let sources = [
        "def calc(x):\n    return x\n",
        "def calc(x):\n    if x:\n        return 1\n    return 0\n",
    ];
    let scores: Vec<_> = sources
        .iter()
        .map(|source| {
            metrics::compute(
                "python",
                source,
                &[declaration("python", source.len())],
                &[],
                &[],
                &mut || Ok(()),
            )
            .unwrap()
            .remove(0)
            .metrics
        })
        .collect();
    let selected = &["cyclomatic", "maintainabilityIndex", "halstead", "fanIn"];
    let sum = metrics::aggregate(&scores, "sum", selected).unwrap();
    let max = metrics::aggregate(&scores, "max", selected).unwrap();
    let mean = metrics::aggregate(&scores, "mean", selected).unwrap();
    assert_eq!(sum["cyclomatic"], 3);
    assert_eq!(max["cyclomatic"], 2);
    assert_eq!(mean["cyclomatic"], 1.5);
    assert_eq!(
        sum["halstead"]["N1"],
        scores
            .iter()
            .map(|s| s.halstead.total_operators)
            .sum::<usize>()
    );
    let volume_sum =
        (scores.iter().map(|s| s.halstead.volume).sum::<f64>() * 1_000_000.0).round() / 1_000_000.0;
    assert_eq!(sum["halstead"]["volume"], volume_sum);
    let mi_mean = (scores.iter().map(|s| s.maintainability_index).sum::<f64>() * 500_000.0).round()
        / 1_000_000.0;
    assert_eq!(mean["maintainabilityIndex"], mi_mean);
    assert!(mean["fanIn"]["value"].is_null());
}

#[test]
fn interpolated_literal_chunks_are_operands_without_delimiter_operators() {
    for (lang, source) in [
        ("javascript", "function calc(x) { return `a${x}b`; }"),
        (
            "python",
            r#"def calc(x):
    return f"a{x}b"
"#,
        ),
    ] {
        let score = metrics::compute(
            lang,
            source,
            &[declaration(lang, source.len())],
            &[],
            &[],
            &mut || Ok(()),
        )
        .unwrap()
        .remove(0)
        .metrics;
        assert_eq!((score.halstead.n1, score.halstead.n2), (2, 4), "{lang}");
        assert_eq!(
            (
                score.halstead.total_operators,
                score.halstead.total_operands
            ),
            (2, 5),
            "{lang}"
        );
    }
}

#[test]
fn resident_memo_limit_evicts_fifo_and_never_caches_oversized_score() {
    let src = fixture("rust");
    let declaration = declaration("rust", src.len());
    let mut tiny = MetricsMemo::new(1);
    for _ in 0..2 {
        metrics::compute_memoized(
            &mut tiny,
            SourceInput {
                language: "rust",
                content_hash: "a",
                text: src,
            },
            std::slice::from_ref(&declaration),
            &[],
            &[],
            &mut || Ok(()),
        )
        .unwrap();
    }
    assert!(tiny.is_empty());
    let entry_bytes = 4 * std::mem::size_of::<metrics::Score>()
        + std::mem::size_of::<((String, String, String), Vec<metrics::Score>, usize)>()
        + metrics::VERSION.len()
        + "rust".len()
        + "a".len();
    let mut memo = MetricsMemo::new(entry_bytes + 8);
    for hash in ["a", "b"] {
        metrics::compute_memoized(
            &mut memo,
            SourceInput {
                language: "rust",
                content_hash: hash,
                text: src,
            },
            std::slice::from_ref(&declaration),
            &[],
            &[],
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(memo.len(), 1);
    }
}

#[test]
fn lambda_and_closure_nesting_increases_cognitive_but_not_cyclomatic() {
    let cases = [
        (
            "java",
            "class C { int calc() { java.util.function.Function<Integer,Integer> f = x -> x > 0 ? 1 : 0; return 0; } }",
            2,
        ),
        (
            "python",
            "def calc(x):\n    f = lambda y: 1 if y else 0\n    return f(x)\n",
            2,
        ),
        (
            "javascript",
            "function calc() { [1].map(x => x ? 1 : 0); }",
            2,
        ),
        (
            "rust",
            "fn calc() { let f = |x| if x { 1 } else { 0 }; }",
            3,
        ),
    ];
    for (lang, source, cognitive) in cases {
        let row = metrics::compute(
            lang,
            source,
            &[declaration(lang, source.len())],
            &[],
            &[],
            &mut || Ok(()),
        )
        .unwrap()
        .remove(0);
        assert_eq!(row.metrics.cyclomatic, 2, "{lang}");
        assert_eq!(row.metrics.cognitive, cognitive, "{lang}");
    }
}

#[test]
fn framework_arrow_callback_is_scored_separately_when_native_represents_it() {
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        capture::Capture, indexer::IndexOptions, model::CancelFlag, native_evidence::from_capture,
    };
    let source = "function calc(xs) { xs.map(x => { if (x) return x; return 0; }); return 0; }";
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("example.js"), source).unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(root.path().into()), &cancel, &|_| {}).unwrap();
    let artifact = from_capture(&capture, root.path(), &identity.record_id, &cancel).unwrap();
    let rows = metrics::compute(
        "javascript",
        source,
        &artifact.declarations,
        &artifact.calls,
        &artifact.control_regions,
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(rows.len(), 2);
    let parent = rows.iter().find(|r| r.range.start == 0).unwrap();
    let callback = rows.iter().find(|r| r.range.start != 0).unwrap();
    assert_eq!(parent.metrics.cyclomatic, 1);
    assert_eq!(parent.metrics.fan_out, 1);
    assert_eq!(callback.metrics.cyclomatic, 2);
    assert_eq!(callback.metrics.exits.returns, 2);
}

#[test]
fn deadline_from_parser_and_cancellation_during_score_walk_keep_memo_empty() {
    let src = "fn calc(x: i32) -> i32 { if x > 0 { x } else { 0 } }";
    let decl = declaration("rust", src.len());
    let mut memo = MetricsMemo::new(4096);
    let mut parse_checks = 0;
    let error = metrics::compute_memoized(
        &mut memo,
        SourceInput {
            language: "rust",
            content_hash: "parse",
            text: src,
        },
        std::slice::from_ref(&decl),
        &[],
        &[],
        &mut || {
            parse_checks += 1;
            if parse_checks == 2 {
                anyhow::bail!("deadline_exceeded")
            }
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "deadline_exceeded");
    assert!(parse_checks >= 2);
    assert!(memo.is_empty());
    let mut checks_without_scoring = 0;
    metrics::compute("rust", src, &[], &[], &[], &mut || {
        checks_without_scoring += 1;
        Ok(())
    })
    .unwrap();
    let mut score_checks = 0;
    let error = metrics::compute_memoized(
        &mut memo,
        SourceInput {
            language: "rust",
            content_hash: "walk",
            text: src,
        },
        std::slice::from_ref(&decl),
        &[],
        &[],
        &mut || {
            score_checks += 1;
            if score_checks == checks_without_scoring + 2 {
                anyhow::bail!("cancelled")
            }
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "cancelled");
    assert!(memo.is_empty());
}

#[test]
fn checked_serialization_rejects_cancellation_at_final_check() {
    let rows = vec!["value"; 10];
    let mut baseline = 0;
    metrics::serialize_checked(&rows, 4096, &mut || {
        baseline += 1;
        Ok(())
    })
    .unwrap();
    let mut checks = 0;
    let error = metrics::serialize_checked(&rows, 4096, &mut || {
        checks += 1;
        if checks == baseline {
            anyhow::bail!("cancelled")
        }
        Ok(())
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "cancelled");
    assert_eq!(checks, baseline);
}

#[test]
fn java_javascript_rust_halstead_raw_and_derived_are_hand_checked() {
    let cases = [
        (
            "java",
            "class C { int calc(int x) { return x + 1; } }",
            "int calc",
            (3, 3, 4, 4),
        ),
        (
            "javascript",
            "function calc(x) { return x + 1; }",
            "function calc",
            (3, 3, 3, 4),
        ),
        ("rust", "fn calc() { return 1; }", "fn calc", (2, 2, 2, 2)),
    ];
    for (lang, source, begin, expected) in cases {
        let mut decl = declaration(lang, source.len());
        decl.range.start = source.find(begin).unwrap();
        if lang == "java" {
            decl.range.end = source.find("; }").unwrap() + 3;
        }
        let score = metrics::compute(lang, source, &[decl], &[], &[], &mut || Ok(()))
            .unwrap()
            .remove(0)
            .metrics;
        assert_eq!(
            (
                score.halstead.n1,
                score.halstead.n2,
                score.halstead.total_operators,
                score.halstead.total_operands
            ),
            expected,
            "{lang}"
        );
        assert_eq!(score.loc, 1, "{lang}");
        let h = &score.halstead;
        let length = expected.2 + expected.3;
        let vocabulary = expected.0 + expected.1;
        assert_eq!(
            h.volume,
            ((length as f64 * (vocabulary as f64).log2()) * 1e6).round() / 1e6,
            "{lang}"
        );
        assert_eq!(
            h.difficulty,
            (expected.0 as f64 / 2.0 * expected.3 as f64 / expected.1 as f64 * 1e6).round() / 1e6,
            "{lang}"
        );
        assert_eq!(
            h.effort,
            (h.difficulty * h.volume * 1e6).round() / 1e6,
            "{lang}"
        );
        assert_eq!(
            h.estimated_bugs,
            (h.volume / 3000.0 * 1e6).round() / 1e6,
            "{lang}"
        );
        let mi = ((171.0 - 5.2 * h.volume.max(1.0).ln() - 0.23 * score.cyclomatic as f64) * 100.0
            / 171.0)
            .clamp(0.0, 100.0);
        assert_eq!(
            score.maintainability_index,
            (mi * 1e6).round() / 1e6,
            "{lang}"
        );
    }
}

#[test]
fn java_annotation_is_excluded_from_halstead_without_dropping_method() {
    let plain = "class C { int calc() { return 1; } }";
    let decorated = "class C { @Deprecated int calc() { return 1; } }";
    let a = metrics::compute(
        "java",
        plain,
        &[declaration("java", plain.len())],
        &[],
        &[],
        &mut || Ok(()),
    )
    .unwrap()
    .remove(0)
    .metrics;
    let b = metrics::compute(
        "java",
        decorated,
        &[declaration("java", decorated.len())],
        &[],
        &[],
        &mut || Ok(()),
    )
    .unwrap()
    .remove(0)
    .metrics;
    assert_eq!(a.halstead, b.halstead);
    assert_eq!(a.sloc, b.sloc);
}
