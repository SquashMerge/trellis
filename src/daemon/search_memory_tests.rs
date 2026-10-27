//! Normal-size deterministic checks for daemon matrix admission and lifetimes.
use super::*;
use crate::{
    daemon::search_head::{HeadChange, HeadUpdate},
    search_rank::{SearchCandidate, SearchCandidateKey, SearchMatrix},
};

struct Source;
impl FillSource for Source {
    fn page(&self, _: Option<IndexPin>, _: Option<&str>) -> Result<HeadSearchCards> {
        unreachable!("admission tests build captured heads directly")
    }
}
fn pin(revision: u64) -> IndexPin {
    IndexPin {
        index_generation: uuid::Uuid::nil(),
        index_revision: revision,
    }
}
fn cards(rows: usize) -> Arc<[SearchCandidate]> {
    (0..rows)
        .map(|row| SearchCandidate {
            key: SearchCandidateKey {
                version_id: row as i64,
                syntax_id: format!("sid-{row}"),
            },
            card_hash: format!("{row:064x}"),
        })
        .collect::<Vec<_>>()
        .into()
}
fn update(revision: u64, rows: usize) -> HeadUpdate {
    HeadUpdate {
        pin: pin(revision),
        declarations: rows,
        change: HeadChange::Full(cards(rows)),
    }
}
fn core(limit: usize) -> Arc<Core> {
    Arc::new(Core {
        budget: Arc::new(MatrixBudget(Mutex::new(BudgetState { limit, used: 0 }))),
        ..Core::default()
    })
}
fn register(core: &Arc<Core>) -> (Arc<dyn FillSource>, FillRegistration) {
    let source: Arc<dyn FillSource> = Arc::new(Source);
    let registration = core.register_source(Arc::downgrade(&source));
    (source, registration)
}
fn used(core: &Core) -> usize {
    core.budget.0.lock().unwrap().used
}
fn build(head: &HeadSlot, revision: u64, rows: usize) -> Result<()> {
    head.build(update(revision, rows), "model", 1, None, &mut || Ok(()))
}
#[test]
fn oversized_head_is_lexical_only_without_allocating_or_evicting_others() {
    let one = SearchMatrix::estimated_new_bytes(&cards(1)) + MATRIX_FILL_SCRATCH;
    let core = core(one);
    let (_a, resident) = register(&core);
    build(&resident.head, 1, 1).unwrap();
    let before = used(&core);
    let (_b, oversized) = register(&core);
    let error = build(&oversized.head, 2, 32).unwrap_err();
    assert!(error.is::<MemoryDeclined>());
    assert!(!oversized.head.resident().unwrap());
    assert!(
        !oversized.head.needs_build(),
        "unchanged oversized head must not loop"
    );
    assert!(resident.head.resident().unwrap());
    assert_eq!(used(&core), before);
    // A changed pin can become small enough to admit.
    build(&oversized.head, 3, 1).unwrap();
    assert!(oversized.head.resident().unwrap());
    assert!(!resident.head.resident().unwrap());
}
#[test]
fn pressure_evicts_the_least_recently_queried_head_not_the_last_registered() {
    let estimate = SearchMatrix::estimated_new_bytes(&cards(1)) + MATRIX_FILL_SCRATCH;
    let probe = HeadSlot::new(Weak::new(), 99, Weak::new());
    build(&probe, 1, 1).unwrap();
    let resident_bytes = probe.storage_bytes() + MATRIX_FILL_SCRATCH;
    let core = core(estimate + resident_bytes);
    let (_a, a) = register(&core);
    let (_b, b) = register(&core);
    let (_c, c) = register(&core);
    build(&a.head, 1, 1).unwrap();
    build(&b.head, 2, 1).unwrap();
    a.head.resident().unwrap();
    b.head.resident().unwrap();
    // Rank, rather than registration or vector filling, makes A most recent.
    a.head
        .rank(pin(1), "model", 1, &[], None, &mut || Ok(()))
        .unwrap();
    build(&c.head, 3, 1).unwrap();
    assert!(a.head.resident().unwrap());
    assert!(!b.head.resident().unwrap());
    assert!(c.head.resident().unwrap());
    assert!(used(&core) <= estimate + resident_bytes);
}
#[test]
fn startup_stops_at_cap_without_evicting_an_active_matrix() {
    let estimate = SearchMatrix::estimated_new_bytes(&cards(1)) + MATRIX_FILL_SCRATCH;
    let core = core(estimate);
    let (_a, active) = register(&core);
    build(&active.head, 1, 1).unwrap();
    let (_b, startup) = register(&core);
    let before = used(&core);
    assert!(
        startup
            .head
            .build_mode(update(2, 1), "model", 1, None, true, &mut || Ok(()))
            .unwrap_err()
            .is::<MemoryDeclined>()
    );
    assert!(core.startup_at_cap());
    assert!(active.head.resident().unwrap());
    assert_eq!(used(&core), before);
    // Demand is still admitted, and can replace the active LRU.
    build(&startup.head, 2, 1).unwrap();
    assert!(startup.head.resident().unwrap());
}
#[test]
fn unpublished_delta_keeps_the_old_charge_and_releases_everything_on_cancel() {
    let core = core(1_000_000);
    let (_source, registration) = register(&core);
    build(&registration.head, 1, 2).unwrap();
    let old = used(&core);
    let mut observed_unpublished = false;
    let result = registration.head.build(
        HeadUpdate {
            pin: pin(2),
            declarations: 2,
            change: HeadChange::Delta {
                removed_version_ids: vec![0],
                added: cards(1),
            },
        },
        "model",
        1,
        None,
        &mut || {
            assert!(used(&core) <= 1_000_000);
            if !registration.head.resident()? {
                observed_unpublished = true;
                assert!(
                    used(&core) > old,
                    "old allocation and staging remain charged after take"
                );
                return Err(EmbedError::Cancelled.into());
            }
            Ok(())
        },
    );
    assert!(result.unwrap_err().is::<EmbedError>());
    assert!(observed_unpublished);
    assert_eq!(used(&core), 0);
}
#[test]
fn cold_replacement_and_stop_keep_inflight_bytes_until_the_build_drops() {
    let core = core(1_000_000);
    let (_source, registration) = register(&core);
    build(&registration.head, 1, 1).unwrap();
    let old = used(&core);
    let mut stopped = false;
    let result = registration
        .head
        .build(update(2, 2), "model", 1, None, &mut || {
            if used(&core) > old && !stopped {
                stopped = true;
                registration.head.stop();
                assert!(
                    used(&core) > 0,
                    "unpublished allocation remains reserved after stop"
                );
            }
            Ok(())
        });
    assert!(result.unwrap_err().is::<EmbedError>());
    assert!(stopped);
    assert_eq!(used(&core), 0);
}
#[test]
fn registration_drop_idle_clear_and_failed_build_release_reservations() {
    let core = core(1_000_000);
    let (_source, registration) = register(&core);
    build(&registration.head, 1, 1).unwrap();
    assert!(used(&core) > 0);
    registration.head.clear();
    assert_eq!(used(&core), 0);
    build(&registration.head, 2, 1).unwrap();
    drop(registration);
    assert_eq!(used(&core), 0);
    let (_source, registration) = register(&core);
    let mut update = update(3, 1);
    update.declarations = 2;
    assert!(
        registration
            .head
            .build(update, "model", 1, None, &mut || Ok(()))
            .is_err()
    );
    assert_eq!(used(&core), 0);
}
#[test]
fn known_fault_is_not_hidden_by_nonresidency() {
    let core = core(1);
    let (_source, registration) = register(&core);
    registration
        .head
        .finish(&Err(crate::search_vectors::StoreUnavailable.into()));
    assert!(
        build(&registration.head, 1, 1)
            .unwrap_err()
            .is::<MemoryDeclined>()
    );
    assert!(
        registration
            .head
            .resident()
            .unwrap_err()
            .is::<crate::search_vectors::StoreUnavailable>()
    );
}

