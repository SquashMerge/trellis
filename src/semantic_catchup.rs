//! One checkout-owned, bounded semantic unit. No main-index snapshot survives a return.
//!
//! The S scheduler: it reads the main index only through storage's snapshot handles
//! (`store::native_first::unit_read`), which it can neither build nor keep across a return.
use crate::model::CancelFlag;
#[cfg(feature = "test-support")]
use crate::model::IndexPin;
use crate::semantic_batch::RetainedStageContinuation;
use crate::semantic_batch::{
    StageOwner,
    counters::{BatchCounters, LookupBudget},
    limits::BatchLimits,
};
use crate::semantic_import::{DestinationMutations, JoinContinuation};
use crate::store::batch_input::AdmittedBatch;
use crate::store::native_first::unit_read::{
    AuthorityOutcome, HeldRead, UnitFailure, WitnessAuthority, WitnessProgress,
};
use crate::store::native_first::{
    PendingSemanticTarget, PreparedOutput, PreparedSemantic, UNAVAILABLE_DIAGNOSTIC,
};
use crate::store::{Store, topology};
use anyhow::{Context, Result, ensure};
#[cfg(feature = "test-support")]
use std::collections::BTreeSet;
#[cfg(feature = "test-support")]
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SemanticUnitOutcome {
    Idle,
    Progress,
    Published,
    Deferred,
}

/// How long one S unit may keep stepping inside its snapshot before it returns.
const STEP_SLICE: std::time::Duration = std::time::Duration::from_millis(50);

#[cfg(all(test, feature = "test-support"))]
thread_local! {
    static STEP_SLICE_OVERRIDE: std::cell::Cell<Option<std::time::Duration>> =
        const { std::cell::Cell::new(None) };
}

/// The S slice; tests that observe single steps set it to zero on their own thread.
pub(crate) fn step_slice() -> std::time::Duration {
    #[cfg(all(test, feature = "test-support"))]
    if let Some(slice) = STEP_SLICE_OVERRIDE.with(std::cell::Cell::get) {
        return slice;
    }
    STEP_SLICE
}

/// Make this thread's S units take one bounded step each.
#[cfg(all(test, feature = "test-support"))]
pub(crate) fn set_semantic_step_slice_for_tests(slice: std::time::Duration) {
    STEP_SLICE_OVERRIDE.with(|cell| cell.set(Some(slice)));
}

pub(crate) struct SemanticContinuation {
    target: PendingSemanticTarget,
    read: HeldRead,
    next: usize,
    current: Option<ProducerContinuation>,
    prepared: Vec<PreparedSemantic>,
    target_stage: Option<crate::store::staging::Stage>,
    /// Test-support cost counters of this operation only, carried across yields so work
    /// another owner does between units (an import, a native publication) is never counted.
    #[cfg(feature = "test-support")]
    work: crate::semantic_import::PublicationWork,
}
struct ProducerContinuation {
    producer: crate::semantic_records::Producer,
    admission: Option<RetainedStageContinuation>,
    admitted: Option<AdmittedBatch>,
    join: Option<JoinContinuation>,
    counters: BatchCounters,
    budget: LookupBudget,
    /// Storage's cursors of the witnessed route: authority, LOCAL targets, preparation.
    witness: WitnessProgress,
    authority: Option<WitnessAuthority>,
    full: bool,
    target_index_done: bool,
    witness_ready: bool,
    /// FULL for a batch with no original import: first join it against its own capture.
    capture_witness: bool,
    capture_join: Option<JoinContinuation>,
    /// The independent FULL rejoin the pre-commit oracle computed, for the post-commit check.
    #[cfg(feature = "test-support")]
    oracle_full: Option<crate::semantic_records::SemanticPublication>,
}

impl SemanticContinuation {
    fn stages(&mut self) -> impl Iterator<Item = &mut crate::store::staging::Stage> {
        let (admission, admitted) = match self.current.as_mut() {
            Some(current) => (
                current.admission.as_mut().and_then(|a| a.stage_mut()),
                current.admitted.as_mut().map(|a| a.stage_mut()),
            ),
            None => (None, None),
        };
        self.target_stage
            .as_mut()
            .into_iter()
            .chain(admission)
            .chain(admitted)
    }

    /// Every private stage the slice starts with writes in one transaction per stage.
    fn batch_stage_connections(&mut self) {
        for stage in self.stages() {
            stage.begin_batch();
        }
    }

