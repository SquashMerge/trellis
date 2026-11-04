//! One checkout-owned, bounded semantic unit. No main-index snapshot survives a return.
use super::*;
use crate::semantic_batch::RetainedStageContinuation;
use crate::semantic_batch::{AdmittedBatch, BatchCounters, LookupBudget, StageOwner};
use crate::semantic_import::{DestinationMutations, JoinContinuation};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SemanticUnitOutcome {
    Idle,
    Progress,
    Published,
    Deferred,
}

/// How long one S unit may keep stepping inside its snapshot before it returns.
const STEP_SLICE: std::time::Duration = std::time::Duration::from_millis(50);

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static STEP_SLICE_OVERRIDE: std::cell::Cell<Option<std::time::Duration>> =
        const { std::cell::Cell::new(None) };
}

/// The S slice; tests that observe single steps set it to zero on their own thread.
pub(crate) fn step_slice() -> std::time::Duration {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(slice) = STEP_SLICE_OVERRIDE.with(std::cell::Cell::get) {
        return slice;
    }
    STEP_SLICE
}

/// Make this thread's S units take one bounded step each.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn set_semantic_step_slice_for_tests(slice: std::time::Duration) {
    STEP_SLICE_OVERRIDE.with(|cell| cell.set(Some(slice)));
}

pub(crate) struct SemanticContinuation {
    target: PendingSemanticTarget,
    next: usize,
    current: Option<ProducerContinuation>,
    prepared: Vec<PreparedSemantic>,
    target_stage: Option<crate::semantic_batch::staging::Stage>,
    /// Test-support cost counters of this operation only, carried across yields so work
    /// another owner does between units (an import, a native publication) is never counted.
    #[cfg(feature = "test-support")]
    work: crate::semantic_import::PublicationWork,
}
struct ProducerContinuation {
    producer: crate::semantic_evidence::Producer,
    admission: Option<RetainedStageContinuation>,
    admitted: Option<AdmittedBatch>,
    join: Option<JoinContinuation>,
    counters: BatchCounters,
    budget: LookupBudget,
    authority_cursor: super::witness::OriginalAdmissionCursor,
    authority: Option<Box<super::witness::SourceAuthority>>,
    full: bool,
    target_cursor: super::local_targets::TargetIndexCursor,
    target_index_done: bool,
    witness_preparation: super::witness_preparation::WitnessPreparation,
    witness_ready: bool,
    /// FULL for a batch with no original import: first join it against its own capture.
    capture_witness: bool,
    capture_join: Option<JoinContinuation>,
}

impl SemanticContinuation {
    fn stages(&mut self) -> impl Iterator<Item = &mut crate::semantic_batch::staging::Stage> {
        let (admission, admitted) = match self.current.as_mut() {
            Some(current) => (
                current.admission.as_mut().and_then(|a| a.stage_mut()),
                current.admitted.as_mut().map(|a| a.stage_mut()),
            ),
            None => (None, None),
        };
        self.target_stage.as_mut().into_iter().chain(admission).chain(admitted)
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

    /// Opt-in (`TRELLIS_INDEX_DIAGNOSTICS=1`), best-effort S phase marker for profiling;
    /// never carries IDs, paths or pins.
    pub(super) fn semantic_phase_diagnostic(phase: &'static str, detail: &'static str) {
        if std::env::var("TRELLIS_INDEX_DIAGNOSTICS").as_deref() == Ok("1") {
            use std::io::Write as _;
            let wall_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis());
            let (in_use, allocated) = crate::heap::bytes();
            // SAFETY: reads SQLite's global memory counter; no connection is involved.
            let sqlite = unsafe { rusqlite::ffi::sqlite3_memory_used() };
            let _ = writeln!(
                std::io::stderr().lock(),
                "semantic-s phase={phase} detail={detail} wall_ms={wall_ms} heap_in_use_mib={} heap_allocated_mib={} sqlite_mib={}",
                in_use >> 20,
                allocated >> 20,
                sqlite >> 20
            );
        }
    }