#[test]
fn cold_streaming_estimate_covers_normal_build_without_tripling_vector_storage() {
    let candidates = cards(64);
    let estimate = SearchMatrix::estimated_new_bytes(&candidates);
    let matrix = SearchMatrix::new(candidates, &mut || Ok(())).unwrap();
    let resident = matrix.storage_bytes();
    assert!(estimate >= resident);
    // Project a representative 820 MiB resident head using normal-size rows.
    // No production-size input or allocation is used in this arithmetic check.
    let projected_cold = (820_u64 * 1024 * 1024).div_ceil(resident as u64) * estimate as u64;
    assert!(projected_cold < MATRIX_RAM_LIMIT as u64);
}

struct BatchFactory;
struct BatchEmbedder;
impl crate::search_embed::SearchEmbedderFactory for BatchFactory {
    fn load(&self, _: &EmbeddingControl<'_>) -> Result<Box<dyn SearchEmbedder>, EmbedError> {
        Ok(Box::new(BatchEmbedder))
    }
}
impl SearchEmbedder for BatchEmbedder {
    fn embed(
        &self,
        texts: &[String],
        _: InputKind,
        control: &EmbeddingControl<'_>,
    ) -> Result<Vec<crate::search_embed::QuantizedVector>, EmbedError> {
        control.check()?;
        Ok(texts
            .iter()
            .map(|_| crate::search_embed::QuantizedVector {
                values: vec![1; DIMENSIONS],
            })
            .collect())
    }
}
struct BatchSource;
impl FillSource for BatchSource {
    fn head(
        &self,
        _: Option<IndexPin>,
        _: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Option<HeadUpdate>> {
        Ok(Some(update(1, BATCH)))
    }
    fn page(&self, _: Option<IndexPin>, after: Option<&str>) -> Result<HeadSearchCards> {
        Ok(HeadSearchCards {
            pin: pin(1),
            cards: if after.is_some() {
                Vec::new()
            } else {
                cards(BATCH)
                    .iter()
                    .map(|candidate| crate::store::HeadSearchCard {
                        syntax_id: candidate.key.syntax_id.clone(),
                        card_hash: candidate.card_hash.clone(),
                        card_text: "normal-size staging allowance fixture".into(),
                    })
                    .collect()
            },
        })
    }
}
#[test]
fn exact_cold_limit_keeps_fill_allowance_and_completes_without_another_query() {
    let directory = tempfile::tempdir().unwrap();
    let roots = crate::store::topology::TopologyRoots::isolated_for_tests(
        directory.path().join("cache"),
        directory.path().join("data"),
    );
    let service =
        Arc::new(SearchService::with_embedder_for_tests(roots, Arc::new(BatchFactory)).unwrap());
    let limit = SearchMatrix::estimated_new_bytes(&cards(BATCH)) + MATRIX_FILL_SCRATCH;
    let owner = SearchFill::start_with_limit_for_tests(service.clone(), limit).unwrap();
    let source: Arc<dyn FillSource> = Arc::new(BatchSource);
    let registration = owner.register_source(Arc::downgrade(&source));
    registration.head.request();
    owner.wait_until_idle_for_tests();
    assert!(owner.matrix_storage_bytes() <= limit);
    let matrix_bytes = registration.head.storage_bytes();
    assert_eq!(
        owner.matrix_storage_bytes(),
        matrix_bytes + MATRIX_FILL_SCRATCH
    );
    let model_id = model_id_for_card_format(CardFormat::V1.as_str());
    let ranked = registration
        .head
        .rank(
            pin(1),
            &model_id,
            service.cache_epoch(),
            &[],
            None,
            &mut || Ok(()),
        )
        .unwrap()
        .unwrap();
    assert_eq!(ranked.embedded, BATCH as u64);
}

#[test]
fn fill_staging_charge_outlives_unregistration_until_owner_job_drops_it() {
    let core = core(1_000_000);
    let (_source, registration) = register(&core);
    build(&registration.head, 1, 1).unwrap();
    let scratch = registration.head.fill_scratch().unwrap();
    drop(registration);
    assert_eq!(used(&core), MATRIX_FILL_SCRATCH);
    drop(scratch);
    assert_eq!(used(&core), 0);
}

#[test]
fn ready_selection_prioritizes_demand_and_preserves_each_fifo() {
    let core = core(1_000_000);
    let (_a, startup_a) = register(&core);
    let (_b, demand_a) = register(&core);
    let (_c, startup_b) = register(&core);
    let (_d, demand_b) = register(&core);
    let mut inbox = core.inbox.lock().unwrap();
    inbox.slots.get_mut(&startup_a.id).unwrap().startup = true;
    inbox.slots.get_mut(&startup_b.id).unwrap().startup = true;
    inbox
        .ready
        .extend([startup_a.id, demand_a.id, startup_b.id, demand_b.id]);
    assert_eq!(inbox.next_ready(), Some(demand_a.id));
    assert_eq!(inbox.next_ready(), Some(demand_b.id));
    assert_eq!(inbox.next_ready(), Some(startup_a.id));
    assert_eq!(inbox.next_ready(), Some(startup_b.id));
    drop(inbox);
}

struct PausedStagingBackend {
    entered: std::sync::mpsc::Sender<()>,
    resume: Mutex<std::sync::mpsc::Receiver<()>>,
}
impl FillBackend for PausedStagingBackend {
    fn prepare(&self, _: &dyn FillSource, _: &AtomicBool, head: &HeadSlot) -> Result<()> {
        build(head, 1, 1)
    }
    fn run(
        &self,
        _: &dyn FillSource,
        cancel: &AtomicBool,
        _: Option<Completed>,
        _: &dyn Fn(IndexPin),
    ) -> Outcome {
        let vectors = vec![vec![1_i8; DIMENSIONS]; BATCH];
        self.entered.send(()).unwrap();
        self.resume
            .lock()
            .unwrap()
            .recv_timeout(crate::testing::HANG_BACKSTOP)
            .unwrap();
        let result = check_cancel(cancel);
        drop(vectors);
        Outcome {
            pin: Some(pin(1)),
            cache_epoch: 1,
            result,
        }
    }
}
#[test]
fn paused_owner_staging_stays_charged_after_unregister_until_cancel_cleanup() {
    let (entered, arrived) = std::sync::mpsc::channel();
    let (resume, released) = std::sync::mpsc::channel();
    let owner = SearchFill::start_with(Arc::new(PausedStagingBackend {
        entered,
        resume: Mutex::new(released),
    }))
    .unwrap();
    let (_source, registration) = register(&owner.core);
    registration.notify();
    arrived.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
    drop(registration);
    assert_eq!(owner.matrix_storage_bytes(), MATRIX_FILL_SCRATCH);
    resume.send(()).unwrap();
    let deadline = Instant::now() + crate::testing::HANG_BACKSTOP;
    let mut inbox = owner.core.inbox.lock().unwrap();
    while owner.matrix_storage_bytes() != 0 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "cancelled owner staging was not released"
        );
        inbox = owner.core.changed.wait_timeout(inbox, remaining).unwrap().0;
    }
}

