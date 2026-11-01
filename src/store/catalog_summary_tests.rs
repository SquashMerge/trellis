//! Summary admission, selected CID reuse, and recovery from incomplete selections.
use super::*;
use crate::{
    capture::Capture,
    classes::{Catalog, Limits},
    index_coordinator::IndexJobCoordinator,
    indexer::IndexOptions,
    model::FileExtraction,
};
use std::{fs, sync::atomic::AtomicBool};

fn fixture() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Store,
    IndexOptions,
    IndexPin,
) {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    for (path, source) in [
        (
            "A.java",
            "class A extends Base { Base field; int run(){ return 1; } }\n",
        ),
        (
            "B.py",
            "class B(Base):\n    value: Base\n    def run(self):\n        return 2\n",
        ),
        ("C.java", "class C { int later; int run(){ return 3; } }\n"),
        ("edit.js", "function edit(){ return 1; }\n"),
    ] {
        fs::write(work.path().join(path), source).unwrap();
    }
    let options = IndexOptions::new(work.path().to_owned());
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let pin = reindex(&store, &options, false);
    (state, work, store, options, pin)
}

fn reindex(store: &Store, options: &IndexOptions, expect_local: bool) -> IndexPin {
    let local = AtomicBool::new(false);
    let pin = IndexJobCoordinator::prepare(store, None)
        .unwrap()
        .run(options, &Arc::new(AtomicBool::new(false)), |progress| {
            if progress.phase == "mode:local" {
                local.store(true, Ordering::Relaxed);
            }
        })
        .unwrap();
    assert_eq!(local.load(Ordering::Relaxed), expect_local);
    pin
}

fn catalog(store: &Store, pin: IndexPin) -> Catalog {
    let db = store.cache_write().unwrap();
    let selected = store.read_revision_for(&db, Some(pin), true).unwrap();
    let classes = db
        .prepare(
            "SELECT c.payload FROM revision_documents m JOIN classes c ON c.cid=m.cid
         WHERE m.revision_id=?1 ORDER BY m.path,c.id",
        )
        .unwrap()
        .query_map([&selected.key], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|row| serde_json::from_str(&row.unwrap()).unwrap())
        .collect();
    let relations = db
        .prepare(
            "SELECT c.payload FROM revision_documents m JOIN class_relations c ON c.cid=m.cid
         WHERE m.revision_id=?1 ORDER BY m.path,c.id",
        )
        .unwrap()
        .query_map([&selected.key], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|row| serde_json::from_str(&row.unwrap()).unwrap())
        .collect();
    let (warnings, truncated) = class_metadata(&db, &selected).unwrap();
    Catalog {
        classes,
        relations,
        warnings,
        truncated,
    }
}

