//! #715: compact find and search, result-language `scopeCoverage` and the coverage
//! diagnostic table.
use super::*;
use crate::mcp::search::SearchResult;
use crate::search_model::{LexicalState, SearchIndexState, SearchIndexStatus};
use crate::store::mcp_read::McpFindWithLines;

const MACRO_GAP: &str = "opaque Rust macro/attribute token trees skipped; calls and definitions inside were not measured";
/// Java, Rust (mostly partial) and JavaScript tuples are selected; Python has none.
const SCOPE: [[u64; 5]; 5] = [
    [4, 4, 0, 0, 0],
    [117, 6, 111, 0, 0],
    [0; 5],
    [140, 140, 0, 0, 0],
    [0; 5],
];

/// Hits `(name, language, path, partial)`, each keyed `path::name`.
fn fixture(hits: &[(&str, &str, &str, bool)], has_more: bool) -> McpFindWithLines {
    let rows: Vec<_> = hits
        .iter()
        .map(|(name, _, path, _)| ((*name).to_owned(), "source-set:v1:test", (*path).to_owned()))
        .collect();
    let mut result = super::test_support::fixture(&rows, SCOPE, has_more);
    for (i, (symbol, (name, language, path, partial))) in
        result.symbols.iter_mut().zip(hits).enumerate()
    {
        symbol.symbol.declaration.syntax_id = format!("sid:v1:{i:032x}");
        symbol.symbol.declaration.document.language = (*language).into();
        symbol.source.document.document.language = (*language).into();
        symbol.symbol.coverage.language = (*language).into();
        if *partial {
            symbol.symbol.coverage.state = "partial".into();
            symbol.symbol.coverage.diagnostic = Some(MACRO_GAP.into());
        }
        symbol.source.stable_lookup_key = Some(format!("{path}::{name}"));
        let proof = &symbol.symbol.declaration.provenance_id;
        for row in result.provenance.iter_mut().filter(|row| &row.id == proof) {
            row.document.language = (*language).into();
        }
    }
    result
}

fn mixed() -> McpFindWithLines {
    fixture(
        &[
            ("navigation_at", "rust", "src/store.rs", true),
            ("navigation_in", "rust", "src/navigation.rs", true),
            ("navigationAt", "javascript", "web/app.js", false),
        ],
        false,
    )
}

fn search_result(symbols: McpFindWithLines) -> SearchResult {
    SearchResult {
        role: crate::search_rank::SearchRole::default(),
        matched_by: symbols
            .symbols
            .iter()
            .map(|_| vec!["lexical".into()])
            .collect(),
        symbols,
        search_index: SearchIndexStatus {
            state: SearchIndexState::Unavailable(
                crate::search_model::SearchUnavailableReason::ModelNotConfigured,
            ),
            model_id: None,
            model_name: None,
            backend: None,
            declarations: 261,
            embedded: 0,
            lexical: LexicalState::Complete,
        },
    }
}

fn root() -> &'static std::path::Path {
    std::path::Path::new("/workspace")
}

fn find_page(result: McpFindWithLines, compact: bool) -> Value {
    find(
        &json!(1),
        result,
        &Default::default(),
        root(),
        false,
        true,
        compact,
    )
    .unwrap()
    .unwrap()
}

fn search_page(result: McpFindWithLines, compact: bool) -> Value {
    search(
        &json!(1),
        search_result(result),
        &Default::default(),
        root(),
        false,
        true,
        compact,
    )
    .unwrap()
    .unwrap()
}

fn frame_bytes(tool: &str, envelope: &Value) -> usize {
    let frame = json!({"jsonrpc":"2.0","id":1,
        "result":crate::mcp::tools::result(tool, envelope.clone(), true)});
    serde_json::to_vec(&frame).unwrap().len() + 1
}

fn languages(envelope: &Value) -> Vec<&str> {
    envelope["data"]["scopeCoverage"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["language"].as_str().unwrap())
        .collect()
}

