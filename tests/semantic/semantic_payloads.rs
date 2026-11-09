//! #680, Decision 0021: shared semantic payloads. A payload holds the revision-neutral rows of
//! one document; a revision's manifest row is what makes it evidence of that revision.
#![cfg(feature = "test-support")]

use crate::semantic_batch_store::{
    DEFINITIONS_A, DEFINITIONS_B, World, local_edit, world, write_files,
};
use crate::semantic_batch_support::DocSpec;
use std::collections::BTreeMap;
use std::path::PathBuf;
use trellis::model::IndexPin;
use trellis::semantic_evidence::Freshness;

const PATHS: [&str; 6] = [
    "src/A.js", "src/B.js", "src/C.js", "src/D.js", "src/E.js", "src/F.js",
];
const EXCLUDED: &str = "src/G.js";

fn key(pin: IndexPin) -> String {
    format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision)
}

fn small(name: &str) -> DocSpec {
    DocSpec::new(&format!("src/{}.js", name.to_uppercase()))
        .occurrence(&[0, 9, 10], &[], &format!("pkg {name}."), 1)
        .symbol(&format!("pkg {name}."), 17, &[])
}

/// The checkout every payload test starts from.
fn payload_files(root: &std::path::Path) {
    let small_text = |name: &str| format!("function {name}(){{ }}\n");
    write_files(
        root,
        &[
            ("src/A.js", DEFINITIONS_A),
            ("src/B.js", DEFINITIONS_B),
            ("src/C.js", &small_text("c")),
            ("src/D.js", &small_text("d")),
            ("src/E.js", &small_text("e")),
            ("src/F.js", &small_text("f")),
            ("src/G.js", &small_text("g")),
        ],
    );
}

/// Six fact-bearing documents (`A` calls `B`) and `G`, which the batch excludes as
/// `notRequested`. The batch is imported; the oracle and work counts are on.
fn payload_world() -> (World, IndexPin, PathBuf) {
    let world = world(payload_files);
    let counts = world._state.path().join("work-counts.json");
    // SAFETY: each test runs in its own process under nextest, and no other thread reads these.
    unsafe {
        std::env::set_var("TRELLIS_TEST_LOCAL_WORK_COUNTS_FILE", &counts);
        std::env::set_var("TRELLIS_TEST_LOCAL_FULL_ORACLE", "1");
    }
    world.store.set_retention_clock_for_tests(1_000, 0);
    let descriptor = payload_batch(&world);
    let (import, _, _) = world.publish(&descriptor).unwrap();
    (world, import, counts)
}

/// The batch over `payload_files`: every document but `G`, which it excludes as `notRequested`.
fn payload_batch(world: &World) -> (PathBuf, String) {
    let (a, b) = crate::semantic_batch_store::definitions_docs();
    let mut fixture = world.fixture().reference("scip-typescript");
    fixture.artifact(
        "javascript",
        vec![a, b, small("c"), small("d"), small("e"), small("f")],
    );
    fixture.exclude("javascript", EXCLUDED, "notRequested");
    world.write(&fixture)
}

fn edit_b(world: &World, counts: &std::path::Path, body: &str) -> IndexPin {
    local_edit(
        world,
        "src/B.js",
        &format!("function target(){{ {body} }}\n"),
        counts,
    );
    world.store.index_baseline().unwrap()
}

fn payload_of(world: &World, revision: IndexPin, path: &str) -> Option<i64> {
    world
        .db()
        .query_row(
            "SELECT payload_id FROM semantic_coverage WHERE revision_id=?1 AND path=?2",
            rusqlite::params![key(revision), path],
            |row| row.get(0),
        )
        .unwrap()
}