    /// Commits each slice transaction. A failed commit is the slice's storage error. The
    /// private connections stay open with their page caches across yields: no other
    /// connection opens a stage while S owns it, and a joined stage is closed when it is
    /// queued for publication, so the publishing writer attaches only closed stage files.
    fn commit_stage_batches(&mut self) -> rusqlite::Result<()> {
        let mut committed = Ok(());
        for stage in self.stages() {
            let result = stage.commit_batch();
            if committed.is_ok() {
                committed = result;
            }
        }
        committed
    }
}

impl Store {
    pub(crate) fn semantic_maintenance_unit(
        &self,
        session: &topology::WriterSession,
        continuation: &mut Option<SemanticContinuation>,
        cancel: &CancelFlag,
    ) -> Result<SemanticUnitOutcome> {
        #[cfg(feature = "test-support")]
        let outer = crate::semantic_import::publication_work_checkpoint();
        #[cfg(feature = "test-support")]
        if let Some(state) = continuation.as_ref() {
            crate::semantic_import::restore_publication_work(state.work.clone());
        }
        let result = self.semantic_unit(session, continuation, cancel);
        #[cfg(feature = "test-support")]
        {
            if let Some(state) = continuation.as_mut() {
                state.work = crate::semantic_import::publication_work_checkpoint();
            }
            crate::semantic_import::restore_publication_work(outer);
        }
        result
    }

