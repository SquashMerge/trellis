//! One checkout-owned, bounded semantic unit. No main-index snapshot survives a return.
use super::*;
use crate::semantic_batch::RetainedStageContinuation;
use crate::semantic_batch::{
    StageOwner,
    counters::{BatchCounters, LookupBudget},
    limits::BatchLimits,
};
use crate::semantic_import::{DestinationMutations, JoinContinuation};
use crate::store::batch_input::AdmittedBatch;

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

/// The index read connection, with its use guard, that one S continuation keeps across its
/// yields (#671): each unit takes a fresh snapshot on it instead of reopening the index.
/// Dropping the continuation closes it. Exclusive use transitions and retirement close it
/// first through [`Store::release_semantic_reads`]; the next unit then reopens.
#[derive(Default)]
pub(crate) struct HeldIndexRead(Mutex<Option<IndexConnection>>);

pub(crate) struct SemanticContinuation {
    target: PendingSemanticTarget,
    read: Arc<HeldIndexRead>,
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
    /// Close every read connection an S continuation keeps across yields. The worker thread
    /// never runs a transition inside an S unit, so on that thread the connection is always
    /// in its slot here. A unit running on another thread keeps its connection to the end of
    /// its slice, exactly as an unheld connection would.
    pub(crate) fn release_semantic_reads(&self) {
        let held: Vec<_> = self
            .semantic_reads
            .lock()
            .unwrap()
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .collect();
        for read in held {
            let connection = read.0.lock().unwrap().take();
            drop(connection);
        }
    }

    /// How many S continuations keep an open read connection across yields now.
    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn held_semantic_reads_for_tests(&self) -> usize {
        let held: Vec<_> = self
            .semantic_reads
            .lock()
            .unwrap()
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .collect();
        held.iter()
            .filter(|read| read.0.lock().unwrap().is_some())
            .count()
    }

    /// Classify the index for exceptional recreation, as a read finding an obsolete or
    /// exceptional format does.
    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn mark_recreate_pending_for_tests(&self) {
        self.mark_recovery(RecoveryDisposition::RecreatePending);
    }