// Hashes, not SQLite integer keys, identify projections across independent stores.
fn selections(store: &Store, pin: IndexPin) -> BTreeMap<String, (String, String, bool)> {
    let db = store.cache_write().unwrap();
    let selected = store.read_revision_for(&db, Some(pin), true).unwrap();
    db.prepare(
        "SELECT m.path,g.graph_hash,c.content_hash,m.class_complete
         FROM revision_documents m JOIN graph_projections g ON g.gid=m.gid
         JOIN class_projections c ON c.cid=m.cid WHERE m.revision_id=?1 ORDER BY m.path",
    )
    .unwrap()
    .query_map([&selected.key], |row| {
        Ok((row.get(0)?, (row.get(1)?, row.get(2)?, row.get(3)?)))
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

fn counters() -> (usize, usize) {
    (
        LOCAL_CLASS_EXTRACTION_READS.with(|n| n.get()),
        LOCAL_CLASS_PROJECTION_HASHES.with(|n| n.get()),
    )
}

fn assert_fresh_full_parity(store: &Store, options: &IndexOptions, pin: IndexPin) {
    let state = tempfile::tempdir().unwrap();
    let full = Store::open_for_tests(state.path(), &options.workspace_root).unwrap();
    let expected = reindex(&full, options, false);
    assert_eq!(catalog(store, pin), catalog(&full, expected));
    assert_eq!(selections(store, pin), selections(&full, expected));
}

#[test]
fn both_full_routes_persist_summaries_and_unchanged_copies_completeness() {
    let (_state, _work, streamed, options, streamed_pin) = fixture();
    let plain_state = tempfile::tempdir().unwrap();
    let plain = Store::open_for_tests(plain_state.path(), &options.workspace_root).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) =
        crate::indexer::index_workspace_bundle(&options, plain.root_id(), &cancel, |_| {}).unwrap();
    let baseline = plain.index_baseline().unwrap();
    let leader = plain.writer().unwrap();
    let plain_pin = plain
        .publish_native(&graph, &capture, &native, &leader, baseline, &cancel)
        .unwrap();
    drop(leader);
    for (store, first) in [(&streamed, streamed_pin), (&plain, plain_pin)] {
        let db = store.cache_write().unwrap();
        let selected = store.read_revision_for(&db, Some(first), true).unwrap();
        let mut ready = 0;
        for row in db.prepare(
            "SELECT m.language,g.class_extraction_payload,g.class_summary_payload,m.class_complete
             FROM revision_documents m JOIN graph_projections g ON g.gid=m.gid WHERE m.revision_id=?1",
        ).unwrap().query_map([&selected.key], |row| Ok((
            row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?, row.get::<_, bool>(3)?,
        ))).unwrap() {
            let (language, extraction, summary, complete) = row.unwrap();
            assert!(complete);
            if crate::classes::supports_language(&language) {
                ready += 1;
                let extraction: FileExtraction = serde_json::from_str(&extraction.unwrap()).unwrap();
                let summary: crate::classes::FileSummary = serde_json::from_str(&summary.unwrap()).unwrap();
                assert_eq!(summary, extraction.summary());
            } else {
                assert!(extraction.is_none() && summary.is_none());
            }
        }
        assert_eq!(
            ready, 4,
            "every document language is a class-catalog language (#452)"
        );
        drop(db);
        let original = selections(store, first);
        let before = counters();
        let unchanged = reindex(store, &options, true);
        assert_eq!(selections(store, unchanged), original);
        assert_eq!(counters(), before);
    }
}

#[test]
fn no_hit_local_edits_equal_independent_full_and_reuse_complete_cids() {
    for edit in ["body", "declaration", "remove", "language", "nonclass"] {
        let (_state, work, store, options, first) = fixture();
        let original = selections(&store, first);
        assert!(original.values().all(|(_, _, complete)| *complete));
        let unchanged = match edit {
            "body" => {
                fs::write(
                    work.path().join("A.java"),
                    "class A extends Base { Base field; int run(){ return 99; } }\n",
                )
                .unwrap();
                "B.py"
            }
            "declaration" => {
                fs::write(
                    work.path().join("A.java"),
                    "class Renamed extends Other { Other field; int changed(){ return 1; } }\n",
                )
                .unwrap();
                "B.py"
            }
            "remove" => {
                fs::remove_file(work.path().join("A.java")).unwrap();
                "B.py"
            }
            "language" => {
                fs::remove_file(work.path().join("A.java")).unwrap();
                fs::write(work.path().join("A.js"), "function moved(){ return 1; }\n").unwrap();
                "B.py"
            }
            "nonclass" => {
                fs::write(
                    work.path().join("edit.js"),
                    "function changed(){ return 2; }\n",
                )
                .unwrap();
                "A.java"
            }
            _ => unreachable!(),
        };
        let before = counters();
        let next = reindex(&store, &options, true);
        assert_eq!(
            counters(),
            before,
            "{edit}: no old F loads or unchanged hashes"
        );
        assert_eq!(
            selections(&store, next)[unchanged],
            original[unchanged],
            "{edit}"
        );
        assert_fresh_full_parity(&store, &options, next);
    }
}

/// Model a prior globally clipped publication using the real bounded composer.
/// This is not a test of FULL's limit injection: honest immutable F, summaries,
/// native rows and GIDs come from FULL, while selected CIDs contain its clipped
/// output. Conservative false bits match the global-hit publication contract.
fn select_clipped_catalog(store: &Store, pin: IndexPin, limits: Limits) -> Catalog {
    let mut db = store.cache_write().unwrap();
    let selected = store.read_revision_for(&db, Some(pin), true).unwrap();
    let extractions: Vec<FileExtraction> = db
        .prepare(
            "SELECT g.class_extraction_payload FROM revision_documents m
         JOIN graph_projections g ON g.gid=m.gid
         WHERE m.revision_id=?1 AND g.class_extraction_state='ready' ORDER BY m.path",
        )
        .unwrap()
        .query_map([&selected.key], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|row| serde_json::from_str(&row.unwrap()).unwrap())
        .collect();
    let graph = store.graph_at(Some(pin)).unwrap();
    let clipped =
        Catalog::compose(&extractions, graph.files.len(), graph.stats.symbols, limits).unwrap();
    assert!(clipped.truncated);
    let old: Vec<(String, i64, String)> = db
        .prepare(
            "SELECT m.path,m.gid,g.graph_hash FROM revision_documents m
         JOIN graph_projections g ON g.gid=m.gid WHERE m.revision_id=?1
         AND m.language IN ('java','python','javascript','rust') ORDER BY m.path",
        )
        .unwrap()
        .query_map([&selected.key], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let tx = db.transaction().unwrap();
    let mut immutable = ImmutableAppend::default();
    for (path, gid, graph_hash) in old {
        let grouped = PublicationRows {
            classes: clipped
                .classes
                .iter()
                .filter(|class| class.symbol.path == path)
                .collect(),
            relations: clipped
                .relations
                .iter()
                .filter(|relation| relation.path == path)
                .collect(),
            ..PublicationRows::default()
        };
        let graph_id = format!("graph:v1:{graph_hash}");
        let (class_id, class_hash) = v8_class_projection(&graph_id, &grouped);
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO class_projections(gid,content_hash,state) VALUES(?1,?2,'ready')",
            params![gid, class_hash],
        ).unwrap();
        if inserted == 1 {
            write_graph_document(
                &tx,
                &grouped,
                &V8DocumentProjection {
                    version_id: String::new(),
                    graph_id,
                    graph_hash,
                    class_id,
                    class_hash: class_hash.clone(),
                },
                ReusedFamilies {
                    native: true,
                    graph: true,
                    class: false,
                },
                &mut immutable,
            )
            .unwrap();
        }
        tx.execute(
            "UPDATE revision_documents SET cid=(SELECT cid FROM class_projections WHERE content_hash=?3),
             class_complete=0 WHERE revision_id=?1 AND path=?2",
            params![selected.key, path, class_hash],
        ).unwrap();
    }
    tx.execute(
        "UPDATE native_revisions SET class_warnings=?2,class_truncated=1 WHERE id=?1",
        params![selected.key, json(&clipped.warnings).unwrap()],
    )
    .unwrap();
    tx.commit().unwrap();
    clipped
}

#[test]
fn formerly_cut_declarations_and_details_recover_immediately_when_summaries_fit() {
    for detail_cut in [false, true] {
        let (_state, work, store, options, first) = fixture();
        let limits = if detail_cut {
            Limits {
                records: 0,
                ..Limits::default()
            }
        } else {
            Limits {
                classes: 1,
                ..Limits::default()
            }
        };
        let clipped = select_clipped_catalog(&store, first, limits);
        if detail_cut {
            assert_eq!(
                clipped.classes.len(),
                3,
                "detail exhaustion keeps later declarations"
            );
            assert!(
                clipped
                    .classes
                    .iter()
                    .all(|class| class.fields.is_empty() && class.methods.is_empty())
            );
        } else {
            assert_eq!(clipped.classes.len(), 1);
            assert!(
                !catalog(&store, first)
                    .classes
                    .iter()
                    .any(|class| class.symbol.path == "C.java")
            );
        }
        let prior = selections(&store, first);
        assert!(
            !prior["edit.js"].2,
            "a catalog document without classes is still selected"
        );
        assert!(!prior["B.py"].2 && !prior["C.java"].2);
        fs::remove_file(work.path().join("A.java")).unwrap();
        let before = counters();
        let next = reindex(&store, &options, true);
        assert_eq!(
            counters(),
            (before.0 + 3, before.1 + 3),
            "only the three unchanged incomplete class selections (B.py, C.java, edit.js) need F and hashes"
        );
        assert!(
            selections(&store, next)
                .values()
                .all(|(_, _, complete)| *complete)
        );
        let recovered = catalog(&store, next);
        assert!(!recovered.truncated, "global truncation must not stick");
        assert!(
            recovered
                .classes
                .iter()
                .any(|class| class.symbol.path == "C.java" && !class.methods.is_empty())
        );
        assert!(!recovered.relations.is_empty());
        assert_fresh_full_parity(&store, &options, next);
    }
}

#[test]
fn malformed_or_missing_summary_fails_typed_before_publication_then_repairs() {
    for payload in [Some("not-json"), None, Some(r#"{"source_bytes":-1}"#)] {
        let (_state, work, store, options, first) = fixture();
        let baseline = store.publication_index_baseline().unwrap();
        fs::write(
            work.path().join("edit.js"),
            "function edit(){ return 2; }\n",
        )
        .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
        {
            let db = store.cache_write().unwrap();
            db.pragma_update(None, "ignore_check_constraints", "ON")
                .unwrap();
            db.execute(
                "UPDATE graph_projections SET class_summary_payload=?1 WHERE gid=(
                SELECT gid FROM revision_documents WHERE path='A.java')",
                [payload],
            )
            .unwrap();
        }
        let error = store
            .prepare_local_revision(&capture, &baseline, &cancel)
            .err()
            .unwrap();
        assert!(
            error.downcast_ref::<SelectedIntegrity>().is_some(),
            "{error:#}"
        );
        assert_eq!(recovery_class(&error), RecoveryClass::Rebuild, "{error:#}");
        assert_eq!(store.disposition(), RecoveryDisposition::Rebuild);
        {
            let db = store.cache_write().unwrap();
            let (generation, revision, rows): (String, i64, i64) = db.query_row(
                "SELECT index_generation,index_revision,(SELECT count(*) FROM native_revisions) FROM index_metadata",
                [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
            ).unwrap();
            assert_eq!(
                (generation, revision, rows),
                (
                    first.index_generation.to_string(),
                    first.index_revision as i64,
                    1
                )
            );
        }
        let repaired = reindex(&store, &options, false);
        assert_ne!(repaired.index_generation, first.index_generation);
        assert_eq!(store.disposition(), RecoveryDisposition::Ready);
        assert_fresh_full_parity(&store, &options, repaired);
    }
}

#[test]
fn intrinsic_file_truncation_is_a_complete_selection_on_no_global_hit() {
    let (_state, work, store, options, _first) = fixture();
    fs::write(
        work.path().join("Broken.java"),
        "class Broken { int field;\n",
    )
    .unwrap();
    let pin = reindex(&store, &options, true);
    let before = counters();
    let db = store.cache_write().unwrap();
    let selected = store.read_revision_for(&db, Some(pin), true).unwrap();
    let (payload, complete): (String, bool) = db
        .query_row(
            "SELECT g.class_extraction_payload,m.class_complete FROM revision_documents m
         JOIN graph_projections g ON g.gid=m.gid WHERE m.revision_id=?1 AND m.path='Broken.java'",
            [&selected.key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let extraction: FileExtraction = serde_json::from_str(&payload).unwrap();
    assert!(
        extraction.truncated,
        "recovered syntax must exercise intrinsic truncation"
    );
    assert!(
        complete,
        "complete means all immutable F output, not !F.truncated"
    );
    drop(db);
    fs::write(
        work.path().join("edit.js"),
        "function edit(){ return 3; }\n",
    )
    .unwrap();
    let next = reindex(&store, &options, true);
    assert_eq!(counters(), before);
    assert_eq!(
        selections(&store, pin)["Broken.java"],
        selections(&store, next)["Broken.java"]
    );
    assert_fresh_full_parity(&store, &options, next);
}

#[test]
fn rust_local_edit_recomposes_impl_members_merged_into_another_document() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::create_dir(work.path().join("src")).unwrap();
    let ops = "use crate::model::Account;\nimpl Account {\n    pub fn first(&self) -> u64 { 1 }\n    pub fn second(&self) -> u64 { 2 }\n}\n";
    for (path, source) in [
        ("src/lib.rs", "pub mod model;\npub mod ops;\n"),
        ("src/model.rs", "pub struct Account {\n    id: u64,\n}\n"),
        ("src/ops.rs", ops),
    ] {
        fs::write(work.path().join(path), source).unwrap();
    }
    let options = IndexOptions::new(work.path().to_owned());
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let first = reindex(&store, &options, false);
    let account = |pin| {
        catalog(&store, pin)
            .classes
            .into_iter()
            .find(|c| c.qualified_name == "model::Account")
            .unwrap()
    };
    let before = account(first);
    assert_eq!(
        before
            .methods
            .iter()
            .map(|m| (m.name.as_str(), m.path.as_str()))
            .collect::<Vec<_>>(),
        [("first", "src/ops.rs"), ("second", "src/ops.rs")]
    );
    // A body-only edit of the contributing file is a local publication. The
    // members merged into src/model.rs's projection must follow its new ranges.
    fs::write(
        work.path().join("src/ops.rs"),
        ops.replace("{ 1 }", "{ 100000 }"),
    )
    .unwrap();
    let next = reindex(&store, &options, true);
    let after = account(next);
    assert_ne!(after.methods[1].range, before.methods[1].range);
    let graph = store.graph_at(Some(next)).unwrap();
    for method in &after.methods {
        let node = graph
            .nodes
            .iter()
            .find(|n| Some(&n.id) == method.symbol_id.as_ref())
            .unwrap();
        assert_eq!(node.range, method.range, "{}", method.name);
    }
    assert_fresh_full_parity(&store, &options, next);
}