/// Compact keeps every follow-up handle (IDs, names, paths, lines, kind, role, evidence
/// tier, `stableLookupKey`) and the completeness of the full answer, and drops the
/// document table, coverage and provenance.
#[test]
fn compact_find_and_search_keep_handles_and_drop_proof_detail() {
    for (tool, items, project) in [
        (
            "trellis_find_symbols",
            "symbols",
            find_page as fn(McpFindWithLines, bool) -> Value,
        ),
        ("trellis_search_symbols", "hits", search_page),
    ] {
        let full = project(mixed(), false);
        let page = project(mixed(), true);
        let data = page["data"].as_object().unwrap();
        let mut fields: Vec<_> = data.keys().map(String::as_str).collect();
        fields.sort_unstable();
        if tool == "trellis_search_symbols" {
            assert_eq!(fields, ["hits", "role", "scopeCoverage", "searchIndex"]);
            assert_eq!(data["searchIndex"], full["data"]["searchIndex"]);
            assert_eq!(data["role"], full["data"]["role"]);
        } else {
            assert_eq!(fields, ["scopeCoverage", "symbols"]);
        }
        let shown = page["data"][items].as_array().unwrap();
        let whole = full["data"][items].as_array().unwrap();
        assert_eq!(shown.len(), 3, "{tool}");
        for (item, full_item) in shown.iter().zip(whole) {
            let mut kept: Vec<_> = item
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            kept.sort_unstable();
            let mut expected = vec![
                "evidenceTier",
                "kind",
                "lines",
                "name",
                "path",
                "role",
                "stableLookupKey",
                "symbolId",
            ];
            if tool == "trellis_search_symbols" {
                expected.extend(["matchedBy", "rank"]);
                expected.sort_unstable();
            }
            assert_eq!(kept, expected, "{tool}");
            for field in kept.iter().filter(|field| **field != "path") {
                assert_eq!(item[*field], full_item[*field], "{tool} {field}");
            }
            let doc = full_item["doc"].as_u64().unwrap() as usize;
            assert_eq!(
                item["path"],
                full["data"]["documents"][doc]["document"]["path"]
            );
            assert!(item["stableLookupKey"].as_str().unwrap().contains("::"));
        }
        for field in ["warnings", "partial", "truncated", "truncationReason"] {
            assert_eq!(page[field], full[field], "{tool} {field}");
        }
        assert_eq!(page["data"]["scopeCoverage"], full["data"]["scopeCoverage"]);
        assert_eq!(page["partial"], true, "compact never hides incompleteness");
        assert!(frame_bytes(tool, &page) * 2 < frame_bytes(tool, &full));
        super::size_tests::assert_catalog_valid(tool, &[&full, &page]);
    }
    // An exact lookup-key find takes compact as well.
    let lookup = find_lookup(
        &json!(1),
        fixture(&[("navigation_at", "rust", "src/store.rs", true)], false),
        &Default::default(),
        root(),
        false,
        true,
        true,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        lookup["data"]["symbols"][0]["stableLookupKey"],
        "src/store.rs::navigation_at"
    );
    assert!(lookup["data"].get("documents").is_none());
    super::size_tests::assert_catalog_valid("trellis_find_symbols", &[&lookup]);
}

/// Full and compact `scopeCoverage` list only the languages of the emitted items, in
/// #22 order; with no items, the searched languages with selected tuples. Completeness
/// still covers the whole searched scope.
#[test]
fn scope_coverage_lists_only_the_result_languages() {
    for compact in [false, true] {
        let javascript = find_page(
            fixture(
                &[("navigationAt", "javascript", "web/app.js", false)],
                false,
            ),
            compact,
        );
        assert_eq!(languages(&javascript), ["javascript"]);
        assert_eq!(
            javascript["data"]["scopeCoverage"][0],
            language_status(&SCOPE, &Default::default())[3]
        );
        // The Rust scope is partial although no Rust row is shown.
        assert_eq!(javascript["partial"], true);
        assert_eq!(javascript["warnings"][0]["code"], "coverageIncomplete");
        assert_eq!(
            languages(&find_page(mixed(), compact)),
            ["rust", "javascript"]
        );
        assert_eq!(
            languages(&search_page(mixed(), compact)),
            ["rust", "javascript"]
        );
        let empty = find_page(fixture(&[], false), compact);
        assert_eq!(languages(&empty), ["java", "rust", "javascript"]);
        assert_eq!(empty["partial"], true);
        let empty = search_page(fixture(&[], false), compact);
        assert_eq!(languages(&empty), ["java", "rust", "javascript"]);
        super::size_tests::assert_catalog_valid("trellis_find_symbols", &[&javascript]);
    }
    // Lookup alternatives for the other tools keep all five rows.
    let candidates = lookup_candidates(
        &json!(1),
        "trellis_declaration",
        "src/a.rs::a",
        mixed(),
        &Default::default(),
        root(),
        false,
        true,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        languages(&candidates),
        ["java", "rust", "python", "javascript", "typescript"]
    );
}

