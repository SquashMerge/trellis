//! The storage side of one semantic catch-up unit (S14): what the S scheduler may read, as
//! handles only storage builds. A handle lives for one snapshot closure, borrows that
//! snapshot's connection and never hands it out, and is never opened on a caller's pin.
use super::continuation::HeldIndexRead;
use super::local_targets::{TargetAdvance, TargetIndexCursor};
use super::retained_batch::RetainedBatchRows;
use super::witness::{AuthorityStep, OriginalAdmissionCursor, SourceAuthority};
use super::witness_preparation::WitnessPreparation;
use super::*;
use crate::semantic_batch::{
    counters::{BatchCounters, LookupBudget},
    limits::BatchLimits,
};
use crate::semantic_records::Producer;
use crate::store::batch_input::AdmittedBatch;
use crate::store::staging::Stage;

/// The read connection one S continuation keeps across its yields, opaque above storage.
/// Dropping the continuation's handle closes the connection.
#[derive(Clone)]
pub(crate) struct HeldRead(Arc<HeldIndexRead>);

/// The scheduler's per-producer witness state, opaque above storage: where the bounded
/// original-authority check, the LOCAL target index and the witness preparation stand.
#[derive(Default)]
pub(crate) struct WitnessProgress {
    authority: OriginalAdmissionCursor,
    targets: TargetIndexCursor,
    preparation: WitnessPreparation,
}

/// The original import's authority for one witnessed selection, opaque above storage.
pub(crate) struct WitnessAuthority(Box<SourceAuthority>);

/// One bounded step of the original-authority check.
pub(crate) enum AuthorityOutcome {
    Progress,
    Eligible(WitnessAuthority),
    /// A witness was attempted and declined: FULL, counted as a fallback with its reason.
    Full(&'static str),
    /// The witnessed route does not apply: FULL by rule.
    Ineligible,
}

/// How a failed S step ends.
pub(crate) enum UnitFailure {
    /// Cancellation, a storage or integrity failure, or (test support) an oracle mismatch:
    /// the continuation is discarded and this error is the unit's.
    Terminal(anyhow::Error),
    /// Anything else: a witness fallback or the producer's content failure.
    Content(anyhow::Error),
}

impl UnitFailure {
    /// Sorts a failed step, in this order: cancellation, storage, oracle mismatch, integrity.
    pub(crate) fn of(error: anyhow::Error, cancel: &CancelFlag) -> Self {
        if let Err(cancelled) = check_cancel(cancel) {
            return Self::Terminal(cancelled);
        }
        if rejoin_storage_failure(&error) {
            return Self::Terminal(rejoin_storage_error(error));
        }
        #[cfg(feature = "test-support")]
        if error.is::<RejoinOracleMismatch>() {
            return Self::Terminal(error);
        }
        if error.is::<SelectedIntegrity>() {
            return Self::Terminal(error);
        }
        Self::Content(error)
    }
}

impl Store {
    /// What every S unit checks before it reads: the writer session still owns the checkout,
    /// the root identity holds, and the operation is not cancelled.
    pub(crate) fn begin_semantic_unit(
        &self,
        session: &topology::WriterSession,
        cancel: &CancelFlag,
    ) -> Result<()> {
        self.verify_writer_session(session)
            .and_then(|_| self.identity.verify())
            .and_then(|_| check_cancel(cancel))
    }

    /// A new held read, registered so [`Store::release_semantic_reads`] can close it.
    pub(crate) fn open_semantic_read(&self) -> HeldRead {
        let read = Arc::new(HeldIndexRead::default());
        {
            let mut reads = self.semantic_reads.lock().unwrap();
            reads.retain(|read| read.strong_count() > 0);
            reads.push(Arc::downgrade(&read));
        }
        HeldRead(read)
    }

    /// `pending_semantic_target` on a continuation's held connection.
    fn pending_semantic_target_held(
        &self,
        held: &HeldIndexRead,
    ) -> Result<Option<PendingSemanticTarget>> {
        if self.disposition() == RecoveryDisposition::UpgradePending || self.is_recreate_pending() {
            return Err(
                topology::IndexNotReady::new("exceptional index reconciliation pending").into(),
            );
        }
        self.with_held_read(held, |db| {
            let target = pending_target(db)?;
            if let Some(target) = &target {
                selected_ensure!(
                    target.root_id == self.identity.root_key,
                    "incompatible_index: pending semantic root differs"
                );
            }
            Ok(target)
        })
    }

    /// The pending semantic target, on `held` when the continuation has one.
    pub(crate) fn pending_semantic_target_on(
        &self,
        held: Option<&HeldRead>,
    ) -> Result<Option<PendingSemanticTarget>> {
        match held {
            Some(held) => self.pending_semantic_target_held(&held.0),
            None => self.pending_semantic_target(),
        }
    }

