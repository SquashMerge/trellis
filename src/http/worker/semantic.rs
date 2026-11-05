//! Lowest-priority semantic units, driven by the checkout's existing worker.
//!
//! Only this stack-owned driver retains private staging between selections.
//! A yield releases the index snapshot and returns to FIFO/watcher selection.
use super::{DaemonState, RuntimeState, STEP_RETRY};
use crate::model::CancelFlag;
use crate::store::{SemanticContinuation, SemanticUnitOutcome};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

#[derive(Default)]
pub(super) struct SemanticMaintenance {
    continuation: Option<SemanticContinuation>,
    owner: Option<uuid::Uuid>,
    retry_after: Option<Instant>,
}

impl SemanticMaintenance {
    /// Startup may sweep abandoned stages, and a released session cannot lend
    /// authority to its successor. Dispose before either transition runs.
    pub(super) fn before_foreground(&mut self, state: &DaemonState) {
        if !matches!(*state.runtime.lock().unwrap(), RuntimeState::Ready(_))
            || !state.store.is_ready_disposition()
        {
            self.reset();
        }
    }

    fn reset(&mut self) {
        self.continuation = None;
        self.owner = None;
        self.retry_after = None;
    }

    /// One actual storage continuation unit, never a background join/actor.
    /// Return only a scheduling deadline; a deadline is not completion evidence.
    pub(super) fn run_unit(
        &mut self,
        state: &Arc<DaemonState>,
        cancel: &CancelFlag,
    ) -> Option<Instant> {
        let _step = state.step_lock.lock().unwrap();
        let session = match &*state.runtime.lock().unwrap() {
            RuntimeState::Ready(session) => session.clone(),
            _ => {
                self.reset();
                return None;
            }
        };
        if self.owner != Some(session.incarnation()) {
            self.reset();
            self.owner = Some(session.incarnation());
        }
        // The foreground owner has just published a new head or completion.
        // Select foreground again before doing any maintenance. Its new native
        // target also overrides an old attempt's contention backoff.
        if state.step_progress.load(Ordering::Acquire) {
            self.retry_after = None;
            return None;
        }
        let now = state.worker_now();
        if let Some(retry) = self.retry_after
            && now < retry
        {
            return Some(retry);
        }
        let options = state.config().options.clone();
        if state
            .maintenance_priority_reason(&session, &options)
            .is_some()
            || cancel.load(Ordering::Acquire)
        {
            return None;
        }
        let started = Instant::now();
        let incarnation = session.incarnation().to_string();
        crate::index_coordinator::diagnostic_marker(&incarnation, "semantic_unit_start", "");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::index_coordinator::cooperative_semantic_unit(
                &state.store,
                &session,
                &mut self.continuation,
                cancel,
                || {
                    state
                        .maintenance_priority_reason(&session, &options)
                        .is_none()
                },
            )
        }));
        let outcome = match &result {
            Ok(Ok(SemanticUnitOutcome::Idle)) => "idle",
            Ok(Ok(SemanticUnitOutcome::Progress)) => "progress",
            Ok(Ok(SemanticUnitOutcome::Published)) => "published",
            Ok(Ok(SemanticUnitOutcome::Deferred)) => "deferred",
            Ok(Err(_)) => "error",
            Err(_) => "panic",
        };
        crate::index_coordinator::diagnostic_marker(
            &incarnation,
            "semantic_unit_end",
            &format!(
                "outcome={outcome} duration_us={}",
                started.elapsed().as_micros()
            ),
        );
        match result {
            Ok(Ok(SemanticUnitOutcome::Idle)) => {
                self.retry_after = None;
                None
            }
            Ok(Ok(SemanticUnitOutcome::Progress)) => {
                self.retry_after = None;
                Some(state.worker_now())
            }
            Ok(Ok(SemanticUnitOutcome::Published)) => {
                self.retry_after = None;
                state.note_state_change();
                Some(state.worker_now())
            }
            Ok(Ok(SemanticUnitOutcome::Deferred)) => {
                self.retry_after = Some(state.worker_now() + STEP_RETRY);
                self.retry_after
            }
            failure => {
                let category = match &failure {
                    Ok(Err(error)) => {
                        // Private staging uses disposable non-journalled writes.
                        // Even a busy error may follow partial stage progress.
                        self.continuation = None;
                        super::super::maintenance_error_class(error)
                    }
                    _ => {
                        self.continuation = None;
                        ("worker_join", 0)
                    }
                };
                if state
                    .maintenance_error_limiter
                    .lock()
                    .unwrap()
                    .permit(Instant::now())
                {
                    let notice = super::super::maintenance_error_notice(category.0, category.1);
                    super::super::best_effort_queue_stderr(
                        std::io::stderr(),
                        format_args!("{notice}"),
                    );
                }
                self.retry_after = Some(state.worker_now() + STEP_RETRY);
                self.retry_after
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    #[test]
    fn semantic_units_wait_for_fifo_selection() {
        let (_tmp, store, state, options) =
            crate::http::exceptional_recovery_tests::serving_queue_watch_fixture();
        state.step_progress.store(false, Ordering::Release);
        let mut semantic = SemanticMaintenance::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let queued = state.enqueue_index(&options, None).unwrap();
        assert_eq!(semantic.run_unit(&state, &cancel), None);
        assert_eq!(
            store.request_by_id(&queued.id).unwrap().unwrap().state,
            "queued"
        );
        assert!(semantic.continuation.is_none());

        // Foreground progress returns to selection, even if a previous semantic
        // unit had a contention retry due in the distant future.
        semantic.retry_after = Some(state.worker_now() + Duration::from_secs(30));
        state.step_progress.store(true, Ordering::Release);
        assert_eq!(semantic.run_unit(&state, &cancel), None);
        assert!(semantic.retry_after.is_none());
    }

    #[test]
    fn semantic_units_wait_for_accepted_watcher_intent() {
        let (_tmp, _store, state, options) =
            crate::http::exceptional_recovery_tests::serving_queue_watch_fixture();
        state.step_progress.store(false, Ordering::Release);
        let guard = state.watcher_work.lock().unwrap();
        let watcher = &guard.as_ref().unwrap().1;
        state.publish_watch_status(watcher, &options);
        watcher.submit_watch_event_for_tests(Ok(notify::Event::new(notify::EventKind::Modify(
            notify::event::ModifyKind::Any,
        ))
        .add_path(options.workspace_root.join("a.js"))));
        drop(guard);
        assert!(state.checkout_watch_pending());
        let mut semantic = SemanticMaintenance::default();
        assert_eq!(
            semantic.run_unit(&state, &Arc::new(AtomicBool::new(false))),
            None
        );
        assert!(semantic.continuation.is_none());
    }

    #[test]
    fn semantic_retry_deadline_is_not_completion() {
        let (_tmp, _store, state, _options) =
            crate::http::exceptional_recovery_tests::serving_queue_watch_fixture();
        state.step_progress.store(false, Ordering::Release);
        let now = Instant::now();
        *state.test_worker_now.lock().unwrap() = Some(now);
        let mut semantic = SemanticMaintenance {
            owner: state
                .runtime
                .lock()
                .unwrap()
                .session()
                .map(|session| session.incarnation()),
            retry_after: Some(now + STEP_RETRY),
            ..SemanticMaintenance::default()
        };
        assert_eq!(
            semantic.run_unit(&state, &Arc::new(AtomicBool::new(false))),
            Some(now + STEP_RETRY)
        );
        assert_eq!(semantic.retry_after, Some(now + STEP_RETRY));
        assert_eq!(state.runtime_phase(), super::super::RuntimePhase::Ready);
    }

    #[test]
    fn retirement_discards_local_retry_without_clearing_durable_work() {
        let (_tmp, store, state, _options) =
            crate::http::exceptional_recovery_tests::serving_queue_watch_fixture();
        let head = store.index_baseline().unwrap();
        let mut semantic = SemanticMaintenance {
            owner: state
                .runtime
                .lock()
                .unwrap()
                .session()
                .map(|session| session.incarnation()),
            retry_after: Some(state.worker_now() + STEP_RETRY),
            ..SemanticMaintenance::default()
        };
        let session = state.runtime.lock().unwrap().session().cloned();
        *state.runtime.lock().unwrap() = RuntimeState::RetireRequested {
            cutoff: None,
            session,
        };
        semantic.before_foreground(&state);
        assert!(semantic.owner.is_none());
        assert!(semantic.retry_after.is_none());
        assert_eq!(store.index_baseline().unwrap(), head);
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn native_request_finishes_before_semantic_units_and_supersedes_private_stage() {
        use crate::capture::Capture;
        use crate::index_coordinator::IndexJobCoordinator;
        use crate::semantic_batch::{
            BatchCounters, BatchLimits, LookupBudget, StageInput, StageOwner, stage_batch,
            testing::single_artifact_batch,
        };
        use crate::semantic_import::{DestinationMutations, join_staged};
        use protobuf::Message;

        let (_tmp, store, state, options) =
            crate::http::exceptional_recovery_tests::serving_queue_watch_fixture();
        let cancel = Arc::new(AtomicBool::new(false));
        let session = state.runtime.lock().unwrap().session().unwrap().clone();
        let mut options = options;
        options.workspace_root = std::fs::canonicalize(&options.workspace_root).unwrap();
        let source = options.workspace_root.join("a.js");
        std::fs::write(&source, "function f(){g();}\n").unwrap();
        let captured_pin = IndexJobCoordinator::prepare_with_session(&store, None, session.clone())
            .unwrap()
            .run_serving(&options, &cancel, |_| {})
            .unwrap();
        let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
        let mut scip = scip::types::Index::parse_from_bytes(
            &hex::decode("122e0a087372632f412e6a73220a6a617661736372697074120b0a0300090a1202412318011a090a0241232811320166").unwrap(),
        ).unwrap();
        scip.documents[0].relative_path = "a.js".into();
        let artifact = scip.write_to_bytes().unwrap();
        let (descriptor, pages) =
            single_artifact_batch(&capture, "scip-typescript", "javascript", &artifact).unwrap();
        for (path, bytes) in [
            ("artifact.scip", artifact.as_slice()),
            ("descriptor.json", descriptor.as_slice()),
            ("source-00000.json", pages[0].as_slice()),
            ("coverage-00000.json", pages[1].as_slice()),
        ] {
            std::fs::write(options.workspace_root.join(path), bytes).unwrap();
        }
        let limits = BatchLimits::PRODUCTION;
        let mut admitted = stage_batch(
            &store,
            &StageInput::external(&options.workspace_root).unwrap(),
            StageOwner {
                request_id: "owner-unit-fixture",
                incarnation: "owner-unit-fixture",
            },
            &options.workspace_root.join("descriptor.json"),
            &crate::scip_raw::sha(&descriptor),
            &limits,
            &mut BatchCounters::default(),
            &cancel,
        )
        .unwrap();
        let joined = join_staged(
            &mut admitted,
            &store,
            captured_pin,
            &limits,
            &mut LookupBudget::new(&limits),
            &mut BatchCounters::default(),
            DestinationMutations::default(),
            &cancel,
        )
        .unwrap();
        let basis = store
            .publish_semantic_batch(
                &joined,
                admitted.stage_mut(),
                captured_pin,
                session.writer_guard().unwrap(),
                &cancel,
            )
            .unwrap();
        drop(admitted);

        std::fs::write(&source, "function f(){g();} // first edit\n").unwrap();
        let request = state.enqueue_index(&options, None).unwrap();
        state.worker_step().unwrap();
        let done = store.request_by_id(&request.id).unwrap().unwrap();
        assert_eq!(done.state, "done");
        let native = done.revision.unwrap();
        assert_ne!(native, basis);
        let target = store.pending_semantic_target().unwrap().unwrap();
        let response = store.evidence_response().unwrap();
        assert!(
            response
                .source_at("a.js", Some(native))
                .unwrap()
                .unwrap()
                .1
                .text
                .contains("first edit")
        );
        response.finish(()).unwrap();
        drop(response); // finish verifies the fence; Drop releases the DELETE read snapshot
        state.worker_step().unwrap(); // next foreground selection prunes completed local FIFO
        state.next_wake(); // consume foreground progress before maintenance selection
        state.inbox.wait(Some(Instant::now())); // consume the accepted enqueue wake
        assert_eq!(state.maintenance_priority_reason(&session, &options), None);
        // Observe single bounded steps, not a whole time slice.
        crate::store::set_semantic_step_slice_for_tests(Duration::ZERO);
        let mut semantic = SemanticMaintenance::default();
        assert!(semantic.run_unit(&state, &cancel).is_some());
        assert!(semantic.continuation.is_some());
        assert_eq!(store.index_baseline().unwrap(), native);

        // A second accepted native request wins the next selection even with
        // an actual private catch-up continuation in memory.
        std::fs::write(&source, "function f(){g();} // second edit\n").unwrap();
        let successor = state.enqueue_index(&options, None).unwrap();
        assert_eq!(semantic.run_unit(&state, &cancel), None);
        state.worker_step().unwrap();
        assert_eq!(
            store.request_by_id(&successor.id).unwrap().unwrap().state,
            "done"
        );
        assert_ne!(store.pending_semantic_target().unwrap().unwrap(), target);
        state.worker_step().unwrap(); // next foreground selection prunes completed local FIFO
        state.next_wake();
        state.inbox.wait(Some(Instant::now()));
        assert_eq!(state.maintenance_priority_reason(&session, &options), None);
        semantic.run_unit(&state, &cancel);
        assert!(semantic.continuation.is_some());
        assert!(store.pending_semantic_target().unwrap().is_some());
        // Dropping the operation releases only its own stage, not the target.
        drop(semantic);
        assert!(store.pending_semantic_target().unwrap().is_some());

        let pending_native = store.index_baseline().unwrap();
        let projection = store
            .semantic_projection_at(pending_native, "scip-typescript", "javascript", "a.js")
            .unwrap()
            .unwrap();
        assert_eq!(
            projection.semantic_status,
            crate::model::SemanticStatus::CatchingUp
        );
        let native_revision = store
            .with_native_reader(pending_native, |reader| {
                Ok(reader.header()?.revision.id.clone())
            })
            .unwrap();
        let target = store.pending_semantic_target().unwrap().unwrap();
        for corrupt in [
            "DELETE FROM semantic_pending_target",
            "UPDATE semantic_pending_target SET root_id='wrong-root'",
            "UPDATE semantic_pending_selections SET captured_pin=basis_pin",
        ] {
            // Independent read handles isolate each corruption disposition.
            // Restore the exact saved SQL rows before the next case.
            let probe = crate::store::Store::open_for_tests(
                &_tmp.path().join("state"),
                &options.workspace_root,
            )
            .unwrap();
            store
                .with_index_sql_for_tests(|db| {
                    db.execute_batch(corrupt)?;
                    Ok(())
                })
                .unwrap();
            let invalid = probe.validate_semantic_read_at(pending_native);
            store
                .with_index_sql_for_tests(|db| {
                    // Restore from the authenticated target captured before faults.
                    db.execute("DELETE FROM semantic_pending_target", [])?;
                    db.execute(
                        "INSERT INTO semantic_pending_target VALUES(1,?1,?2,?3)",
                        rusqlite::params![
                            format!(
                                "pin:v1:{}:{}",
                                target.pin.index_generation, target.pin.index_revision
                            ),
                            target.native_revision_id,
                            target.root_id
                        ],
                    )?;
                    for selection in &target.selections {
                        db.execute(
                            "UPDATE semantic_pending_selections SET captured_pin=?1
                        WHERE revision_id=?2 AND producer_id=?3",
                            rusqlite::params![
                                format!(
                                    "pin:v1:{}:{}",
                                    selection.captured_pin.index_generation,
                                    selection.captured_pin.index_revision
                                ),
                                format!(
                                    "pin:v1:{}:{}",
                                    target.pin.index_generation, target.pin.index_revision
                                ),
                                selection.producer_id
                            ],
                        )?;
                    }
                    Ok(())
                })
                .unwrap();
            let error = invalid.expect_err(corrupt);
            assert!(
                crate::store::Store::is_rebuild_failure(&error),
                "{corrupt}: {error:#}"
            );
        }
        let mut continuation = None;
        let mut published = false;
        // One small fixture has a fixed finite number of phase/document units.
        // The cap is an assertion backstop, never evidence of completion.
        for _ in 0..128 {
            let _owner = state.step_lock.lock().unwrap();
            let outcome = crate::index_coordinator::cooperative_semantic_unit(
                &store,
                &session,
                &mut continuation,
                &cancel,
                || true,
            )
            .unwrap();
            match outcome {
                SemanticUnitOutcome::Progress => {
                    assert_eq!(store.index_baseline().unwrap(), pending_native);
                    assert!(store.pending_semantic_target().unwrap().is_some());
                }
                SemanticUnitOutcome::Published => {
                    published = true;
                    break;
                }
                other => panic!("unexpected retained catch-up unit: {other:?}"),
            }
        }
        assert!(published, "bounded catch-up never returned Published");
        let semantic_pin = store.index_baseline().unwrap();
        assert_ne!(semantic_pin, pending_native);
        assert_eq!(
            semantic_pin.index_generation,
            pending_native.index_generation
        );
        assert_eq!(
            store
                .with_native_reader(semantic_pin, |reader| {
                    Ok(reader.header()?.revision.id.clone())
                })
                .unwrap(),
            native_revision
        );
        assert!(store.pending_semantic_target().unwrap().is_none());
        store.validate_semantic_read_at(pending_native).unwrap();
        assert_eq!(
            store
                .semantic_projection_at(pending_native, "scip-typescript", "javascript", "a.js",)
                .unwrap()
                .unwrap(),
            projection,
            "retained N projection changed after S"
        );
        assert!(
            store
                .semantic_projection_at(semantic_pin, "scip-typescript", "javascript", "a.js",)
                .unwrap()
                .is_none()
        );

        // An explicit semantic-only import between maintenance units fulfills
        // this producer's target. The stale private cursor must not overwrite it.
        std::fs::write(&source, "function f(){g();} // third edit\n").unwrap();
        let third = IndexJobCoordinator::prepare_with_session(&store, None, session.clone())
            .unwrap()
            .run_serving(&options, &cancel, |_| {})
            .unwrap();
        let third_revision = store
            .with_native_reader(third, |reader| Ok(reader.header()?.revision.id.clone()))
            .unwrap();
        let mut interrupted = None;
        assert_eq!(
            store
                .semantic_maintenance_unit(&session, &mut interrupted, &cancel)
                .unwrap(),
            SemanticUnitOutcome::Progress
        );
        assert!(interrupted.is_some());
        let mut imported = stage_batch(
            &store,
            &StageInput::external(&options.workspace_root).unwrap(),
            StageOwner {
                request_id: "interrupting-import",
                incarnation: "interrupting-import",
            },
            &options.workspace_root.join("descriptor.json"),
            &crate::scip_raw::sha(&descriptor),
            &limits,
            &mut BatchCounters::default(),
            &cancel,
        )
        .unwrap();
        let imported_join = join_staged(
            &mut imported,
            &store,
            third,
            &limits,
            &mut LookupBudget::new(&limits),
            &mut BatchCounters::default(),
            DestinationMutations::default(),
            &cancel,
        )
        .unwrap();
        let imported_pin = store
            .publish_semantic_batch(
                &imported_join,
                imported.stage_mut(),
                third,
                session.writer_guard().unwrap(),
                &cancel,
            )
            .unwrap();
        assert_ne!(imported_pin, third);
        assert_eq!(
            store
                .with_native_reader(imported_pin, |reader| {
                    Ok(reader.header()?.revision.id.clone())
                })
                .unwrap(),
            third_revision
        );
        assert!(store.pending_semantic_target().unwrap().is_none());
        assert_eq!(
            store
                .semantic_maintenance_unit(&session, &mut interrupted, &cancel)
                .unwrap(),
            SemanticUnitOutcome::Idle
        );
        assert!(interrupted.is_none());
        assert_eq!(
            store.index_baseline().unwrap(),
            imported_pin,
            "obsolete maintenance overwrote explicit semantic publication"
        );
        drop(imported);

        // A failed independent S oracle must preserve the already committed N,
        // not roll back native publication or diagnose producer unavailability.
        std::fs::write(&source, "function f(){g();} // oracle mismatch edit\n").unwrap();
        let oracle_native =
            IndexJobCoordinator::prepare_with_session(&store, None, session.clone())
                .unwrap()
                .run_serving(&options, &cancel, |_| {})
                .unwrap();
        let oracle_target = store.pending_semantic_target().unwrap().unwrap();
        let mut oracle_continuation = None;
        let rejected = store
            .with_local_oracle_mismatch_for_tests(|| -> anyhow::Result<()> {
                for _ in 0..10_000 {
                    let _owner = state.step_lock.lock().unwrap();
                    match store.semantic_maintenance_unit(
                        &session,
                        &mut oracle_continuation,
                        &cancel,
                    )? {
                        SemanticUnitOutcome::Progress => {
                            assert_eq!(store.index_baseline().unwrap(), oracle_native);
                        }
                        outcome => panic!("oracle mismatch unexpectedly returned {outcome:?}"),
                    }
                }
                panic!("forced oracle mismatch never rejected completed stage")
            })
            .expect_err("forced independent oracle mismatch must reject S");
        assert!(
            format!("{rejected:#}").contains("independent FULL rejoin oracle"),
            "unexpected mismatch error: {rejected:#}"
        );
        assert_eq!(
            store.index_baseline().unwrap(),
            oracle_native,
            "S oracle failure changed the committed native head"
        );
        assert_eq!(
            store.pending_semantic_target().unwrap().unwrap(),
            oracle_target
        );
        let response = store.evidence_response().unwrap();
        assert!(
            response
                .source_at("a.js", Some(oracle_native))
                .unwrap()
                .unwrap()
                .1
                .text
                .contains("oracle mismatch edit")
        );
        response.finish(()).unwrap();
        drop(response);
    }

    #[cfg(feature = "test-support")]
    fn two_pending_producers_fixture() -> (
        tempfile::TempDir,
        crate::store::Store,
        Arc<DaemonState>,
        crate::indexer::IndexOptions,
        CancelFlag,
        crate::model::IndexPin,
    ) {
        two_producer_edit_fixture(false)
    }

    #[cfg(feature = "test-support")]
    fn two_producer_edit_fixture(
        context_only: bool,
    ) -> (
        tempfile::TempDir,
        crate::store::Store,
        Arc<DaemonState>,
        crate::indexer::IndexOptions,
        CancelFlag,
        crate::model::IndexPin,
    ) {
        use crate::capture::Capture;
        use crate::index_coordinator::IndexJobCoordinator;
        use crate::semantic_batch::{
            BatchCounters, BatchLimits, LookupBudget, StageInput, StageOwner, stage_batch,
            testing::single_artifact_batch,
        };
        use crate::semantic_import::{DestinationMutations, join_staged};
        use protobuf::Message;

        let (_tmp, store, state, mut options) =
            crate::http::exceptional_recovery_tests::serving_queue_watch_fixture();
        options.workspace_root = std::fs::canonicalize(&options.workspace_root).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let session = state.runtime.lock().unwrap().session().unwrap().clone();
        let source = options.workspace_root.join("a.js");
        std::fs::write(&source, "function f(){g();}\n").unwrap();
        let mut selected = IndexJobCoordinator::prepare_with_session(&store, None, session.clone())
            .unwrap()
            .run_serving(&options, &cancel, |_| {})
            .unwrap();
        let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
        let mut scip = scip::types::Index::parse_from_bytes(
            &hex::decode("122e0a087372632f412e6a73220a6a617661736372697074120b0a0300090a1202412318011a090a0241232811320166").unwrap(),
        ).unwrap();
        scip.documents[0].relative_path = "a.js".into();
        let artifact = scip.write_to_bytes().unwrap();
        let limits = BatchLimits::PRODUCTION;
        for producer in ["scip-typescript", "trellis-synthetic-scip"] {
            let (descriptor, pages) =
                single_artifact_batch(&capture, producer, "javascript", &artifact).unwrap();
            for (path, bytes) in [
                ("artifact.scip", artifact.as_slice()),
                ("descriptor.json", descriptor.as_slice()),
                ("source-00000.json", pages[0].as_slice()),
                ("coverage-00000.json", pages[1].as_slice()),
            ] {
                std::fs::write(options.workspace_root.join(path), bytes).unwrap();
            }
            let mut admitted = stage_batch(
                &store,
                &StageInput::external(&options.workspace_root).unwrap(),
                StageOwner {
                    request_id: producer,
                    incarnation: "two-pending-producers",
                },
                &options.workspace_root.join("descriptor.json"),
                &crate::scip_raw::sha(&descriptor),
                &limits,
                &mut BatchCounters::default(),
                &cancel,
            )
            .unwrap();
            let joined = join_staged(
                &mut admitted,
                &store,
                selected,
                &limits,
                &mut LookupBudget::new(&limits),
                &mut BatchCounters::default(),
                DestinationMutations::default(),
                &cancel,
            )
            .unwrap();
            selected = store
                .publish_semantic_batch(
                    &joined,
                    admitted.stage_mut(),
                    selected,
                    session.writer_guard().unwrap(),
                    &cancel,
                )
                .unwrap();
            drop(admitted);
        }
        if context_only {
            std::fs::write(options.workspace_root.join("Cargo.lock"), "version = 4\n").unwrap();
        } else {
            std::fs::write(&source, "function f(){g();} // pending two producers\n").unwrap();
        }
        let native = IndexJobCoordinator::prepare_with_session(&store, None, session.clone())
            .unwrap()
            .run_serving(&options, &cancel, |_| {})
            .unwrap();
        (_tmp, store, state, options, cancel, native)
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn missing_one_of_two_pending_producers_rejects_semantic_unit() {
        let (_tmp, store, state, _options, cancel, native) = two_pending_producers_fixture();
        let session = state.runtime.lock().unwrap().session().unwrap().clone();
        let target = store.pending_semantic_target().unwrap().unwrap();
        assert_eq!(target.selections.len(), 2);
        let key = format!(
            "pin:v1:{}:{}",
            native.index_generation, native.index_revision
        );
        store
            .with_index_read_for_tests(|db| {
                let expected: i64 = db.query_row(
                    "SELECT count(*) FROM semantic_revision_raw_selections r
                 JOIN semantic_projection_origins o ON o.revision_id=r.revision_id
                    AND o.producer_id=r.producer_id WHERE r.revision_id=?1",
                    [&key],
                    |row| row.get(0),
                )?;
                assert_eq!(expected, 2);
                Ok(())
            })
            .unwrap();
        store
            .with_index_sql_for_tests(|db| {
                let removed = db.execute(
                    "DELETE FROM semantic_pending_selections
                 WHERE revision_id=?1 AND producer_id='trellis-synthetic-scip'",
                    [&key],
                )?;
                assert_eq!(removed, 1);
                Ok(())
            })
            .unwrap();

        let mut continuation = None;
        let _owner = state.step_lock.lock().unwrap();
        let error = store
            .semantic_maintenance_unit(&session, &mut continuation, &cancel)
            .expect_err("missing one required pending producer must reject the first unit");
        assert!(crate::store::Store::is_rebuild_failure(&error), "{error:#}");
        assert!(store.recovery_pending_for_tests());
        assert!(continuation.is_none());
        store.with_index_read_for_tests(|db| {
            let head: (String, i64) = db.query_row(
                "SELECT index_generation,index_revision FROM index_metadata WHERE singleton=1", [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            assert_eq!(head, (native.index_generation.to_string(), native.index_revision as i64));
            let marker: (String, String, String) = db.query_row(
                "SELECT revision_id,native_revision_id,root_id FROM semantic_pending_target WHERE singleton=1", [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            assert_eq!(marker, (key.clone(), target.native_revision_id.clone(), target.root_id.clone()));
            let pending: i64 = db.query_row(
                "SELECT count(*) FROM semantic_pending_selections WHERE revision_id=?1", [&key],
                |row| row.get(0),
            )?;
            assert_eq!(pending, 1, "failed unit cleared or repaired the surviving target");
            let committed: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM semantic_catchup_commits WHERE target_revision_id=?1)",
                [&key], |row| row.get(0),
            )?;
            assert!(!committed, "partial S was recorded for a corrupt target");
            Ok(())
        }).unwrap();
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn explicit_import_fulfills_one_of_two_pending_producers() {
        use crate::semantic_batch::{
            BatchCounters, BatchLimits, LookupBudget, StageInput, StageOwner, stage_batch,
        };
        use crate::semantic_import::{DestinationMutations, join_staged};

        let (_tmp, store, state, options, cancel, native) = two_pending_producers_fixture();
        let session = state.runtime.lock().unwrap().session().unwrap().clone();
        let target = store.pending_semantic_target().unwrap().unwrap();
        assert_eq!(target.selections.len(), 2);
        let key = |pin: crate::model::IndexPin| {
            format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision)
        };
        let counts = |pin: crate::model::IndexPin| {
            store
                .with_index_read_for_tests(|db| {
                    let key = key(pin);
                    let count = |table: &str| -> anyhow::Result<i64> {
                        Ok(db.query_row(
                            &format!("SELECT count(*) FROM {table} WHERE revision_id=?1"),
                            [&key],
                            |row| row.get(0),
                        )?)
                    };
                    Ok((
                        count("semantic_revision_raw_selections")?,
                        count("semantic_projection_origins")?,
                        count("semantic_pending_selections")?,
                    ))
                })
                .unwrap()
        };
        assert_eq!(counts(native), (2, 2, 2));
        let retained = store
            .semantic_projection_at(native, "scip-typescript", "javascript", "a.js")
            .unwrap()
            .unwrap();
        let limits = BatchLimits::PRODUCTION;
        // The fixture's last descriptor belongs to its synthetic producer.
        let descriptor = std::fs::read(options.workspace_root.join("descriptor.json")).unwrap();
        let mut admitted = stage_batch(
            &store,
            &StageInput::external(&options.workspace_root).unwrap(),
            StageOwner {
                request_id: "fulfill-one",
                incarnation: "two-pending-producers",
            },
            &options.workspace_root.join("descriptor.json"),
            &crate::scip_raw::sha(&descriptor),
            &limits,
            &mut BatchCounters::default(),
            &cancel,
        )
        .unwrap();
        let joined = join_staged(
            &mut admitted,
            &store,
            native,
            &limits,
            &mut LookupBudget::new(&limits),
            &mut BatchCounters::default(),
            DestinationMutations::default(),
            &cancel,
        )
        .unwrap();
        let imported = store
            .publish_semantic_batch(
                &joined,
                admitted.stage_mut(),
                native,
                session.writer_guard().unwrap(),
                &cancel,
            )
            .unwrap();
        drop(admitted);
        assert_ne!(imported, native);
        assert_eq!(counts(imported), (2, 1, 1));
        let remaining = store.pending_semantic_target().unwrap().unwrap();
        assert_eq!(remaining.pin, imported);
        assert_eq!(remaining.native_revision_id, target.native_revision_id);
        assert_eq!(remaining.selections.len(), 1);
        assert_eq!(remaining.selections[0].producer_id, "scip-typescript");
        assert_eq!(
            remaining.selections[0],
            target
                .selections
                .into_iter()
                .find(|s| s.producer_id == "scip-typescript")
                .unwrap()
        );

        let mut continuation = None;
        let mut published = false;
        for _ in 0..128 {
            let _owner = state.step_lock.lock().unwrap();
            match store
                .semantic_maintenance_unit(&session, &mut continuation, &cancel)
                .unwrap()
            {
                SemanticUnitOutcome::Progress => {
                    assert_eq!(store.index_baseline().unwrap(), imported)
                }
                SemanticUnitOutcome::Published => {
                    published = true;
                    break;
                }
                other => panic!("unexpected remaining-producer unit: {other:?}"),
            }
        }
        assert!(published, "remaining producer never published S");
        let semantic = store.index_baseline().unwrap();
        assert_ne!(semantic, imported);
        assert_eq!(
            store
                .with_native_reader(semantic, |reader| Ok(reader.header()?.revision.id.clone()))
                .unwrap(),
            remaining.native_revision_id
        );
        assert_eq!(counts(semantic), (2, 0, 0));
        assert!(store.pending_semantic_target().unwrap().is_none());
        assert_eq!(
            store
                .semantic_projection_at(native, "scip-typescript", "javascript", "a.js")
                .unwrap()
                .unwrap(),
            retained
        );
        assert_eq!(
            counts(native),
            (2, 2, 2),
            "retained N selections were mutated"
        );
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn context_only_semantic_units_yield_to_fifo_and_watcher() {
        let (_tmp, store, state, options, cancel, native) = two_producer_edit_fixture(true);
        let session = state.runtime.lock().unwrap().session().unwrap().clone();
        let target = store.pending_semantic_target().unwrap().unwrap();
        assert_eq!(target.pin, native);
        assert_eq!(target.selections.len(), 2);
        let response = store.evidence_response().unwrap();
        assert_eq!(
            response
                .source_at("a.js", Some(native))
                .unwrap()
                .unwrap()
                .1
                .text,
            "function f(){g();}\n"
        );
        response.finish(()).unwrap();
        drop(response);
        state.worker_step().unwrap();
        state.next_wake();
        state.inbox.wait(Some(Instant::now()));
        assert_eq!(state.maintenance_priority_reason(&session, &options), None);
        // Observe single bounded steps, not a whole time slice.
        crate::store::set_semantic_step_slice_for_tests(Duration::ZERO);
        let mut semantic = SemanticMaintenance::default();
        assert!(semantic.run_unit(&state, &cancel).is_some());
        assert!(semantic.continuation.is_some());
        assert_eq!(store.index_baseline().unwrap(), native);

        let request = state.enqueue_index(&options, None).unwrap();
        assert_eq!(semantic.run_unit(&state, &cancel), None);
        assert_eq!(
            store.request_by_id(&request.id).unwrap().unwrap().state,
            "queued"
        );
        assert_eq!(store.pending_semantic_target().unwrap().unwrap(), target);
        state.worker_step().unwrap();
        assert_eq!(
            store.request_by_id(&request.id).unwrap().unwrap().state,
            "done"
        );
        state.worker_step().unwrap();
        state.next_wake();
        state.inbox.wait(Some(Instant::now()));

        {
            let mut work = state.watcher_work.lock().unwrap();
            let watcher = &mut work.as_mut().unwrap().1;
            watcher.submit_watch_event_for_tests(Ok(notify::Event::new(
                notify::EventKind::Modify(notify::event::ModifyKind::Any),
            )
            .add_path(options.workspace_root.join("a.js"))));
            // Existing forced inventory makes this accepted hint due without a timer.
            watcher.force_periodic_inventory_for_tests();
            state.publish_watch_status(watcher, &options);
        }
        assert!(state.checkout_watch_pending());
        assert_eq!(semantic.run_unit(&state, &cancel), None);
        assert_eq!(store.index_baseline().unwrap(), native);
        state.worker_step().unwrap();
        state.next_wake();
        state.inbox.wait(Some(Instant::now()));
        assert!(!state.checkout_watch_pending());
        assert_eq!(state.maintenance_priority_reason(&session, &options), None);
        assert_eq!(store.pending_semantic_target().unwrap().unwrap(), target);

        let native_revision = target.native_revision_id.clone();
        let mut published = None;
        for _ in 0..256 {
            state.worker_step().unwrap();
            state.next_wake();
            state.inbox.wait(Some(Instant::now()));
            assert_eq!(state.maintenance_priority_reason(&session, &options), None);
            semantic.run_unit(&state, &cancel);
            let head = store.index_baseline().unwrap();
            if head != native {
                published = Some(head);
                break;
            }
            assert_eq!(store.pending_semantic_target().unwrap().unwrap(), target);
        }
        let published = published.expect("bounded context units never committed S");
        assert_eq!(published.index_generation, native.index_generation);
        assert_eq!(
            store
                .with_native_reader(published, |reader| Ok(reader.header()?.revision.id.clone()))
                .unwrap(),
            native_revision
        );
        assert!(store.pending_semantic_target().unwrap().is_none());
        assert!(semantic.continuation.is_none());
        store
            .with_index_read_for_tests(|db| {
                let committed: bool = db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM semantic_catchup_commits
                 WHERE revision_id=?1 AND target_revision_id=?2)",
                    rusqlite::params![
                        format!(
                            "pin:v1:{}:{}",
                            published.index_generation, published.index_revision
                        ),
                        format!(
                            "pin:v1:{}:{}",
                            native.index_generation, native.index_revision
                        )
                    ],
                    |row| row.get(0),
                )?;
                assert!(
                    committed,
                    "head movement was not this owner's target S commit"
                );
                Ok(())
            })
            .unwrap();
    }

    /// A catch-up whose continuation keeps its index read connection across a yield (#671).
    #[cfg(feature = "test-support")]
    fn held_read_fixture() -> (
        tempfile::TempDir,
        crate::store::Store,
        Arc<DaemonState>,
        CancelFlag,
        SemanticMaintenance,
    ) {
        let (tmp, store, state, options, cancel, _native) = two_pending_producers_fixture();
        let session = state.runtime.lock().unwrap().session().unwrap().clone();
        state.worker_step().unwrap();
        state.next_wake();
        state.inbox.wait(Some(Instant::now()));
        assert_eq!(state.maintenance_priority_reason(&session, &options), None);
        drop(session);
        crate::store::set_semantic_step_slice_for_tests(Duration::ZERO);
        let mut semantic = SemanticMaintenance::default();
        // The first unit only creates the continuation; the second opens its connection.
        assert!(semantic.run_unit(&state, &cancel).is_some());
        assert_eq!(store.held_semantic_reads_for_tests(), 0);
        assert!(semantic.run_unit(&state, &cancel).is_some());
        assert!(semantic.continuation.is_some());
        assert_eq!(store.held_semantic_reads_for_tests(), 1);
        // A later unit reuses it: still one connection, and the catch-up still progresses.
        assert!(semantic.run_unit(&state, &cancel).is_some());
        assert_eq!(store.held_semantic_reads_for_tests(), 1);
        (tmp, store, state, cancel, semantic)
    }

    /// Recreation runs on the worker between S units. Its exclusive use admission closes the
    /// held read connection first, so the same step recreates instead of deferring as busy.
    #[cfg(feature = "test-support")]
    #[test]
    fn recreate_closes_a_held_semantic_read_connection() {
        let (_tmp, store, state, cancel, mut semantic) = held_read_fixture();
        let old = state
            .runtime
            .lock()
            .unwrap()
            .session()
            .unwrap()
            .incarnation();
        store.mark_recreate_pending_for_tests();
        let at_exclusive = Arc::new(std::sync::Mutex::new(None));
        let observed = at_exclusive.clone();
        let hook_store = store.clone();
        store.set_exclusive_recovery_hook(move || {
            *observed.lock().unwrap() = Some(hook_store.held_semantic_reads_for_tests());
        });
        // No `before_foreground`: the transition itself must release the connection.
        state.worker_step().unwrap();
        assert_eq!(
            *at_exclusive.lock().unwrap(),
            Some(0),
            "recreation did not reach exclusive use with the S connection closed"
        );
        assert!(!store.is_recreate_pending());
        let session = state.runtime.lock().unwrap().session().unwrap().clone();
        assert_ne!(session.incarnation(), old);
        assert_eq!(store.held_semantic_reads_for_tests(), 0);
        // The stale continuation belongs to the released session: it is dropped, not resumed.
        drop(session);
        semantic.before_foreground(&state);
        semantic.run_unit(&state, &cancel);
        assert!(semantic.owner != Some(old));
    }

    /// A FIFO native publication needs no release: between units the held connection has no
    /// open transaction, so it holds no SQLite lock. Its next unit sees the new target.
    #[cfg(feature = "test-support")]
    #[test]
    fn native_publication_commits_beside_a_held_semantic_read_connection() {
        let (_tmp, store, state, cancel, mut semantic) = held_read_fixture();
        let target = store.pending_semantic_target().unwrap().unwrap();
        let options = state.config().options.clone();
        std::fs::write(
            options.workspace_root.join("a.js"),
            "function f(){g();} // FIFO beside held S\n",
        )
        .unwrap();
        let request = state.enqueue_index(&options, None).unwrap();
        state.worker_step().unwrap();
        assert_eq!(
            store.request_by_id(&request.id).unwrap().unwrap().state,
            "done"
        );
        assert_eq!(store.held_semantic_reads_for_tests(), 1);
        assert_ne!(store.pending_semantic_target().unwrap().unwrap(), target);
        state.worker_step().unwrap();
        state.next_wake();
        state.inbox.wait(Some(Instant::now()));
        // The held connection's fresh snapshot sees the new target: the superseded
        // continuation and its connection are dropped, and a new one starts unopened.
        assert!(semantic.run_unit(&state, &cancel).is_some());
        assert!(semantic.continuation.is_some());
        assert_eq!(store.held_semantic_reads_for_tests(), 0);
    }

    /// Retirement closes the held read connection before it releases the writer, so a
    /// retired runtime keeps no index connection while its worker stack still owns S.
    #[cfg(feature = "test-support")]
    #[test]
    fn retirement_closes_a_held_semantic_read_connection() {
        let (_tmp, store, state, _cancel, semantic) = held_read_fixture();
        assert_eq!(
            state.request_retire(|_| true),
            super::super::RetireOutcome::Requested
        );
        let at_close = Arc::new(std::sync::Mutex::new(None));
        let observed = at_close.clone();
        let hook_store = store.clone();
        state.set_closing_hook_for_tests(move || {
            *observed.lock().unwrap() = Some(hook_store.held_semantic_reads_for_tests());
        });
        state.worker_step().unwrap();
        assert!(state.retired());
        assert_eq!(*at_close.lock().unwrap(), Some(0));
        assert!(semantic.continuation.is_some());
        assert_eq!(store.held_semantic_reads_for_tests(), 0);
    }

    #[test]
    fn shutdown_signal_cancels_the_same_unit_flag() {
        let inbox = super::super::WorkerInbox::default();
        let cancel = inbox.shutdown.clone();
        assert!(!cancel.load(Ordering::Acquire));
        inbox.shut_down();
        assert!(cancel.load(Ordering::Acquire));
        assert!(inbox.wake_pending());
    }
}
