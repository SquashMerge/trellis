//! Independent W2 ordering model, using an in-memory search-table fixture.
use super::*;
use proptest::{
    prelude::*,
    test_runner::{Config, RngSeed, TestRunner},
};

const REVISION: &str = "pin:v1:test:1";
const SEED: u64 = 0x607_f17d_0ade;

#[derive(Clone, Debug)]
struct Row {
    id: String,
    name: Option<String>,
    test: bool,
    selected: bool,
}

fn fixture(rows: &[Row]) -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE native_version_declarations(vid INTEGER,syntax_id BLOB,name TEXT,role TEXT);
         CREATE INDEX native_version_declarations_search_name ON native_version_declarations(name COLLATE BINARY,syntax_id,vid);
         CREATE INDEX native_version_declarations_search_folded ON native_version_declarations(lower(name),name COLLATE BINARY,syntax_id,vid);
         CREATE TABLE document_versions(vid INTEGER PRIMARY KEY,id TEXT);
         CREATE TABLE revision_documents(revision_id TEXT,vid INTEGER);
         CREATE INDEX revision_documents_version ON revision_documents(vid,revision_id);
         INSERT INTO document_versions VALUES(1,'selected'),(2,'retained');
         INSERT INTO revision_documents VALUES('pin:v1:test:1',1);
         CREATE VIRTUAL TABLE native_declaration_trigrams USING fts5(name,content='',columnsize=0,tokenize='trigram case_sensitive 1');",
    ).unwrap();
    for (i, row) in rows.iter().enumerate() {
        let rowid = i as i64 + 1;
        // Encoding is only the storage adapter; the model retains the original ID.
        let id = super::super::id_codec::encode_sid(&row.id).unwrap();
        db.execute(
            "INSERT INTO native_version_declarations(rowid,vid,syntax_id,name,role) VALUES(?1,?2,?3,?4,?5)",
            rusqlite::params![rowid, if row.selected { 1 } else { 2 }, id, row.name, if row.test { "test" } else { "production" }],
        ).unwrap();
        db.execute(
            "INSERT INTO native_declaration_trigrams(rowid,name) VALUES(?1,lower(?2))",
            rusqlite::params![rowid, row.name],
        )
        .unwrap();
    }
    db
}

// Rust's ASCII folding and string/Option ordering independently express SQLite
// lower/instr and BINARY, with null first. The canonical ASCII ID ordering is
// identical to the encoded 16-byte ID ordering used by the fixture.
fn reference(rows: &[Row], query: &str) -> Vec<(String, String)> {
    let q = query.to_ascii_lowercase();
    let mut matches: Vec<_> = rows
        .iter()
        .filter_map(|r| {
            if !r.selected {
                return None;
            }
            let folded = r.name.as_ref().map(|n| n.to_ascii_lowercase());
            let tier = match folded.as_deref() {
                Some(n) if n == q => 0,
                Some(n) if n.starts_with(&q) => 1,
                Some(n) if n.contains(&q) => 2,
                _ if r.id.to_ascii_lowercase().contains(&q) => 2,
                _ => return None,
            };
            Some((tier, r.test, r.name.as_deref(), r.id.as_str()))
        })
        .collect();
    matches.sort_unstable();
    matches
        .into_iter()
        .map(|(_, _, _, id)| ("selected".into(), id.into()))
        .collect()
}

fn check(rows: &[Row], query: &str, limit: usize) {
    let db = fixture(rows);
    let all = reference(rows, query);
    let expected: Vec<_> = all.iter().take(limit + 1).cloned().collect();
    let lookahead = (limit + 1) as i64;
    let compare = |label: &str, hits: Vec<(String, String)>| {
        assert_eq!(
            hits, expected,
            "seed={SEED:x} path={label} query={query:?} limit={limit} rows={rows:?}"
        );
        assert_eq!(
            hits.iter().take(limit).collect::<Vec<_>>(),
            all.iter().take(limit).collect::<Vec<_>>()
        );
        assert_eq!(
            hits.len() > limit,
            all.len() > limit,
            "has_more seed={SEED:x} {label} {query:?} {limit}"
        );
    };
    compare(
        "dispatch",
        w2_hits(&db, REVISION, query, lookahead).unwrap(),
    );
    compare(
        "literal",
        with_literal_scan(|| w2_hits(&db, REVISION, query, lookahead)).unwrap(),
    );
    if match_expression(query).is_some() {
        let before = ORDERED_SEARCHES.get();
        FORCE_ORDERED.set(true);
        let hits = w2_hits(&db, REVISION, query, lookahead).unwrap();
        FORCE_ORDERED.set(false);
        assert_eq!(ORDERED_SEARCHES.get(), before + 1);
        compare("forced ordered", hits);
    }
}

fn row(n: usize, name: Option<String>, test: bool, selected: bool) -> Row {
    Row {
        id: format!("sid:v1:{:032x}", ((n as u128 + 1) << 16) | 0xabc),
        name,
        test,
        selected,
    }
}