struct DeltaPeakSource {
    revised: AtomicBool,
    heads: Mutex<Vec<Option<IndexPin>>>,
}
impl FillSource for DeltaPeakSource {
    fn head(
        &self,
        previous: Option<IndexPin>,
        check: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Option<HeadUpdate>> {
        check()?;
        self.heads.lock().unwrap().push(previous);
        if self.revised.load(Ordering::Acquire) {
            Ok(Some(HeadUpdate {
                pin: pin(2),
                declarations: 2,
                change: if previous == Some(pin(1)) {
                    HeadChange::Delta {
                        removed_version_ids: vec![0],
                        added: cards(1),
                    }
                } else {
                    HeadChange::Full(cards(2))
                },
            }))
        } else {
            Ok(Some(update(1, 2)))
        }
    }
    fn page(&self, _: Option<IndexPin>, _: Option<&str>) -> Result<HeadSearchCards> {
        Ok(HeadSearchCards {
            pin: pin(if self.revised.load(Ordering::Acquire) {
                2
            } else {
                1
            }),
            cards: cards(2)
                .iter()
                .map(|candidate| crate::store::HeadSearchCard {
                    syntax_id: candidate.key.syntax_id.clone(),
                    card_hash: candidate.card_hash.clone(),
                    card_text: "cached delta peak fixture".into(),
                })
                .collect(),
        })
    }
}
#[test]
fn local_delta_peak_decline_retries_cold_in_same_owner_job_without_second_query() {
    let directory = tempfile::tempdir().unwrap();
    let roots = crate::store::topology::TopologyRoots::isolated_for_tests(
        directory.path().join("cache"),
        directory.path().join("data"),
    );
    let service =
        Arc::new(SearchService::with_embedder_for_tests(roots, Arc::new(BatchFactory)).unwrap());
    let model_id = model_id_for_card_format(CardFormat::V1.as_str());
    service
        .with_vector_writer(|writer| {
            let candidates = cards(2);
            let vector = [1_i8; DIMENSIONS];
            let entries = candidates
                .iter()
                .map(|card| {
                    (
                        model_id.as_str(),
                        card.card_hash.as_str(),
                        vector.as_slice(),
                    )
                })
                .collect::<Vec<_>>();
            writer.put_many(&entries)
        })
        .unwrap();
    let limit = SearchMatrix::estimated_new_bytes(&cards(2)) + MATRIX_FILL_SCRATCH;
    let owner = SearchFill::start_with_limit_for_tests(service.clone(), limit).unwrap();
    let source = Arc::new(DeltaPeakSource {
        revised: AtomicBool::new(false),
        heads: Mutex::new(Vec::new()),
    });
    let erased: Arc<dyn FillSource> = source.clone();
    let registration = owner.register_source(Arc::downgrade(&erased));
    registration.head.request();
    owner.wait_until_idle_for_tests();
    assert_eq!(
        registration
            .head
            .declarations(pin(1), &model_id, service.cache_epoch())
            .unwrap(),
        Some(2)
    );
    // Establish the exact peak rejection precondition using the same estimator
    // as build_mode, rather than assuming this small fixture exceeds the cap.
    let mut matrix = SearchMatrix::new(cards(2), &mut || Ok(())).unwrap();
    matrix.insert(0, &[1; DIMENSIONS]).unwrap();
    matrix.insert(1, &[1; DIMENSIONS]).unwrap();
    let delta_peak = matrix.estimated_delta_candidate_bytes(&cards(1))
        + MATRIX_FILL_SCRATCH
        + matrix.active_count() * (DIMENSIONS + 128);
    assert!(owner.matrix_storage_bytes() + delta_peak > limit);
    source.heads.lock().unwrap().clear();
    source.revised.store(true, Ordering::Release);
    // One ordinary publication hint only. No second query or request retries it.
    registration.notify();
    owner.wait_until_idle_for_tests();
    assert_eq!(*source.heads.lock().unwrap(), vec![Some(pin(1)), None]);
    assert_eq!(
        registration
            .head
            .declarations(pin(1), &model_id, service.cache_epoch())
            .unwrap(),
        None
    );
    assert_eq!(
        registration
            .head
            .declarations(pin(2), &model_id, service.cache_epoch())
            .unwrap(),
        Some(2)
    );
    let ranked = registration
        .head
        .rank(
            pin(2),
            &model_id,
            service.cache_epoch(),
            &[],
            None,
            &mut || Ok(()),
        )
        .unwrap()
        .unwrap();
    assert_eq!(ranked.embedded, 2);
    assert!(owner.matrix_storage_bytes() <= limit);
}
