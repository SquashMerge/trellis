//! Production matrix reads: exact membership changes, admitted pins and typed failures.
use super::*;
use crate::indexer::{IndexOptions, index_workspace_bundle};
use crate::search_rank::SearchCandidate;
use std::fs;

struct Fixture {
    _state: tempfile::TempDir,
    root: tempfile::TempDir,
    store: Store,
    cancel: CancelFlag,
}
impl Fixture {
    fn new() -> Self {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("edited.rs"), "fn orchard() { apple(); }\n").unwrap();
        fs::write(root.path().join("steady.rs"), "fn orchard_steady() {}\n").unwrap();
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        Self {
            _state: state,
            root,
            store,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }
    fn publish(&self) -> IndexPin {
        let (graph, native, capture) = index_workspace_bundle(
            &IndexOptions::new(self.root.path().to_owned()),
            self.store.root_id(),
            &self.cancel,
            |_| {},
        )
        .unwrap();
        self.store
            .publish_native(
                &graph,
                &capture,
                &native,
                &self.store.writer().unwrap(),
                self.store.index_baseline().unwrap(),
                &self.cancel,
            )
            .unwrap()
    }
    fn edit(&self) {
        fs::write(
            self.root.path().join("edited.rs"),
            "fn orchard() { pear(); } fn orchard_helper() {}\n",
        )
        .unwrap();
    }
    fn head(&self, pin: IndexPin) -> Arc<[SearchCandidate]> {
        let response = self.store.evidence_response().unwrap();
        let (actual, cards) = response.mcp_search_head(Some(pin), &mut || Ok(())).unwrap();
        assert_eq!(actual, pin);
        response.finish(cards).unwrap()
    }
    fn delta(&self, old: IndexPin, new: IndexPin) -> SearchHeadDelta {
        let response = self.store.evidence_response().unwrap();
        let delta = response
            .mcp_search_head_delta(old, new, &mut || Ok(()))
            .unwrap();
        response.finish(delta).unwrap()
    }
}

fn patched(cards: &[SearchCandidate], delta: &SearchHeadDelta) -> Vec<SearchCandidate> {
    let mut cards: Vec<_> = cards
        .iter()
        .filter(|card| !delta.removed_version_ids.contains(&card.key.version_id))
        .cloned()
        .chain(delta.added.iter().cloned())
        .collect();
    cards.sort_by(|a, b| a.key.syntax_id.cmp(&b.key.syntax_id));
    assert_eq!(cards.len(), delta.head_card_count);
    cards
}

#[test]
fn two_document_edits_and_deletion_patch_exactly_without_retained_candidates() {
    let fixture = Fixture::new();
    let first = fixture.publish();
    let original = fixture.head(first);
    assert_eq!(original.len(), 2);
    fixture.edit();
    let second = fixture.publish();
    let edited = fixture.head(second);
    let change = fixture.delta(first, second);
    assert_eq!(change.pin, second);
    assert_eq!(change.removed_version_ids.len(), 1);
    assert_eq!(change.added.len(), 2);
    assert!(change.added.iter().all(|card| !original.contains(card)));
    assert_eq!(patched(&original, &change), edited.as_ref());
    let unchanged: Vec<_> = original
        .iter()
        .filter(|card| edited.contains(card))
        .collect();
    assert_eq!(
        unchanged.len(),
        1,
        "the other document version is unchanged"
    );

    // The compatibility API and split production reads have identical output.
    let response = fixture.store.evidence_response().unwrap();
    let count = response
        .mcp_search_head_count(second, &mut || Ok(()))
        .unwrap();
    assert_eq!(count, edited.len());
    let policy = response
        .mcp_search_policy(
            "orchard",
            second,
            crate::search_rank::SearchRole::Any,
            &mut || Ok(()),
        )
        .unwrap();
    let lexical = response
        .mcp_search_lexical("orchard", second, count, &policy, &mut || Ok(()))
        .unwrap();
    assert!(!lexical.is_empty());
    assert!(
        lexical
            .iter()
            .all(|key| edited.iter().any(|card| &card.key == key))
    );
    let combined = response
        .mcp_search_candidates("orchard", Some(second), &policy, &mut || Ok(()))
        .unwrap();
    assert_eq!(combined.pin, second);
    assert_eq!(combined.cards, edited.as_ref());
    assert_eq!(combined.lexical, lexical);
    response.finish(()).unwrap();
    drop(response);

    fs::remove_file(fixture.root.path().join("edited.rs")).unwrap();
    let third = fixture.publish();
    let deleted = fixture.head(third);
    assert_eq!(deleted.len(), 1);
    let deletion = fixture.delta(second, third);
    assert_eq!(deletion.removed_version_ids.len(), 1);
    assert!(deletion.added.is_empty());
    assert_eq!(patched(&edited, &deletion), deleted.as_ref());
    // Deltas may skip intervening retained revisions.
    let across = fixture.delta(first, third);
    assert_eq!(patched(&original, &across), deleted.as_ref());
    assert_eq!(
        fixture
            .store
            .retained_native_snapshot_at(first)
            .unwrap()
            .0
            .search_cards
            .len(),
        2
    );
    let noop = fixture.delta(third, third);
    assert!(noop.added.is_empty());
    assert!(noop.removed_version_ids.is_empty());
    assert_eq!(noop.head_card_count, 1);
}