    fn semantic_unit(
        &self,
        session: &topology::WriterSession,
        continuation: &mut Option<SemanticContinuation>,
        cancel: &CancelFlag,
    ) -> Result<SemanticUnitOutcome> {
        if let Err(error) = self.begin_semantic_unit(session, cancel) {
            *continuation = None;
            return Err(error);
        }
        let target =
            self.pending_semantic_target_on(continuation.as_ref().map(|state| &state.read));
        let target = match target {
            Ok(target) => target,
            Err(error) => {
                *continuation = None;
                return Err(error);
            }
        };
        if continuation
            .as_ref()
            .is_some_and(|state| Some(&state.target) != target.as_ref())
        {
            // Drop only this operation's private stages; never clear a successor marker.
            *continuation = None;
        }
        let Some(target) = target else {
            return Ok(SemanticUnitOutcome::Idle);
        };
        if continuation.is_none() {
            #[cfg(feature = "test-support")]
            crate::semantic_import::reset_publication_work(
                &target.native_revision_id,
                BTreeSet::new(),
            );
            let read = self.open_semantic_read();
            *continuation = Some(SemanticContinuation {
                #[cfg(feature = "test-support")]
                work: crate::semantic_import::publication_work_checkpoint(),
                target,
                read,
                next: 0,
                current: None,
                prepared: Vec::new(),
                target_stage: None,
            });
            return Ok(SemanticUnitOutcome::Progress);
        }
        let state = continuation.as_mut().unwrap();
        if state.current.is_none() && state.next == state.target.selections.len() {
            // Final publication is its own owner unit, never appended to preparation work.
            let published = match self.publish_pending_semantic(
                &state.target,
                &mut state.prepared,
                session,
                cancel,
            ) {
                Ok(published) => published,
                Err(error) => {
                    *continuation = None;
                    return Err(error);
                }
            };
            if let Some(_published_pin) = published {
                #[cfg(feature = "test-support")]
                if self.semantic_oracle_enabled()
                    && let Err(mismatch) =
                        self.semantic_oracle_check_published(_published_pin, &state.prepared)?
                {
                    *continuation = None;
                    return Err(mismatch);
                }
                #[cfg(feature = "test-support")]
                let counts_result = write_semantic_work_counts(
                    self.semantic_test_hooks().work_counts_file.as_deref(),
                    _published_pin,
                    &state.target,
                );
                *continuation = None;
                #[cfg(feature = "test-support")]
                counts_result?;
                return Ok(SemanticUnitOutcome::Published);
            }
            return Ok(SemanticUnitOutcome::Deferred);
        }
        let selection = state.target.selections[state.next].clone();
        let stage_parent = self.semantic_stage_parent()?;
        let limits = self.semantic_unit_limits();
        let target = state.target.clone();
        let held = state.read.clone();
        let result = self.with_pending_semantic_read(&target, &selection, &held, |read| {
            // One coherent snapshot serves several bounded steps; it never survives a return
            // to worker selection, and the slice keeps foreground work waiting briefly.
            let started = std::time::Instant::now();
            state.batch_stage_connections();
            loop {
                let step = (|| {
                    #[cfg(feature = "test-support")]
                    read.content_fault()?;
                    let captured = read.captured();
                    let destination = read.destination();
                    if state.current.is_none() {
                        let producer = read.producer()?;
                        #[cfg(feature = "test-support")]
                        crate::semantic_import::note_producer_attempt();
                        state.current = Some(ProducerContinuation {
                            producer,
                            admission: None,
                            admitted: None,
                            join: None,
                            counters: BatchCounters::default(),
                            budget: LookupBudget::new(&limits),
                            witness: WitnessProgress::default(),
                            authority: None,
                            // Test-support switch: take the bounded FULL path without a witness attempt.
                            #[cfg(feature = "test-support")]
                            full: self.semantic_test_hooks().force_full_rejoin,
                            #[cfg(not(feature = "test-support"))]
                            full: false,
                            target_index_done: false,
                            witness_ready: false,
                            capture_witness: false,
                            capture_join: None,
                            #[cfg(feature = "test-support")]
                            oracle_full: None,
                        });
                    }
                    let current = state.current.as_mut().unwrap();
                    if !current.full && current.authority.is_none() {
                        match read.source_authority(&mut current.witness, &limits, cancel)? {
                            AuthorityOutcome::Progress => return Ok(None),
                            AuthorityOutcome::Eligible(authority) => {
                                Self::semantic_phase_diagnostic("authority", "witnessed");
                                current.authority = Some(authority)
                            }
                            AuthorityOutcome::Ineligible => {
                                Self::semantic_phase_diagnostic("authority", "ineligible");
                                current.full = true
                            }
                            AuthorityOutcome::Full(reason) => {
                                Self::semantic_phase_diagnostic("authority", reason);
                                current.full = true;
                                current.capture_witness = reason == "original_import_missing";
                                #[cfg(feature = "test-support")]
                                crate::semantic_import::note_local_witness_outcome(Some(reason));
                            }
                        }
                        return Ok(None);
                    }
                    if !current.full && !current.target_index_done {
                        if state.target_stage.is_none() {
                            let mut stage = crate::store::staging::Stage::create(
                                &stage_parent,
                                &format!("native-target-{}", uuid::Uuid::new_v4()),
                                &session.incarnation().to_string(),
                            )?;
                            stage.release_connection();
                            state.target_stage = Some(stage);
                            return Ok(None);
                        }
                        let stage = state.target_stage.as_mut().unwrap();
                        let advanced = read.advance_local_targets(
                            stage,
                            &mut current.witness,
                            &limits,
                            cancel,
                        );
                        let advanced = advanced?;
                        current.target_index_done = advanced.done;
                        if advanced.done {
                            Self::semantic_phase_diagnostic("target_index", "done");
                        }
                        #[cfg(feature = "test-support")]
                        crate::semantic_import::note_local_target_work(
                            advanced.builds,
                            advanced.changed_documents,
                            advanced.current_target_rows,
                        );
                        return Ok(None);
                    }
                    if current.admission.is_none() && current.admitted.is_none() {
                        let request = format!("native-catchup-{}", uuid::Uuid::new_v4());
                        let incarnation = session.incarnation().to_string();
                        let owner = StageOwner {
                            request_id: &request,
                            incarnation: &incarnation,
                        };
                        let rows = read.retained_batch();
                        current.admission = Some(if current.full {
                            RetainedStageContinuation::new(
                                &rows,
                                &captured,
                                &stage_parent,
                                owner,
                                &limits,
                                cancel,
                            )?
                            .authenticated()
                        } else {
                            RetainedStageContinuation::new_witnessed(
                                &rows,
                                &captured,
                                &stage_parent,
                                owner,
                                &limits,
                                cancel,
                            )?
                            .copying(read.copy_filter())
                        });
                        return Ok(None);
                    }
                    if let Some(admission) = current.admission.as_mut() {
                        if let Some(admitted) = admission.advance(
                            &read.retained_batch(),
                            &captured,
                            &mut current.counters,
                            cancel,
                        )? {
                            Self::semantic_phase_diagnostic(
                                "admitted",
                                if current.full { "full" } else { "witnessed" },
                            );
                            current.admitted = Some(admitted);
                            current.admission = None;
                            if current.full {
                                current.join = Some(JoinContinuation::new(
                                    target.pin,
                                    limits.clone(),
                                    DestinationMutations::default(),
                                ));
                                if current.capture_witness {
                                    current.capture_join = Some(JoinContinuation::new(
                                        selection.captured_pin,
                                        limits.clone(),
                                        DestinationMutations::default(),
                                    ));
                                }
                            }
                        }
                        return Ok(None);
                    }
                    let admitted = current
                        .admitted
                        .as_mut()
                        .context("missing admitted semantic stage")?;
                    if !current.full && !current.witness_ready {
                        let stage = state
                            .target_stage
                            .as_mut()
                            .context("missing LOCAL target index")?;
                        let result = read.prepare_witness(
                            &mut current.witness,
                            current
                                .authority
                                .as_ref()
                                .context("missing original witness authority")?,
                            admitted,
                            stage,
                            &limits,
                            &mut current.counters,
                            &mut current.budget,
                            cancel,
                        );
                        current.witness_ready = result?;
                        if current.witness_ready {
                            Self::semantic_phase_diagnostic("witness_prepared", "done");
                            current.join = Some(JoinContinuation::new_witnessed(
                                target.pin,
                                witness_join_limits(self, &limits)?,
                            ));
                        }
                        return Ok(None);
                    }
                    if let Some(capture_join) = current.capture_join.as_mut() {
                        // The batch's own captured==captured rows, kept privately until the S commit.
                        let result = capture_join.advance(
                            admitted,
                            &captured,
                            &captured,
                            &mut current.budget,
                            &mut current.counters,
                            cancel,
                        );
                        let staged = result.and_then(|joined| {
                            if joined.is_some() {
                                let producer = admitted.producer().producer();
                                let parts = admitted.join_parts()?;
                                crate::store::captured_witness::stage_documents(
                                    parts.conn, &producer,
                                )?;
                            }
                            Ok(joined.is_some())
                        });
                        if staged? {
                            Self::semantic_phase_diagnostic("capture_join", "done");
                            current.capture_join = None;
                        }
                        return Ok(None);
                    }
                    let result = current
                        .join
                        .as_mut()
                        .context("missing semantic join continuation")?
                        .advance(
                            admitted,
                            &captured,
                            &destination,
                            &mut current.budget,
                            &mut current.counters,
                            cancel,
                        );
                    let joined = result?;
                    if let Some(joined) = &joined {
                        Self::semantic_phase_diagnostic("joined", "done");
                        ensure!(
                            admitted.producer().producer() == current.producer,
                            "selected batch producer descriptor mismatch"
                        );
                        ensure!(
                            joined.destination_revision_id() == target.native_revision_id,
                            "semantic continuation destination identity mismatch"
                        );
                        #[cfg(feature = "test-support")]
                        if self.semantic_oracle_enabled() {
                            let comparison = read.oracle_check(&current.producer, admitted, cancel);
                            admitted.stage_mut().release_connection();
                            // Oracle failures are test failures, never producer unavailability.
                            current.oracle_full = Some(comparison?);
                        }
                    }
                    Ok(joined)
                })();
                // Private stage connections outlive the step and the slice: reopening one
                // (schema load, pragmas, a cold page cache) cost more than the step.
                let step = match step {
                    Ok(None) if started.elapsed() < step_slice() => match read.check_cancel(cancel)
                    {
                        Ok(()) => continue,
                        Err(error) => Err(error),
                    },
                    step => step,
                };
                // A failed stage commit wins over the step's own outcome, a fallback or content
                // error included: it is a storage error, so the whole continuation and every
                // stage it owns (the shared LOCAL target stage too) is discarded, never reused.
                return match state.commit_stage_batches() {
                    Ok(()) => step,
                    Err(error) => Err(error.into()),
                };
            }
        });
        match result {
            Ok(None) => {
                *continuation = None;
                Ok(SemanticUnitOutcome::Progress)
            }
            Ok(Some(Some(joined))) => {
                let mut current = state.current.take().unwrap();
                let mut admitted = current.admitted.take().unwrap();
                // A joined stage waits for publication closed; the publisher attaches it.
                admitted.stage_mut().release_connection();
                #[cfg(feature = "test-support")]
                if !current.full {
                    crate::semantic_import::note_local_witness_outcome(None);
                }
                state.prepared.push(PreparedSemantic {
                    producer: current.producer,
                    output: PreparedOutput::Joined {
                        admitted: Box::new(admitted),
                        joined,
                    },
                    capture_witness: current.capture_witness.then_some(selection.batch_id),
                    #[cfg(feature = "test-support")]
                    oracle_full: current.oracle_full,
                });
                state.next += 1;
                Ok(SemanticUnitOutcome::Progress)
            }
            Ok(Some(None)) => Ok(SemanticUnitOutcome::Progress),
            Err(error) => {
                let error = match UnitFailure::of(error, cancel) {
                    UnitFailure::Terminal(error) => {
                        *continuation = None;
                        return Err(error);
                    }
                    UnitFailure::Content(error) => error,
                };
                // A witnessed attempt that cannot complete within its bounds (#826: the
                // witness's conservative reservation for a large pending dependent) takes the
                // FULL fallback, as an ineligible witness does. Only FULL's own failure is the
                // producer's terminal unavailable disposition below.
                let fallback = error
                    .downcast_ref::<crate::store::staging::WitnessFallback>()
                    .map(|fallback| fallback.0)
                    .or_else(|| resource_limit(&error).then_some(WITNESS_LIMIT_FALLBACK));
                if let Some(reason) = fallback {
                    let current = state
                        .current
                        .as_mut()
                        .context("missing witness fallback owner")?;
                    if !current.full {
                        current.admission = None;
                        current.admitted = None;
                        current.join = None;
                        current.authority = None;
                        current.full = true;
                        Self::semantic_phase_diagnostic("full_fallback", reason);
                        // FULL is bounded as a fresh attempt, not by the witness's spent steps.
                        current.budget = LookupBudget::new(&limits);
                        #[cfg(feature = "test-support")]
                        crate::semantic_import::note_local_witness_outcome(Some(reason));
                        return Ok(SemanticUnitOutcome::Progress);
                    }
                }
                // Content failures have a terminal, explicit unavailable disposition; they
                // do not create an endless background retry of the same selected artifact.
                let _full = state.current.as_ref().is_some_and(|current| current.full);
                let producer = if let Some(current) = state.current.take() {
                    current.producer
                } else {
                    self.pending_producer_at(&target, &selection)?
                };
                // FULL's own resource bound is the contract's typed outcome, which the
                // unbounded in-memory oracle cannot model; every other unavailable S must
                // be one the independent FULL rejoin also fails (#826).
                #[cfg(feature = "test-support")]
                if self.semantic_oracle_enabled() && !(_full && resource_limit(&error)) {
                    let checked =
                        self.with_pending_semantic_read(&target, &selection, &held, |read| {
                            read.oracle_check_unavailable(&producer, cancel)
                        });
                    // A mismatch, cancellation or storage failure ends the continuation.
                    if let Err(error) = checked {
                        *continuation = None;
                        return Err(error);
                    }
                }
                state.prepared.push(PreparedSemantic {
                    producer,
                    capture_witness: None,
                    #[cfg(feature = "test-support")]
                    oracle_full: None,
                    output: PreparedOutput::Unavailable {
                        diagnostic: format!("{}: {error:#}", UNAVAILABLE_DIAGNOSTIC)
                            .chars()
                            .take(512)
                            .collect(),
                    },
                });
                state.next += 1;
                Ok(SemanticUnitOutcome::Progress)
            }
        }
    }
}