const TABLES: [&str; 22] = [
    "semantic_payloads",
    "semantic_coverage",
    "semantic_coverage_freshness",
    "semantic_coverage_roles",
    "semantic_coverage_reasons",
    "semantic_basis",
    "semantic_basis_lookup_dependencies",
    "semantic_provenance",
    "semantic_symbols",
    "semantic_symbol_targets",
    "semantic_declaration_bindings",
    "semantic_declaration_binding_symbols",
    "semantic_declaration_join_candidates",
    "semantic_references",
    "semantic_reference_roles",
    "semantic_reference_candidates",
    "semantic_call_bindings",
    "semantic_call_candidates",
    "semantic_call_possible_dispatch",
    "semantic_call_join_candidates",
    "semantic_type_relationships",
    "semantic_projection_dependencies",
];

fn row_counts(world: &World) -> BTreeMap<&'static str, i64> {
    TABLES
        .iter()
        .map(|table| {
            (
                *table,
                world.count(&format!("SELECT COUNT(*) FROM {table}")),
            )
        })
        .collect()
}

/// The rows of `table` that belong to `payloads`, by the table's own payload scope.
fn payload_rows(world: &World, table: &str, payloads: &[i64]) -> i64 {
    let list = payloads
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let scope = match table {
        "semantic_payloads" => format!("payload_id IN ({list})"),
        "semantic_symbol_targets" => format!(
            "symbol_id IN (SELECT symbol_id FROM semantic_symbols WHERE payload_id IN ({list}))"
        ),
        "semantic_basis_lookup_dependencies" => format!(
            "basis_id IN (SELECT basis_id FROM semantic_basis WHERE payload_id IN ({list}))"
        ),
        "semantic_declaration_binding_symbols" | "semantic_declaration_join_candidates" => format!(
            "binding_id IN (SELECT binding_id FROM semantic_declaration_bindings WHERE payload_id IN ({list}))"
        ),
        "semantic_call_candidates"
        | "semantic_call_possible_dispatch"
        | "semantic_call_join_candidates" => format!(
            "binding_id IN (SELECT binding_id FROM semantic_call_bindings WHERE payload_id IN ({list}))"
        ),
        _ => format!("payload_id IN ({list})"),
    };
    world.count(&format!("SELECT COUNT(*) FROM {table} WHERE {scope}"))
}

/// AC1: an S that changes `B` writes the rows of the two payloads it changes (`B`, and `A`,
/// which depends on it) and one manifest row, plus a freshness row, per document.
#[test]
fn s_commit_writes_changed_and_dependent_rows_only() {
    let (world, import, counts) = payload_world();
    let before = row_counts(&world);
    let max_before = world.count("SELECT MAX(payload_id) FROM semantic_payloads");
    let unchanged_before: Vec<_> = PATHS[2..]
        .iter()
        .map(|path| payload_of(&world, import, path))
        .collect();
    let head = edit_b(&world, &counts, "return 1;");
    let after = row_counts(&world);
    let new_payloads: Vec<i64> =
        ((max_before + 1)..=world.count("SELECT MAX(payload_id) FROM semantic_payloads")).collect();
    assert_eq!(new_payloads.len(), 2, "payloads written for A and B");
    assert_eq!(
        after["semantic_payloads"] - before["semantic_payloads"],
        2,
        "{before:?} {after:?}"
    );
    // Each revision has a manifest row per document, and a freshness row per fact-bearing one.
    assert_eq!(
        after["semantic_coverage"] - before["semantic_coverage"],
        7,
        "six documents and the notRequested exclusion"
    );
    // A changed document the producer fails (B) has no facts and so no basis to be fresh about.
    let bearing = world.count(&format!(
        "SELECT COUNT(*) FROM semantic_coverage c WHERE c.revision_id='{}' AND EXISTS(SELECT 1 FROM semantic_provenance p WHERE p.payload_id=c.payload_id)",
        key(head)
    ));
    assert_eq!(bearing, 5);
    assert_eq!(
        after["semantic_coverage_freshness"] - before["semantic_coverage_freshness"],
        bearing
    );
    // Every payload-keyed table grows by exactly the two new payloads' rows.
    for table in TABLES {
        if matches!(table, "semantic_coverage" | "semantic_coverage_freshness") {
            continue;
        }
        // Roles and reasons also carry the exclusion's manifest-owned rows, counted below.
        let manifest_owned = match table {
            "semantic_coverage_roles" | "semantic_coverage_reasons" => world.count(&format!(
                "SELECT COUNT(*) FROM {table} r JOIN semantic_coverage c ON c.tuple_id=r.tuple_id WHERE c.revision_id='{}'",
                key(head)
            )),
            _ => 0,
        };
        assert_eq!(
            after[table] - before[table],
            payload_rows(&world, table, &new_payloads) + manifest_owned,
            "{table}"
        );
    }
    // Unchanged documents name the payloads they named before.
    let unchanged_after: Vec<_> = PATHS[2..]
        .iter()
        .map(|path| payload_of(&world, head, path))
        .collect();
    assert_eq!(unchanged_before, unchanged_after);
    assert!(unchanged_after.iter().all(Option::is_some));
    // The exclusion costs one manifest row, its supported roles and at most one reasons row,
    // and names no payload.
    assert_eq!(payload_of(&world, head, EXCLUDED), None);
    let (roles, reasons): (i64, i64) = world
        .db()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM semantic_coverage_roles r WHERE r.tuple_id=c.tuple_id),
                    (SELECT COUNT(*) FROM semantic_coverage_reasons r WHERE r.tuple_id=c.tuple_id)
             FROM semantic_coverage c WHERE c.revision_id=?1 AND c.path=?2",
            rusqlite::params![key(head), EXCLUDED],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(roles <= 7 && reasons <= 1, "{roles} {reasons}");
    println!("S row deltas: {:?}", {
        let mut delta = BTreeMap::new();
        for table in TABLES {
            delta.insert(table, after[table] - before[table]);
        }
        delta
    });
}