#[test]
fn head_admission_precedes_candidate_decoding_and_delta_materialization() {
    #[derive(Debug)]
    struct AdmissionDeclined;
    impl std::fmt::Display for AdmissionDeclined {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("candidate admission declined")
        }
    }
    impl std::error::Error for AdmissionDeclined {}
    let fixture = Fixture::new();
    let first = fixture.publish();
    let response = fixture.store.evidence_response().unwrap();
    let mut count = None;
    let error = response
        .mcp_search_head_admitted(Some(first), &mut || Ok(()), &mut |rows| {
            count = Some(rows);
            Err(AdmissionDeclined.into())
        })
        .unwrap_err();
    assert_eq!(count, Some(2));
    assert!(error.is::<AdmissionDeclined>());
    response.finish(()).unwrap();
    drop(response);
    fixture.edit();
    let second = fixture.publish();
    let response = fixture.store.evidence_response().unwrap();
    let mut counts = None;
    let error = response
        .mcp_search_head_delta_admitted(first, second, &mut || Ok(()), &mut |added, removed| {
            counts = Some((added, removed));
            Err(AdmissionDeclined.into())
        })
        .unwrap_err();
    assert_eq!(counts, Some((2, 1)));
    assert!(error.is::<AdmissionDeclined>());
    response.finish(()).unwrap();
}

#[test]
fn split_reads_reject_wrong_head_and_invalid_previous_pins() {
    let fixture = Fixture::new();
    let pin = fixture.publish();
    let response = fixture.store.evidence_response().unwrap();
    let wrong = IndexPin {
        index_revision: pin.index_revision + 1,
        ..pin
    };
    for error in [
        response
            .mcp_search_head(Some(wrong), &mut || Ok(()))
            .unwrap_err(),
        response
            .mcp_search_head_count(wrong, &mut || Ok(()))
            .unwrap_err(),
        response
            .mcp_search_lexical(
                "orchard",
                wrong,
                2,
                &crate::search_rank::SearchPolicy::default(),
                &mut || Ok(()),
            )
            .unwrap_err(),
        response
            .mcp_search_head_delta(pin, wrong, &mut || Ok(()))
            .unwrap_err(),
        response
            .mcp_search_head_delta(wrong, pin, &mut || Ok(()))
            .unwrap_err(),
        response
            .mcp_search_head_delta(
                IndexPin {
                    index_revision: 0,
                    ..pin
                },
                pin,
                &mut || Ok(()),
            )
            .unwrap_err(),
    ] {
        assert!(
            error.to_string().starts_with("revision conflict:"),
            "{error:#}"
        );
        assert!(!error.is::<SearchDeltaColdRebuild>());
    }
}