/// The witnessed settlement's limits: the operation's own. Test support may lower them alone
/// (`SemanticTestHooks::witness_join_limits`, a `BatchLimits` JSON object), so a witnessed
/// attempt that exceeds its bounds while FULL fits (#826) stays reachable at fixture size: since
/// #865 the witness charges a pending dependent what FULL charges, so no shared limit separates
/// them.
fn witness_join_limits(_store: &Store, limits: &BatchLimits) -> Result<BatchLimits> {
    #[cfg(feature = "test-support")]
    if let Some(text) = _store.semantic_test_hooks().witness_join_limits {
        return serde_json::from_str(&text).context("test witness join limits");
    }
    Ok(limits.clone())
}

/// The fallback reason a witnessed S records when a resource limit stopped it.
const WITNESS_LIMIT_FALLBACK: &str = "witness_limit_exceeded";

/// Whether `error` is a bounded-resource failure (a `LimitExceeded` anywhere in its chain).
fn resource_limit(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<crate::scip_raw::ScipCaptureError>(),
            Some(crate::scip_raw::ScipCaptureError::LimitExceeded(_))
        )
    })
}

#[cfg(feature = "test-support")]
fn write_semantic_work_counts(
    file: Option<&Path>,
    pin: IndexPin,
    target: &PendingSemanticTarget,
) -> Result<()> {
    if let Some(file) = file {
        let mut work = crate::semantic_import::publication_work();
        work["indexPin"] = serde_json::to_value(pin)?;
        work["producerIds"] = serde_json::to_value(
            target
                .selections
                .iter()
                .map(|selection| &selection.producer_id)
                .collect::<Vec<_>>(),
        )?;
        std::fs::write(file, serde_json::to_vec(&work)?)?;
    }
    Ok(())
}
