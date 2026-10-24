//! Decision 0013: W2 and the browser/CLI symbol search answer through the
//! trigram prefilter exactly as through the literal scan.
use crate::store::Store;
use crate::store::symbol_trigrams::with_literal_scan;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Names in mixed case, with non-ASCII letters (some with uppercase forms
/// that ASCII `lower` leaves alone), and enough `fill` names to truncate.
/// `d.js` holds five names starting with each all-hex trigram in
/// `HEX_GROUPS`: those queries can also match random IDs, ranked after the
/// five name-prefix hits.
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
    let hex: String = HEX_GROUPS
        .iter()
        .flat_map(|group| (0..5).map(move |n| format!("function {group}{n}() {{}}\n")))
        .collect();
    std::fs::write(workspace.path().join("d.js"), hex).unwrap();
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

const HEX_GROUPS: &[&str] = &[
    "abc", "bad", "cab", "dad", "fab", "ace", "bed", "cad", "dab", "fed", "add", "bee", "caf",
    "dec", "eba", "fac",
];

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

/// A malformed trigram table fails without a literal fallback and closes
/// subsequent admission until the exceptional index has been recreated.
#[test]
fn a_corrupt_trigram_table_is_store_unavailable_without_fallback() {
    let (_state, workspace, store, pin) = fixture();
    store.corrupt_symbol_trigrams_for_tests().unwrap();
    let read = store.evidence_response().unwrap();
    let error = read
        .mcp_find_with_lines("alpha", 20, Some(pin))
        .map_err(|error| read.report_index_failure(error))
        .err()
        .unwrap();
    assert!(
        !crate::store::cli_read_retryable_contention(&error),
        "{error:#}"
    );
    assert_eq!(super::evidence_failure_code(&error), "store_unavailable");
    drop(read);
    let next = store.evidence_response();
    assert!(
        next.is_err(),
        "corrupt FTS read did not close admission: {error:#}"
    );
    drop(next);
    let error = store
        .symbols_at("al", 50)
        .expect_err("even a short query must wait for index recovery");
    assert!(
        !crate::store::cli_read_retryable_contention(&error),
        "{error:#}"
    );
    let (rebuilt, _) = crate::index_coordinator::reconcile_workspace(
        &store,
        &crate::indexer::IndexOptions::new(workspace.path().to_owned()),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let read = store.evidence_response().unwrap();
    assert!(
        !read
            .mcp_find_with_lines("alpha", 20, Some(rebuilt))
            .unwrap()
            .symbols
            .is_empty()
    );
    read.finish(()).unwrap();
    assert!(!store.symbols_at("alpha", 50).unwrap().1.is_empty());
    assert!(!store.symbols_at("al", 50).unwrap().1.is_empty());
}

/// ID-shaped queries: the hex groups, slices of real IDs (hex only, across
/// the `sid:v1:` boundary, in upper case), whole IDs, and an over-long one.
fn id_queries(store: &Store, pin: crate::model::IndexPin) -> Vec<String> {
    let read = store.evidence_response().unwrap();
    let ids: Vec<String> = read
        .mcp_find("sid:v1:", 50, Some(pin))
        .unwrap()
        .symbols
        .iter()
        .map(|s| s.declaration.syntax_id.clone())
        .collect();
    read.finish(()).unwrap();
    let mut queries: Vec<String> = HEX_GROUPS.iter().map(|g| g.to_string()).collect();
    for (n, id) in ids.iter().take(12).enumerate() {
        let start = 7 + n * 2;
        queries.push(id[start..start + 4].to_string());
        queries.push(id[start..start + 3].to_ascii_uppercase());
        queries.push(id[4..12 + n].to_string());
        queries.push(id[..10].to_ascii_uppercase());
        queries.push(id.clone());
        queries.push(format!("{id}0"));
    }
    queries
}

/// Byte equality for ID-shaped queries: ID-only hits, results that mix name
/// and ID hits, and limits where the ID-scan shortcut does and does not fire.
#[test]
fn id_shaped_queries_are_identical_with_and_without_the_shortcut() {
    use crate::store::symbol_trigrams::id_scans;
    let (_state, workspace, store, pin) = fixture();
    let id = json!(9);
    let (mut id_only, mut mixed, mut fired, mut scanned) = (0, 0, 0, 0);
    for query in id_queries(&store, pin) {
        let query = query.as_str();
        let lowered = query.to_ascii_lowercase();
        for limit in [4, 5, 20, 50] {
            let before = id_scans();
            let read = store.evidence_response().unwrap();
            let found = read.mcp_find_with_lines(query, limit, Some(pin)).unwrap();
            let names: Vec<bool> = found
                .symbols
                .iter()
                .map(|s| {
                    let d = &s.symbol.declaration;
                    let by_name = d
                        .name
                        .as_deref()
                        .is_some_and(|n| n.to_ascii_lowercase().contains(&lowered));
                    assert!(by_name || d.syntax_id.contains(&lowered), "{query:?}");
                    by_name
                })
                .collect();
            let envelope =
                super::projection::find(&id, found, workspace.path(), false, true).unwrap();
            read.finish(()).unwrap();
            if query.chars().count() >= 3 {
                if id_scans() == before {
                    fired += 1
                } else {
                    scanned += 1
                }
            }
            id_only += names.iter().filter(|by_name| !**by_name).count();
            mixed += usize::from(names.contains(&true) && names.contains(&false));
            let literal = with_literal_scan(|| {
                let read = store.evidence_response().unwrap();
                let found = read.mcp_find_with_lines(query, limit, Some(pin)).unwrap();
                let envelope =
                    super::projection::find(&id, found, workspace.path(), false, true).unwrap();
                read.finish(()).unwrap();
                envelope
            });
            assert_eq!(
                serde_json::to_vec(&envelope).unwrap(),
                serde_json::to_vec(&literal).unwrap(),
                "W2 query {query:?} limit {limit}"
            );
        }
        for limit in [5, 6, 50, 150] {
            let answer = || serde_json::to_vec(&store.symbols_at(query, limit).unwrap().1).unwrap();
            assert_eq!(
                answer(),
                with_literal_scan(answer),
                "browser query {query:?} limit {limit}"
            );
        }
    }
    // Not vacuous: ID-only hits and mixed results occur, and the shortcut
    // both fires (limit 4: five name-prefix hits fill limit+1) and does not.
    assert!(id_only > 0 && mixed > 0, "id_only={id_only} mixed={mixed}");
    assert!(fired > 0 && scanned > 0, "fired={fired} scanned={scanned}");
    // At limit 5 a group's sixth hit (the lookahead) can only come from an ID.
    let read = store.evidence_response().unwrap();
    let truncated_by_an_id = HEX_GROUPS
        .iter()
        .filter(|group| read.mcp_find(group, 5, Some(pin)).unwrap().has_more)
        .count();
    read.finish(()).unwrap();
    assert!(truncated_by_an_id > 0);
}
