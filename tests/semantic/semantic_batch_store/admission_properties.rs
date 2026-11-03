//! Small stateful admission histories. All artifacts and publications use the real in-process path.
use super::*;
use proptest::prelude::*;

#[derive(Clone, Debug)]
enum Op {
    Publish(bool, bool),
    Duplicate(bool),
    BadRange(bool),
    CandidateLimit(bool, u8),
    Rollback(bool, u8),
}

fn strategy() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(
        prop_oneof![
            (any::<bool>(), any::<bool>()).prop_map(|(a, b)| Op::Publish(a, b)),
            any::<bool>().prop_map(Op::Duplicate),
            any::<bool>().prop_map(Op::BadRange),
            (any::<bool>(), 0u8..3).prop_map(|(a, b)| Op::CandidateLimit(a, b)),
            (any::<bool>(), 0u8..3).prop_map(|(a, b)| Op::Rollback(a, b)),
        ],
        1..7,
    )
}

fn descriptor(world: &World, swap: bool, page_one: bool, fault: u8) -> (PathBuf, String) {
    let (a, b) = definitions_docs();
    let (first, second) = if swap { (b, a) } else { (a, b) };
    let mut fixture = world.fixture().rows_per_page(if page_one { 1 } else { 10 });
    fixture.artifact("javascript", vec![first.clone()]);
    fixture.artifact("javascript", vec![second.clone()]);
    let second_docs = match fault {
        1 => vec![second.clone(), second],
        2 => {
            let mut bad = second;
            bad.occurrences.last_mut().unwrap().range = vec![0, 0, 999];
            vec![bad]
        }
        _ => vec![second],
    };
    for (ordinal, docs) in [(0, vec![first]), (1, second_docs)] {
        let bytes = fixture.scip_bytes("javascript", &docs);
        let mut index = scip::types::Index::parse_from_bytes(&bytes).unwrap();
        index
            .metadata
            .as_mut()
            .unwrap()
            .tool_info
            .as_mut()
            .unwrap()
            .arguments
            .push(format!("batch-{}", world.batches.get()));
        fixture.override_artifact_raw(ordinal, index.write_to_bytes().unwrap());
    }
    world.write(&fixture)
}

fn counts(world: &World) -> Vec<i64> {
    [
        "semantic_raw_batches",
        "semantic_raw_captures",
        "semantic_raw_documents",
        "semantic_raw_proofs",
        "semantic_raw_exclusions",
        "semantic_revision_raw_selections",
        "semantic_coverage",
        "semantic_import_reports",
    ]
    .iter()
    .map(|table| world.count(&format!("SELECT COUNT(*) FROM {table}")))
    .collect()
}

fn assert_model(snapshot: &trellis::semantic_evidence::SemanticSnapshot) {
    let reference = snapshot
        .references
        .iter()
        .find(|r| r.document.path == "src/A.js" && r.spelling == "target")
        .expect("caller target reference");
    assert_eq!(reference.resolution, Resolution::Resolved);
    assert!(
        matches!(&reference.declared_target, Some(trellis::semantic_evidence::Target::Internal { document, .. }) if document.path == "src/B.js")
    );
    assert!(!snapshot.provenance.is_empty());
    assert!(!snapshot.call_bindings.is_empty());
}