#[test]
fn generation_change_and_released_old_basis_request_explicit_cold_rebuild() {
    let fixture = Fixture::new();
    fixture.store.set_retention_clock_for_tests(1_000, 0);
    let first = fixture.publish();
    fixture.edit();
    fixture.store.set_retention_clock_for_tests(1_001, 1);
    let second = fixture.publish();
    {
        let response = fixture.store.evidence_response().unwrap();
        let foreign = IndexPin {
            index_generation: uuid::Uuid::new_v4(),
            ..first
        };
        let error = response
            .mcp_search_head_delta(foreign, second, &mut || Ok(()))
            .unwrap_err();
        assert!(error.is::<SearchDeltaColdRebuild>(), "{error:#}");
        let (_, cold) = response
            .mcp_search_head(Some(second), &mut || Ok(()))
            .unwrap();
        assert_eq!(response.finish(cold).unwrap().len(), 3);
    }
    fixture.store.set_retention_clock_for_tests(1_902, 902);
    fixture
        .store
        .release_revision(first, &fixture.store.writer().unwrap())
        .unwrap();
    let response = fixture.store.evidence_response().unwrap();
    let error = response
        .mcp_search_head_delta(first, second, &mut || Ok(()))
        .unwrap_err();
    assert!(error.is::<SearchDeltaColdRebuild>(), "{error:#}");
    let (_, cold) = response
        .mcp_search_head(Some(second), &mut || Ok(()))
        .unwrap();
    assert_eq!(response.finish(cold).unwrap().len(), 3);
}

#[derive(Debug)]
struct Stopped;
impl std::fmt::Display for Stopped {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("typed test cancellation")
    }
}
impl std::error::Error for Stopped {}

fn stop_after(checks: usize) -> impl FnMut() -> Result<()> {
    let mut remaining = checks;
    move || {
        remaining -= 1;
        if remaining == 0 {
            Err(Stopped.into())
        } else {
            Ok(())
        }
    }
}

#[test]
fn cancellation_preserves_original_type_and_discards_work_in_every_read() {
    let fixture = Fixture::new();
    let first = fixture.publish();
    fixture.edit();
    let second = fixture.publish();
    let response = fixture.store.evidence_response().unwrap();
    for error in [
        response
            .mcp_search_head(Some(second), &mut stop_after(2))
            .unwrap_err(),
        response
            .mcp_search_head_delta(first, second, &mut stop_after(2))
            .unwrap_err(),
        response
            .mcp_search_head_count(second, &mut stop_after(2))
            .unwrap_err(),
        response
            .mcp_search_lexical(
                "orchard",
                second,
                3,
                &crate::search_rank::SearchPolicy::default(),
                &mut stop_after(2),
            )
            .unwrap_err(),
    ] {
        assert!(error.is::<Stopped>(), "{error:#}");
    }
    // Cancellation unregisters progress and never poisons another read.
    assert_eq!(
        response
            .mcp_search_head_count(second, &mut || Ok(()))
            .unwrap(),
        3
    );
    response.finish(()).unwrap();
}

#[test]
fn retained_revision_corruption_is_not_a_cold_rebuild() {
    let fixture = Fixture::new();
    let first = fixture.publish();
    fixture.edit();
    let second = fixture.publish();
    {
        let db = fixture.store.cache_write().unwrap();
        db.execute(
            "UPDATE revision_producer_bindings SET binding_sha=?1 WHERE revision_id=?2",
            params![
                "0".repeat(64),
                format!("pin:v1:{}:{}", first.index_generation, first.index_revision)
            ],
        )
        .unwrap();
    }
    let response = fixture.store.evidence_response().unwrap();
    let error = response
        .mcp_search_head_delta(first, second, &mut || Ok(()))
        .unwrap_err();
    assert!(error.is::<SelectedIntegrity>(), "{error:#}");
    assert!(!error.is::<SearchDeltaColdRebuild>());
}