    fn semantic_unit(
        &self,
        session: &topology::WriterSession,
        continuation: &mut Option<SemanticContinuation>,
        cancel: &CancelFlag,
    ) -> Result<SemanticUnitOutcome> {
        if let Err(error) = self
            .verify_writer_session(session)
            .and_then(|_| self.identity.verify())
            .and_then(|_| check_cancel(cancel))
        {
            *continuation = None;
            return Err(error);
        }
        let target = match self.pending_semantic_target() {
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
            *continuation = Some(SemanticContinuation {
                #[cfg(feature = "test-support")]
                work: crate::semantic_import::publication_work_checkpoint(),
                target,
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
                let counts_result = write_semantic_work_counts(_published_pin, &state.target);
                *continuation = None;
                #[cfg(feature = "test-support")]
                counts_result?;
                return Ok(SemanticUnitOutcome::Published);
            }
            return Ok(SemanticUnitOutcome::Deferred);
        }
        let selection = state.target.selections[state.next].clone();
        let stage_parent = self.semantic_stage_parent()?;
        let limits = batch_publication_limits();
        let target = state.target.clone();
        let result = self.with_pending_semantic_inputs(&target, |db, destination_read| {
            // One coherent snapshot serves several bounded steps; it never survives a return
            // to worker selection, and the slice keeps foreground work waiting briefly.
            let started = std::time::Instant::now();
            state.batch_stage_connections();
            loop {
                let step = (|| {
                    let captured_read = ReadRevision::new(
                        selection.captured_pin,
                        semantic_pin_key(selection.captured_pin),
                    );
                    let captured = NativeReader {
                        store: self,
                        db,
                        selected: &captured_read,
                    };
                    let destination = NativeReader {
                        store: self,
                        db,
                        selected: destination_read,
                    };
                    if state.current.is_none() {
                        let producer = pending_producer(db, &target, &selection.producer_id)?;
                        state.current = Some(ProducerContinuation {
                            producer,
                            admission: None,
                            admitted: None,
                            join: None,
                            counters: BatchCounters::default(),
                            budget: LookupBudget::new(&limits),
                            authority_cursor: super::witness::OriginalAdmissionCursor::default(),
                            authority: None,
                            // Test-support switch: take the bounded FULL path without a witness attempt.
                            #[cfg(feature = "test-support")]
                            full: std::env::var_os("TRELLIS_TEST_FORCE_FULL_REJOIN").is_some(),
                            #[cfg(not(feature = "test-support"))]
                            full: false,
                            target_cursor: super::local_targets::TargetIndexCursor::default(),
                            target_index_done: false,
                            witness_preparation:
                                super::witness_preparation::WitnessPreparation::default(),
                            witness_ready: false,
                            capture_witness: false,
                            capture_join: None,
                        });
                    }
                    let current = state.current.as_mut().unwrap();
                    if !current.full && current.authority.is_none() {
                        match super::witness::source_authority(
                            db,
                            &target,
                            &selection,
                            &mut current.authority_cursor,
                            &limits,
                            cancel,
                        )? {
                            super::witness::AuthorityStep::Progress => return Ok(None),
                            super::witness::AuthorityStep::Eligible(authority) => {
                                Self::semantic_phase_diagnostic("authority", "witnessed");
                                current.authority = Some(authority)
                            }
                            super::witness::AuthorityStep::Ineligible => {
                                Self::semantic_phase_diagnostic("authority", "ineligible");
                                current.full = true
                            }
                            super::witness::AuthorityStep::Full(reason) => {
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
                            let mut stage = crate::semantic_batch::staging::Stage::create(
                                &stage_parent,
                                &format!("native-target-{}", uuid::Uuid::new_v4()),
                                &session.incarnation().to_string(),
                            )?;
                            stage.release_connection();
                            state.target_stage = Some(stage);
                            return Ok(None);
                        }
                        let stage = state.target_stage.as_mut().unwrap();
                        let advanced = super::local_targets::advance(
                            db,
                            &target,
                            selection.captured_pin,
                            stage,
                            &mut current.target_cursor,
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
                        current.admission = Some(if current.full {
                            RetainedStageContinuation::new(
                                db,
                                &captured,
                                selection.batch_id,
                                &stage_parent,
                                owner,
                                &limits,
                                cancel,
                            )?
                            .authenticated()
                        } else {
                            RetainedStageContinuation::new_witnessed(
                                db,
                                &captured,
                                selection.batch_id,
                                &stage_parent,
                                owner,
                                &limits,
                                cancel,
                            )?
                        });
                        return Ok(None);
                    }
                    if let Some(admission) = current.admission.as_mut() {
                        if let Some(admitted) =
                            admission.advance(db, &captured, &mut current.counters, cancel)?
                        {
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
                        let result = current.witness_preparation.advance(
                            db,
                            &target,
                            current
                                .authority
                                .as_deref()
                                .context("missing original witness authority")?,
                            &destination,
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
                            current.join =
                                Some(JoinContinuation::new_witnessed(target.pin, limits.clone()));
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
                                super::super::captured_witness::stage_documents(
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
                        if super::oracle::enabled() {
                            let comparison = super::oracle::check(
                                self,
                                db,
                                &target,
                                &selection,
                                &current.producer,
                                admitted,
                                cancel,
                            );
                            admitted.stage_mut().release_connection();
                            // Oracle failures are test failures, never producer unavailability.
                            comparison.map_err(|error| error.context(RejoinOracleMismatch))?;
                        }
                    }
                    Ok(joined)
                })();
                // Private stage connections outlive the step and the slice: reopening one
                // (schema load, pragmas, a cold page cache) cost more than the step.
                let step = match step {
                    Ok(None) if started.elapsed() < step_slice() => match check_cancel(cancel) {
                        Ok(()) => continue,
                        Err(error) => Err(error),
                    },
                    step => step,
                };
                let committed = state.commit_stage_batches();
                return match (step, committed) {
                    (Ok(_), Err(error)) => Err(error.into()),
                    (step, _) => step,
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
                });
                state.next += 1;
                Ok(SemanticUnitOutcome::Progress)
            }
            Ok(Some(None)) => Ok(SemanticUnitOutcome::Progress),
            Err(error) => {
                if let Err(cancelled) = check_cancel(cancel) {
                    *continuation = None;
                    return Err(cancelled);
                }
                if rejoin_storage_failure(&error) {
                    *continuation = None;
                    return Err(rejoin_storage_error(error));
                }
                #[cfg(feature = "test-support")]
                if error.is::<RejoinOracleMismatch>() {
                    *continuation = None;
                    return Err(error);
                }
                if error.is::<SelectedIntegrity>() {
                    *continuation = None;
                    return Err(error);
                }
                if let Some(_fallback) =
                    error.downcast_ref::<crate::semantic_batch::staging::WitnessFallback>()
                {
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
                        #[cfg(feature = "test-support")]
                        crate::semantic_import::note_local_witness_outcome(Some(_fallback.0));
                        return Ok(SemanticUnitOutcome::Progress);
                    }
                }
                // Content failures have a terminal, explicit unavailable disposition; they
                // do not create an endless background retry of the same selected artifact.
                let producer = if let Some(current) = state.current.take() {
                    current.producer
                } else {
                    self.native_at_reconcile(target.pin, |db, _| {
                        pending_producer(db, &target, &selection.producer_id)
                    })?
                };
                state.prepared.push(PreparedSemantic {
                    producer,
                    capture_witness: None,
                    output: PreparedOutput::Unavailable {
                        diagnostic: format!("selected raw capture unavailable: {error:#}")
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

#[cfg(feature = "test-support")]
fn write_semantic_work_counts(pin: IndexPin, target: &PendingSemanticTarget) -> Result<()> {
    if let Some(file) = std::env::var_os("TRELLIS_TEST_LOCAL_WORK_COUNTS_FILE") {
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

fn pending_producer(
    db: &Connection,
    target: &PendingSemanticTarget,
    id: &str,
) -> Result<crate::semantic_evidence::Producer> {
    let key = semantic_pin_key(target.pin);
    let (version,hash,kind,encoding):(String,String,String,String)=db.query_row(
        "SELECT p.version,p.executable_hash,p.kind,p.position_encoding FROM semantic_revision_producers s JOIN semantic_producers p ON p.id=s.id AND p.version=s.version WHERE s.revision_id=?1 AND s.id=?2",
        params![key,id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    let languages = semantic_ordered(
        db,
        "SELECT ordinal,language FROM semantic_producer_languages WHERE id=?1 AND version=?2 ORDER BY ordinal",
        &[&id, &version],
    )?;
    Ok(crate::semantic_evidence::Producer {
        id: id.to_owned(),
        version,
        executable_hash: hash,
        kind: semantic_enum(kind)?,
        position_encoding: semantic_enum(encoding)?,
        languages,
    })
}