    /// One read snapshot on a continuation's held connection, opened on first use. A
    /// failed read closes the connection, so the next unit reopens and revalidates it.
    pub(super) fn with_held_read<T>(
        &self,
        held: &HeldIndexRead,
        read: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        let mut connection = held.0.lock().unwrap().take();
        let result = (|| {
            let db = match &mut connection {
                Some(db) => {
                    // What `connect_index` checks after its open, without reopening.
                    (|| {
                        self.ensure_not_recreate_pending()?;
                        self.identity.verify()?;
                        db._use_guard.verify()
                    })()
                    .inspect_err(|error| {
                        self.note_index_failure(error);
                    })?;
                    READ_GATE
                        .with(|gate| *gate.borrow_mut() = Arc::downgrade(&self.publication_gate));
                    db
                }
                None => connection.insert(self.cache()?),
            };
            let tx = storage_result(db.transaction())?;
            begin_read_snapshot(
                &tx,
                &self.roots.index_db(&self.identity),
                Duration::from_secs(5),
            )?;
            read(&tx)
        })()
        .map_err(|error| self.report_index_failure(error));
        if result.is_ok()
            && let Some(connection) = connection
        {
            *held.0.lock().unwrap() = Some(connection);
        }
        result
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
            // SQLite allocates from the process heap, so `heap_in_use_mib` includes it;
            // its own counter is off (`SQLITE_CONFIG_MEMSTATUS=0`, #673).
            let (in_use, allocated) = crate::heap::bytes();
            let _ = writeln!(
                std::io::stderr().lock(),
                "semantic-s phase={phase} detail={detail} wall_ms={wall_ms} heap_in_use_mib={} heap_allocated_mib={}",
                in_use >> 20,
                allocated >> 20
            );
        }
    }

    /// Opt-in (`TRELLIS_INDEX_DIAGNOSTICS=1`) page-cache use of the S commit's
    /// writer just before COMMIT: whether it spilled to `index.db` (#674).
    pub(super) fn semantic_commit_cache_diagnostic(db: &Connection) {
        if std::env::var("TRELLIS_INDEX_DIAGNOSTICS").as_deref() == Ok("1") {
            use std::io::Write as _;
            let status = |op| {
                let (mut current, mut highwater) = (0, 0);
                // SAFETY: the handle belongs to a live connection used on this
                // thread, and the out-pointers are valid locals.
                unsafe {
                    rusqlite::ffi::sqlite3_db_status(
                        db.handle(),
                        op,
                        &mut current,
                        &mut highwater,
                        0,
                    )
                };
                current
            };
            let used = status(rusqlite::ffi::SQLITE_DBSTATUS_CACHE_USED);
            let spill = status(rusqlite::ffi::SQLITE_DBSTATUS_CACHE_SPILL);
            let written = status(rusqlite::ffi::SQLITE_DBSTATUS_CACHE_WRITE);
            let _ = writeln!(
                std::io::stderr().lock(),
                "semantic-s phase=commit_cache detail=before_commit cache_used_mib={} cache_spill_pages={spill} cache_write_pages={written}",
                used >> 20
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
        let target = match continuation.as_ref() {
            Some(state) => self.pending_semantic_target_held(&state.read),
            None => self.pending_semantic_target(),
        };
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
            let read = Arc::new(HeldIndexRead::default());
            {
                let mut reads = self.semantic_reads.lock().unwrap();
                reads.retain(|read| read.strong_count() > 0);
                reads.push(Arc::downgrade(&read));
            }
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
                if super::oracle::enabled(self) {
                    let durable = self.cache()?;
                    for prepared in &state.prepared {
                        if let Some(full) = &prepared.oracle_full
                            && let Err(error) = super::oracle::check_published(
                                &durable,
                                _published_pin,
                                &prepared.producer,
                                full,
                            )
                        {
                            *continuation = None;
                            return Err(error.context(RejoinOracleMismatch));
                        }
                    }
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
        let limits = self.batch_publication_limits();
        let target = state.target.clone();
        let held = state.read.clone();
        let result = self.with_pending_semantic_inputs(&target, &held, |db, destination_read| {
            // One coherent snapshot serves several bounded steps; it never survives a return
            // to worker selection, and the slice keeps foreground work waiting briefly.
            let started = std::time::Instant::now();
            state.batch_stage_connections();
            loop {
                let step = (|| {
                    #[cfg(feature = "test-support")]
                    anyhow::ensure!(
                        !TEST_SEMANTIC_CONTENT_FAULT.get(),
                        "test-support semantic content fault"
                    );
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
                        #[cfg(feature = "test-support")]
                        crate::semantic_import::note_producer_attempt();
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
                            full: self.semantic_test_hooks().force_full_rejoin,
                            #[cfg(not(feature = "test-support"))]
                            full: false,
                            target_cursor: super::local_targets::TargetIndexCursor::default(),
                            target_index_done: false,
                            witness_preparation:
                                super::witness_preparation::WitnessPreparation::default(),
                            witness_ready: false,
                            capture_witness: false,
                            capture_join: None,
                            #[cfg(feature = "test-support")]
                            oracle_full: None,
                        });
                    }
                    let current = state.current.as_mut().unwrap();
                    if !current.full && current.authority.is_none() {
                        match super::witness::source_authority(
                            db,
                            &target,
                            &selection,
                            &mut current.authority_cursor,
                            self.payload_key_inputs().join_inputs(),
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
                            .copying(super::copy::CopyFilter {
                                target: target.clone(),
                                captured_pin: selection.captured_pin,
                                producer: selection.producer_id.clone(),
                                batch_id: selection.batch_id,
                                join_inputs: self.payload_key_inputs().join_inputs().to_owned(),
                            })
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
                        if super::oracle::enabled(self) {
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
                            current.oracle_full = Some(
                                comparison.map_err(|error| error.context(RejoinOracleMismatch))?,
                            );
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
                    self.native_at_reconcile(target.pin, |db, _| {
                        pending_producer(db, &target, &selection.producer_id)
                    })?
                };
                // FULL's own resource bound is the contract's typed outcome, which the
                // unbounded in-memory oracle cannot model; every other unavailable S must
                // be one the independent FULL rejoin also fails (#826).
                #[cfg(feature = "test-support")]
                if super::oracle::enabled(self) && !(_full && resource_limit(&error)) {
                    let checked = self.with_pending_semantic_inputs(&target, &held, |db, _| {
                        super::oracle::check_unavailable(
                            self, db, &target, &selection, &producer, cancel,
                        )
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
                        diagnostic: format!("{}: {error:#}", super::UNAVAILABLE_DIAGNOSTIC)
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

fn pending_producer(
    db: &Connection,
    target: &PendingSemanticTarget,
    id: &str,
) -> Result<crate::semantic_records::Producer> {
    let key = semantic_pin_key(target.pin);
    let (version,hash,kind,encoding):(String,String,String,String)=db.query_row(
        "SELECT p.version,p.executable_hash,p.kind,p.position_encoding FROM semantic_revision_producers s JOIN semantic_producers p ON p.id=s.id AND p.version=s.version WHERE s.revision_id=?1 AND s.id=?2",
        params![key,id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    let languages = semantic_ordered(
        db,
        "SELECT ordinal,language FROM semantic_producer_languages WHERE id=?1 AND version=?2 ORDER BY ordinal",
        &[&id, &version],
    )?;
    Ok(crate::semantic_records::Producer {
        id: id.to_owned(),
        version,
        executable_hash: hash,
        kind: semantic_enum(kind)?,
        position_encoding: semantic_enum(encoding)?,
        languages,
    })
}