    /// One snapshot on `held` for one selection of `target`: `Ok(None)` when the pending
    /// target is no longer `target`.
    ///
    /// The destination is selected in the snapshot. The captured pin is the second form of
    /// selection, **selection by a validated pending row**: `selection` must be one of
    /// `target.selections`, and `target` is compared with the pending rows storage validates
    /// in this snapshot, so the captured pin is one storage read and checked here, never a
    /// caller's. It is retained while the selection references it and is not checked for
    /// release.
    pub(crate) fn with_pending_semantic_read<T>(
        &self,
        target: &PendingSemanticTarget,
        selection: &PendingSemanticSelection,
        held: &HeldRead,
        read: impl FnOnce(&PendingSemanticRead<'_>) -> Result<T>,
    ) -> Result<Option<T>> {
        ensure!(
            target.selections.contains(selection),
            "semantic selection is not one of the pending target's"
        );
        self.with_pending_semantic_inputs(target, &held.0, |db, destination| {
            read(&PendingSemanticRead {
                store: self,
                db,
                target,
                selection,
                destination,
                captured: ReadRevision::new(
                    selection.captured_pin,
                    semantic_pin_key(selection.captured_pin),
                ),
            })
        })
    }

    /// The producer row recorded at the pending target, in its own snapshot.
    pub(crate) fn pending_producer_at(
        &self,
        target: &PendingSemanticTarget,
        selection: &PendingSemanticSelection,
    ) -> Result<Producer> {
        self.native_at_reconcile(target.pin, |db, _| {
            pending_producer(db, target, &selection.producer_id)
        })
    }

    /// The limits one S operation applies.
    pub(crate) fn semantic_unit_limits(&self) -> BatchLimits {
        self.batch_publication_limits()
    }

    /// Whether this store runs the independent FULL rejoin oracle.
    #[cfg(feature = "test-support")]
    pub(crate) fn semantic_oracle_enabled(&self) -> bool {
        super::oracle::enabled(self)
    }

    /// The committed head's reader-visible rows of every prepared producer against the FULL
    /// rejoin its pre-commit oracle computed. The outer error is the head read's; the inner
    /// one is a mismatch.
    #[cfg(feature = "test-support")]
    pub(crate) fn semantic_oracle_check_published(
        &self,
        published: IndexPin,
        prepared: &[PreparedSemantic],
    ) -> Result<Result<()>> {
        let durable = self.cache()?;
        for prepared in prepared {
            if let Some(full) = &prepared.oracle_full
                && let Err(error) =
                    super::oracle::check_published(&durable, published, &prepared.producer, full)
            {
                return Ok(Err(error.context(RejoinOracleMismatch)));
            }
        }
        Ok(Ok(()))
    }
}

fn pending_producer(db: &Connection, target: &PendingSemanticTarget, id: &str) -> Result<Producer> {
    let key = semantic_pin_key(target.pin);
    let (version,hash,kind,encoding):(String,String,String,String)=db.query_row(
        "SELECT p.version,p.executable_hash,p.kind,p.position_encoding FROM semantic_revision_producers s JOIN semantic_producers p ON p.id=s.id AND p.version=s.version WHERE s.revision_id=?1 AND s.id=?2",
        params![key,id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    let languages = semantic_ordered(
        db,
        "SELECT ordinal,language FROM semantic_producer_languages WHERE id=?1 AND version=?2 ORDER BY ordinal",
        &[&id, &version],
    )?;
    Ok(Producer {
        id: id.to_owned(),
        version,
        executable_hash: hash,
        kind: semantic_enum(kind)?,
        position_encoding: semantic_enum(encoding)?,
        languages,
    })
}

/// One coherent snapshot of the main index for one selection of the pending target.
pub(crate) struct PendingSemanticRead<'a> {
    store: &'a Store,
    db: &'a Connection,
    target: &'a PendingSemanticTarget,
    selection: &'a PendingSemanticSelection,
    destination: &'a ReadRevision,
    captured: ReadRevision,
}

impl PendingSemanticRead<'_> {
    /// Native rows at the pending target's pin.
    pub(crate) fn destination(&self) -> NativeReader<'_> {
        NativeReader {
            store: self.store,
            db: self.db,
            selected: self.destination,
        }
    }

    /// Native rows at the selection's captured pin.
    pub(crate) fn captured(&self) -> NativeReader<'_> {
        NativeReader {
            store: self.store,
            db: self.db,
            selected: &self.captured,
        }
    }

    /// The selection's producer row, as recorded at the pending target.
    pub(crate) fn producer(&self) -> Result<Producer> {
        pending_producer(self.db, self.target, &self.selection.producer_id)
    }

    /// Storage's cancellation check, between the steps of one slice.
    pub(crate) fn check_cancel(&self, cancel: &CancelFlag) -> Result<()> {
        check_cancel(cancel)
    }