#[test]
fn invalid_added_hash_and_native_id_fail_whole_delta() {
    for fault in ["hash", "syntax_id"] {
        let fixture = Fixture::new();
        let first = fixture.publish();
        fixture.edit();
        let second = fixture.publish();
        {
            let db = fixture.store.cache_write().unwrap();
            // Exercise corrupt decoding, not constraints or hostile mutation.
            db.pragma_update(None, "foreign_keys", "OFF").unwrap();
            let sql = if fault == "hash" {
                "UPDATE native_version_search_cards SET card_hash=?1 WHERE rowid=(
                 SELECT max(c.rowid) FROM native_version_search_cards c
                 JOIN revision_documents m ON m.vid=c.vid WHERE m.path='edited.rs'
                 AND m.revision_id=?2)"
            } else {
                "UPDATE native_version_search_cards SET syntax_id=?1 WHERE rowid=(
                 SELECT max(c.rowid) FROM native_version_search_cards c
                 JOIN revision_documents m ON m.vid=c.vid WHERE m.path='edited.rs'
                 AND m.revision_id=?2)"
            };
            let key = format!(
                "pin:v1:{}:{}",
                second.index_generation, second.index_revision
            );
            if fault == "hash" {
                db.execute(sql, params!["g".repeat(64), key]).unwrap();
            } else {
                db.execute(sql, params![vec![0_u8; 15], key]).unwrap();
            }
        }
        let response = fixture.store.evidence_response().unwrap();
        let error = response
            .mcp_search_head_delta(first, second, &mut || Ok(()))
            .unwrap_err();
        assert!(
            matches!(recovery_class(&error), RecoveryClass::Rebuild),
            "{fault}: {error:#}"
        );
        assert!(!error.is::<SearchDeltaColdRebuild>());
    }
}

#[test]
fn orphan_added_card_cannot_be_admitted_by_warm_delta() {
    let fixture = Fixture::new();
    let first = fixture.publish();
    fixture.edit();
    let second = fixture.publish();
    {
        let db = fixture.store.cache_write().unwrap();
        db.pragma_update(None, "foreign_keys", "OFF").unwrap();
        assert_eq!(
            db.execute(
                "DELETE FROM native_version_declarations WHERE rowid=(
                 SELECT d.rowid FROM native_version_declarations d
                 JOIN native_version_search_cards c ON c.vid=d.vid AND c.syntax_id=d.syntax_id
                 JOIN revision_documents m ON m.vid=c.vid
                 WHERE m.path='edited.rs' AND m.revision_id=?1
                 ORDER BY c.syntax_id LIMIT 1)",
                [format!(
                    "pin:v1:{}:{}",
                    second.index_generation, second.index_revision
                )],
            )
            .unwrap(),
            1
        );
    }
    let response = fixture.store.evidence_response().unwrap();
    let cold_error = response
        .mcp_search_head(Some(second), &mut || Ok(()))
        .unwrap_err();
    assert!(cold_error.is::<SelectedIntegrity>(), "{cold_error:#}");
    let delta_error = response
        .mcp_search_head_delta(first, second, &mut || Ok(()))
        .unwrap_err();
    assert!(delta_error.is::<SelectedIntegrity>(), "{delta_error:#}");
    assert!(!delta_error.is::<SearchDeltaColdRebuild>());
}

#[test]
fn canonical_head_count_matches_eligible_cards_not_all_native_declarations() {
    let fixture = Fixture::new();
    fixture.publish();
    fs::write(
        fixture.root.path().join("edited.rs"),
        "const APPLES: usize = 3; fn orchard() {} fn orchard_helper() {}\n",
    )
    .unwrap();
    let pin = fixture.publish();
    let cards = fixture.head(pin);
    assert_eq!(cards.len(), 3);
    let (native, _) = fixture.store.retained_native_snapshot_at(pin).unwrap();
    assert!(native.declarations.len() > cards.len());

    let response = fixture.store.evidence_response().unwrap();
    let count = response.mcp_search_head_count(pin, &mut || Ok(())).unwrap();
    assert_eq!(count, cards.len());
    let lexical = response
        .mcp_search_lexical(
            "orchard",
            pin,
            count,
            &response
                .mcp_search_policy(
                    "orchard",
                    pin,
                    crate::search_rank::SearchRole::Any,
                    &mut || Ok(()),
                )
                .unwrap(),
            &mut || Ok(()),
        )
        .unwrap();
    assert!(!lexical.is_empty());
    assert!(
        response
            .mcp_search_lexical(
                "a",
                pin,
                count,
                &crate::search_rank::SearchPolicy::default(),
                &mut || Ok(())
            )
            .unwrap()
            .is_empty()
    );
    response.finish(()).unwrap();
}