#[test]
fn witness_match_tier_then_stored_role_then_binary_name_and_id() {
    let rows = vec![
        row(0, Some("OCA".into()), true, true),
        row(1, Some("ocaZ".into()), false, true),
        row(2, Some("ocaA".into()), true, true),
        row(3, Some("ocaZ".into()), false, true),
        row(4, Some("OCA".into()), false, false),
    ];
    let ids: Vec<_> = reference(&rows, "oca")
        .into_iter()
        .map(|(_, id)| id)
        .collect();
    assert_eq!(ids, [0, 1, 3, 2].map(|i| rows[i].id.clone()));
    for limit in [1, 2, 3, 4, 5, 50] {
        check(&rows, "oca", limit);
    }
}

#[test]
fn seeded_generated_order_matches_independent_reference() {
    use std::cell::Cell;
    let counts = [const { Cell::new(0usize) }; 7];
    let queries = ["oc", "oca", "abc", "SID", "x\"y", "Éab", "%_"];
    let strategy = (
        prop::sample::select(queries.to_vec()),
        prop::collection::vec((0u8..8, any::<bool>(), any::<bool>(), any::<bool>()), 0..24),
    );
    let mut runner = TestRunner::new(Config {
        cases: 128,
        rng_seed: RngSeed::Fixed(SEED),
        failure_persistence: None,
        ..Config::default()
    });
    runner
        .run(&strategy, |(query, specs)| {
            let index = queries.iter().position(|q| *q == query).unwrap();
            counts[index].set(counts[index].get() + 1);
            // Deliberately interacting ranks and roles in every case; the random
            // suffix supplies duplicate names, nulls, revisions and extra ties.
            let names = [
                query.to_ascii_uppercase(),
                query.to_ascii_lowercase(),
                format!("{}Z", query.to_ascii_lowercase()),
                format!("{}A", query.to_ascii_uppercase()),
                format!("a_{query}"),
                format!("z_{query}"),
                "unrelated".to_owned(),
            ];
            let mut rows = vec![
                row(0, Some(names[0].clone()), true, true),
                row(1, Some(names[1].clone()), false, true),
                row(2, Some(names[2].clone()), false, true),
                row(3, Some(names[3].clone()), true, true),
                row(4, Some(names[4].clone()), false, true),
                row(5, Some(names[5].clone()), true, true),
                row(6, None, false, true),
                row(7, Some(names[0].clone()), false, false),
            ];
            for (i, (kind, test, selected, duplicate)) in specs.into_iter().enumerate() {
                let name = if kind == 7 {
                    None
                } else {
                    Some(names[if duplicate { 2 } else { kind as usize }].clone())
                };
                rows.push(row(i + 8, name, test, selected));
            }
            let n = reference(&rows, query).len();
            prop_assert!(n >= 6, "generator must exercise all name tiers");
            for limit in [
                1,
                2,
                3,
                n.saturating_sub(1).min(50),
                n.min(50),
                (n + 1).min(50),
                50,
            ] {
                check(&rows, query, limit);
            }
            Ok(())
        })
        .unwrap();
    let counts = counts.map(|count| count.get());
    assert!(counts.iter().all(|count| *count > 0), "{counts:?}");
    eprintln!("seed={SEED:x} query case counts {queries:?}: {counts:?}");
}

#[test]
fn store_find_reports_actual_limit_and_has_more() {
    use crate::native_evidence::DeclarationRole;
    use std::sync::{Arc, atomic::AtomicBool};
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("sample.js"),
        "function OCA() {}\nfunction ocaZ() {}\nfunction ocaA() {}\nfunction z_oca() {}\n",
    )
    .unwrap();
    let store = super::super::Store::open_for_tests(state.path(), root.path()).unwrap();
    crate::index_coordinator::reconcile_workspace(
        &store,
        &crate::indexer::IndexOptions::new(root.path().to_owned()),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let read = store.evidence_response().unwrap();
    // Enumerate the small published inventory using the common ID prefix;
    // the model independently filters and sorts it for each query below.
    let inventory = read.mcp_find("sid:v1:", 50, None).unwrap();
    assert!(!inventory.has_more);
    let rows: Vec<_> = inventory
        .symbols
        .into_iter()
        .map(|s| Row {
            id: s.declaration.syntax_id,
            name: s.declaration.name,
            test: s.declaration.role == DeclarationRole::Test,
            selected: true,
        })
        .collect();
    assert!(
        rows.iter()
            .filter(|r| r
                .name
                .as_deref()
                .is_some_and(|n| n.to_ascii_lowercase().contains("oca")))
            .count()
            >= 4
    );
    for query in ["oc", "oca", "absent"] {
        let expected = reference(&rows, query);
        for limit in [1, 2, 3, 4, 5, 50] {
            let found = read.mcp_find(query, limit, None).unwrap();
            let ids: Vec<_> = found
                .symbols
                .into_iter()
                .map(|s| s.declaration.syntax_id)
                .collect();
            assert_eq!(
                ids,
                expected
                    .iter()
                    .take(limit)
                    .map(|(_, id)| id.clone())
                    .collect::<Vec<_>>(),
                "{query} limit={limit}"
            );
            assert_eq!(
                found.has_more,
                expected.len() > limit,
                "{query} limit={limit}"
            );
        }
    }
    read.finish(()).unwrap();
}
