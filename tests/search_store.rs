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
    let leader = store.publication_for_tests().unwrap();
    let first = publish(&store, root.path(), &leader, &cancel);
    let before = all_cards(&store, first);
    let db = Connection::open(db_path(state.path(), root.path())).unwrap();
    let before_fts = corpus(&db);
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
        artifacts: BTreeMap::from([(producer.id.clone(), b"artifact".to_vec())]),
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
        24
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
        .mcp_search_candidates("orchard", Some(first), &mut || Ok(()))
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
    assert_eq!(candidates.scope[1], [2, 2, 0, 0, 0]);
    let selected = read
        .mcp_search_hydrate(&candidates.lexical[..1], first, &mut || Ok(()))
        .unwrap();
    assert_eq!(selected.pin, first);
    assert_eq!(selected.scope, candidates.scope);
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
    assert_eq!(empty.scope, candidates.scope);
    read.finish(()).unwrap();
    drop(read);
    fs::write(root.path().join("cards.rs"), "fn changed() {}\n").unwrap();
    let second = publish(&store, root.path(), &leader, &cancel);
    let read = store.evidence_response().unwrap();
    let conflict = read
        .mcp_search_candidates("orchard", Some(first), &mut || Ok(()))
        .unwrap_err();
    assert!(conflict.to_string().contains("revision conflict"));
    assert_eq!(
        read.mcp_search_candidates("orchard", Some(second), &mut || Ok(()))
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
        .mcp_search_candidates("the a _", Some(pin), &mut || Ok(()))
        .unwrap();
    assert!(candidates.lexical.is_empty());
    assert_eq!(candidates.scope[1], [1, 0, 1, 0, 0]);
    let error = read
        .mcp_search_candidates("orchard", Some(pin), &mut || {
            anyhow::bail!("deadline_exceeded")
        })
        .unwrap_err();
    assert_eq!(error.to_string(), "deadline_exceeded");
    let candidates = read
        .mcp_search_candidates("orchard", Some(pin), &mut || Ok(()))
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
    let read = store.evidence_response().unwrap();
    assert!(
        read.mcp_search_candidates("the a _", Some(pin), &mut || Ok(()))
            .is_err(),
        "termless query cannot hide required head table failure"
    );
}

#[test]
fn lexical_bm25_then_sid_order_matches_the_head_fts_without_raw_query_parsing() {
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
    let expression = trellis::search_lexical::query_expression(&db, "orchard")
        .unwrap()
        .unwrap();
    let expected: Vec<Vec<u8>> = db.prepare("SELECT syntax_id FROM head_search_fts WHERE head_search_fts MATCH ?1 ORDER BY bm25(head_search_fts),syntax_id").unwrap()
        .query_map([expression],|row| row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    let read = store.evidence_response().unwrap();
    let candidates = read
        .mcp_search_candidates("orchard", Some(pin), &mut || Ok(()))
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
        read.mcp_search_candidates(query, Some(pin), &mut || Ok(()))
            .unwrap();
    }
    read.finish(()).unwrap();
}
