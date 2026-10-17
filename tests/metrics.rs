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
        assert_eq!(rows[0].metrics.cyclomatic, 2, "{lang}");
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
    let err = memo
        .source(
            "rust",
            "hash",
            src,
            &[declaration("rust", src.len())],
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
    memo.source(
        "rust",
        "hash",
        src,
        &[declaration("rust", src.len())],
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
    assert_eq!(rebound[0].metrics.cognitive, rows[0].metrics.cognitive + 1);
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
        4
    );
    assert_eq!(
        fold.project("mean", &["cyclomatic"]).unwrap()["cyclomatic"],
        2.0
    );
    assert!(fold.project("sum", &["fanIn"]).unwrap()["fanIn"]["value"].is_null());
}