/// TypeScript follows the #715 rule: a result lists only its own languages, in #22 order,
/// and with no items the searched languages that have selected tuples.
#[test]
fn typescript_scope_rows_follow_the_715_rule() {
    let typescript = |hits: &[(&str, &str, &str, bool)]| {
        let mut result = fixture(hits, false);
        result.scope[4] = [5, 5, 0, 0, 0];
        result
    };
    for compact in [false, true] {
        let only = find_page(
            typescript(&[("Model", "typescript", "src/model.ts", false)]),
            compact,
        );
        assert_eq!(languages(&only), ["typescript"]);
        assert_eq!(
            only["data"]["scopeCoverage"][0],
            language_status(&typescript(&[]).scope, &Default::default())[4]
        );
        let both = find_page(
            typescript(&[
                ("helper", "typescript", "src/util.ts", false),
                ("helper", "javascript", "web/util.js", false),
            ]),
            compact,
        );
        assert_eq!(languages(&both), ["javascript", "typescript"]);
        let searched = search_page(
            typescript(&[("Model", "typescript", "src/model.ts", false)]),
            compact,
        );
        assert_eq!(languages(&searched), ["typescript"]);
        let empty = find_page(typescript(&[]), compact);
        assert_eq!(
            languages(&empty),
            ["java", "rust", "javascript", "typescript"]
        );
        super::size_tests::assert_catalog_valid("trellis_find_symbols", &[&only, &both, &empty]);
        super::size_tests::assert_catalog_valid("trellis_search_symbols", &[&searched]);
    }
}

/// Path order fights language order here (`src/a.ts` sorts before `src/b.js` by bytes),
/// so only the #22 language rank puts JavaScript first.
#[test]
fn javascript_sorts_before_typescript() {
    let hits = [
        ("a", "typescript", "src/a.ts", false),
        ("b", "javascript", "src/b.js", false),
    ];
    let result = fixture(&hits, false);
    let typescript = document_key(&result.symbols[0].source.document);
    let javascript = document_key(&result.symbols[1].source.document);
    assert!(javascript < typescript);
    assert_eq!(language_rank("typescript"), 4);
    assert!(language_rank("javascript") < language_rank("typescript"));
    let page = find_page(result, false);
    let documents = page["data"]["documents"].as_array().unwrap();
    assert_eq!(documents[0]["ref"], 0);
    assert_eq!(documents[0]["document"]["language"], "javascript");
    assert_eq!(documents[0]["document"]["path"], "src/b.js");
    assert_eq!(documents[1]["document"]["language"], "typescript");
    let coverage: Vec<&str> = page["data"]["coverage"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["language"].as_str().unwrap())
        .collect();
    assert_eq!(coverage, ["javascript", "typescript"]);
    let lookup = lookup_candidates(
        &json!(1),
        "trellis_declaration",
        "src/a.ts::a",
        fixture(&hits, false),
        &Default::default(),
        root(),
        false,
        true,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        lookup["data"]["documents"][0]["document"]["language"],
        "javascript"
    );
}

