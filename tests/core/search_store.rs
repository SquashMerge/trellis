#![cfg(feature = "test-support")]
use rusqlite::Connection;
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    indexer::{IndexOptions, index_workspace_bundle},
    model::{CancelFlag, IndexPin},
    native_evidence as n, semantic_evidence as s,
    store::{SearchHeadChanged, Store, topology::WriterGuard},
};

fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Store, CancelFlag) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("cards.rs"),
        "fn orchard() { let fruit = \"apple\"; harvest(); }\n",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    (state, root, store, Arc::new(AtomicBool::new(false)))
}
fn db_path(state: &Path, root: &Path) -> std::path::PathBuf {
    let identity = trellis::store::topology::WorkspaceIdentity::discover(Some(root), root).unwrap();
    state
        .join("cache/indexes")
        .join(identity.root_key)
        .join("index.db")
}
fn publish(store: &Store, root: &Path, leader: &WriterGuard, cancel: &CancelFlag) -> IndexPin {
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.to_owned()),
        store.root_id(),
        cancel,
        |_| {},
    )
    .unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            leader,
            store.index_baseline().unwrap(),
            cancel,
        )
        .unwrap()
}
fn corpus(db: &Connection) -> Vec<(i64, Vec<u8>, String, f64)> {
    db.prepare(r#"SELECT vid,syntax_id,terms,bm25(head_search_fts) FROM head_search_fts WHERE head_search_fts MATCH '"orchard"' ORDER BY syntax_id"#).unwrap()
        .query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap()
        .collect::<rusqlite::Result<_>>().unwrap()
}
fn all_cards(store: &Store, pin: IndexPin) -> Vec<trellis::store::HeadSearchCard> {
    store.head_search_cards(Some(pin), None, 16).unwrap().cards
}

fn fts_rows(db: &Connection) -> BTreeMap<(i64, Vec<u8>), i64> {
    db.prepare("SELECT vid,syntax_id,rowid FROM head_search_fts ORDER BY vid,syntax_id")
        .unwrap()
        .query_map([], |row| Ok(((row.get(0)?, row.get(1)?), row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn edited_document_changes_only_its_fts_rows_and_keeps_head_bm25() {
    let (state, root, store, cancel) = fixture();
    fs::write(
        root.path().join("cards.rs"),
        "fn orchard() { apple(); } fn orchard_helper() { basket(); }\n",
    )
    .unwrap();
    fs::write(root.path().join("steady.rs"), "fn orchard_steady() {}\n").unwrap();
    fs::write(root.path().join("other.rs"), "fn orchard_other() {}\n").unwrap();
    let leader = store.publication_for_tests().unwrap();
    let first = publish(&store, root.path(), &leader, &cancel);
    let db = Connection::open(db_path(state.path(), root.path())).unwrap();
    let before = fts_rows(&db);
    assert_eq!(before.len(), 4);
    fs::write(
        root.path().join("cards.rs"),
        "fn orchard() { pear(); } fn orchard_replacement() { gather(); }\n",
    )
    .unwrap();
    let second = publish(&store, root.path(), &leader, &cancel);
    assert_ne!(first, second);
    let after = fts_rows(&db);
    let removed: Vec<_> = before
        .keys()
        .filter(|key| !after.contains_key(*key))
        .collect();
    let added: Vec<_> = after
        .keys()
        .filter(|key| !before.contains_key(*key))
        .collect();
    let retained: Vec<_> = before
        .keys()
        .filter(|key| after.contains_key(*key))
        .collect();
    assert_eq!(
        removed.len(),
        2,
        "both cards of the changed vid leave the head"
    );
    assert_eq!(added.len(), 2, "all cards of the new vid enter the head");
    assert_eq!(
        retained.len(),
        2,
        "the other document versions stay in place"
    );
    for key in retained {
        assert_eq!(
            before[key], after[key],
            "unchanged card keeps its FTS rowid"
        );
    }
    let mut expected: Vec<_> = removed
        .iter()
        .map(|key| (rusqlite::ffi::SQLITE_DELETE, before[*key]))
        .chain(
            added
                .iter()
                .map(|key| (rusqlite::ffi::SQLITE_INSERT, after[*key])),
        )
        .collect();
    let mut writes = store.take_search_fts_writes_for_tests();
    expected.sort();
    writes.sort();
    assert_eq!(
        writes, expected,
        "writes are bounded by the actual card delta"
    );
    // A fresh head-only corpus inserted in reverse order has the same scores.
    // Retained old card versions must not influence BM25 statistics.
    db.execute_batch(
        "CREATE VIRTUAL TABLE temp.expected_fts USING fts5(terms,vid UNINDEXED,syntax_id UNINDEXED,tokenize='porter unicode61 remove_diacritics 2');
         INSERT INTO expected_fts SELECT terms,vid,syntax_id FROM head_search_fts ORDER BY syntax_id DESC;",
    )
    .unwrap();
    let expected_scores: Vec<(i64, Vec<u8>, String, f64)> = db
        .prepare(r#"SELECT vid,syntax_id,terms,bm25(expected_fts) FROM expected_fts WHERE expected_fts MATCH '"orchard"' ORDER BY syntax_id"#)
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(corpus(&db), expected_scores);
    assert_eq!(all_cards(&store, second).len(), 4);
}

#[test]
fn mutation_deletion_and_retention_keep_exact_cards_and_head_only_statistics() {
    let (state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    store.set_retention_clock_for_tests(1_000, 0);
    let first = publish(&store, root.path(), &leader, &cancel);
    let original = all_cards(&store, first);
    assert_eq!(original.len(), 1);
    assert!(original[0].card_text.contains("apple"));
    let db = Connection::open(db_path(state.path(), root.path())).unwrap();
    let facts: String = db
        .query_row(
            "SELECT facts_json FROM native_version_search_cards",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let facts: serde_json::Value = serde_json::from_str(&facts).unwrap();
    assert_eq!(
        facts
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["ancestors", "facts"]
    );
    assert_eq!(facts["facts"]["strings"][0], "apple");
    let expected_hash = trellis::native_ids::digest(
        b"trellis.search-card.v1\0",
        original[0].card_text.as_bytes(),
    );
    assert_eq!(original[0].card_hash, expected_hash);
    fs::write(
        root.path().join("cards.rs"),
        "fn orchard() { let fruit = \"pear\"; gather(); }\n",
    )
    .unwrap();
    store.set_retention_clock_for_tests(1_001, 1);
    let second = publish(&store, root.path(), &leader, &cancel);
    let edited = all_cards(&store, second);
    assert_eq!(edited[0].syntax_id, original[0].syntax_id);
    assert_ne!(edited[0].card_hash, original[0].card_hash);
    assert!(edited[0].card_text.contains("pear"));
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM native_version_search_cards",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 2, "both document versions retain their own cards");
    let before_release = corpus(&db);
    assert_eq!(before_release.len(), 1);
    store.set_retention_clock_for_tests(1_901, 901);
    store.release_revision(first, &leader).unwrap();
    store.collect_unreferenced(&leader).unwrap();
    assert_eq!(corpus(&db), before_release);
    assert_eq!(all_cards(&store, second), edited);
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM native_version_search_cards",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    fs::remove_file(root.path().join("cards.rs")).unwrap();
    let third = publish(&store, root.path(), &leader, &cancel);
    assert!(all_cards(&store, third).is_empty());
    assert!(corpus(&db).is_empty());
    assert_eq!(
        store
            .retained_native_snapshot_at(second)
            .unwrap()
            .0
            .search_cards[0]
            .card
            .hash,
        edited[0].card_hash
    );
    assert!(
        store
            .head_search_cards(Some(second), None, 16)
            .unwrap_err()
            .downcast_ref::<SearchHeadChanged>()
            .is_some()
    );
}

#[test]
fn publication_rollback_keeps_card_rows_and_fts_together() {
    let (state, root, store, cancel) = fixture();
    fs::write(root.path().join("steady.rs"), "fn unchanged() {}\n").unwrap();
    fs::write(root.path().join("other.rs"), "fn untouched() {}\n").unwrap();
    let leader = store.publication_for_tests().unwrap();
    let first = publish(&store, root.path(), &leader, &cancel);
    let before = all_cards(&store, first);
    let db = Connection::open(db_path(state.path(), root.path())).unwrap();
    let before_fts = corpus(&db);
    let before_rows = fts_rows(&db);
    fs::write(
        root.path().join("cards.rs"),
        "fn orchard() { different(); }\n",
    )
    .unwrap();
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let interrupted = cancel.clone();
    store.set_publication_before_commit_hook_for_tests(move || {
        interrupted.store(true, std::sync::atomic::Ordering::Relaxed)
    });
    assert!(
        store
            .publish_native(&graph, &capture, &native, &leader, first, &cancel)
            .is_err()
    );
    cancel.store(false, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(store.index_baseline().unwrap(), first);
    assert_eq!(all_cards(&store, first), before);
    assert_eq!(corpus(&db), before_fts);
    assert_eq!(fts_rows(&db), before_rows);
    let writes = store.take_search_fts_writes_for_tests();
    let deleted: Vec<_> = writes
        .iter()
        .filter(|(operation, _)| *operation == rusqlite::ffi::SQLITE_DELETE)
        .map(|(_, rowid)| *rowid)
        .collect();
    let expected_deleted: Vec<_> = before_fts
        .iter()
        .map(|(vid, sid, _, _)| before_rows[&(*vid, sid.clone())])
        .collect();
    assert_eq!(
        deleted, expected_deleted,
        "the changed card delta was attempted"
    );
    assert_eq!(
        writes.len(),
        before_fts.len() * 2,
        "only the replacement card was inserted before rollback"
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM native_version_search_cards",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        i64::try_from(before.len()).unwrap()
    );
}

#[test]
fn captured_sidecars_are_required_but_never_added_to_evidence_json() {
    let (_state, root, store, cancel) = fixture();
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    assert!(!native.search_cards.is_empty());
    let wire = serde_json::to_value(&native).unwrap();
    assert!(wire.get("searchCards").is_none());
    assert!(wire.get("search_cards").is_none());
    let imported: n::Artifact = serde_json::from_value(wire).unwrap();
    imported
        .validate(&capture, root.path(), store.root_id(), &cancel)
        .unwrap();
    let baseline = store.index_baseline().unwrap();
    let leader = store.publication_for_tests().unwrap();
    let error = store
        .publish_native(&graph, &capture, &imported, &leader, baseline, &cancel)
        .unwrap_err();
    assert!(
        format!("{error:#}").contains(
            "native_evidence_required: eligible declaration missing captured search card"
        ),
        "{error:#}"
    );
    assert_eq!(store.index_baseline().unwrap(), baseline);
}

#[test]
fn pages_are_bounded_and_current_head_guarded() {
    let (_state, root, store, cancel) = fixture();
    fs::write(
        root.path().join("cards.rs"),
        "fn orchard() {} fn banana() {} fn coconut() {}\n",
    )
    .unwrap();
    let leader = store.publication_for_tests().unwrap();
    let first = publish(&store, root.path(), &leader, &cancel);
    let page = store.head_search_cards(None, None, 2).unwrap();
    assert_eq!(page.pin, first);
    assert_eq!(page.cards.len(), 2);
    let next = store
        .head_search_cards(Some(first), Some(&page.cards[1].syntax_id), 2)
        .unwrap();
    assert_eq!(next.cards.len(), 1);
    assert!(next.cards[0].syntax_id > page.cards[1].syntax_id);
    assert!(store.head_search_cards(Some(first), None, 17).is_err());
    assert!(store.head_search_cards(Some(first), None, 0).is_err());
    assert!(
        store
            .head_search_cards(Some(first), Some("malformed"), 1)
            .is_err()
    );
    fs::write(root.path().join("cards.rs"), "fn changed() {}\n").unwrap();
    publish(&store, root.path(), &leader, &cancel);
    assert!(
        store
            .head_search_cards(Some(first), Some(&page.cards[1].syntax_id), 2)
            .unwrap_err()
            .downcast_ref::<SearchHeadChanged>()
            .is_some()
    );
}

fn omitted_semantics(
    db: &Connection,
    native: &n::Artifact,
    capture: &trellis::capture::Capture,
    diagnostic: &str,
) -> (s::SemanticPublication, s::CapturedComponents) {
    let (toolchain,config,dependencies):(String,String,String) = db.query_row(
        "SELECT retained_toolchain,retained_config,retained_dependencies FROM native_revisions ORDER BY published_index_revision DESC LIMIT 1",
        [],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    ).unwrap();
    let toolchain: serde_json::Value = serde_json::from_str(&toolchain).unwrap();
    let config: Vec<serde_json::Value> = serde_json::from_str(&config).unwrap();
    let dependencies: Vec<serde_json::Value> = serde_json::from_str(&dependencies).unwrap();
    let producer = s::Producer {
        id: "semantic.search-store-test".into(),
        version: "v1".into(),
        executable_hash: trellis::native_ids::digest(b"", b"synthetic"),
        kind: s::SemanticKind::Semantic,
        languages: vec![s::Language::Rust],
        position_encoding: s::PositionEncoding::Utf8,
    };
    let semantic = s::SemanticPublication {
        producers: vec![producer.clone()],
        coverage: native
            .revision
            .documents
            .iter()
            .map(|d| s::Coverage {
                producer_id: producer.id.clone(),
                language: s::Language::Rust,
                source_set_id: native.source_set.id.clone(),
                document_path: d.key.path.clone(),
                revision_id: native.revision.id.clone(),
                requested: true,
                selected: false,
                state: s::CoverageState::Omitted,
                supported_roles: vec![s::Role::Definition],
                observed_roles: vec![],
                diagnostic: Some(diagnostic.into()),
                reasons: None,
            })
            .collect(),
        provenance: vec![],
        symbols: vec![],
        declaration_bindings: vec![],
        references: vec![],
        call_bindings: vec![],
        type_relationships: vec![],
    };
    let captured = s::CapturedComponents {
        producer_executables: BTreeMap::from([(producer.id.clone(), b"synthetic".to_vec())]),
        artifacts: BTreeMap::from([(
            (
                producer.id.clone(),
                trellis::native_ids::digest(b"", b"artifact"),
            ),
            b"artifact".to_vec(),
        )]),
        sources: native
            .revision
            .documents
            .iter()
            .map(|d| {
                let file = capture
                    .files
                    .iter()
                    .find(|file| file.path == d.key.path)
                    .unwrap();
                (d.key.clone(), file.text.as_bytes().to_vec())
            })
            .collect(),
        captured_revision: native.revision.clone(),
        native_toolchain: toolchain.clone(),
        native_config: config.clone(),
        native_dependencies: dependencies.clone(),
        requested_producers: vec![producer],
        toolchain: trellis::native_ids::canonical(&toolchain),
        config: trellis::native_ids::canonical(&serde_json::json!(config)),
        dependency: trellis::native_ids::canonical(&serde_json::json!(dependencies)),
        native_extraction_components: BTreeMap::new(),
    };
    (semantic, captured)
}

#[test]
fn semantic_only_same_version_preserves_exact_corpus_without_rewriting_fts() {
    let (state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let first = publish(&store, root.path(), &leader, &cancel);
    let before = all_cards(&store, first);
    let db = Connection::open(db_path(state.path(), root.path())).unwrap();
    let before_fts = corpus(&db);
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let (semantic, captured) = omitted_semantics(&db, &native, &capture, "unavailable");
    let second = store
        .publish_native_semantic(
            &graph, &capture, &native, &semantic, &captured, &leader, first, &cancel,
        )
        .unwrap();
    assert_ne!(first, second);
    assert_eq!(
        store.semantic_at(second).unwrap().coverage[0]
            .diagnostic
            .as_deref(),
        Some("unavailable")
    );
    assert_eq!(all_cards(&store, second), before);
    assert_eq!(corpus(&db), before_fts);
    assert!(store.take_search_fts_writes_for_tests().is_empty());
    assert_eq!(
        db.query_row("SELECT count(*) FROM document_versions", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM native_version_search_cards",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}

#[test]
fn streaming_selective_and_document_local_publication_refresh_head_cards() {
    for mode in ["stream", "selective", "local"] {
        let (state, root, store, cancel) = fixture();
        fs::write(root.path().join("steady.rs"), "fn unchanged() {}\n").unwrap();
        let leader = store.publication_for_tests().unwrap();
        let first = publish(&store, root.path(), &leader, &cancel);
        let before = all_cards(&store, first);
        fs::write(
            root.path().join("cards.rs"),
            "fn orchard() { let fruit = \"plum\"; prune(); }\n",
        )
        .unwrap();
        let options = IndexOptions::new(root.path().to_owned());
        let (graph, native, capture) =
            index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
        let next = match mode {
            "stream" => {
                let capture = trellis::capture::Capture::admit(&options, &cancel, &|_| {}).unwrap();
                store
                    .publish_stream_native_only_for_tests(&options, &capture, &leader, &cancel)
                    .unwrap()
            }
            "selective" => store
                .publish_native_recovery_selective_for_tests(
                    &graph, &capture, &native, &leader, &cancel,
                )
                .unwrap(),
            _ => {
                let db = Connection::open(db_path(state.path(), root.path())).unwrap();
                let (semantic, captured) = omitted_semantics(&db, &native, &capture, "omitted");
                // Local publication owns its one graph projection; admit a fresh capture.
                let capture = trellis::capture::Capture::admit(&options, &cancel, &|_| {}).unwrap();
                store
                    .publish_local_semantic_for_tests(
                        &options, &capture, &semantic, &captured, &leader, &cancel, false,
                    )
                    .unwrap()
            }
        };
        let after = all_cards(&store, next);
        assert_eq!(after.len(), 2, "{mode}");
        let changed = after.iter().find(|c| c.card_text.contains("plum")).unwrap();
        assert!(
            before
                .iter()
                .any(|c| c.syntax_id == changed.syntax_id && c.card_hash != changed.card_hash),
            "{mode}"
        );
        let unchanged = before
            .iter()
            .find(|c| c.card_text.contains("unchanged"))
            .unwrap();
        assert!(after.contains(unchanged), "{mode}");
        let db = Connection::open(db_path(state.path(), root.path())).unwrap();
        assert_eq!(corpus(&db).len(), 1, "{mode}");
        let fts_count: i64 = db
            .query_row("SELECT count(*) FROM head_search_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, 2, "{mode}");
        assert_eq!(
            db.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r
                .get::<_, i64>(
                0
            ))
            .unwrap(),
            0
        );
    }
}

#[test]
fn old_disposable_format_rebuild_discards_stale_cards_and_regenerates_fts() {
    let (state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let first = publish(&store, root.path(), &leader, &cancel);
    let expected = all_cards(&store, first);
    let path = db_path(state.path(), root.path());
    drop(leader);
    let queued = store
        .enqueue_request(&IndexOptions::new(root.path().to_owned()), None)
        .unwrap();
    drop(store);
    {
        let db = Connection::open(&path).unwrap();
        db.execute("UPDATE native_version_search_cards SET card_text='obsolete format',card_hash=?1,facts_json='{}'",["0".repeat(64)]).unwrap();
        db.execute_batch("PRAGMA ignore_check_constraints=ON; UPDATE index_metadata SET schema_version=23; PRAGMA user_version=23;").unwrap();
    }
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let (next, _session) = trellis::index_coordinator::reconcile_workspace(
        &store,
        &IndexOptions::new(root.path().to_owned()),
        &cancel,
        |_| {},
    )
    .unwrap();
    assert_ne!(next.index_generation, first.index_generation);
    assert_eq!(
        store.request_by_id(&queued.id).unwrap().unwrap().seq,
        queued.seq
    );
    assert_eq!(all_cards(&store, next), expected);
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        34
    );
    assert_eq!(corpus(&db).len(), 1);
}

#[test]
fn admitted_search_snapshot_is_head_guarded_and_hydrates_ranked_prefix_only() {
    let (_state, root, store, cancel) = fixture();
    fs::write(root.path().join("second.rs"), "fn orchard_extra() {}\n").unwrap();
    let leader = store.publication_for_tests().unwrap();
    let first = publish(&store, root.path(), &leader, &cancel);
    let read = store.evidence_response().unwrap();
    let candidates = read
        .mcp_search_candidates(
            "orchard",
            Some(first),
            &read
                .mcp_search_policy(
                    "orchard",
                    first,
                    trellis::search_rank::SearchRole::Any,
                    &mut || Ok(()),
                )
                .unwrap(),
            &mut || Ok(()),
        )
        .unwrap();
    assert_eq!(candidates.pin, first);
    assert_eq!(candidates.cards.len(), 2);
    assert_eq!(candidates.lexical.len(), 2);
    assert!(
        candidates
            .cards
            .windows(2)
            .all(|pair| pair[0].key.syntax_id.as_bytes() < pair[1].key.syntax_id.as_bytes())
    );
    let selected = read
        .mcp_search_hydrate(&candidates.lexical[..1], first, &mut || Ok(()))
        .unwrap();
    assert_eq!(selected.pin, first);
    assert_eq!(selected.scope[1], [2, 2, 0, 0, 0]);
    assert_eq!(selected.symbols.len(), 1);
    assert_eq!(selected.provenance.len(), 1);
    assert_eq!(
        selected.symbols[0].symbol.declaration.syntax_id,
        candidates.lexical[0].syntax_id
    );
    assert_eq!(selected.symbols[0].source.lines.unwrap().start, 1);
    let empty = read.mcp_search_hydrate(&[], first, &mut || Ok(())).unwrap();
    assert!(empty.symbols.is_empty());
    assert!(empty.provenance.is_empty());
    assert_eq!(empty.scope, selected.scope);
    read.finish(()).unwrap();
    drop(read);
    fs::write(root.path().join("cards.rs"), "fn changed() {}\n").unwrap();
    let second = publish(&store, root.path(), &leader, &cancel);
    let read = store.evidence_response().unwrap();
    let conflict = read
        .mcp_search_candidates(
            "orchard",
            Some(first),
            &trellis::search_rank::SearchPolicy {
                exact: BTreeMap::new(),
                role: trellis::search_rank::SearchRole::Any,
            },
            &mut || Ok(()),
        )
        .unwrap_err();
    assert!(conflict.to_string().contains("revision conflict"));
    assert_eq!(
        read.mcp_search_candidates(
            "orchard",
            Some(second),
            &read
                .mcp_search_policy(
                    "orchard",
                    second,
                    trellis::search_rank::SearchRole::Any,
                    &mut || Ok(())
                )
                .unwrap(),
            &mut || Ok(())
        )
        .unwrap()
        .cards
        .len(),
        2
    );
}

#[test]
fn termless_search_retains_scope_and_requires_fts_and_cancellation_is_typed() {
    let (_state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(&store, root.path(), &leader, &cancel);
    store
        .mcp_set_coverage_for_tests(&leader, "cards.rs", true, "partial")
        .unwrap();
    let read = store.evidence_response().unwrap();
    let candidates = read
        .mcp_search_candidates(
            "the a _",
            Some(pin),
            &read
                .mcp_search_policy(
                    "the a _",
                    pin,
                    trellis::search_rank::SearchRole::Any,
                    &mut || Ok(()),
                )
                .unwrap(),
            &mut || Ok(()),
        )
        .unwrap();
    assert!(candidates.lexical.is_empty());
    let empty = read.mcp_search_hydrate(&[], pin, &mut || Ok(())).unwrap();
    assert_eq!(empty.scope[1], [1, 0, 1, 0, 0]);
    let error = read
        .mcp_search_candidates(
            "orchard",
            Some(pin),
            &trellis::search_rank::SearchPolicy {
                exact: BTreeMap::new(),
                role: trellis::search_rank::SearchRole::Any,
            },
            &mut || anyhow::bail!("deadline_exceeded"),
        )
        .unwrap_err();
    assert_eq!(error.to_string(), "deadline_exceeded");
    let candidates = read
        .mcp_search_candidates(
            "orchard",
            Some(pin),
            &read
                .mcp_search_policy(
                    "orchard",
                    pin,
                    trellis::search_rank::SearchRole::Any,
                    &mut || Ok(()),
                )
                .unwrap(),
            &mut || Ok(()),
        )
        .unwrap();
    assert_eq!(
        candidates.lexical.len(),
        1,
        "progress handler removed on failure"
    );
    assert!(
        read.mcp_search_hydrate(&vec![candidates.cards[0].key.clone(); 51], pin, &mut || Ok(
            ()
        ))
        .is_err()
    );
    read.finish(()).unwrap();
    drop(read);
    store.corrupt_search_fts_for_tests(&leader).unwrap();
    match store.evidence_response() {
        Ok(read) => assert!(
            read.mcp_search_candidates(
                "the a _",
                Some(pin),
                &trellis::search_rank::SearchPolicy {
                    exact: BTreeMap::new(),
                    role: trellis::search_rank::SearchRole::Any
                },
                &mut || Ok(())
            )
            .is_err(),
            "termless query cannot hide required head table failure"
        ),
        Err(error) => assert!(
            error.to_string().starts_with("incompatible_index:"),
            "missing required search table must fail admission: {error:#}"
        ),
    }
}

#[test]
fn search_hydration_validates_coverage_only_for_hydrated_documents() {
    let (_state, root, store, cancel) = fixture();
    fs::write(root.path().join("other.rs"), "fn other() {}\n").unwrap();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(&store, root.path(), &leader, &cancel);
    store
        .mcp_set_coverage_for_tests(&leader, "other.rs", false, "failed")
        .unwrap();
    let read = store.evidence_response().unwrap();
    let candidates = read
        .mcp_search_candidates(
            "the a _",
            Some(pin),
            &trellis::search_rank::SearchPolicy {
                exact: BTreeMap::new(),
                role: trellis::search_rank::SearchRole::Any,
            },
            &mut || Ok(()),
        )
        .unwrap();
    assert!(candidates.lexical.is_empty());
    // #261: a defect in a document no candidate reaches does not fail hydration.
    let empty = read.mcp_search_hydrate(&[], pin, &mut || Ok(())).unwrap();
    assert!(empty.symbols.is_empty());
    assert_eq!(empty.scope[1], [1, 1, 0, 0, 1]);
    let other = read
        .mcp_search_candidates(
            "other",
            Some(pin),
            &read
                .mcp_search_policy(
                    "other",
                    pin,
                    trellis::search_rank::SearchRole::Any,
                    &mut || Ok(()),
                )
                .unwrap(),
            &mut || Ok(()),
        )
        .unwrap();
    assert_eq!(other.lexical.len(), 1);
    let error = read
        .mcp_search_hydrate(&other.lexical, pin, &mut || Ok(()))
        .map(|_| ())
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("store_unavailable: inconsistent native coverage tuple"),
        "{error:#}"
    );
}

#[test]
fn lexical_nonexact_production_bm25_then_sid_matches_head_fts() {
    let (state, root, store, cancel) = fixture();
    fs::write(
        root.path().join("cards.rs"),
        "fn orchard() {} fn orchard_extra() {} fn punctuation() {}\n",
    )
    .unwrap();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(&store, root.path(), &leader, &cancel);
    // Existing fixture reader opens only after publication and is not used for writes.
    let db = Connection::open(db_path(state.path(), root.path())).unwrap();
    let expression = trellis::search_lexical::query_expression(&db, "orchards")
        .unwrap()
        .unwrap();
    let expected: Vec<Vec<u8>> = db.prepare("SELECT syntax_id FROM head_search_fts WHERE head_search_fts MATCH ?1 ORDER BY bm25(head_search_fts),syntax_id").unwrap()
        .query_map([expression],|row| row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    let read = store.evidence_response().unwrap();
    let candidates = read
        .mcp_search_candidates(
            "orchards",
            Some(pin),
            &read
                .mcp_search_policy(
                    "orchards",
                    pin,
                    trellis::search_rank::SearchRole::Any,
                    &mut || Ok(()),
                )
                .unwrap(),
            &mut || Ok(()),
        )
        .unwrap();
    let expected: Vec<_> = expected
        .into_iter()
        .map(|bytes| format!("sid:v1:{}", hex::encode(bytes)))
        .collect();
    assert_eq!(
        candidates
            .lexical
            .iter()
            .map(|key| key.syntax_id.clone())
            .collect::<Vec<_>>(),
        expected
    );
    for query in [
        r#"" OR ( NEAR *"#,
        "orchard OR punctuation",
        "orchard -punctuation",
    ] {
        read.mcp_search_candidates(
            query,
            Some(pin),
            &read
                .mcp_search_policy(
                    query,
                    pin,
                    trellis::search_rank::SearchRole::Any,
                    &mut || Ok(()),
                )
                .unwrap(),
            &mut || Ok(()),
        )
        .unwrap();
    }
    read.finish(()).unwrap();
}

#[test]
fn exact_declaration_name_precedes_partial_name_and_body_matches() {
    let (state, root, store, cancel) = fixture();
    fs::write(
        root.path().join("cards.rs"),
        r#"/// Read request status from persistent storage, preserving queued, running,
/// completed, cancelled and failed outcomes across daemon restarts. The caller
/// receives the latest committed result together with its request identifier,
/// submission timestamp and completion details.
fn read_status_for() {}
fn request_status() { read_status_for(); }
#[test]
fn read_status_for_returns_status() { read_status_for(); }
"#,
    )
    .unwrap();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(&store, root.path(), &leader, &cancel);
    let db = Connection::open(db_path(state.path(), root.path())).unwrap();
    let roles: BTreeMap<String, String> = db
        .prepare("SELECT name,role FROM native_version_declarations WHERE name IS NOT NULL")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(roles["read_status_for"], "production");
    assert_eq!(roles["request_status"], "production");
    assert_eq!(roles["read_status_for_returns_status"], "test");
    let read = store.evidence_response().unwrap();
    let candidates = read
        .mcp_search_candidates(
            "read_status_for",
            Some(pin),
            &read
                .mcp_search_policy(
                    "read_status_for",
                    pin,
                    trellis::search_rank::SearchRole::Any,
                    &mut || Ok(()),
                )
                .unwrap(),
            &mut || Ok(()),
        )
        .unwrap();
    assert_eq!(candidates.lexical.len(), 3);
    let hydrated = read
        .mcp_search_hydrate(&candidates.lexical, pin, &mut || Ok(()))
        .unwrap();
    let names: Vec<_> = hydrated
        .symbols
        .iter()
        .map(|hit| hit.symbol.declaration.name.as_deref().unwrap())
        .collect();
    assert_eq!(
        names[0], "read_status_for",
        "an exact production declaration must lead partial-name and body matches: {names:?}"
    );
    read.finish(()).unwrap();
}

#[test]
fn lexical_natural_language_prefers_production_to_long_test_names() {
    let (state, root, store, cancel) = fixture();
    fs::write(
        root.path().join("cards.rs"),
        r#"/// Read request status from persistent storage, preserving queued, running,
/// completed, cancelled and failed outcomes across daemon restarts. The caller
/// receives the latest committed result together with its request identifier,
/// submission timestamp and completion details.
fn read_status_for() {}
#[test]
fn read_request_status_survives_restart_and_reports_missing_requests() {}
#[test]
fn read_request_status_keeps_completed_requests_visible_after_restart() {}
"#,
    )
    .unwrap();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(&store, root.path(), &leader, &cancel);
    let db = Connection::open(db_path(state.path(), root.path())).unwrap();
    let roles: BTreeMap<String, String> = db
        .prepare("SELECT name,role FROM native_version_declarations WHERE name IS NOT NULL")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(roles["read_status_for"], "production");
    for name in [
        "read_request_status_survives_restart_and_reports_missing_requests",
        "read_request_status_keeps_completed_requests_visible_after_restart",
    ] {
        assert_eq!(roles[name], "test");
    }
    let read = store.evidence_response().unwrap();
    let candidates = read
        .mcp_search_candidates(
            "read request status",
            Some(pin),
            &read
                .mcp_search_policy(
                    "read request status",
                    pin,
                    trellis::search_rank::SearchRole::Any,
                    &mut || Ok(()),
                )
                .unwrap(),
            &mut || Ok(()),
        )
        .unwrap();
    assert_eq!(candidates.lexical.len(), 3);
    let hydrated = read
        .mcp_search_hydrate(&candidates.lexical, pin, &mut || Ok(()))
        .unwrap();
    let names: Vec<_> = hydrated
        .symbols
        .iter()
        .map(|hit| hit.symbol.declaration.name.as_deref().unwrap())
        .collect();
    assert_eq!(
        names[0], "read_status_for",
        "lexical natural-language search must prefer production over long matching test names: {names:?}"
    );
    read.finish(()).unwrap();
}

#[test]
fn exact_short_stopword_and_camel_names_use_raw_stored_equality() {
    use trellis::search_rank::SearchRole;
    let (_state, root, store, cancel) = fixture();
    fs::write(
        root.path().join("cards.rs"),
        "fn a() {} fn the() {} fn readStatusFor() {} fn caller() { readStatusFor(); }\n",
    )
    .unwrap();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(&store, root.path(), &leader, &cancel);
    let read = store.evidence_response().unwrap();
    for query in ["a", "the", "readStatusFor"] {
        let policy = read
            .mcp_search_policy(query, pin, SearchRole::Any, &mut || Ok(()))
            .unwrap();
        assert_eq!(policy.role, SearchRole::Any);
        assert_eq!(policy.exact.len(), 1, "{query}");
        let candidates = read
            .mcp_search_candidates(query, Some(pin), &policy, &mut || Ok(()))
            .unwrap();
        assert_eq!(Some(&candidates.lexical[0]), policy.exact.keys().next());
        if query == "readStatusFor" {
            assert!(
                candidates.lexical.len() > 1,
                "exact retrieval stays additive"
            );
        }
    }
    for query in [" a", "a ", "A", "ReadStatusFor", "readStatus"] {
        assert!(
            read.mcp_search_policy(query, pin, SearchRole::Any, &mut || Ok(()))
                .unwrap()
                .exact
                .is_empty(),
            "{query:?} must not become a raw exact match"
        );
    }
    read.finish(()).unwrap();
}

#[test]
fn exact_match_is_admitted_before_the_lexical_depth_cutoff() {
    use trellis::search_rank::{CANDIDATE_DEPTH, SearchRole};
    let (state, root, store, cancel) = fixture();
    let mut source = format!(
        "/// {}\nfn target_name() {{}}\n",
        (0..500)
            .map(|i| format!("detail{i}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    for i in 0..CANDIDATE_DEPTH + 5 {
        source.push_str(&format!("fn target_name_partial_{i}() {{}}\n"));
    }
    source.push_str("#[test] fn target_name_test() {}\n");
    fs::write(root.path().join("cards.rs"), source).unwrap();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(&store, root.path(), &leader, &cancel);
    let read = store.evidence_response().unwrap();
    let policy = read
        .mcp_search_policy("target_name", pin, SearchRole::Any, &mut || Ok(()))
        .unwrap();
    let exact = policy.exact.keys().next().unwrap();
    let db = Connection::open(db_path(state.path(), root.path())).unwrap();
    let expression = trellis::search_lexical::query_expression(&db, "target_name")
        .unwrap()
        .unwrap();
    let old_order: Vec<Vec<u8>> = db.prepare("SELECT syntax_id FROM head_search_fts WHERE head_search_fts MATCH ?1 ORDER BY bm25(head_search_fts),syntax_id").unwrap()
        .query_map([expression], |row| row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    let old_rank = old_order
        .iter()
        .position(|sid| format!("sid:v1:{}", hex::encode(sid)) == exact.syntax_id)
        .unwrap();
    assert!(
        old_rank >= CANDIDATE_DEPTH,
        "fixture must exclude exact from ordinary BM25 depth: {old_rank}"
    );
    let candidates = read
        .mcp_search_candidates("target_name", Some(pin), &policy, &mut || Ok(()))
        .unwrap();
    assert_eq!(candidates.lexical.len(), CANDIDATE_DEPTH);
    assert_eq!(&candidates.lexical[0], exact);
    assert!(candidates.lexical[1..].iter().all(|key| key != exact));
    let tests = read
        .mcp_search_candidates(
            "target_name",
            Some(pin),
            &read
                .mcp_search_policy("target_name", pin, SearchRole::Test, &mut || Ok(()))
                .unwrap(),
            &mut || Ok(()),
        )
        .unwrap();
    assert_eq!(
        tests.lexical.len(),
        1,
        "role filtering must precede depth truncation"
    );
    read.finish(()).unwrap();
}

#[test]
fn role_filter_uses_inline_stored_roles_and_keeps_complete_head_counts() {
    use trellis::{native_evidence::DeclarationRole, search_rank::SearchRole};
    let (_state, root, store, cancel) = fixture();
    fs::write(
        root.path().join("cards.rs"),
        "fn read_status_for() {}\nfn test_named_status() {}\n#[cfg(test)]\nmod checks { fn read_status_for() {} #[test] fn status_smoke() {} }\n",
    )
    .unwrap();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(&store, root.path(), &leader, &cancel);
    let read = store.evidence_response().unwrap();
    let count = read.mcp_search_head_count(pin, &mut || Ok(())).unwrap();
    let any = read
        .mcp_search_policy("read_status_for", pin, SearchRole::Any, &mut || Ok(()))
        .unwrap();
    assert_eq!(any.exact.len(), 2);
    let all = read
        .mcp_search_candidates(
            "status",
            Some(pin),
            &read
                .mcp_search_policy("status", pin, SearchRole::Any, &mut || Ok(()))
                .unwrap(),
            &mut || Ok(()),
        )
        .unwrap();
    assert_eq!(all.cards.len(), count);
    let exact = read
        .mcp_search_candidates("read_status_for", Some(pin), &any, &mut || Ok(()))
        .unwrap();
    assert_eq!(any.exact[&exact.lexical[0]], DeclarationRole::Production);
    assert_eq!(any.exact[&exact.lexical[1]], DeclarationRole::Test);
    for (role, stored, expected_matches) in [
        (SearchRole::Production, DeclarationRole::Production, 2),
        (SearchRole::Test, DeclarationRole::Test, 3),
    ] {
        let policy = read
            .mcp_search_policy("read_status_for", pin, role, &mut || Ok(()))
            .unwrap();
        assert_eq!(policy.exact.len(), 1);
        assert_eq!(policy.exact.values().next(), Some(&stored));
        let candidates = read
            .mcp_search_candidates(
                "status",
                Some(pin),
                &read
                    .mcp_search_policy("status", pin, role, &mut || Ok(()))
                    .unwrap(),
                &mut || Ok(()),
            )
            .unwrap();
        assert_eq!(
            candidates.cards.len(),
            count,
            "coverage still describes the whole head"
        );
        assert_eq!(candidates.lexical.len(), expected_matches);
        assert!(candidates.lexical.iter().all(|key| {
            candidates
                .cards
                .iter()
                .any(|card| &card.key == key && card.role == stored)
        }));
        assert!(
            candidates
                .lexical
                .iter()
                .any(|key| policy.exact.contains_key(key))
        );
    }
    read.finish(()).unwrap();
}

#[test]
fn query_policy_tracks_admitted_publication_and_has_typed_cancellation() {
    use trellis::search_rank::SearchRole;
    let (_state, root, store, cancel) = fixture();
    let leader = store.publication_for_tests().unwrap();
    let first = publish(&store, root.path(), &leader, &cancel);
    let read = store.evidence_response().unwrap();
    let before = read
        .mcp_search_policy("orchard", first, SearchRole::Production, &mut || Ok(()))
        .unwrap();
    assert_eq!(before.exact.len(), 1);
    let stopped = read
        .mcp_search_policy("orchard", first, SearchRole::Any, &mut || {
            anyhow::bail!("deadline_exceeded")
        })
        .unwrap_err();
    assert_eq!(stopped.to_string(), "deadline_exceeded");
    read.finish(()).unwrap();
    drop(read);
    fs::write(root.path().join("cards.rs"), "#[test] fn orchard() {}\n").unwrap();
    let second = publish(&store, root.path(), &leader, &cancel);
    let read = store.evidence_response().unwrap();
    assert!(
        read.mcp_search_policy("orchard", first, SearchRole::Any, &mut || Ok(()))
            .unwrap_err()
            .to_string()
            .contains("revision conflict")
    );
    let production = read
        .mcp_search_policy("orchard", second, SearchRole::Production, &mut || Ok(()))
        .unwrap();
    assert!(production.exact.is_empty());
    assert_eq!(production.role, SearchRole::Production);
    let tests = read
        .mcp_search_policy("orchard", second, SearchRole::Test, &mut || Ok(()))
        .unwrap();
    assert_eq!(tests.exact.len(), 1);
    assert!(
        tests
            .exact
            .keys()
            .all(|key| !before.exact.contains_key(key))
    );
    read.finish(()).unwrap();
}

#[test]
fn indexed_exact_policy_keeps_language_lookup_derivations_and_raw_equality() {
    use trellis::search_rank::SearchRole;
    let (_state, root, store, cancel) = fixture();
    fs::write(root.path().join("cards.rs"), "fn r#type() {}\n").unwrap();
    fs::write(root.path().join("names.py"), "def Kelvin():\n    pass\n").unwrap();
    fs::write(
        root.path().join("Names.java"),
        "class Names { void café() {} }\n",
    )
    .unwrap();
    fs::write(root.path().join("names.js"), r"function caf\u00e9() {}").unwrap();
    let leader = store.publication_for_tests().unwrap();
    let pin = publish(&store, root.path(), &leader, &cancel);
    let read = store.evidence_response().unwrap();
    let count = read.mcp_search_head_count(pin, &mut || Ok(())).unwrap();
    for query in ["r#type", "Kelvin", "café", r"caf\u00e9"] {
        let policy = read
            .mcp_search_policy(query, pin, SearchRole::Any, &mut || Ok(()))
            .unwrap();
        assert_eq!(policy.exact.len(), 1, "{query}");
        let lexical = read
            .mcp_search_lexical(query, pin, count, &policy, &mut || Ok(()))
            .unwrap();
        assert_eq!(lexical.first(), policy.exact.keys().next(), "{query}");
    }
    for query in ["type", "Kelvin", "CAFÉ"] {
        assert!(
            read.mcp_search_policy(query, pin, SearchRole::Any, &mut || Ok(()))
                .unwrap()
                .exact
                .is_empty(),
            "{query}"
        );
    }
    read.finish(()).unwrap();
}