    /// What decides, per document, whether S copies it for this selection.
    pub(crate) fn copy_filter(&self) -> super::copy::CopyFilter {
        super::copy::CopyFilter {
            target: self.target.clone(),
            captured_pin: self.selection.captured_pin,
            producer: self.selection.producer_id.clone(),
            batch_id: self.selection.batch_id,
            join_inputs: self.store.payload_key_inputs().join_inputs().to_owned(),
        }
    }

    /// One bounded step of deciding whether the original import can witness this selection.
    pub(crate) fn source_authority(
        &self,
        progress: &mut WitnessProgress,
        limits: &BatchLimits,
        cancel: &CancelFlag,
    ) -> Result<AuthorityOutcome> {
        Ok(
            match super::witness::source_authority(
                self.db,
                self.target,
                self.selection,
                &mut progress.authority,
                self.store.payload_key_inputs().join_inputs(),
                limits,
                cancel,
            )? {
                AuthorityStep::Progress => AuthorityOutcome::Progress,
                AuthorityStep::Eligible(authority) => {
                    AuthorityOutcome::Eligible(WitnessAuthority(authority))
                }
                AuthorityStep::Full(reason) => AuthorityOutcome::Full(reason),
                AuthorityStep::Ineligible => AuthorityOutcome::Ineligible,
            },
        )
    }

    /// One inventory document or one bounded declaration page into the LOCAL target stage.
    pub(crate) fn advance_local_targets(
        &self,
        stage: &mut Stage,
        progress: &mut WitnessProgress,
        limits: &BatchLimits,
        cancel: &CancelFlag,
    ) -> Result<LocalTargetStep> {
        let TargetAdvance {
            done,
            builds,
            changed_documents,
            current_target_rows,
        } = super::local_targets::advance(
            self.db,
            self.target,
            self.selection.captured_pin,
            stage,
            &mut progress.targets,
            limits,
            cancel,
        )?;
        Ok(LocalTargetStep {
            done,
            builds,
            changed_documents,
            current_target_rows,
        })
    }

    /// One bounded step staging the original association rows into the admitted stage;
    /// `true` once the witness is prepared.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_witness(
        &self,
        progress: &mut WitnessProgress,
        authority: &WitnessAuthority,
        admitted: &mut AdmittedBatch,
        target_stage: &mut Stage,
        limits: &BatchLimits,
        counters: &mut BatchCounters,
        budget: &mut LookupBudget,
        cancel: &CancelFlag,
    ) -> Result<bool> {
        progress.preparation.advance(
            self.db,
            self.target,
            &authority.0,
            &self.destination(),
            admitted,
            target_stage,
            limits,
            counters,
            budget,
            cancel,
        )
    }

    /// The selected retained raw batch, as typed rows of this snapshot.
    pub(crate) fn retained_batch(&self) -> RetainedBatchRows<'_> {
        RetainedBatchRows::new(
            self.db,
            self.selection.batch_id,
            self.selection.captured_pin,
        )
    }

    /// The test-support switch that makes every S step fail with a content error.
    #[cfg(feature = "test-support")]
    pub(crate) fn content_fault(&self) -> Result<()> {
        anyhow::ensure!(
            !TEST_SEMANTIC_CONTENT_FAULT.get(),
            "test-support semantic content fault"
        );
        Ok(())
    }

    /// The independent FULL rejoin of this selection against the joined stage; a failure is
    /// an oracle mismatch, never producer unavailability.
    #[cfg(feature = "test-support")]
    pub(crate) fn oracle_check(
        &self,
        producer: &Producer,
        admitted: &mut AdmittedBatch,
        cancel: &CancelFlag,
    ) -> Result<crate::semantic_records::SemanticPublication> {
        super::oracle::check(
            self.store,
            self.db,
            self.target,
            self.selection,
            producer,
            admitted,
            cancel,
        )
        .map_err(|error| error.context(RejoinOracleMismatch))
    }

    /// An S that ended unavailable must be one the independent FULL rejoin also fails.
    #[cfg(feature = "test-support")]
    pub(crate) fn oracle_check_unavailable(
        &self,
        producer: &Producer,
        cancel: &CancelFlag,
    ) -> Result<()> {
        super::oracle::check_unavailable(
            self.store,
            self.db,
            self.target,
            self.selection,
            producer,
            cancel,
        )
    }
}

/// What one LOCAL target step did.
// Work fields feed the test-support cost counters only.
#[cfg_attr(not(feature = "test-support"), allow(dead_code))]
pub(crate) struct LocalTargetStep {
    pub(crate) done: bool,
    pub(crate) builds: usize,
    pub(crate) changed_documents: usize,
    pub(crate) current_target_rows: usize,
}