/// PR #767 review: the empty prefix lists every searched language, a hit only its own.
/// A large unrelated Rust row that overflows the empty frame must not reject a
/// JavaScript hit that fits; a genuinely zero-hit answer still keeps the Rust row and,
/// too large, is `range_too_large`.
#[test]
fn a_large_unrelated_scope_row_does_not_reject_a_fitting_hit() {
    let mut tiers: Tiers = Default::default();
    // Rows are bounded since #930; an oversized note stands in for any large row.
    tiers[1].note = Some("n".repeat(crate::mcp::wire::RESPONSE_BYTES));
    let rust_row = serde_json::to_vec(&language_status(&SCOPE, &tiers)[1])
        .unwrap()
        .len();
    assert!(rust_row > crate::mcp::wire::RESPONSE_BYTES, "{rust_row}");
    let hit = || {
        fixture(
            &[("navigationAt", "javascript", "web/app.js", false)],
            false,
        )
    };
    for compact in [false, true] {
        let found = find(&json!(1), hit(), &tiers, root(), false, true, compact)
            .unwrap()
            .expect("the JavaScript hit fits");
        assert_eq!(found["data"]["symbols"].as_array().unwrap().len(), 1);
        assert_eq!(languages(&found), ["javascript"]);
        assert_eq!(found["truncated"], false);
        assert_eq!(found["partial"], true, "the Rust scope is still partial");
        let searched = search(
            &json!(1),
            search_result(hit()),
            &tiers,
            root(),
            false,
            true,
            compact,
        )
        .unwrap()
        .expect("the JavaScript hit fits");
        assert_eq!(searched["data"]["hits"].as_array().unwrap().len(), 1);
        assert_eq!(languages(&searched), ["javascript"]);
        let empty = find(
            &json!(1),
            fixture(&[], false),
            &tiers,
            root(),
            false,
            true,
            compact,
        );
        assert!(
            empty.unwrap().is_none(),
            "zero hits keep the oversized Rust row"
        );
    }
}

/// #930: during catch-up a language row counts its projected documents by read status
/// instead of listing them, so more documents than fit in a response as the #620 list
/// leave find, search and describe answering, even with zero hits.
#[test]
fn catch_up_rows_count_document_statuses_instead_of_listing_them() {
    let status = |i: usize| match i % 7 {
        0 => "catchingUp",
        1 => "needsReimport",
        _ => "olderBasis",
    };
    let mut tiers: Tiers = Default::default();
    tiers[1].documents = (0..700)
        .map(|i| {
            serde_json::from_value(json!({"producerId":"rust-analyzer","language":"rust",
            "path":format!("src/projected/module_{i:04}.rs"),
            "projection":{"currentPin":{"indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":2},
                "semanticBasisPin":{"indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":1},
                "basisNativeRevisionId":"revision:v2:historical","semanticStatus":status(i)}}))
            .unwrap()
        })
        .collect();
    let listed = serde_json::to_vec(&tiers[1].documents).unwrap().len();
    assert!(listed > crate::mcp::wire::RESPONSE_BYTES, "{listed}");
    let counts = json!({"olderBasis":500,"catchingUp":100,"needsReimport":100});
    let rows = language_status(&SCOPE, &tiers);
    assert_eq!(rows[1]["semantic"]["documentStatus"], counts);
    assert!(rows[1]["semantic"].get("documents").is_none());
    let zero = json!({"olderBasis":0,"catchingUp":0,"needsReimport":0});
    assert_eq!(rows[0]["semantic"]["documentStatus"], zero);
    for compact in [false, true] {
        for hits in [
            &[("navigationAt", "javascript", "web/app.js", false)][..],
            &[],
        ] {
            let found = find(
                &json!(1),
                fixture(hits, false),
                &tiers,
                root(),
                false,
                true,
                compact,
            )
            .unwrap()
            .expect("find fits during catch-up");
            // A hit lists only its own language; zero hits list every searched one.
            if hits.is_empty() {
                let rust = found["data"]["scopeCoverage"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|row| row["language"] == "rust")
                    .expect("the searched Rust scope row");
                assert_eq!(rust["semantic"]["documentStatus"], counts);
            }
            let searched = search(
                &json!(1),
                search_result(fixture(hits, false)),
                &tiers,
                root(),
                false,
                true,
                compact,
            )
            .unwrap()
            .expect("search fits during catch-up");
            assert_eq!(
                searched["data"]["hits"].as_array().unwrap().len(),
                hits.len()
            );
        }
    }
    let described = describe(
        &json!(1),
        "w",
        "ready",
        None,
        Some((SCOPE, tiers)),
        Some(json!({"indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":2})),
        false,
    );
    assert_eq!(
        described["data"]["languages"][1]["semantic"]["documentStatus"],
        counts
    );
    assert_eq!(
        described["partial"], true,
        "pending documents keep describe partial"
    );
    assert!(line_bytes(&json!(1), &described, true) < crate::mcp::wire::RESPONSE_BYTES);
}

