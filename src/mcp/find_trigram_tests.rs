//! Decision 0013: W2 and the browser/CLI symbol search answer through the
//! trigram prefilter exactly as through the literal scan.
use crate::store::Store;
use crate::store::symbol_trigrams::with_literal_scan;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Names in mixed case, with non-ASCII letters (some with uppercase forms
/// that ASCII `lower` leaves alone), and enough `fill` names to truncate.
fn fixture() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Store,
    crate::model::IndexPin,
) {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("a.js"),
        "function AlphaBeta() {}\nfunction alphabet() {}\nfunction ALPHA() {}\n\
         function ÜNÏcode() {}\nfunction ünïcode() {}\nfunction Éaux() {}\n\
         function éaGlobal() {}\nfunction 日本語名() {}\n\
         class Shape { area() {} near() {} and() {} }\n",
    )
    .unwrap();
    let fills: String = (0..60)
        .map(|n| format!("function fill{n}() {{}}\n"))
        .collect();
    std::fs::write(workspace.path().join("b.js"), fills).unwrap();
    std::fs::write(
        workspace.path().join("c.py"),
        "def snake_case_name():\n    pass\n\nclass Upper:\n    def AND(self):\n        pass\n",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let (pin, _owner) = crate::index_coordinator::reconcile_workspace(
        &store,
        &crate::indexer::IndexOptions::new(workspace.path().to_owned()),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    (state, workspace, store, pin)
}

const QUERIES: &[&str] = &[
    // One, two and three characters; `éa` is three bytes but two characters.
    "a",
    "A",
    "ab",
    "AB",
    "é",
    "éa",
    "Éa",
    "日",
    "日本",
    "alp",
    "ALP",
    "日本語",
    "aux",
    // ASCII and mixed case, prefix, exact and substring ranks.
    "alpha",
    "ALPHA",
    "AlphaB",
    "alphabet",
    "pha",
    "Shape",
    "area",
    "rea",
    // Non-ASCII: ASCII `lower` folds only the ASCII letters.
    "ÜNÏ",
    "ünï",
    "Ünï",
    "ÜNÏCODE",
    "éau",
    "ÉAU",
    "Éaux",
    "éag",
    "本語名",
    // Embedded NUL.
    "a\0b",
    "\0ab",
    "abc\0",
    "\0",
    // FTS5 syntax is literal text.
    "\"",
    "\"a\"",
    "\"\"\"",
    "*",
    "a*",
    "al*",
    "-al",
    "AND",
    "and",
    "NEAR",
    "near",
    "NEAR(a b)",
    "(a)",
    "a OR b",
    "x:alpha",
    "^al",
    "{a}",
    "a+b",
    "a b",
    // Many hits, IDs and misses.
    "fill",
    "FILL",
    "fil",
    "ill1",
    "sid:v1:",
    "sid",
    "v1:",
    "snake_case",
    "_case_",
    "Upper",
    "absent",
    "zzz",
];

#[test]
fn w2_output_is_byte_identical_through_the_prefilter_and_the_literal_scan() {
    let (_state, workspace, store, pin) = fixture();
    let long = "a".repeat(256);
    let id = json!(7);
    let mut hits_seen = 0;
    for query in QUERIES.iter().copied().chain([long.as_str()]) {
        for limit in [20, 1, 5, 50] {
            let answer = || {
                let read = store.evidence_response().unwrap();
                let found = read.mcp_find_with_lines(query, limit, Some(pin)).unwrap();
                let envelope =
                    super::projection::find(&id, found, workspace.path(), false, true).unwrap();
                read.finish(()).unwrap();
                serde_json::to_vec(&envelope).unwrap()
            };
            let prefiltered = answer();
            let literal = with_literal_scan(answer);
            assert_eq!(
                String::from_utf8_lossy(&prefiltered),
                String::from_utf8_lossy(&literal),
                "query {query:?} limit {limit}"
            );
            let envelope: serde_json::Value = serde_json::from_slice(&prefiltered).unwrap();
            hits_seen += envelope["data"]["symbols"].as_array().unwrap().len();
            if limit == 5 && ["fill", "sid:v1:"].contains(&query) {
                assert_eq!(envelope["truncationReason"], "limit", "{query:?}");
            }
        }
    }
    // The comparison is not vacuous: these queries have hits, including the
    // non-ASCII and ID ones that only the literal rule decides.
    assert!(hits_seen > 0);
    let names = |query: &str| -> Vec<String> {
        let read = store.evidence_response().unwrap();
        let found = read.mcp_find(query, 50, Some(pin)).unwrap();
        read.finish(()).unwrap();
        found
            .symbols
            .iter()
            .map(|s| s.declaration.name.clone().unwrap_or_default())
            .collect()
    };
    assert_eq!(names("ALP"), ["ALPHA", "AlphaBeta", "alphabet"]);
    assert_eq!(names("ÜNÏ"), ["ÜNÏcode"]);
    assert_eq!(names("ünï"), ["ünïcode"]);
    assert_eq!(names("éa"), ["éaGlobal"]);
    assert_eq!(names("AND"), ["AND", "and"]);
    assert_eq!(names("fill").len(), 50);
    assert_eq!(names("sid:v1:").len(), 50);
}

#[test]
fn browser_symbols_are_identical_through_the_prefilter_and_the_literal_scan() {
    let (_state, _workspace, store, pin) = fixture();
    let long = "a".repeat(300);
    for query in QUERIES.iter().copied().chain(["", long.as_str()]) {
        for limit in [50, 1, 150] {
            let answer = || {
                let (revision, items) = store.symbols_at(query, limit).unwrap();
                assert_eq!(revision, pin);
                serde_json::to_vec(&items).unwrap()
            };
            let prefiltered = answer();
            assert_eq!(
                prefiltered,
                with_literal_scan(answer),
                "query {query:?} limit {limit}"
            );
        }
    }
    let names = |query: &str| -> Vec<String> {
        store
            .symbols_at(query, 150)
            .unwrap()
            .1
            .into_iter()
            .map(|s| s.name)
            .collect()
    };
    assert_eq!(names("ÜNÏ"), ["ÜNÏcode"]);
    assert_eq!(names("a.js"), ["a.js"]);
    assert_eq!(names("fill").len(), 60);
}

/// A malformed trigram table fails the read as `store_unavailable`. It is
/// never answered by the literal scan, which still serves short queries.
#[test]
fn a_corrupt_trigram_table_is_store_unavailable_without_fallback() {
    let (_state, _workspace, store, pin) = fixture();
    store.corrupt_symbol_trigrams_for_tests().unwrap();
    let read = store.evidence_response().unwrap();
    let error = read
        .mcp_find_with_lines("alpha", 20, Some(pin))
        .err()
        .unwrap();
    assert!(
        !crate::store::cli_read_retryable_contention(&error),
        "{error:#}"
    );
    assert_eq!(super::evidence_failure_code(&error), "store_unavailable");
    assert!(
        !read
            .mcp_find("al", 20, Some(pin))
            .unwrap()
            .symbols
            .is_empty()
    );
    read.finish(()).unwrap();
    let error = store.symbols_at("alpha", 50).err().unwrap();
    assert!(
        !crate::store::cli_read_retryable_contention(&error),
        "{error:#}"
    );
    assert_eq!(super::evidence_failure_code(&error), "store_unavailable");
    assert!(!store.symbols_at("al", 50).unwrap().1.is_empty());
}