#[test]
fn cold_count_detects_missing_and_extra_fts_rows_even_without_query_terms() {
    for extra in [false, true] {
        let fixture = Fixture::new();
        let pin = fixture.publish();
        let expected = fixture.head(pin).len();
        {
            let db = fixture.store.cache_write().unwrap();
            let changed = db
                .execute(
                    if extra {
                        "INSERT INTO head_search_fts(terms,vid,syntax_id)
                         SELECT terms,vid,syntax_id FROM head_search_fts LIMIT 1"
                    } else {
                        "DELETE FROM head_search_fts
                         WHERE rowid=(SELECT min(rowid) FROM head_search_fts)"
                    },
                    [],
                )
                .unwrap();
            assert_eq!(changed, 1);
        }
        let response = fixture.store.evidence_response().unwrap();
        let count = response.mcp_search_head_count(pin, &mut || Ok(())).unwrap();
        assert_eq!(count, expected, "the canonical count must not trust FTS");
        for query in ["orchard", "a"] {
            let error = response
                .mcp_search_lexical(
                    query,
                    pin,
                    count,
                    &crate::search_rank::SearchPolicy::default(),
                    &mut || Ok(()),
                )
                .unwrap_err();
            assert!(error.is::<SelectedIntegrity>(), "{error:#}");
            assert!(!error.is::<SearchDeltaColdRebuild>());
        }
    }
}

#[test]
fn lexical_required_corpus_faults_fail_even_without_query_terms() {
    for drop_table in [false, true] {
        let fixture = Fixture::new();
        let pin = fixture.publish();
        {
            let db = fixture.store.cache_write().unwrap();
            db.execute_batch(if drop_table {
                "DROP TABLE head_search_fts"
            } else {
                "DELETE FROM head_search_fts WHERE rowid=(SELECT min(rowid) FROM head_search_fts)"
            })
            .unwrap();
        }
        let response = match fixture.store.evidence_response() {
            Ok(response) => response,
            Err(error) => {
                assert!(drop_table, "{error:#}");
                assert!(
                    matches!(recovery_class(&error), RecoveryClass::Rebuild),
                    "{error:#}"
                );
                continue;
            }
        };
        let error = response
            .mcp_search_lexical(
                "a",
                pin,
                2,
                &crate::search_rank::SearchPolicy::default(),
                &mut || Ok(()),
            )
            .unwrap_err();
        if drop_table {
            assert!(error.is::<rusqlite::Error>(), "{error:#}");
        } else {
            assert!(error.is::<SelectedIntegrity>(), "{error:#}");
        }
        assert!(!error.is::<SearchDeltaColdRebuild>());
        assert!(response.mcp_search_head(Some(pin), &mut || Ok(())).is_err());
    }
}