/// AC3: a payload is evidence of a revision only through that revision's manifest row.
#[test]
fn payload_named_by_r1_is_not_evidence_at_r2_without_manifest_row() {
    let (world, import, counts) = payload_world();
    let head = edit_b(&world, &counts, "return 1;");
    let shared = payload_of(&world, import, "src/C.js").unwrap();
    assert_eq!(payload_of(&world, head, "src/C.js"), Some(shared));
    world
        .store
        .with_index_sql_for_tests(|db| {
            db.execute(
                "DELETE FROM semantic_coverage_freshness WHERE revision_id=?1 AND tuple_id IN (SELECT tuple_id FROM semantic_coverage WHERE revision_id=?1 AND path='src/C.js')",
                [key(head)],
            )?;
            Ok(db.execute(
                "DELETE FROM semantic_coverage WHERE revision_id=?1 AND path='src/C.js'",
                [key(head)],
            )?)
        })
        .unwrap();
    let at_head = world.store.semantic_at(head).unwrap();
    assert!(
        at_head
            .coverage
            .iter()
            .all(|c| c.document_path != "src/C.js")
    );
    assert!(at_head.symbols.iter().all(|s| s.key.symbol != "pkg c."));
    let at_import = world.store.semantic_at(import).unwrap();
    assert!(at_import.symbols.iter().any(|s| s.key.symbol == "pkg c."));
}