/// Full find and search list each distinct coverage diagnostic once in
/// `coverageDiagnostics`; each coverage row refers to it by `diagnosticRef`, and
/// re-inlining gives back the native coverage rows.
#[test]
fn coverage_diagnostics_are_listed_once() {
    for (tool, project) in [
        (
            "trellis_find_symbols",
            find_page as fn(McpFindWithLines, bool) -> Value,
        ),
        ("trellis_search_symbols", search_page),
    ] {
        let page = project(mixed(), false);
        let data = &page["data"];
        assert_eq!(data["coverageDiagnostics"], json!([MACRO_GAP]));
        let rows = data["coverage"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        let mut expected: Vec<_> = mixed()
            .symbols
            .iter()
            .map(|symbol| serde_json::to_value(&symbol.symbol.coverage).unwrap())
            .collect();
        expected.sort_by_key(|row| {
            (
                row["language"] != "rust",
                row["documentPath"].as_str().unwrap().to_owned(),
            )
        });
        for (row, native) in rows.iter().zip(&expected) {
            assert!(row.get("diagnostic").is_none());
            let mut inlined = row.clone();
            let reference = inlined
                .as_object_mut()
                .unwrap()
                .remove("diagnosticRef")
                .unwrap();
            inlined["diagnostic"] = match reference.as_u64() {
                Some(index) => data["coverageDiagnostics"][index as usize].clone(),
                None => Value::Null,
            };
            assert_eq!(&inlined, native, "{tool}");
        }
        assert_eq!(rows[0]["diagnosticRef"], 0);
        assert_eq!(rows[1]["diagnosticRef"], 0);
        assert_eq!(rows[2]["diagnosticRef"], Value::Null);
        super::size_tests::assert_catalog_valid(tool, &[&page]);
    }
}

/// Twenty Rust hits in twenty partial documents, the dogfood's shape: compact is under
/// a third of the full page, and the full page is smaller than before #715 (four
/// scope rows and the diagnostic inline on every coverage row).
#[test]
fn compact_symbol_pages_are_a_fraction_of_the_full_page() {
    let names: Vec<_> = (0..20)
        .map(|i| {
            (
                format!("catching_up_{i}"),
                format!("src/daemon/module_{i}.rs"),
            )
        })
        .collect();
    let hits: Vec<_> = names
        .iter()
        .map(|(name, path)| (name.as_str(), "rust", path.as_str(), true))
        .collect();
    for (tool, project) in [
        (
            "trellis_find_symbols",
            find_page as fn(McpFindWithLines, bool) -> Value,
        ),
        ("trellis_search_symbols", search_page),
    ] {
        let full = project(fixture(&hits, true), false);
        let page = project(fixture(&hits, true), true);
        let mut old = full.clone();
        let data = old["data"].as_object_mut().unwrap();
        let diagnostics = data.remove("coverageDiagnostics").unwrap();
        for row in data["coverage"].as_array_mut().unwrap() {
            let row = row.as_object_mut().unwrap();
            let reference = row.remove("diagnosticRef").unwrap();
            row.insert(
                "diagnostic".into(),
                diagnostics[reference.as_u64().unwrap() as usize].clone(),
            );
        }
        data.insert(
            "scopeCoverage".into(),
            json!(language_status(&SCOPE, &Default::default())),
        );
        let (old, full_bytes, compact) = (
            frame_bytes(tool, &old),
            frame_bytes(tool, &full),
            frame_bytes(tool, &page),
        );
        eprintln!("#715 {tool}: before {old} B, full {full_bytes} B, compact {compact} B");
        assert!(
            full_bytes + 20 * 80 < old,
            "{tool}: full {full_bytes}, old {old}"
        );
        assert!(
            compact * 3 < full_bytes,
            "{tool}: compact {compact}, full {full_bytes}"
        );
        assert!(compact < 8 * 1024, "{tool}: compact {compact}");
        assert_eq!(page["truncationReason"], "limit");
    }
}
