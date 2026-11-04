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

pub(crate) struct SemanticContinuation {
    target: PendingSemanticTarget,
    next: usize,
    current: Option<ProducerContinuation>,
    prepared: Vec<PreparedSemantic>,
    target_stage: Option<crate::semantic_batch::staging::Stage>,
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
}

impl Store {
    pub(crate) fn semantic_maintenance_unit(
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
            *continuation = Some(SemanticContinuation {
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
                    witness_preparation: super::witness_preparation::WitnessPreparation::default(),
                    witness_ready: false,
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
                        current.authority = Some(authority)
                    }
                    super::witness::AuthorityStep::Full(_reason) => {
                        current.full = true;
                        #[cfg(feature = "test-support")]
                        crate::semantic_import::note_local_witness_outcome(Some(_reason));
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
                stage.release_connection();
                let advanced = advanced?;
                current.target_index_done = advanced.done;
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
                    current.admitted = Some(admitted);
                    current.admission = None;
                    if current.full {
                        current.join = Some(JoinContinuation::new(
                            target.pin,
                            limits.clone(),
                            DestinationMutations::default(),
                        ));
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
                stage.release_connection();
                admitted.stage_mut().release_connection();
                current.witness_ready = result?;
                if current.witness_ready {
                    current.join =
                        Some(JoinContinuation::new_witnessed(target.pin, limits.clone()));
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
            admitted.stage_mut().release_connection();
            let joined = result?;
            if let Some(joined) = &joined {
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
        });
        match result {
            Ok(None) => {
                *continuation = None;
                Ok(SemanticUnitOutcome::Progress)
            }
            Ok(Some(Some(joined))) => {
                let current = state.current.take().unwrap();
                #[cfg(feature = "test-support")]
                if !current.full {
                    crate::semantic_import::note_local_witness_outcome(None);
                }
                state.prepared.push(PreparedSemantic {
                    producer: current.producer,
                    output: PreparedOutput::Joined {
                        admitted: Box::new(current.admitted.unwrap()),
                        joined,
                    },
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