/// AC3: one payload read at two revisions is each revision's own evidence.
#[test]
fn pinned_read_projects_shared_payload_as_reading_revision() {
    let (world, import, counts) = payload_world();
    let head = edit_b(&world, &counts, "return 1;");
    assert_eq!(
        payload_of(&world, import, "src/C.js"),
        payload_of(&world, head, "src/C.js")
    );
    let mut proofs = Vec::new();
    for pin in [import, head] {
        let native = world
            .store
            .retained_native_snapshot_at(pin)
            .unwrap()
            .0
            .revision
            .id;
        let snapshot = world.store.semantic_at(pin).unwrap();
        let c = snapshot
            .coverage
            .iter()
            .find(|c| c.document_path == "src/C.js")
            .unwrap();
        assert_eq!(c.revision_id, native);
        let ids: Vec<_> = snapshot
            .provenance
            .iter()
            .filter(|p| p.document.path == "src/C.js")
            .inspect(|p| assert_eq!(p.revision_id, native))
            .map(|p| p.id.clone())
            .collect();
        assert!(!ids.is_empty());
        assert!(
            snapshot
                .references
                .iter()
                .filter(|r| r.document.path == "src/C.js")
                .all(|r| r.revision_id == native)
        );
        for relationship in &snapshot.symbols {
            for target in &relationship.declarations {
                if let trellis::semantic_evidence::Target::Internal { revision_id, .. } = target {
                    assert_eq!(revision_id, &native);
                }
            }
        }
        proofs.push(ids);
    }
    assert_ne!(proofs[0], proofs[1], "rejoined IDs are each revision's own");
}

/// AC3: freshness is a fact of the revision, tuple and basis, not of the shared payload.
#[test]
fn freshness_is_per_revision_tuple_basis() {
    let (world, import, counts) = payload_world();
    let head = edit_b(&world, &counts, "return 1;");
    let freshness = |pin: IndexPin| {
        world
            .store
            .semantic_at(pin)
            .unwrap()
            .provenance
            .iter()
            .filter(|p| p.document.path == "src/C.js")
            .map(|p| p.freshness)
            .collect::<Vec<_>>()
    };
    assert!(freshness(import).iter().all(|f| *f == Freshness::Fresh));
    assert!(
        freshness(head)
            .iter()
            .all(|f| *f == Freshness::PossiblyStale)
    );
}

/// AC4: restoring the captured bytes of a document reuses the payload the import wrote.
#[test]
fn restored_captured_version_hits_retained_payload() {
    let (world, import, counts) = payload_world();
    edit_b(&world, &counts, "return 1;");
    let payloads = world.count("SELECT COUNT(*) FROM semantic_payloads");
    let head = {
        local_edit(&world, "src/B.js", DEFINITIONS_B, &counts);
        world.store.index_baseline().unwrap()
    };
    for path in ["src/A.js", "src/B.js"] {
        assert_eq!(
            payload_of(&world, head, path),
            payload_of(&world, import, path),
            "{path}"
        );
    }
    assert_eq!(
        world.count("SELECT COUNT(*) FROM semantic_payloads"),
        payloads,
        "the restored documents wrote no payload"
    );
}