fn run(ops: &[Op]) {
    let world = definitions_world();
    // Begin with real selected semantic evidence, so failure checks cannot pass vacuously.
    let initial = descriptor(&world, false, false, 0);
    let (mut head, batch, _) = world.publish(&initial).unwrap();
    drop(batch);
    let first = world.store.semantic_at(head).unwrap();
    assert_model(&first);

    for op in ops {
        let before = world.store.semantic_at(head).unwrap();
        let before_counts = counts(&world);
        match *op {
            Op::Publish(swap, page_one) => {
                let input = descriptor(&world, swap, page_one, 0);
                let (next, mut batch, _) = world.publish(&input).unwrap();
                assert_eq!(next.index_revision, head.index_revision + 1, "{ops:?}");
                let stored = world.store.semantic_at(next).unwrap();
                assert_published_matches_staged(
                    &stored,
                    &testing::staged_publication(&mut batch).unwrap(),
                );
                assert_model(&stored);
                drop(batch);
                head = next;
            }
            Op::Duplicate(swap) | Op::BadRange(swap) => {
                let fault = if matches!(op, Op::Duplicate(_)) { 1 } else { 2 };
                let input = descriptor(&world, swap, true, fault);
                let err = expect_err(world.admit(&input));
                assert!(
                    matches!(
                        err.downcast_ref::<ScipCaptureError>(),
                        Some(
                            ScipCaptureError::InvalidArtifact(_)
                                | ScipCaptureError::InvalidAdmission(_)
                        )
                    ),
                    "{err:#}; {ops:?}"
                );
            }
            Op::CandidateLimit(swap, boundary) => {
                let input = descriptor(&world, swap, true, 0);
                // Two occurrence/symbol entries in B and three in A; the budget is
                // cumulative across both artifacts, including the later original.
                let limit = 4 + u64::from(boundary);
                let limits = BatchLimits {
                    candidates: limit,
                    ..BatchLimits::PRODUCTION
                };
                let mut counters = BatchCounters::default();
                let result = world.stage(&input, &limits, &mut counters);
                if limit < 5 {
                    let err = expect_err(result);
                    assert!(
                        matches!(err.downcast_ref::<ScipCaptureError>(), Some(ScipCaptureError::LimitExceeded(name)) if name == "candidate facts"),
                        "{err:#}; {ops:?}"
                    );
                } else {
                    let mut batch = result.unwrap();
                    assert_eq!(counters.candidates, 5);
                    assert_eq!(claim_count(&mut batch), 5);
                    drop(batch);
                }
            }
            Op::Rollback(swap, kind) => {
                use trellis::store::BatchPublishFault;
                let input = descriptor(&world, swap, true, 0);
                let (mut batch, joined, _) = world
                    .join_with(
                        &input,
                        &BatchLimits::PRODUCTION,
                        DestinationMutations::default(),
                    )
                    .unwrap();
                let leader = world.store.publication_for_tests().unwrap();
                let fault = match kind {
                    0 => BatchPublishFault::SqlFault,
                    1 => BatchPublishFault::InsertDiskFull,
                    _ => BatchPublishFault::PrecommitCancel,
                };
                let error = world
                    .store
                    .publish_semantic_batch_with_fault_for_tests(
                        &joined,
                        batch.stage_mut(),
                        head,
                        &leader,
                        &world.cancel,
                        fault,
                    )
                    .unwrap_err();
                match kind {
                    0 => assert!(format!("{error:#}").contains("NOT NULL")),
                    1 => assert!(error.is::<trellis::store::StorageFull>()),
                    _ => assert!(format!("{error:#}").contains("cancelled")),
                }
                world
                    .cancel
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                assert_eq!(world.store.index_baseline().unwrap(), head, "{ops:?}");
                assert_eq!(world.store.semantic_at(head).unwrap(), before, "{ops:?}");
                assert_eq!(counts(&world), before_counts, "{ops:?}");
                let next = world
                    .store
                    .publish_semantic_batch(
                        &joined,
                        batch.stage_mut(),
                        head,
                        &leader,
                        &world.cancel,
                    )
                    .unwrap();
                assert_eq!(next.index_revision, head.index_revision + 1);
                let stored = world.store.semantic_at(next).unwrap();
                assert_model(&stored);
                assert_published_matches_staged(
                    &stored,
                    &testing::staged_publication(&mut batch).unwrap(),
                );
                drop(batch);
                head = next;
            }
        }
        assert!(world.stage_dirs().is_empty(), "{ops:?}");
        if !matches!(op, Op::Publish(..) | Op::Rollback(..)) {
            assert_eq!(world.store.index_baseline().unwrap(), head, "{ops:?}");
            assert_eq!(world.store.semantic_at(head).unwrap(), before, "{ops:?}");
            assert_eq!(counts(&world), before_counts, "{ops:?}");
        }
    }
}

#[test]
fn controls() {
    run(&[
        Op::Duplicate(false),
        Op::BadRange(true),
        Op::CandidateLimit(false, 0),
        Op::CandidateLimit(true, 1),
        Op::CandidateLimit(false, 2),
        Op::Publish(true, true),
        Op::Duplicate(true),
        Op::Rollback(false, 0),
        Op::Rollback(true, 1),
        Op::Rollback(false, 2),
        Op::Publish(false, false),
    ]);
}

#[test]
fn both_original_orders_and_limit_edges() {
    for swap in [false, true] {
        run(&[
            Op::Duplicate(swap),
            Op::BadRange(swap),
            Op::CandidateLimit(swap, 0),
            Op::CandidateLimit(swap, 1),
            Op::CandidateLimit(swap, 2),
            Op::Publish(swap, true),
        ]);
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("TRELLIS_ADMISSION_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(16), ..ProptestConfig::default() })]
    #[test]
    fn admission_histories(ops in strategy()) {
        run(&ops);
    }
}