#[test]
fn exact_policy_and_lexical_plans_use_indexed_candidate_joins() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.path().join("edited.rs"),
        "fn orchard() { unique_needle(); } fn orchard_other() {}\n",
    )
    .unwrap();
    let pin = fixture.publish();
    let response = fixture.store.evidence_response().unwrap();
    let key = format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision);
    let lookup_keys = json(&search_exact_lookup_keys("orchard")).unwrap();
    let policy_plan: Vec<String> = response
        .db
        .prepare(&format!("EXPLAIN QUERY PLAN {SEARCH_POLICY_SQL}"))
        .unwrap()
        .query_map(
            params![key, lookup_keys, "orchard", Option::<&str>::None],
            |row| row.get(3),
        )
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(
        policy_plan
            .iter()
            .any(|step| step.contains("SEARCH d USING INDEX native_version_declarations_lookup")),
        "{policy_plan:?}"
    );
    assert!(
        policy_plan.iter().all(|step| !step.starts_with("SCAN d")),
        "{policy_plan:?}"
    );

    for (query, expected_count) in [("unique_needle", 1), ("orchard", 3)] {
        let policy = response
            .mcp_search_policy(query, pin, crate::search_rank::SearchRole::Any, &mut || {
                Ok(())
            })
            .unwrap();
        let expression = crate::search_lexical::query_expression(&response.db, query).unwrap();
        let sql = search_lexical_sql(expression.is_some());
        let plan: Vec<String> = response
            .db
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(
                params![
                    key,
                    query,
                    Option::<&str>::None,
                    expression,
                    crate::search_rank::CANDIDATE_DEPTH as i64
                ],
                |row| row.get(3),
            )
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let fts = plan
            .iter()
            .position(|step| step.starts_with("SCAN head_search_fts VIRTUAL TABLE INDEX"))
            .unwrap_or_else(|| panic!("{plan:?}"));
        for alias in ["c", "m", "d"] {
            let indexed = plan
                .iter()
                .position(|step| step.starts_with(&format!("SEARCH {alias} USING")))
                .unwrap_or_else(|| panic!("{plan:?}"));
            assert!(fts < indexed, "{plan:?}");
            assert!(
                plan.iter()
                    .all(|step| !step.starts_with(&format!("SCAN {alias} "))),
                "{plan:?}"
            );
        }
        assert!(
            plan.iter().all(|step| !step.contains("AUTOMATIC")),
            "{plan:?}"
        );
        let lexical = response
            .mcp_search_lexical(query, pin, 3, &policy, &mut || Ok(()))
            .unwrap();
        assert_eq!(lexical.len(), expected_count, "{query}");
        let hydrated = response
            .mcp_search_hydrate(&lexical, pin, &mut || Ok(()))
            .unwrap();
        assert!(hydrated.symbols.iter().all(|hit| {
            hit.symbol
                .declaration
                .name
                .as_deref()
                .unwrap()
                .starts_with("orchard")
        }));
    }
    response.finish(()).unwrap();
}

#[test]
fn candidate_roles_follow_selected_declarations_in_head_and_delta() {
    use crate::native_evidence::DeclarationRole;
    let fixture = Fixture::new();
    let first = fixture.publish();
    let original = fixture.head(first);
    assert!(
        original
            .iter()
            .all(|card| card.role == DeclarationRole::Production)
    );
    fs::write(
        fixture.root.path().join("edited.rs"),
        "#[test] fn orchard() {} fn orchard_helper() {}\n",
    )
    .unwrap();
    let second = fixture.publish();
    let delta = fixture.delta(first, second);
    assert_eq!(
        delta
            .added
            .iter()
            .filter(|card| card.role == DeclarationRole::Test)
            .count(),
        1
    );
    assert_eq!(
        delta
            .added
            .iter()
            .filter(|card| card.role == DeclarationRole::Production)
            .count(),
        1
    );
    assert_eq!(patched(&original, &delta), fixture.head(second).as_ref());
}

#[test]
fn invalid_selected_role_fails_whole_head_and_delta() {
    let fixture = Fixture::new();
    let first = fixture.publish();
    fixture.edit();
    let second = fixture.publish();
    {
        let db = fixture.store.cache_write().unwrap();
        db.pragma_update(None, "ignore_check_constraints", "ON")
            .unwrap();
        db.execute(
            "UPDATE native_version_declarations SET role='invalid' WHERE vid IN
             (SELECT vid FROM revision_documents WHERE path='edited.rs' AND revision_id=?1)",
            [format!(
                "pin:v1:{}:{}",
                second.index_generation, second.index_revision
            )],
        )
        .unwrap();
    }
    let response = fixture.store.evidence_response().unwrap();
    for error in [
        response
            .mcp_search_head(Some(second), &mut || Ok(()))
            .unwrap_err(),
        response
            .mcp_search_head_delta(first, second, &mut || Ok(()))
            .unwrap_err(),
    ] {
        assert!(error.is::<SelectedIntegrity>(), "{error:#}");
        assert!(
            error.to_string().contains("invalid declaration role"),
            "{error:#}"
        );
    }
}

#[test]
fn exact_lookup_prefilter_includes_raw_javascript_fallback() {
    let query = r"invalid\uQQQQ";
    assert!(crate::native_evidence::lookup("javascript", query).is_err());
    assert!(search_exact_lookup_keys(query).contains(query));
    assert!(search_exact_lookup_keys("r#type").contains("type"));
    assert!(search_exact_lookup_keys("Kelvin").contains("Kelvin"));
    assert!(search_exact_lookup_keys(r"caf\u00e9").contains("café"));
}