/// AC4: restoring a version that was never captured reuses the payload written for it, with
/// freshness of its own.
#[test]
fn restored_noncaptured_version_hits_retained_payload() {
    let (world, _import, counts) = payload_world();
    let v1 = edit_b(&world, &counts, "return 1;");
    edit_b(&world, &counts, "return 2;");
    let payloads = world.count("SELECT COUNT(*) FROM semantic_payloads");
    let v3 = edit_b(&world, &counts, "return 1;");
    for path in ["src/A.js", "src/B.js"] {
        assert_eq!(
            payload_of(&world, v3, path),
            payload_of(&world, v1, path),
            "{path}"
        );
    }
    assert_eq!(
        world.count("SELECT COUNT(*) FROM semantic_payloads"),
        payloads
    );
    let freshness = |pin: IndexPin, path: &str| -> (i64, String) {
        world
            .db()
            .query_row(
                "SELECT f.tuple_id,f.freshness FROM semantic_coverage_freshness f JOIN semantic_coverage c ON c.revision_id=f.revision_id AND c.tuple_id=f.tuple_id WHERE c.revision_id=?1 AND c.path=?2",
                rusqlite::params![key(pin), path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    };
    // The same payload, a freshness row of each revision's own. `A`'s bytes are unchanged but
    // its target `B` is not the captured one, so its basis is `possiblyStale`; `B`, which the
    // producer fails once edited, has no basis and so no freshness row.
    let (a3, a1) = (freshness(v3, "src/A.js"), freshness(v1, "src/A.js"));
    assert_ne!(a3.0, a1.0);
    assert_eq!(
        (a3.1.as_str(), a1.1.as_str()),
        ("possiblyStale", "possiblyStale")
    );
}

/// AC4: a document whose bytes changed has another version, so another key, and writes a
/// payload of its own.
#[test]
fn edited_document_does_not_hit_the_retained_payload() {
    let (world, import, counts) = payload_world();
    local_edit(&world, "src/C.js", "function c(){ }\n// edited\n", &counts);
    let head = world.store.index_baseline().unwrap();
    assert_ne!(
        payload_of(&world, head, "src/C.js"),
        payload_of(&world, import, "src/C.js")
    );
}

/// AC4: an unchanged document whose dependency changed does not reuse the payload.
#[test]
fn dependency_version_change_misses() {
    let (world, import, counts) = payload_world();
    let head = edit_b(&world, &counts, "return 1;");
    assert_ne!(
        payload_of(&world, head, "src/A.js"),
        payload_of(&world, import, "src/A.js")
    );
    assert_eq!(
        payload_of(&world, head, "src/C.js"),
        payload_of(&world, import, "src/C.js")
    );
}

/// AC4: a target that goes away and returns is settled again, not copied from the revision
/// that omitted its association.
#[test]
fn reappearing_target_takes_validated_path() {
    let (world, import, counts) = payload_world();
    std::fs::remove_file(world.root.join("src/B.js")).unwrap();
    local_edit(&world, "src/C.js", "function c(){ return 1; }\n", &counts);
    let gone = world.store.index_baseline().unwrap();
    write_files(&world.root, &[("src/B.js", DEFINITIONS_B)]);
    local_edit(&world, "src/C.js", "function c(){ }\n", &counts);
    let back = world.store.index_baseline().unwrap();
    assert_ne!(gone, back);
    // With the oracle on, the S that brought the target back equals the FULL rejoin; the
    // restored documents name the import's payloads.
    for path in ["src/A.js", "src/B.js", "src/C.js"] {
        assert_eq!(
            payload_of(&world, back, path),
            payload_of(&world, import, path),
            "{path}"
        );
    }
}

/// AC4 and D6: a payload that omitted an association has no reuse key.
#[test]
fn omitted_association_payload_is_never_reused() {
    let (world, _import, counts) = payload_world();
    std::fs::remove_file(world.root.join("src/B.js")).unwrap();
    local_edit(&world, "src/C.js", "function c(){ return 1; }\n", &counts);
    let omitted = world.count(
        "SELECT COUNT(*) FROM semantic_payloads p JOIN semantic_coverage_reasons r ON r.payload_id=p.payload_id WHERE r.omitted=1",
    );
    assert!(omitted > 0, "the missing target omitted an association");
    assert_eq!(
        world.count(
            "SELECT COUNT(*) FROM semantic_payloads p JOIN semantic_coverage_reasons r ON r.payload_id=p.payload_id WHERE r.omitted=1 AND p.payload_key IS NOT NULL"
        ),
        0
    );
}

/// AC4 and #826: a `failed` row an unavailable S published names no payload, so no later S
/// reuses it.
#[test]
fn unavailable_failed_row_is_never_reused() {
    let (world, _import, counts) = payload_world();
    std::fs::write(world.root.join("src/C.js"), "function c(){ return 1; }\n").unwrap();
    let _ = std::fs::remove_file(&counts);
    let options = trellis::indexer::IndexOptions::new(world.root.clone());
    let capture = trellis::capture::Capture::admit(&options, &world.cancel, &|_| {}).unwrap();
    let leader = world.store.publication_for_tests().unwrap();
    world
        .store
        .publish_local_for_tests(&options, &capture, &leader, &world.cancel)
        .unwrap();
    drop(leader);
    // SAFETY: as in `payload_world`.
    unsafe { std::env::remove_var("TRELLIS_TEST_LOCAL_FULL_ORACLE") };
    world
        .store
        .with_semantic_content_fault_for_tests(|| {
            world.store.catch_up_semantic_for_tests(&world.cancel)
        })
        .unwrap();
    let head = world.store.index_baseline().unwrap();
    let failed: i64 = world
        .db()
        .query_row(
            "SELECT COUNT(*) FROM semantic_coverage WHERE revision_id=?1 AND state='failed' AND payload_id IS NULL",
            [key(head)],
            |row| row.get(0),
        )
        .unwrap();
    assert!(failed > 0, "the unavailable S published failed rows");
    world.store.catch_up_semantic_for_tests(&world.cancel).ok();
    local_edit(&world, "src/C.js", "function c(){ return 2; }\n", &counts);
    let after = world.store.index_baseline().unwrap();
    assert_eq!(
        world.count(&format!(
            "SELECT COUNT(*) FROM semantic_coverage WHERE revision_id='{}' AND diagnostic LIKE 'selected raw capture unavailable%'",
            key(after)
        )),
        0,
        "the next S joins the documents again"
    );
}

/// AC5: a payload another revision still names survives the release of one revision.
#[test]
fn shared_payload_survives_release_of_one_revision() {
    let (world, _import, counts) = payload_world();
    let first = edit_b(&world, &counts, "return 1;");
    let head = edit_b(&world, &counts, "return 2;");
    let shared = payload_of(&world, first, "src/C.js").unwrap();
    let first_only = payload_of(&world, first, "src/A.js").unwrap();
    assert_eq!(payload_of(&world, head, "src/C.js"), Some(shared));
    assert_ne!(payload_of(&world, head, "src/A.js"), Some(first_only));
    world.store.set_retention_clock_for_tests(3_000, 2_000);
    let leader = world.store.publication_for_tests().unwrap();
    for _ in 0..4096 {
        match world.store.release_revision(first, &leader) {
            Ok(()) => break,
            Err(error) if format!("{error:#}").starts_with("storage_busy") => {}
            Err(error) => panic!("{error:#}"),
        }
    }
    drop(leader);
    let exists = |payload: i64| {
        world.count(&format!(
            "SELECT COUNT(*) FROM semantic_payloads WHERE payload_id={payload}"
        ))
    };
    assert_eq!(exists(shared), 1, "still named by the import and the head");
    assert_eq!(exists(first_only), 0, "named by no revision any more");
    assert_eq!(
        world.count("SELECT COUNT(*) FROM semantic_release_candidate_payloads"),
        0
    );
    assert!(!world.store.semantic_at(head).unwrap().symbols.is_empty());
}

/// AC5 and Decision 0006: a revision that is held keeps its payloads past the grace.
#[test]
fn held_revision_payloads_are_never_collected() {
    let (world, import, counts) = payload_world();
    let head = edit_b(&world, &counts, "return 1;");
    let before = world.count("SELECT COUNT(*) FROM semantic_payloads");
    world.store.set_retention_clock_for_tests(5_000, 4_000);
    let leader = world.store.publication_for_tests().unwrap();
    world.store.maintain_revisions(&leader).unwrap();
    drop(leader);
    // The import's batch is still selected by the head, so the import is held.
    assert_eq!(
        world.count("SELECT COUNT(*) FROM semantic_payloads"),
        before
    );
    assert!(!world.store.semantic_at(import).unwrap().symbols.is_empty());
    assert!(!world.store.semantic_at(head).unwrap().symbols.is_empty());
}

/// AC10: a corrupt shared payload fails every revision that names it.
#[test]
fn corrupt_shared_payload_fails_every_naming_revision() {
    let (world, import, counts) = payload_world();
    let head = edit_b(&world, &counts, "return 1;");
    let shared = payload_of(&world, import, "src/C.js").unwrap();
    assert_eq!(payload_of(&world, head, "src/C.js"), Some(shared));
    world
        .store
        .with_index_sql_for_tests(|db| {
            db.execute_batch("PRAGMA ignore_check_constraints=ON")?;
            let changed = db.execute(
                "UPDATE semantic_symbols SET key_scope='bogus' WHERE payload_id=?1",
                [shared],
            )?;
            db.execute_batch("PRAGMA ignore_check_constraints=OFF")?;
            Ok(changed)
        })
        .unwrap();
    let error = world.store.semantic_at(head).unwrap_err();
    assert!(
        format!("{error:#}").contains("invalid semantic enum"),
        "{error:#}"
    );
    let error = world.store.semantic_at(import).unwrap_err();
    assert!(
        format!("{error:#}").contains("incompatible_index"),
        "{error:#}"
    );
    assert!(world.store.recovery_pending_for_tests());
}

/// AC10: a freshness row the manifest needs is typed corruption, never a guess.
#[test]
fn missing_freshness_row_is_typed_corruption() {
    let (world, _import, counts) = payload_world();
    let head = edit_b(&world, &counts, "return 1;");
    world
        .store
        .with_index_sql_for_tests(|db| {
            Ok(db.execute(
                "DELETE FROM semantic_coverage_freshness WHERE revision_id=?1",
                [key(head)],
            )?)
        })
        .unwrap();
    let error = world.store.semantic_at(head).unwrap_err();
    assert!(
        format!("{error:#}").contains("incompatible_index: semantic freshness missing"),
        "{error:#}"
    );
}

/// AC4, owner ruling 2026-11-09: the join-inputs digest is part of the key. A payload another
/// build of the join code wrote is not named by this build's writer, even for the same bytes.
#[test]
fn payload_written_under_another_join_digest_is_not_reused() {
    let (world, import, counts) = payload_world();
    edit_b(&world, &counts, "return 1;");
    let payloads = world.count("SELECT COUNT(*) FROM semantic_payloads");
    world
        .store
        .set_payload_join_inputs_for_tests(Some("another build of the join"));
    local_edit(&world, "src/B.js", DEFINITIONS_B, &counts);
    let head = world.store.index_baseline().unwrap();
    for path in ["src/A.js", "src/B.js"] {
        assert_ne!(
            payload_of(&world, head, path),
            payload_of(&world, import, path),
            "{path}"
        );
    }
    assert_eq!(
        world.count("SELECT COUNT(*) FROM semantic_payloads"),
        payloads + PATHS.len() as i64,
        "every document wrote a payload of its own, the unchanged ones included"
    );
    // Back on this build's digest the same restore names the payloads it wrote.
    world.store.set_payload_join_inputs_for_tests(None);
    edit_b(&world, &counts, "return 1;");
    local_edit(&world, "src/B.js", DEFINITIONS_B, &counts);
    let head = world.store.index_baseline().unwrap();
    assert_eq!(
        payload_of(&world, head, "src/B.js"),
        payload_of(&world, import, "src/B.js")
    );
}

/// Owner ruling 2026-11-09: a build that changes join output rotates every payload, copies
/// included. An S under another join-inputs digest names no payload the import wrote, even for
/// unchanged documents, and still equals FULL (the oracle is on); under the real digest again
/// the import's payloads are reused and copies resume.
#[test]
fn copy_does_not_name_a_payload_another_join_digest_wrote() {
    let (world, import, counts) = payload_world();
    let unchanged = &PATHS[2..];
    let max_before = world.count("SELECT MAX(payload_id) FROM semantic_payloads");
    world
        .store
        .set_payload_join_inputs_for_tests(Some("another build of the join"));
    let other = edit_b(&world, &counts, "return 1;");
    for path in PATHS {
        let payload = payload_of(&world, other, path).unwrap();
        assert!(
            payload > max_before,
            "{path} names the import's payload {payload}"
        );
    }
    assert_eq!(
        world.count("SELECT COUNT(*) FROM semantic_payloads"),
        max_before + PATHS.len() as i64,
        "every document, changed or not, wrote a payload"
    );
    // The real digest again: the documents key as they did at the import and hit its payloads.
    world.store.set_payload_join_inputs_for_tests(None);
    let rotated = edit_b(&world, &counts, "return 2;");
    for path in unchanged {
        assert_eq!(
            payload_of(&world, rotated, path),
            payload_of(&world, import, path),
            "{path}"
        );
    }
    // And the next S copies them: no payload of theirs is written.
    let payloads = world.count("SELECT COUNT(*) FROM semantic_payloads");
    let copied = edit_b(&world, &counts, "return 3;");
    for path in unchanged {
        assert_eq!(
            payload_of(&world, copied, path),
            payload_of(&world, import, path),
            "{path}"
        );
    }
    assert_eq!(
        world.count("SELECT COUNT(*) FROM semantic_payloads"),
        payloads + 2,
        "only A and B wrote payloads"
    );
}

/// With `value` set in the environment for one catch-up, as the forced-FULL reference runs it.
fn full_edit(world: &World, path: &str, text: &str, counts: &std::path::Path) {
    // SAFETY: each test runs in its own process under nextest, and no other thread reads it.
    unsafe { std::env::set_var("TRELLIS_TEST_FORCE_FULL_REJOIN", "1") };
    local_edit(world, path, text, counts);
    // SAFETY: as above.
    unsafe { std::env::remove_var("TRELLIS_TEST_FORCE_FULL_REJOIN") };
}

/// The call-graph view of `run` at the head, with the index pin (which differs per store)
/// removed.
fn call_graph(world: &World, seed: &str) -> serde_json::Value {
    let view = world
        .store
        .evidence_response()
        .unwrap()
        .query_view(&crate::semantic_batch_store::view_query(seed, None))
        .unwrap()
        .unwrap();
    let mut view = serde_json::to_value(view).unwrap();
    view["revision"] = serde_json::Value::Null;
    view
}

/// AC2, F4: a history run with copies and writer hits publishes exactly what an independent
/// index that can reuse nothing does. Store `reference` is a fresh index over the same
/// checkout whose writers key no payload and whose every S is a forced FULL rejoin.
#[test]
fn s_reuse_snapshot_equals_full_snapshot() {
    let (reuse, _import, counts) = payload_world();
    let reference = crate::semantic_batch_store::reference_world(&reuse);
    reference.store.set_payload_reuse_for_tests(false);
    let descriptor = payload_batch(&reuse);
    reference.publish(&descriptor).unwrap();
    let same = |step: &str| {
        let a = reuse
            .store
            .semantic_at(reuse.store.index_baseline().unwrap());
        let b = reference
            .store
            .semantic_at(reference.store.index_baseline().unwrap());
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(
            serde_json::to_vec(&a).unwrap(),
            serde_json::to_vec(&b).unwrap(),
            "{step}: reuse store and FULL reference differ"
        );
        assert!(!a.provenance.is_empty(), "{step}");
    };
    same("import");
    let history = [
        ("src/B.js", "function target(){ return 1; }\n"),
        ("src/B.js", "function target(){ return 2; }\n"),
        // The captured version again: both its documents hit retained payloads.
        ("src/B.js", DEFINITIONS_B),
        ("src/C.js", "function c(){ }\n// edited\n"),
        ("src/B.js", "function target(){ return 1; }\n"),
    ];
    for (step, (path, text)) in history.iter().enumerate() {
        local_edit(&reuse, path, text, &counts);
        full_edit(&reference, path, text, &counts);
        same(&format!("step {step}: {path}"));
    }
    assert!(
        reuse.count("SELECT COUNT(*) FROM semantic_payloads")
            < reference.count("SELECT COUNT(*) FROM semantic_payloads"),
        "the reference wrote a payload per document and revision, the reuse store shared them"
    );
    let seed = crate::semantic_batch_store::declaration_ids(&reuse)["run"].clone();
    let (graph, expected) = (call_graph(&reuse, &seed), call_graph(&reference, &seed));
    assert!(!graph["calls"].as_array().unwrap().is_empty(), "{graph}");
    assert_eq!(graph, expected);
}
