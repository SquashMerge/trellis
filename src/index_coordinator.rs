//! Shared admission and publication boundary for CLI and authenticated daemon index jobs.
//! A job has one expected pair, one captured source set, and one complete paired commit.
use crate::{
    capture::Capture,
    indexer::{self, IndexOptions},
    model::{CancelFlag, IndexPin, IndexProgress},
    store::{PublishPermit, RecoveryBaseline, Store, topology::LeaderSession},
};
use anyhow::{Context, Result, ensure};
use std::sync::{Arc, atomic::Ordering};

// Optional diagnostics never write on the publication or maintenance thread.
// A full bounded channel increments a loss counter rather than blocking work.
struct DiagnosticMarkers {
    sender: std::sync::mpsc::SyncSender<String>,
    lost: std::sync::atomic::AtomicU64,
    origin: std::time::Instant,
}
impl DiagnosticMarkers {
    fn emit(&self, incarnation: &str, event: &str, detail: &str) {
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| time.as_millis());
        let elapsed = self.origin.elapsed().as_micros();
        let lost = self.lost.swap(0, Ordering::AcqRel);
        let message = format!(
            "maintenance wall_ms={wall} monotonic_us={elapsed} incarnation={incarnation} event={event} detail={detail} marker_lost={lost}\n"
        );
        if self.sender.try_send(message).is_err() {
            self.lost.fetch_add(lost + 1, Ordering::AcqRel);
        }
    }
}

fn diagnostics() -> Option<&'static DiagnosticMarkers> {
    static MARKERS: std::sync::OnceLock<Option<DiagnosticMarkers>> = std::sync::OnceLock::new();
    MARKERS
        .get_or_init(|| {
            if std::env::var("TRELLIS_INDEX_DIAGNOSTICS").as_deref() != Ok("1") {
                return None;
            }
            let (sender, receiver) = std::sync::mpsc::sync_channel::<String>(256);
            std::thread::Builder::new()
                .name("trellis-diagnostic-writer".into())
                .spawn(move || {
                    use std::io::Write;
                    while let Ok(line) = receiver.recv() {
                        // Never hold stderr's global lock while waiting for
                        // the next marker: CLI publication diagnostics and
                        // fixture barriers use stderr on other threads.
                        let mut out = std::io::stderr().lock();
                        let _ = out.write_all(line.as_bytes());
                        let _ = out.flush();
                    }
                })
                .ok()?;
            Some(DiagnosticMarkers {
                sender,
                lost: std::sync::atomic::AtomicU64::new(0),
                origin: std::time::Instant::now(),
            })
        })
        .as_ref()
}
pub(crate) fn diagnostics_enabled() -> bool {
    diagnostics().is_some()
}
pub(crate) fn diagnostic_marker(incarnation: &str, event: &str, detail: &str) {
    if let Some(markers) = diagnostics() {
        markers.emit(incarnation, event, detail);
    }
}

struct PublicationAdmission<C> {
    capture: Option<Capture>,
    unchanged_fast: bool,
    cutoff: C,
}

pub struct IndexJobCoordinator {
    store: Store,
    expected: RecoveryBaseline,
    session: Arc<LeaderSession>,
    publication: Option<PublishPermit>,
}

impl IndexJobCoordinator {
    /// The control baseline admits known old indexes without exposing their evidence to readers.
    /// A supplied HTTP pair is checked before any source admission or worker is started.
    pub fn prepare(store: &Store, requested: Option<IndexPin>) -> Result<Self> {
        // Leader opening has its own Store gate around its data_version→COMMIT
        // window. It must finish before acquiring this publication's permit.
        let session = store.leader_session()?;
        let publication = store.enter_publication(
            &Arc::new(std::sync::atomic::AtomicBool::new(false)),
            std::time::Duration::from_millis(250),
        )?;
        diagnostic_marker(
            &session.incarnation().to_string(),
            "publication_wait",
            &format!(
                "reason={:?} duration_us={}",
                publication.wait_reason(),
                publication.waited_for().as_micros()
            ),
        );
        let expected = store.publication_index_baseline()?;
        ensure!(
            requested.is_none_or(|pin| expected.pin() == Some(pin)),
            "revision conflict: prior index pin is not decodable or changed"
        );
        Self::prepare_with_admitted_publication(store, requested, expected, session, publication)
    }

    pub fn prepare_with_session(
        store: &Store,
        requested: Option<IndexPin>,
        session: Arc<LeaderSession>,
    ) -> Result<Self> {
        let publication = store.enter_publication(
            &Arc::new(std::sync::atomic::AtomicBool::new(false)),
            std::time::Duration::from_millis(250),
        )?;
        diagnostic_marker(
            &session.incarnation().to_string(),
            "publication_wait",
            &format!(
                "reason={:?} duration_us={}",
                publication.wait_reason(),
                publication.waited_for().as_micros()
            ),
        );
        let expected = store.publication_index_baseline()?;
        Self::prepare_with_admitted_publication(store, requested, expected, session, publication)
    }

    fn prepare_with_admitted_publication(
        store: &Store,
        requested: Option<IndexPin>,
        expected: RecoveryBaseline,
        session: Arc<LeaderSession>,
        publication: PublishPermit,
    ) -> Result<Self> {
        ensure!(session.is_leader(), "storage_busy: follower cannot publish");
        store.verify_leader_session(&session)?;
        ensure!(
            requested.is_none_or(|pin| expected.pin() == Some(pin)),
            "revision conflict: prior index pin is not decodable or changed"
        );
        Ok(Self {
            store: store.clone(),
            expected,
            session,
            publication: Some(publication),
        })
    }

    pub fn session(&self) -> Arc<LeaderSession> {
        self.session.clone()
    }
    /// A diagnostic decision for two immutable admissions. Publication classifies
    /// its persisted prior manifest in one private read snapshot instead.
    pub fn staged_capture_decision(
        previous: &Capture,
        current: &Capture,
    ) -> indexer::CapturedChange {
        indexer::measure_captured_change(previous, current)
    }

    /// This stand-alone measurement is nonpublishable without a pinned prior manifest,
    /// authenticated immutable versions and a writer-transaction CAS.
    pub fn staged_capture_measurement(
        previous: &Capture,
        current: &Capture,
        root: &std::path::Path,
        root_id: &str,
        cancel: &CancelFlag,
        on_extract: impl FnMut(&crate::native_evidence::DocumentKey),
    ) -> Result<indexer::StagedNativeMeasurement> {
        indexer::measure_captured_native_change(
            previous, current, root, root_id, cancel, on_extract,
        )
    }

    /// Projection uses the admitted bytes; publication checks drift, cancellation and the
    /// whole expected pair under the writer lock before making graph and native rows visible.
    pub fn run(
        self,
        options: &IndexOptions,
        cancel: &CancelFlag,
        progress: impl Fn(IndexProgress) + Sync,
    ) -> Result<IndexPin> {
        self.run_observed(options, cancel, progress, |_| {})
    }

    /// `observe` sees the job's single capture after admission and before publication.
    pub(crate) fn run_observed(
        mut self,
        options: &IndexOptions,
        cancel: &CancelFlag,
        progress: impl Fn(IndexProgress) + Sync,
        observe: impl FnOnce(&Capture),
    ) -> Result<IndexPin> {
        self.run_with_capture(
            options,
            cancel,
            progress,
            observe,
            |_, _| {},
            PublicationAdmission {
                capture: None,
                unchanged_fast: false,
                cutoff: || Ok(()),
            },
        )
    }

    /// A fresh, guarded unchanged capture may reuse selected native versions
    /// for serving and for a normally claimed explicit request.
    pub fn run_serving(
        mut self,
        options: &IndexOptions,
        cancel: &CancelFlag,
        progress: impl Fn(IndexProgress) + Sync,
    ) -> Result<IndexPin> {
        self.run_with_capture(
            options,
            cancel,
            progress,
            |_| {},
            |_, _| {},
            PublicationAdmission {
                capture: None,
                unchanged_fast: true,
                cutoff: || Ok(()),
            },
        )
    }

    /// Publish exactly the leader's admitted immutable snapshot. A watch cutoff
    /// rejects signals observed before publication; later signals stay pending.
    fn run_captured_serving(
        mut self,
        options: &IndexOptions,
        cancel: &CancelFlag,
        capture: Capture,
        cutoff: impl FnMut() -> Result<()>,
    ) -> Result<(IndexPin, PublishPermit)> {
        let pin = self.run_with_capture(
            options,
            cancel,
            |_| {},
            |_| {},
            |_, _| {},
            PublicationAdmission {
                capture: Some(capture),
                unchanged_fast: false,
                cutoff,
            },
        )?;
        Ok((pin, self.publication.take().expect("publication admitted")))
    }

    fn run_with_capture(
        &mut self,
        options: &IndexOptions,
        cancel: &CancelFlag,
        progress: impl Fn(IndexProgress) + Sync,
        observe: impl FnOnce(&Capture),
        native_observe: impl Fn(
            &crate::native_evidence::DocumentKey,
            crate::native_evidence::FullNativeStage,
        ),
        admission: PublicationAdmission<impl FnMut() -> Result<()>>,
    ) -> Result<IndexPin> {
        let PublicationAdmission {
            capture: admitted,
            unchanged_fast,
            mut cutoff,
        } = admission;
        ensure!(!cancel.load(Ordering::Acquire), "index cancelled");
        let mut phase_start = std::time::Instant::now();
        let report = |name: &str, elapsed: std::time::Duration| {
            progress(IndexProgress {
                phase: format!("timing:{name}"),
                completed: elapsed.as_micros() as usize,
                total: 1,
            });
        };
        self.store.begin_leader_publication(&self.session)?;
        let capture = match admitted {
            Some(capture) => capture,
            None => Capture::admit(options, cancel, &progress)?,
        };
        report("capture", phase_start.elapsed());
        phase_start = std::time::Instant::now();
        if unchanged_fast {
            cutoff()?;
        }
        if unchanged_fast
            && let Some(pin) = self.store.publish_unchanged_native_recovery(
                &capture,
                self.session.leader_guard()?,
                &self.expected,
                cancel,
            )?
        {
            progress(IndexProgress {
                phase: "mode:unchanged".into(),
                completed: 0,
                total: 1,
            });
            report("publish", phase_start.elapsed());
            observe(&capture);
            self.store
                .attest_post_acquisition_reconciliation(&self.session, pin)?;
            return Ok(pin);
        }
        let root = std::fs::canonicalize(&options.workspace_root)?;
        if let Some(prepared) =
            self.store
                .prepare_local_revision(&capture, &self.expected, cancel)?
        {
            report("measure", phase_start.elapsed());
            phase_start = std::time::Instant::now();
            let changed = indexer::project_native_document(
                options,
                &capture,
                &prepared.native,
                &prepared.changed_path,
                cancel,
                &progress,
            )?;
            if let Some(graph) = self
                .store
                .compose_local_revision(&capture, &prepared, changed, cancel)?
            {
                progress(IndexProgress {
                    phase: "mode:local".into(),
                    completed: 0,
                    total: 1,
                });
                report("compose", phase_start.elapsed());
                phase_start = std::time::Instant::now();
                capture.verify(cancel)?;
                observe(&capture);
                ensure!(!cancel.load(Ordering::Acquire), "index cancelled");
                self.store.verify_leader_session(&self.session)?;
                report("attest", phase_start.elapsed());
                phase_start = std::time::Instant::now();
                cutoff()?;
                let pin = self.store.publish_local_native_recovery(
                    &graph,
                    &capture,
                    prepared,
                    self.session.leader_guard()?,
                    self.expected.clone(),
                    cancel,
                )?;
                report("publish", phase_start.elapsed());
                self.store
                    .attest_post_acquisition_reconciliation(&self.session, pin)?;
                return Ok(pin);
            }
            // A moved D4 class cut is unproved: recalculate every native fact
            // from the SAME authenticated capture, without a second admission.
            phase_start = std::time::Instant::now();
        }
        progress(IndexProgress {
            phase: "mode:full".into(),
            completed: 0,
            total: 1,
        });
        let native = crate::native_evidence::from_capture_observed(
            &capture,
            &root,
            self.store.root_id(),
            cancel,
            native_observe,
        )?;
        report("measure", phase_start.elapsed());
        phase_start = std::time::Instant::now();
        let graph = indexer::project_native(options, &capture, &native, cancel, &progress)?;
        report("compose", phase_start.elapsed());
        phase_start = std::time::Instant::now();
        capture.verify(cancel)?;
        observe(&capture);
        ensure!(!cancel.load(Ordering::Acquire), "index cancelled");
        self.store.verify_leader_session(&self.session)?;
        report("attest", phase_start.elapsed());
        phase_start = std::time::Instant::now();
        cutoff()?;
        let published = self.store.publish_native_recovery(
            &graph,
            &capture,
            &native,
            self.session.leader_guard()?,
            self.expected.clone(),
            cancel,
        )?;
        report("publish", phase_start.elapsed());
        self.store
            .attest_post_acquisition_reconciliation(&self.session, published)?;
        Ok(published)
    }
}

#[derive(Debug)]
struct CutoffChanged;
impl std::fmt::Display for CutoffChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("watch hints changed before publication cutoff")
    }
}
impl std::error::Error for CutoffChanged {}

/// The elected owner alone drives the advisory watcher and the durable FIFO.
/// This is an in-memory scheduler, not a second request queue.
pub struct LeaderWork {
    watch: crate::watch::WatchSignals,
    last_inventory: std::time::Instant,
    retry_after: Option<std::time::Instant>,
    last_accounted_generation: Option<u64>,
    last_accounted_pin: Option<IndexPin>,
    options: IndexOptions,
    witness: Option<Arc<crate::daemon::causal_witness::CausalWitness>>,
    witness_owner: Option<uuid::Uuid>,
    witness_epoch: uuid::Uuid,
    witness_ordinal: u64,
    witness_ordinal_source: Option<Arc<std::sync::atomic::AtomicU64>>,
    witness_reported_generation: Option<u64>,
}

impl LeaderWork {
    /// Construct only after the synced incarnation and complete takeover reconcile.
    pub fn new(
        store: &Store,
        session: &Arc<LeaderSession>,
        options: &IndexOptions,
    ) -> Result<Self> {
        store.verify_leader_session(session)?;
        let watch = crate::watch::WatchSignals::new(
            options.workspace_root.clone(),
            options.scip_path.clone(),
            options.manifest_path.clone(),
        );
        Ok(Self {
            watch,
            last_inventory: std::time::Instant::now(),
            retry_after: None,
            last_accounted_generation: None,
            last_accounted_pin: None,
            options: options.clone(),
            witness: None,
            witness_owner: None,
            witness_epoch: uuid::Uuid::nil(),
            witness_ordinal: 0,
            witness_ordinal_source: None,
            witness_reported_generation: None,
        })
    }

    /// Bind the optional fixture reporter only after this verified owner installs
    /// the watcher. Generation numbers belong to this exact watch epoch.
    pub fn attach_causal_witness(
        &mut self,
        reporter: Arc<crate::daemon::causal_witness::CausalWitness>,
        owner: uuid::Uuid,
        ordinal_source: Arc<std::sync::atomic::AtomicU64>,
    ) {
        self.witness_ordinal = ordinal_source.fetch_add(1, Ordering::AcqRel) + 1;
        self.witness_epoch = uuid::Uuid::new_v4();
        self.witness_owner = Some(owner);
        self.witness_ordinal_source = Some(ordinal_source);
        self.witness = Some(reporter);
    }

    pub fn causal_lineage(&self) -> Option<crate::daemon::causal_witness::Lineage> {
        Some(crate::daemon::causal_witness::Lineage {
            owner_incarnation: self.witness_owner?,
            watch_epoch: self.witness_epoch,
            ordinal: self.witness_ordinal,
        })
    }

    pub fn accounted_watch_generation(&self) -> Option<u64> {
        self.last_accounted_generation
    }

    /// Captured while the publication permit still covered the acknowledged
    /// inventory. A later head must not borrow this watch ACK.
    pub fn accounted_watch_pin(&self) -> Option<IndexPin> {
        self.last_accounted_pin
    }

    /// An accepted signal is visible at ingress, before debounce and before
    /// the native stream drains the bounded channel. A selected-options change
    /// has not registered a replacement watcher yet, so it is also intent.
    pub fn accepted_watch_intent(&self, options: &IndexOptions) -> bool {
        let now = std::time::Instant::now();
        // A sticky degraded/full batch whose generation was already verified
        // must not starve maintenance during the 60s inventory idle interval.
        // Once the inventory is due, it gets the same retry eligibility as
        // reconcile_due and takes priority before maintenance admission.
        let periodic_ready = now.duration_since(self.last_inventory)
            >= std::time::Duration::from_secs(60)
            && !self.retry_after.is_some_and(|deadline| deadline > now);
        self.options.workspace_root != options.workspace_root
            || self.options.scip_path != options.scip_path
            || self.options.manifest_path != options.manifest_path
            || self.options.max_file_bytes != options.max_file_bytes
            || self.watch.accepted_unacked()
            || periodic_ready
    }

    #[doc(hidden)]
    pub fn disable_native_watcher_for_tests(&mut self) {
        self.watch.disable_native_watcher_for_tests();
    }

    /// Feed the same bounded ingress as notify in a deterministic scheduler
    /// fixture. The intent is visible before debounce and before drain.
    #[doc(hidden)]
    pub fn submit_watch_event_for_tests(&self, event: notify::Result<notify::Event>) {
        self.watch.submit_event(event);
    }

    /// Model a watcher that lost its event channel without modifying the
    /// workspace capture or publication path. The periodic full inventory
    /// must still discover changes to source and previously absent inputs.
    #[doc(hidden)]
    pub fn suppress_watch_signals_for_tests(&mut self, absent_watch_root: std::path::PathBuf) {
        assert!(!absent_watch_root.exists());
        self.watch = crate::watch::WatchSignals::new(absent_watch_root, None, None);
        assert!(self.watch.degraded());
        self.last_accounted_generation = Some(self.watch.drain().generation);
    }

    /// Advance only the inventory deadline in a test. The next ordinary tick
    /// still uses the production admission and publication fences.
    #[doc(hidden)]
    pub fn force_periodic_inventory_for_tests(&mut self) {
        self.last_inventory = std::time::Instant::now() - std::time::Duration::from_secs(60);
    }

    /// A full capture always checks root, leader and selected publication fences.
    /// A failed capture keeps the dirty generation for a later signal or scan.
    pub fn reconcile_due(
        &mut self,
        store: &Store,
        session: &Arc<LeaderSession>,
        options: &IndexOptions,
        cancel: &CancelFlag,
        force: bool,
    ) -> Result<bool> {
        self.reconcile_due_observed(store, session, options, cancel, force, |_, _| {})
    }

    /// The observer runs after immutable admission; it cannot change the admitted bytes.
    /// The same cutoff applies to production and deterministic race tests.
    pub fn reconcile_due_observed(
        &mut self,
        store: &Store,
        session: &Arc<LeaderSession>,
        options: &IndexOptions,
        cancel: &CancelFlag,
        force: bool,
        observe: impl FnOnce(&Capture, &crate::watch::WatchSignals),
    ) -> Result<bool> {
        self.reconcile_due_with_cutoffs(store, session, options, cancel, force, (observe, |_| {}))
    }

    /// Test seam for a hint delivered after the publication cutoff but before
    /// acknowledgment. Such a hint must stay pending for the next inventory.
    pub fn reconcile_due_with_cutoffs(
        &mut self,
        store: &Store,
        session: &Arc<LeaderSession>,
        options: &IndexOptions,
        cancel: &CancelFlag,
        force: bool,
        cutoffs: (
            impl FnOnce(&Capture, &crate::watch::WatchSignals),
            impl FnOnce(&crate::watch::WatchSignals),
        ),
    ) -> Result<bool> {
        let (observe, after_cutoff) = cutoffs;
        store.verify_leader_session(session)?;
        // The takeover FIFO head may have different selected inputs from this
        // daemon's defaults. Keep its watcher until a new watcher has registered
        // for the new options; its full wake then closes the transition gap.
        if self.options.workspace_root != options.workspace_root
            || self.options.scip_path != options.scip_path
            || self.options.manifest_path != options.manifest_path
            || self.options.max_file_bytes != options.max_file_bytes
        {
            let next = crate::watch::WatchSignals::new(
                options.workspace_root.clone(),
                options.scip_path.clone(),
                options.manifest_path.clone(),
            );
            self.watch = next;
            self.options = options.clone();
            self.last_accounted_generation = None;
            self.last_accounted_pin = None;
            if let Some(source) = &self.witness_ordinal_source {
                self.witness_epoch = uuid::Uuid::new_v4();
                self.witness_reported_generation = None;
                self.witness_ordinal = source.fetch_add(1, Ordering::AcqRel) + 1;
            }
        }
        let now = std::time::Instant::now();
        let periodic =
            now.duration_since(self.last_inventory) >= std::time::Duration::from_secs(60);
        let batch = self.watch.drain();
        // Observe each new generation before its capture can acknowledge it.
        // The queue tick holds leader_work across this call; the bounded
        // reporter only try_sends and never opens a socket on this path.
        if self.witness_reported_generation != Some(batch.generation) {
            if let (Some(reporter), Some(lineage)) = (&self.witness, self.causal_lineage()) {
                reporter.watch_pending(lineage, batch.generation);
            }
            self.witness_reported_generation = Some(batch.generation);
        }
        // A failing periodic inventory obeys the same retry delay as a failed
        // watcher run. Do not spin at the queue tick rate while inputs are bad.
        if !force && self.retry_after.is_some_and(|deadline| deadline > now) {
            return Ok(false);
        }
        if !force
            && !periodic
            && ((self.watch.degraded() && self.last_accounted_generation == Some(batch.generation))
                || !self.watch.batch_ready_at(now))
        {
            return Ok(false);
        }
        // The first post-registration full inventory closes the takeover/watch gap.
        // An identical selected capture needs no gratuitous new revision.
        let outcome: Result<PublishPermit> = (|| {
            let publication =
                store.enter_publication(cancel, std::time::Duration::from_millis(250))?;
            diagnostic_marker(
                &session.incarnation().to_string(),
                "publication_wait",
                &format!(
                    "reason={:?} duration_us={}",
                    publication.wait_reason(),
                    publication.waited_for().as_micros()
                ),
            );
            let captured = Capture::admit(options, cancel, &|_| {})?;
            observe(&captured, &self.watch);
            // Reject a hinted edit before checking the admitted bytes against
            // the selected head, and check again at the publication cutoff.
            let mut cutoff = || {
                let current = self.watch.drain();
                if current.generation != batch.generation {
                    return Err(CutoffChanged.into());
                }
                store.verify_leader_session(session)?;
                store.verify_root()?;
                Ok(())
            };
            cutoff()?;
            let baseline = store.publication_index_baseline()?;
            let unchanged = store.selected_capture_unchanged(
                &captured,
                session.leader_guard()?,
                &baseline,
                cancel,
            )?;
            if unchanged {
                cutoff()?;
            } else {
                let coordinator = IndexJobCoordinator::prepare_with_admitted_publication(
                    store,
                    None,
                    baseline,
                    session.clone(),
                    publication,
                )?;
                return coordinator
                    .run_captured_serving(options, cancel, captured, cutoff)
                    .map(|(_, publication)| publication);
            }
            Ok(publication)
        })();
        match outcome {
            Ok(publication) => {
                // Optional witness data must be captured under the same
                // publication permit as this inventory. An uncertain read
                // suppresses only the fixture event, not real reconciliation.
                let witnessed_pin = self
                    .witness
                    .as_ref()
                    .and_then(|_| store.index_baseline().ok());
                after_cutoff(&self.watch);
                let accounted = self.watch.acknowledge(&batch);
                if accounted {
                    self.last_accounted_generation = Some(batch.generation);
                    self.last_accounted_pin = witnessed_pin;
                } else {
                    self.last_accounted_pin = None;
                    // A concurrent event is not part of this selected capture.
                    // Keep it dirty for the next tick or successor full takeover.
                    self.watch.require_full();
                }
                drop(publication);
                self.last_inventory = std::time::Instant::now();
                self.retry_after = None;
                Ok(accounted)
            }
            Err(error) if error.is::<CutoffChanged>() => {
                self.watch.require_full();
                self.retry_after = None;
                Ok(false)
            }
            Err(error) => {
                self.watch.require_full();
                self.retry_after =
                    Some(std::time::Instant::now() + std::time::Duration::from_millis(250));
                Err(error)
            }
        }
    }
}

/// One explicit index command returns the owner that published its one full capture.
/// Exceptional recovery cannot take the live-baseline path or upgrade shared use.
pub fn reconcile_workspace(
    store: &Store,
    options: &IndexOptions,
    cancel: &CancelFlag,
    progress: impl Fn(IndexProgress) + Sync,
) -> Result<(IndexPin, Arc<LeaderSession>)> {
    if store.is_recreate_pending() {
        let (pin, session) = store.recreate_pending_leader_session(options, cancel)?;
        store.fail_changed_root_requests(&session)?;
        return Ok((pin, session));
    }
    let coordinator = IndexJobCoordinator::prepare(store, None)?;
    let session = coordinator.session();
    let pin = coordinator.run(options, cancel, progress)?;
    session.verify()?;
    Ok((pin, session))
}

/// A verified leader owns the queue stream. Reclaimed running rows retain their FIFO position.
/// Callers serialize this function with startup and other local native work.
pub fn drain_requests(store: &Store, session: &Arc<LeaderSession>) -> Result<usize> {
    drain_requests_observed(store, session, |_, _| {})
}

/// Progress is advisory and local. The durable request row alone controls state.
pub fn drain_requests_observed(
    store: &Store,
    session: &Arc<LeaderSession>,
    progress: impl Fn(&str, IndexProgress) + Sync,
) -> Result<usize> {
    drain_requests_observed_with_native(store, session, progress, |_, _, _| {})
}

/// Observe actual full-native extraction and completed validation for each
/// claimed request. The callback is scoped to this drain and retains no state.
pub fn drain_requests_observed_with_native(
    store: &Store,
    session: &Arc<LeaderSession>,
    progress: impl Fn(&str, IndexProgress) + Sync,
    native_observe: impl Fn(
        &str,
        &crate::native_evidence::DocumentKey,
        crate::native_evidence::FullNativeStage,
    ) + Sync,
) -> Result<usize> {
    drain_requests_observed_with_cancel(
        store,
        session,
        &Arc::new(std::sync::atomic::AtomicBool::new(false)),
        progress,
        native_observe,
        usize::MAX,
        None,
    )
}

/// Admit one FIFO row so the daemon can fairly interleave watcher inventory
/// before the next accepted row without introducing another request queue.
pub fn drain_one_request_observed(
    store: &Store,
    session: &Arc<LeaderSession>,
    progress: impl Fn(&str, IndexProgress) + Sync,
) -> Result<usize> {
    drain_requests_observed_with_cancel(
        store,
        session,
        &Arc::new(std::sync::atomic::AtomicBool::new(false)),
        progress,
        |_, _, _| {},
        1,
        None,
    )
}

fn drain_requests_observed_with_cancel(
    store: &Store,
    session: &Arc<LeaderSession>,
    cancel: &CancelFlag,
    progress: impl Fn(&str, IndexProgress) + Sync,
    native_observe: impl Fn(
        &str,
        &crate::native_evidence::DocumentKey,
        crate::native_evidence::FullNativeStage,
    ) + Sync,
    max_completed: usize,
    cutoff_seq: Option<i64>,
) -> Result<usize> {
    if store.root_path_replaced()? {
        store.fail_changed_root_requests(session)?;
        anyhow::bail!("root_changed: old leader stopped after queue failure transition");
    }
    store.verify_leader_session(session)?;
    store.fail_changed_root_requests(session)?;
    // A prior attempt may have published but failed its queue terminal write. Resolve
    // that exact cached result before any new claim or native publication.
    let mut completed = usize::from(store.retry_recorded_completion(session)?);
    loop {
        ensure!(
            !cancel.load(Ordering::Acquire),
            "index wait interrupted; inspect the accepted request's durable state"
        );
        if let Some(cutoff) = cutoff_seq
            && store
                .earliest_unfinished_request()?
                .is_some_and(|row| row.seq > cutoff)
        {
            break;
        }
        let Some(request) = store.claim_request(session)? else {
            break;
        };
        #[cfg(test)]
        let diagnostic_stage = std::cell::Cell::new("options");
        // Capture the authenticated expected pin from the same publication
        // admission. A BUSY COMMIT can be safely retried only if its terminal
        // queue reread and current control pin still match this baseline.
        let mut before_publish = None;
        let mut publication = None;
        let outcome = (|| {
            let options = request.options(std::path::Path::new(store.workspace_root()))?;
            #[cfg(test)]
            diagnostic_stage.set("prepare");
            let mut coordinator = IndexJobCoordinator::prepare_with_session(
                store,
                request.expected,
                session.clone(),
            )?;
            before_publish = Some(coordinator.expected.pin());
            #[cfg(test)]
            diagnostic_stage.set("run");
            let result = coordinator.run_with_capture(
                &options,
                cancel,
                |p| progress(&request.id, p),
                |_| {},
                |key, stage| native_observe(&request.id, key, stage),
                PublicationAdmission {
                    capture: None,
                    unchanged_fast: true,
                    cutoff: || Ok(()),
                },
            );
            // A post-COMMIT error can be ambiguous. Retain the permit through
            // terminal ACK, cached completion or typed busy requeue even then.
            publication = coordinator.publication.take();
            result
        })();
        #[cfg(test)]
        if let Err(ref error) = outcome {
            // Only allowlisted error classes reach CI; no raw source/path/token
            // or generic error text is copied into public test output.
            let text = format!("{error:#}");
            let category = if text.contains("storage_busy") {
                "storage_busy"
            } else if text.contains("recovery_required") {
                "recovery_required"
            } else if text.contains("index_not_ready") {
                "index_not_ready"
            } else if text.contains("incompatible_index") {
                "incompatible_index"
            } else if text.contains("root_changed") {
                "root_changed"
            } else if text.contains("revision conflict") {
                "revision_conflict"
            } else if text.contains("database is locked") {
                "sqlite_locked"
            } else if text.contains("database disk image is malformed") {
                "sqlite_corrupt"
            } else if text.contains("unsafe or oversized") {
                "unsafe_input"
            } else if text.contains("source changed") {
                "source_changed"
            } else {
                "other"
            };
            eprintln!(
                "queue_failure stage={} category={} owner_leader={} claim_matches={} expected_pin={}",
                diagnostic_stage.get(),
                category,
                session.is_leader(),
                request.claim_incarnation.as_deref()
                    == Some(session.incarnation().to_string().as_str()),
                request.expected.is_some()
            );
        }
        // Ctrl-C stops this CLI waiter, not any accepted client's durable work.
        // The claim stays running under the old incarnation and the next
        // verified leader reclaims the FIFO head after full reconciliation.
        // Even a publication that raced Ctrl-C cannot be reported terminally
        // by a cancelled waiter; repeat-after-commit is explicitly allowed.
        ensure!(
            !cancel.load(Ordering::Acquire),
            "index wait interrupted; accepted request remains running for verified reclaim"
        );
        let capture_drift = outcome.as_ref().is_err_and(|error| {
            error.chain().any(|cause| {
                cause
                    .downcast_ref::<crate::capture::WorkspaceInventoryDrift>()
                    .is_some()
                    || cause
                        .downcast_ref::<crate::capture::SourceInputDrift>()
                        .is_some()
            })
        });
        if capture_drift
            || outcome
                .as_ref()
                .is_err_and(crate::store::transient_storage_contention)
        {
            // An invalidated capture cannot publish. A failed busy COMMIT can
            // be ambiguous. For either one, authenticate the claim and exact
            // unchanged pin before returning the SAME seq to queued. A changed
            // pin stays running for verified successor recovery, never an
            // invented terminal ACK or a duplicate native publication.
            let before = match before_publish {
                Some(pin) => pin,
                None => store.recovery_index_baseline()?.pin(),
            };
            store.requeue_busy_claim(session, &request, before)?;
            return Ok(completed);
        }
        if outcome
            .as_ref()
            .is_err_and(crate::store::nonterminal_storage_busy)
        {
            return outcome.map(|_| completed);
        }
        // No unverified worker can mark a request terminal. On fencing loss leave it running
        // for the next incarnation to reclaim after its complete root reconciliation.
        store.record_and_finish_request(session, &request, outcome)?;
        drop(publication);
        completed += 1;
        if completed >= max_completed {
            break;
        }
    }
    Ok(completed)
}

pub(crate) fn retryable_cli_completion_error(error: &anyhow::Error) -> bool {
    // Include storage_result's plain SQLite contention string, but never a
    // cached-claim or leader/root invariant that also uses storage_busy.
    #[cfg(test)]
    if error.to_string() == "storage_busy: injected terminal write failure" {
        return true;
    }
    crate::store::transient_storage_contention(error)
}

/// The CLI is its own sole queue driver. A transient completion failure must be
/// resolved while this verified leader is still held, not handed to a nonexistent
/// daemon or a future command. Never repeat the native publication for that head.
fn retry_cli_recorded_completion(
    store: &Store,
    session: &LeaderSession,
    cancel: &CancelFlag,
    initial: anyhow::Error,
) -> Result<()> {
    if !store.has_recorded_completion(session)? || !retryable_cli_completion_error(&initial) {
        return Err(initial);
    }
    loop {
        ensure!(
            !cancel.load(Ordering::Acquire),
            "index wait interrupted; accepted request remains queued"
        );
        store.verify_leader_session(session)?;
        match store.retry_recorded_completion(session) {
            Ok(true) => return Ok(()),
            Ok(false) => anyhow::bail!("storage_busy: cached completion disappeared"),
            Err(error) if retryable_cli_completion_error(&error) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => return Err(error),
        }
    }
}

/// One explicit CLI command commits before waiting. A free lock requires a complete
/// takeover reconciliation before any queued request is claimed.
pub fn enqueue_and_wait(
    store: &Store,
    options: &IndexOptions,
    cancel: &CancelFlag,
) -> Result<(crate::model::IndexPin, Arc<LeaderSession>)> {
    enqueue_and_wait_observed(store, options, cancel, |_| {})
}

/// CLI-only progress observer; the durable queue and publication route remain shared.
pub fn enqueue_and_wait_observed(
    store: &Store,
    options: &IndexOptions,
    cancel: &CancelFlag,
    progress: impl Fn(IndexProgress) + Sync,
) -> Result<(crate::model::IndexPin, Arc<LeaderSession>)> {
    let (_, pin, session) = enqueue_and_wait_observed_inner(store, options, cancel, progress)?;
    Ok((pin, session))
}

/// Preserve the ID of the one accepted FIFO request for finite CLI reporting.
pub fn enqueue_and_wait_observed_with_request(
    store: &Store,
    options: &IndexOptions,
    cancel: &CancelFlag,
    progress: impl Fn(IndexProgress) + Sync,
) -> Result<(String, crate::model::IndexPin, Arc<LeaderSession>)> {
    enqueue_and_wait_observed_inner(store, options, cancel, progress)
}

fn enqueue_and_wait_observed_inner(
    store: &Store,
    options: &IndexOptions,
    cancel: &CancelFlag,
    progress: impl Fn(IndexProgress) + Sync,
) -> Result<(String, crate::model::IndexPin, Arc<LeaderSession>)> {
    let request = store.enqueue_request(options, None)?;
    let mut observed_store = store.clone();
    let mut held: Option<Arc<LeaderSession>> = None;
    let mut leader_work: Option<LeaderWork> = None;
    // Only direct typed SQLite admission contention has a finite cumulative
    // wait. A live owner's leader-lock contention can outlast a cold FULL run.
    let mut sqlite_busy_since: Option<std::time::Instant> = None;
    loop {
        let store = &observed_store;
        if store.root_path_replaced()? {
            if let Some(session) = &held {
                store.fail_changed_root_requests(session)?;
            }
            anyhow::bail!("root_changed: captured workspace pathname changed");
        }
        store.verify_root()?;
        if let Some(row) = store.request_by_id(&request.id)? {
            match row.state.as_str() {
                "done" => {
                    crate::store::index_diagnostic_stage("done_row_seen");
                    let session = match held.take() {
                        Some(session) => session,
                        None => match store.follower_session() {
                            Ok(session) => session,
                            Err(error)
                                if store.is_recreate_pending()
                                    && error.to_string()
                                        == "recovery_required: exceptional index recovery deferred" =>
                            {
                                // A daemon finished our accepted row while this
                                // process still held its pre-repair disposition.
                                // Re-admit only the same existing root/index;
                                // never report success from queue bytes alone.
                                let refreshed=store.reopen_existing_current_root().with_context(||format!(
                                    "accepted request {} is done in durable queue, but current publication cannot be verified",
                                    request.id
                                ))?;
                                ensure!(
                                    !refreshed.is_recreate_pending(),
                                    "storage_busy: completed request has no verified publication"
                                );
                                observed_store = refreshed;
                                continue;
                            }
                            Err(error) => return Err(error),
                        },
                    };
                    session.verify()?;
                    crate::store::index_diagnostic_stage("session_verified");
                    if session.is_leader() {
                        // A finite owner finishes one bounded inventory/drain at its
                        // release fence. Later edits remain discoverable by takeover.
                        if let Some(work) = leader_work.as_mut() {
                            let cutoff = store.current_request()?.map(|row| row.seq);
                            let selected_options = store
                                .recorded_index_options()?
                                .unwrap_or_else(|| options.clone());
                            work.reconcile_due(store, &session, &selected_options, cancel, true)?;
                            if let Some(cutoff) = cutoff {
                                drain_requests_observed_with_cancel(
                                    store,
                                    &session,
                                    cancel,
                                    |_, phase| progress(phase),
                                    |_, _, _| {},
                                    usize::MAX,
                                    Some(cutoff),
                                )?;
                            }
                            // A bounded second cutoff accounts for signals raised
                            // while the accepted pre-cutoff FIFO rows were drained.
                            let selected_options = store
                                .recorded_index_options()?
                                .unwrap_or_else(|| options.clone());
                            work.reconcile_due(store, &session, &selected_options, cancel, true)?;
                        }
                    }
                    let pin = row.revision.expect("done request has revision");
                    crate::store::index_diagnostic_stage("selected_proof_start");
                    let response = store.evidence_response()?;
                    response.validate_pin(pin)?;
                    response.finish(())?;
                    crate::store::index_diagnostic_stage("selected_proof_complete");
                    return Ok((request.id, pin, session));
                }
                "failed" => anyhow::bail!(
                    "{}: queued indexing failed",
                    row.error_code.unwrap_or_else(|| "index_failed".into())
                ),
                _ => {}
            }
        }
        ensure!(
            !cancel.load(Ordering::Acquire),
            "index wait interrupted; accepted request remains queued"
        );
        if held.is_none() && store.is_recreate_pending() {
            match store.recreate_pending_leader_session(options, cancel) {
                Ok((_, session)) => {
                    store.fail_changed_root_requests(&session)?;
                    leader_work = Some(LeaderWork::new(store, &session, options)?);
                    held = Some(session);
                }
                Err(error)
                    if error.chain().any(|cause| {
                        cause
                            .downcast_ref::<crate::store::topology::StorageBusy>()
                            .is_some()
                    }) => {}
                Err(error)
                    if error.to_string()
                        == "recovery_required: exceptional index recovery deferred" =>
                {
                    // A second owner may have repaired index.db after our
                    // admission snapshot. Refresh only the same verified root
                    // and existing index; never reuse stale recreation authority.
                    match store.reopen_existing_current_root() {
                        Ok(refreshed) if !refreshed.is_recreate_pending() => {
                            observed_store=refreshed;
                            continue;
                        }
                        Ok(_) => {
                            let row=store.request_by_id(&request.id)?;
                            anyhow::bail!("recovery_required: accepted request {} remains {}; index still requires verified recovery",
                                request.id,row.map(|r|r.state).unwrap_or_else(||"unavailable".into()));
                        }
                        Err(wait) if retryable_cli_completion_error(&wait) => {},
                        Err(wait) => return Err(wait).context(format!(
                            "accepted request {} has a durable queue row; recovery could not be refreshed",
                            request.id)),
                    }
                }
                Err(error) => return Err(error),
            }
        }
        if held.is_none() {
            match store.leader_session() {
                Ok(session) => {
                    sqlite_busy_since = None;
                    // Reconcile the root before claiming any accepted FIFO row.
                    // This publication cannot acknowledge a request: each claim
                    // must produce its own fresh, guarded revision afterwards.
                    store.fail_changed_root_requests(&session)?;
                    let earliest = store.earliest_unfinished_request()?;
                    let reconcile_options = store
                        .recorded_index_options()?
                        .or_else(|| {
                            earliest.as_ref().and_then(|row| {
                                row.options(std::path::Path::new(store.workspace_root()))
                                    .ok()
                            })
                        })
                        .unwrap_or_else(|| {
                            IndexOptions::new(std::path::PathBuf::from(store.workspace_root()))
                        });
                    let startup =
                        IndexJobCoordinator::prepare_with_session(store, None, session.clone())?;
                    startup.run(&reconcile_options, cancel, &progress)?;
                    leader_work = Some(LeaderWork::new(store, &session, options)?);
                    held = Some(session);
                }
                Err(error)
                    if error
                        .downcast_ref::<crate::store::SqliteContention>()
                        .is_some() =>
                {
                    // The same accepted request retains its ID and FIFO place.
                    // An index writer may still hold SQLite EX after we fail to
                    // become leader; retry only this direct typed contention.
                    let first = sqlite_busy_since.get_or_insert_with(std::time::Instant::now);
                    if first.elapsed() >= std::time::Duration::from_secs(15) {
                        if store.root_path_replaced()? {
                            anyhow::bail!("root_changed: captured workspace pathname changed");
                        }
                        store.verify_root()?;
                        return Err(error).context(format!(
                            "storage_busy: request {} is durable and may already be complete; check job status",
                            request.id
                        ));
                    }
                }
                Err(error)
                    if format!("{error:#}").contains("storage_busy")
                        || (store.is_recreate_pending()
                            && format!("{error:#}").contains("recovery_required")) =>
                {
                    // Another verified owner may still hold the old use lock.
                    // Our request is already durable; wait rather than exit
                    // with an unreported queued acknowledgement.
                }
                Err(error) => return Err(error),
            }
        }
        if let Some(session) = &held {
            loop {
                match drain_requests_observed_with_cancel(
                    store,
                    session,
                    cancel,
                    |_, phase| progress(phase),
                    |_, _, _| {},
                    1,
                    None,
                ) {
                    Ok(_) => {
                        if let Some(work) = leader_work.as_mut() {
                            let selected_options = store
                                .recorded_index_options()?
                                .unwrap_or_else(|| options.clone());
                            work.reconcile_due(store, session, &selected_options, cancel, false)?;
                        }
                        break;
                    }
                    Err(error) => {
                        if crate::store::nonterminal_storage_busy(&error)
                            && !store.has_recorded_completion(session)?
                        {
                            // A COMMIT may have published before the BUSY was
                            // reported. The durable row is left unfinished;
                            // do not guess success or repeat under this owner.
                            let disposition = store
                                .request_by_id(&request.id)
                                .ok()
                                .flatten()
                                .map_or("unavailable".to_owned(), |row| row.state);
                            return Err(error).context(format!(
                                "accepted request {} remains durable (state={disposition}) for the next verified leader after ambiguous publication; inspect its job status",
                                request.id
                            ));
                        }
                        retry_cli_recorded_completion(store, session, cancel, error)?;
                    }
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// One private low-priority unit, never a queued publication. The caller
/// refreshes its watcher/session snapshot and re-arbitrates before another unit.
/// Contention is deferred, never returned to a foreground FIFO caller.
pub fn cooperative_maintenance_unit(
    store: &Store,
    session: &Arc<LeaderSession>,
    mut priority: impl FnMut() -> bool,
) -> Result<crate::store::MaintenanceOutcome> {
    use crate::store::{MaintenanceOutcome, MaintenanceQueueState};
    if !priority() || session.verify().is_err() {
        return Ok(MaintenanceOutcome::Deferred);
    }
    let Some(permit) = store.maintenance_try_enter() else {
        return Ok(MaintenanceOutcome::Deferred);
    };
    let probe = match store.open_maintenance_queue_probe() {
        Ok(probe) => probe,
        Err(error) if crate::store::transient_storage_contention(&error) => {
            return Ok(MaintenanceOutcome::Deferred);
        }
        Err(error) => return Err(error),
    };
    if probe.check() != MaintenanceQueueState::Clear || !priority() {
        return Ok(MaintenanceOutcome::Deferred);
    }
    let leader = match session.leader_guard() {
        Ok(leader) => leader,
        Err(_) => return Ok(MaintenanceOutcome::Deferred),
    };
    store.maintenance_step(leader, &permit, &probe, priority)
}

/// Establish one bounded serving owner. A free lock performs one complete
/// reconciliation; contention is admitted only as a verified follower.
pub fn establish_serving_session(
    store: &Store,
    explicit_options: Option<&IndexOptions>,
    cancel: &CancelFlag,
) -> Result<Arc<LeaderSession>> {
    if store.is_recreate_pending() {
        let options = explicit_options.ok_or_else(|| {
            anyhow::anyhow!(
                "recovery_required: index options unavailable; run explicit trellis index"
            )
        })?;
        let (_, session) = store.recreate_pending_leader_session(options, cancel)?;
        store.fail_changed_root_requests(&session)?;
        session.verify()?;
        return Ok(session);
    }
    match store.leader_session() {
        Ok(session) => {
            store.fail_changed_root_requests(&session)?;
            let publication =
                store.enter_publication(cancel, std::time::Duration::from_millis(250))?;
            diagnostic_marker(
                &session.incarnation().to_string(),
                "publication_wait",
                &format!(
                    "reason={:?} duration_us={}",
                    publication.wait_reason(),
                    publication.waited_for().as_micros()
                ),
            );
            let expected = store.publication_index_baseline()?;
            let options = match explicit_options {
                Some(options) => options.clone(),
                None => store.recorded_index_options()?.unwrap_or_else(|| {
                    IndexOptions::new(std::path::PathBuf::from(store.workspace_root()))
                }),
            };
            let coordinator = IndexJobCoordinator::prepare_with_admitted_publication(
                store,
                None,
                expected,
                session.clone(),
                publication,
            )?;
            coordinator.run_serving(&options, cancel, |_| {})?;
            session.verify()?;
            Ok(session)
        }
        Err(error) if format!("{error:#}").contains("storage_busy") => store.follower_session(),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::SourceOperations;
    use std::{
        collections::BTreeMap,
        fs,
        sync::{Arc, atomic::AtomicBool},
    };

    #[test]
    fn acquired_leader_cannot_claim_its_own_row_at_precommit_capture_pause() {
        use crate::store::topology::IndexNotReady;
        use std::{sync::mpsc, time::Duration};
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("a.js"), "function old() {}\n").unwrap();
        let original = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let options = IndexOptions::new(workspace.path().to_owned());
        let cancel = Arc::new(AtomicBool::new(false));
        let (_, old_owner) = reconcile_workspace(&original, &options, &cancel, |_| {}).unwrap();
        drop(old_owner);
        fs::write(workspace.path().join("a.js"), "function new() {}\n").unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let own = store.enqueue_request(&options, None).unwrap();
        let coordinator = IndexJobCoordinator::prepare(&store, None).unwrap();
        let session = coordinator.session();
        let (paused_tx, paused_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        std::thread::scope(|scope| {
            let work = scope.spawn(move || {
                coordinator.run_observed(
                    &options,
                    &cancel,
                    |_| {},
                    |_| {
                        paused_tx.send(()).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                    },
                )
            });
            paused_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(session.is_leader() && session.verify().is_ok());
            let precommit = store.claim_request(&session).unwrap_err();
            assert!(
                precommit.downcast_ref::<IndexNotReady>().is_some(),
                "an acquired EX without its own committed H cannot claim: {precommit:#}"
            );
            let row = store.request_by_id(&own.id).unwrap().unwrap();
            assert_eq!(row.state, "queued");
            assert!(row.claim_incarnation.is_none());
            release_tx.send(()).unwrap();
            let committed = work.join().unwrap().unwrap();
            assert_eq!(store.status().unwrap().revision, committed);
            let claimed = store.claim_request(&session).unwrap().unwrap();
            assert_eq!(claimed.id, own.id);
            assert_eq!(claimed.state, "running");
            assert_eq!(
                claimed.claim_incarnation,
                Some(session.incarnation().to_string())
            );
        });
    }

    #[test]
    fn claimed_request_requeues_after_source_input_open_and_read_edits() {
        for phase in ["open", "read"] {
            let state = tempfile::tempdir().unwrap();
            let workspace = tempfile::tempdir().unwrap();
            let source = workspace.path().join("a.js");
            let manifest = workspace.path().join("package.json");
            fs::write(&source, "function before() {}\n").unwrap();
            fs::write(&manifest, "{\"name\":\"before\"}\n").unwrap();
            let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
            let options = IndexOptions::new(workspace.path().to_owned());
            let cancel = Arc::new(AtomicBool::new(false));
            let (prior, owner) = reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
            fs::write(&source, "function middle() {}\n").unwrap();
            let request = store.enqueue_request(&options, None).unwrap();
            let edited = Arc::new(AtomicBool::new(false));
            let edited_hook = edited.clone();
            let manifest_hook = fs::canonicalize(&manifest).unwrap();
            let source_hook = source.clone();
            let first = crate::capture::with_source_read_hook(
                move |path, point| {
                    if path == manifest_hook
                        && point == phase
                        && !edited_hook.swap(true, Ordering::AcqRel)
                    {
                        let replacement = manifest_hook.with_extension("replacement");
                        fs::write(&replacement, "{\"name\":\"after\"}\n").unwrap();
                        fs::rename(replacement, &manifest_hook).unwrap();
                        fs::write(&source_hook, "function after() {}\n").unwrap();
                    }
                },
                || drain_requests_observed_with_native(&store, &owner, |_, _| {}, |_, _, _| {}),
            )
            .unwrap();
            assert!(edited.load(Ordering::Acquire), "hook did not fire: {phase}");
            assert_eq!(first, 0);
            assert_eq!(store.status().unwrap().revision, prior);
            let pending = store.request_by_id(&request.id).unwrap().unwrap();
            assert_eq!(pending.seq, request.seq);
            assert_eq!(pending.state, "queued");
            assert!(pending.claim_incarnation.is_none() && pending.error_code.is_none());
            assert_eq!(
                drain_requests_observed_with_native(&store, &owner, |_, _| {}, |_, _, _| {})
                    .unwrap(),
                1
            );
            let done = store.request_by_id(&request.id).unwrap().unwrap();
            assert_eq!(done.seq, request.seq);
            assert_eq!(done.state, "done");
            let response = store.evidence_response().unwrap();
            assert_eq!(
                response
                    .source_at("a.js", done.revision)
                    .unwrap()
                    .unwrap()
                    .1
                    .text,
                "function after() {}\n"
            );
            response.finish(()).unwrap();
        }
    }

    #[test]
    fn claimed_request_requeues_same_fifo_row_after_capture_inventory_drift() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("a.js");
        fs::write(&source, "function before() {}\n").unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let options = IndexOptions::new(workspace.path().to_owned());
        let cancel = Arc::new(AtomicBool::new(false));
        let (prior, owner) = reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
        let request = store.enqueue_request(&options, None).unwrap();
        let edited = AtomicBool::new(false);
        let first = drain_requests_observed_with_native(
            &store,
            &owner,
            |id, phase| {
                if id == request.id && phase.phase == "scan" && !edited.swap(true, Ordering::AcqRel)
                {
                    // Capture has the old source, but its final inventory check
                    // must observe this edit and reject that snapshot.
                    fs::write(&source, "function after() {}\n").unwrap();
                }
            },
            |_, _, _| {},
        )
        .unwrap();
        assert!(edited.load(Ordering::Acquire));
        assert_eq!(first, 0);
        assert_eq!(store.status().unwrap().revision, prior);
        let pending = store.request_by_id(&request.id).unwrap().unwrap();
        assert_eq!(pending.seq, request.seq);
        assert_eq!(pending.state, "queued");
        assert!(pending.claim_incarnation.is_none() && pending.error_code.is_none());
        assert!(owner.is_leader() && owner.verify().is_ok());

        let second =
            drain_requests_observed_with_native(&store, &owner, |_, _| {}, |_, _, _| {}).unwrap();
        assert_eq!(second, 1);
        let done = store.request_by_id(&request.id).unwrap().unwrap();
        assert_eq!(done.seq, request.seq);
        assert_eq!(done.state, "done");
        let committed = done.revision.unwrap();
        assert_eq!(committed.index_generation, prior.index_generation);
        assert!(committed.index_revision > prior.index_revision);
        let response = store.evidence_response().unwrap();
        assert_eq!(
            response
                .source_at("a.js", Some(committed))
                .unwrap()
                .unwrap()
                .1
                .text,
            "function after() {}\n"
        );
        response.finish(()).unwrap();
    }

    #[test]
    fn takeover_marker_serves_prior_head_then_reconciles_before_fifo_claim() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("a.js"), "function old() {}\n").unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let options = IndexOptions::new(workspace.path().to_owned());
        let cancel = Arc::new(AtomicBool::new(false));
        let (old_pin, owner) = reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
        let identity = crate::store::topology::WorkspaceIdentity::discover(
            Some(workspace.path()),
            workspace.path(),
        )
        .unwrap();
        let roots = crate::store::topology::TopologyRoots::isolated_for_tests(
            state.path().join("cache"),
            state.path().join("data"),
        );
        let lock_path = roots.leader_lock(&identity);
        let predecessor_marker = fs::read(&lock_path).unwrap();
        drop(owner);
        fs::write(workspace.path().join("a.js"), "function new() {}\n").unwrap();
        let first_capture = AtomicBool::new(false);
        let first_publish = AtomicBool::new(false);
        let takeover_pin = std::sync::Mutex::new(None);
        let (explicit_pin, owner) = enqueue_and_wait_observed(&store, &options, &cancel, |phase| {
            if phase.phase == "timing:capture" && !first_capture.swap(true, Ordering::AcqRel) {
                assert_eq!(store.current_request().unwrap().unwrap().state, "queued");
                use std::os::fd::AsRawFd;
                let successor_marker = fs::read(&lock_path).unwrap();
                assert_ne!(predecessor_marker, successor_marker);
                assert_eq!(successor_marker.len(), 36);
                uuid::Uuid::parse_str(std::str::from_utf8(&successor_marker).unwrap()).unwrap();
                let probe = fs::OpenOptions::new().read(true).open(&lock_path).unwrap();
                assert_ne!(
                    unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                    0
                );
                let follower = Store::open_for_tests(state.path(), workspace.path()).unwrap();
                // The selected old pin remains readable under the new synced
                // marker, but cannot grant the still-queued request a claim.
                let response = follower.evidence_response().unwrap();
                assert_eq!(response.status().unwrap().revision, old_pin);
                assert_eq!(
                    response
                        .source_at("a.js", Some(old_pin))
                        .unwrap()
                        .unwrap()
                        .1
                        .text,
                    "function old() {}\n"
                );
                response.finish(()).unwrap();
                assert_eq!(store.current_request().unwrap().unwrap().state, "queued");
            }
            if phase.phase == "timing:publish" && !first_publish.swap(true, Ordering::AcqRel) {
                assert_eq!(
                    store.current_request().unwrap().unwrap().state,
                    "queued",
                    "takeover reconciliation cannot ACK the explicit row"
                );
                let selected = store.status().unwrap().revision;
                assert!(selected.index_revision > old_pin.index_revision);
                let read = store.evidence_response().unwrap();
                assert_eq!(
                    read.source_at("a.js", Some(selected))
                        .unwrap()
                        .unwrap()
                        .1
                        .text,
                    "function new() {}\n"
                );
                read.finish(()).unwrap();
                *takeover_pin.lock().unwrap() = Some(selected);
            }
        })
        .unwrap();
        assert!(owner.is_leader());
        assert!(first_capture.load(Ordering::Acquire) && first_publish.load(Ordering::Acquire));
        assert!(
            explicit_pin.index_revision
                > takeover_pin.into_inner().unwrap().unwrap().index_revision
        );
        assert_eq!(
            store.current_request().unwrap().unwrap().revision,
            Some(explicit_pin)
        );
    }

    #[test]
    fn cli_final_inventory_keeps_foreign_fifo_head_options_and_both_ack_pins() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let initiating = IndexOptions::new(workspace.path().to_owned());
        let mut foreign = initiating.clone();
        foreign.manifest_path = Some(workspace.path().join("absent-manifest.json"));
        let inserted = std::sync::atomic::AtomicBool::new(false);
        let foreign_id = std::sync::Mutex::new(None);
        let initiating_id = std::sync::Mutex::new(None);
        let cancel = Arc::new(AtomicBool::new(false));
        let (initiating_pin, leader) =
            enqueue_and_wait_observed(&store, &initiating, &cancel, |phase| {
                if phase.phase == "timing:capture" && !inserted.swap(true, Ordering::AcqRel) {
                    *initiating_id.lock().unwrap() =
                        Some(store.current_request().unwrap().unwrap().id);
                    *foreign_id.lock().unwrap() =
                        Some(store.enqueue_request(&foreign, None).unwrap().id);
                }
            })
            .unwrap();
        assert!(leader.is_leader());
        let foreign_id = foreign_id.into_inner().unwrap().unwrap();
        let foreign_row = store.request_by_id(&foreign_id).unwrap().unwrap();
        let initiating_id = initiating_id.into_inner().unwrap().unwrap();
        let initiating_row = store.request_by_id(&initiating_id).unwrap().unwrap();
        assert_eq!(initiating_row.state, "done");
        assert_eq!(initiating_row.revision, Some(initiating_pin));
        assert_eq!(foreign_row.state, "done");
        assert_eq!(store.current_request().unwrap().unwrap().id, foreign_id);
        let foreign_pin = foreign_row.revision.unwrap();
        assert!(foreign_pin.index_revision > initiating_pin.index_revision);
        assert_eq!(
            store.status().unwrap().revision,
            foreign_pin,
            "final CLI inventory cannot revert the selected foreign options"
        );
        assert_eq!(
            store
                .recorded_index_options()
                .unwrap()
                .unwrap()
                .manifest_path,
            foreign.manifest_path
        );
        let response = store.evidence_response().unwrap();
        response.validate_pin(initiating_pin).unwrap();
        response.validate_pin(foreign_pin).unwrap();
        response.finish(()).unwrap();
    }

    #[test]
    fn takeover_watcher_switches_input_options_with_new_full_wake() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let head_options = IndexOptions::new(workspace.path().to_owned());
        let cancel = Arc::new(AtomicBool::new(false));
        let owner = establish_serving_session(&store, Some(&head_options), &cancel).unwrap();
        let mut work = LeaderWork::new(&store, &owner, &head_options).unwrap();
        let mut daemon_options = head_options.clone();
        daemon_options.max_file_bytes = 1024;
        work.reconcile_due(&store, &owner, &daemon_options, &cancel, false)
            .unwrap();
        assert_eq!(work.options.max_file_bytes, 1024);
        assert!(
            work.last_accounted_generation.is_some(),
            "new option watcher must finish its first full inventory"
        );
        assert_eq!(
            store
                .recorded_index_options()
                .unwrap()
                .unwrap()
                .max_file_bytes,
            1024
        );
    }

    #[test]
    fn degraded_watcher_reconciles_once_then_waits_for_periodic_scan() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let options = IndexOptions::new(workspace.path().to_owned());
        let cancel = Arc::new(AtomicBool::new(false));
        let owner = establish_serving_session(&store, Some(&options), &cancel).unwrap();
        let mut work = LeaderWork::new(&store, &owner, &options).unwrap();
        // A registration failure is sticky. It still needs one verified full
        // inventory, but an accounted idle generation must not veto maintenance.
        let absent_watch_root = workspace.path().join("absent_watch_root");
        assert!(!absent_watch_root.exists());
        work.watch = crate::watch::WatchSignals::new(absent_watch_root, None, None);
        assert!(work.watch.degraded());
        assert!(work.accepted_watch_intent(&options));
        assert!(
            work.reconcile_due(&store, &owner, &options, &cancel, false)
                .unwrap()
        );
        let pin = store.status().unwrap().revision;
        assert!(work.last_accounted_generation.is_some());
        assert!(
            !work.accepted_watch_intent(&options),
            "accounted degraded generation must admit maintenance before periodic scan"
        );
        assert!(
            !work
                .reconcile_due(&store, &owner, &options, &cancel, false)
                .unwrap()
        );
        assert_eq!(store.status().unwrap().revision, pin);

        // New accepted ingress preempts before channel drain or debounce.
        work.watch
            .submit_event(Err(notify::Error::generic("watch failed again")));
        assert!(work.accepted_watch_intent(&options));
        assert!(
            work.reconcile_due(&store, &owner, &options, &cancel, false)
                .unwrap()
        );
        assert!(!work.accepted_watch_intent(&options));
        work.watch.require_full();
        assert!(work.accepted_watch_intent(&options));
        assert!(
            work.reconcile_due(&store, &owner, &options, &cancel, false)
                .unwrap()
        );
        assert!(!work.accepted_watch_intent(&options));

        // Due periodic inventory is real work, but retry_after prevents its
        // maintenance veto until the retry deadline has elapsed.
        work.last_inventory -= std::time::Duration::from_secs(60);
        work.retry_after = Some(std::time::Instant::now() + std::time::Duration::from_secs(2));
        assert!(!work.accepted_watch_intent(&options));
        assert!(
            !work
                .reconcile_due(&store, &owner, &options, &cancel, false)
                .unwrap()
        );
        work.retry_after = Some(std::time::Instant::now() - std::time::Duration::from_millis(1));
        assert!(work.accepted_watch_intent(&options));
        assert!(
            work.reconcile_due(&store, &owner, &options, &cancel, false)
                .unwrap()
        );
        assert!(!work.accepted_watch_intent(&options));
    }

    /// Per-source (opens, complete reads, hashes), keyed by path.
    type Counts = BTreeMap<String, (usize, usize, usize)>;

    fn counts(capture: &Capture) -> (Vec<String>, Counts) {
        let files = capture.files.iter().map(|f| f.path.clone()).collect();
        let ops = capture
            .source_operations
            .iter()
            .map(
                |(
                    path,
                    SourceOperations {
                        opens,
                        complete_reads,
                        hashes,
                    },
                )| { (path.clone(), (*opens, *complete_reads, *hashes)) },
            )
            .collect();
        (files, ops)
    }

    #[test]
    fn publish_commit_busy_once_must_keep_accepted_fifo_head_and_finish_same_seq() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let mut h = IndexOptions::new(workspace.path().canonicalize().unwrap());
        h.max_file_bytes = 128;
        let mut a_options = h.clone();
        a_options.max_file_bytes = 32;
        let mut b_options = h.clone();
        b_options.max_file_bytes = 512;
        let assert_selected_options = |expected: &IndexOptions| {
            let observed = store.recorded_index_options().unwrap().unwrap();
            assert_eq!(observed.workspace_root, expected.workspace_root);
            assert_eq!(
                crate::indexer::ReconcileOptions::from(&observed),
                crate::indexer::ReconcileOptions::from(expected),
            );
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let (_, old_owner) = reconcile_workspace(&store, &h, &cancel, |_| {}).unwrap();
        drop(old_owner);
        let browser = store.enqueue_request(&a_options, None).unwrap();
        let cli = store.enqueue_request(&b_options, None).unwrap();
        assert!(browser.seq < cli.seq && browser.id != cli.id);
        let leader = establish_serving_session(&store, None, &cancel).unwrap();
        let base = store.status().unwrap().revision;
        assert_selected_options(&h);
        assert_eq!(
            store.request_by_id(&browser.id).unwrap().unwrap().state,
            "queued"
        );
        assert_eq!(
            store.request_by_id(&cli.id).unwrap().unwrap().state,
            "queued"
        );
        let encoded_a =
            serde_json::to_string(&crate::indexer::ReconcileOptions::from(&a_options)).unwrap();
        let encoded_b =
            serde_json::to_string(&crate::indexer::ReconcileOptions::from(&b_options)).unwrap();
        assert_eq!(browser.options_json, encoded_a);
        assert_eq!(cli.options_json, encoded_b);
        let injection = AtomicBool::new(false);
        assert_eq!(
            drain_requests_observed(&store, &leader, |id, phase| {
                if id == cli.id
                    && phase.phase == "timing:capture"
                    && !injection.swap(true, Ordering::AcqRel)
                {
                    store.fail_next_live_publish_commit_busy();
                }
            })
            .unwrap(),
            1
        );
        assert!(
            injection.load(Ordering::Acquire),
            "inject once at Q2's real commit boundary"
        );
        let a = store.request_by_id(&browser.id).unwrap().unwrap();
        let b = store.request_by_id(&cli.id).unwrap().unwrap();
        assert_eq!(a.state, "done");
        assert_eq!(
            (a.id.as_str(), a.seq, a.options_json.as_str()),
            (browser.id.as_str(), browser.seq, encoded_a.as_str())
        );
        assert_eq!(b.id, cli.id);
        assert_eq!(b.options_json, encoded_b);
        assert_selected_options(&a_options);
        assert_eq!(
            b.seq, cli.seq,
            "accepted CLI ACK cannot be deleted/re-admitted under a new seq"
        );
        assert_eq!(
            b.state, "queued",
            "pre-commit storage_busy must return same-seq accepted ACK to queued, not index_failed"
        );
        assert!(b.finished_at.is_none() && b.error_code.is_none() && b.revision.is_none());
        assert_eq!(
            store.index_baseline().unwrap().index_revision,
            base.index_revision + 1,
            "failed commit must leave only the prior Q1 publication"
        );
        let q1_pin = a.revision.unwrap();
        // Selected status is fenced until Q2 reconciliation succeeds.
        // Check Q1's durable DONE/pin and selected A options above; defer
        // the historical source read until after Q2 publishes and recovers.
        fs::write(workspace.path().join("a.js"), "function b() {}\n").unwrap();
        assert_eq!(
            drain_requests(&store, &leader).unwrap(),
            1,
            "same verified leader may reclaim requeued Q2 on next bounded tick"
        );
        let b = store.request_by_id(&cli.id).unwrap().unwrap();
        assert_eq!(
            b.state, "done",
            "retry must finish the original accepted same-seq Q2"
        );
        assert!(b.finished_at.is_some() && b.error_code.is_none() && b.revision.is_some());
        assert_eq!(
            (
                b.id.as_str(),
                b.seq,
                b.options_json.as_str(),
                b.state.as_str()
            ),
            (cli.id.as_str(), cli.seq, encoded_b.as_str(), "done")
        );
        let final_q1 = store.request_by_id(&browser.id).unwrap().unwrap();
        assert_eq!(
            (
                final_q1.id.as_str(),
                final_q1.seq,
                final_q1.options_json.as_str(),
                final_q1.state.as_str(),
                final_q1.revision,
            ),
            (
                browser.id.as_str(),
                browser.seq,
                encoded_a.as_str(),
                "done",
                Some(q1_pin)
            ),
            "Q2 retry cannot move Q1 out of its exact terminal DONE/pin",
        );
        assert!(final_q1.finished_at.is_some() && final_q1.error_code.is_none());
        assert_selected_options(&b_options);
        assert_eq!(a.revision.unwrap().index_generation, base.index_generation);
        assert_eq!(a.revision.unwrap().index_revision, base.index_revision + 1);
        assert_eq!(
            b.revision.unwrap().index_revision,
            a.revision.unwrap().index_revision + 1,
            "failed commit cannot leave a partially published revision"
        );
        assert_eq!(store.status().unwrap().revision, b.revision.unwrap());
        let read = store.evidence_response().unwrap();
        read.validate_pin(base).unwrap();
        read.validate_pin(q1_pin).unwrap();
        read.validate_pin(b.revision.unwrap()).unwrap();
        assert_eq!(
            read.source_at("a.js", Some(q1_pin))
                .unwrap()
                .unwrap()
                .1
                .text,
            "function a() {}\n"
        );
        assert_eq!(
            read.source_at("a.js", Some(b.revision.unwrap()))
                .unwrap()
                .unwrap()
                .1
                .text,
            "function b() {}\n"
        );
        read.finish(()).unwrap();
        assert!(leader.verify().is_ok());
    }

    #[test]
    fn cached_completion_retry_only_accepts_real_lock_contention_not_claim_invariants() {
        assert!(retryable_cli_completion_error(&anyhow::anyhow!(
            "storage_busy: SQLite lock contention"
        )));
        assert!(!retryable_cli_completion_error(&anyhow::anyhow!(
            "storage_busy: cached claim changed"
        )));
        assert!(!retryable_cli_completion_error(&anyhow::anyhow!(
            "storage_busy: unresolved FIFO completion"
        )));
    }

    #[test]
    fn cli_ambiguous_busy_reports_durable_unfinished_ack_for_next_verified_leader() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let options = IndexOptions::new(workspace.path().to_owned());
        let cancel = Arc::new(AtomicBool::new(false));
        let (_, old) = reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
        drop(old);
        let mut options = options;
        options.max_file_bytes = 1024;
        let earlier = store.enqueue_request(&options, None).unwrap();
        let captures = std::sync::atomic::AtomicUsize::new(0);
        let error = match enqueue_and_wait_observed(&store, &options, &cancel, |phase| {
            // First capture is leader takeover; inject into the accepted
            // FIFO head Q1 while this CLI's own Q2 is durably queued behind it.
            if phase.phase == "timing:capture" && captures.fetch_add(1, Ordering::AcqRel) == 1 {
                store.fail_next_live_publish_post_commit_busy();
            }
        }) {
            Ok(_) => panic!("ambiguous COMMIT must stop this CLI waiter"),
            Err(error) => error,
        };
        let text = format!("{error:#}");
        assert!(
            text.contains("next verified leader") && text.contains("remains durable"),
            "{text}"
        );
        let head = store.request_by_id(&earlier.id).unwrap().unwrap();
        let waiting = store.current_request().unwrap().unwrap();
        assert!(waiting.seq > earlier.seq);
        assert!(
            (head.state == "running" && waiting.state == "queued")
                || (head.state == "done" && waiting.state == "running"),
            "a post-COMMIT ambiguity must leave exactly its own FIFO row running and all later ACKs queued"
        );
        assert!(head.error_code.is_none() && waiting.error_code.is_none());
        assert!(waiting.finished_at.is_none());
        let successor = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let (_, owner) = reconcile_workspace(&successor, &options, &cancel, |_| {}).unwrap();
        let processed = drain_requests(&successor, &owner).unwrap();
        assert!((1..=2).contains(&processed));
        assert_eq!(
            successor.request_by_id(&head.id).unwrap().unwrap().state,
            "done"
        );
        assert_eq!(
            successor.request_by_id(&waiting.id).unwrap().unwrap().state,
            "done"
        );
        drop(owner);
    }

    #[test]
    fn ambiguous_post_commit_busy_must_leave_running_for_verified_reconciliation() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let options = IndexOptions::new(workspace.path().to_owned());
        let cancel = Arc::new(AtomicBool::new(false));
        let (_, old) = reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
        drop(old);
        let mut options = options;
        options.max_file_bytes = 1024;
        let ack = store.enqueue_request(&options, None).unwrap();
        let (before, leader) = reconcile_workspace(
            &store,
            &IndexOptions::new(workspace.path().to_owned()),
            &cancel,
            |_| {},
        )
        .unwrap();
        store.fail_next_live_publish_post_commit_busy();
        let error = drain_requests(&store, &leader).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("publication changed across failed commit"),
            "{error:#}"
        );
        let row = store.request_by_id(&ack.id).unwrap().unwrap();
        assert_eq!(
            row.state, "running",
            "unknown result must not be requeued or failed"
        );
        assert_eq!(row.seq, ack.seq);
        assert!(row.error_code.is_none() && row.revision.is_none() && row.finished_at.is_none());
        let committed = store.index_baseline().unwrap();
        assert_eq!(committed.index_generation, before.index_generation);
        assert_eq!(
            committed.index_revision,
            before.index_revision + 1,
            "test seam committed selected pin before returning BUSY"
        );
        drop(leader);
        let successor = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let (_, new_owner) = reconcile_workspace(&successor, &options, &cancel, |_| {}).unwrap();
        assert_eq!(drain_requests(&successor, &new_owner).unwrap(), 1);
        let done = successor.request_by_id(&ack.id).unwrap().unwrap();
        assert_eq!(done.state, "done");
        assert_eq!(done.seq, ack.seq);
        assert!(done.revision.unwrap().index_revision > committed.index_revision);
    }

    #[test]
    fn cli_ctrl_c_must_not_fail_another_clients_running_fifo_request() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let options = IndexOptions::new(workspace.path().to_owned());
        let cancel = Arc::new(AtomicBool::new(false));
        let (_, old_owner) = reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
        drop(old_owner);
        let ahead = store.enqueue_request(&options, None).unwrap();
        let browser = store.enqueue_request(&options, None).unwrap();
        let captures = std::sync::atomic::AtomicUsize::new(0);
        let error = enqueue_and_wait_observed(&store, &options, &cancel, |phase| {
            if phase.phase == "timing:capture" && captures.fetch_add(1, Ordering::AcqRel) > 1 {
                cancel.store(true, Ordering::Release);
            }
        })
        .unwrap_err();
        assert_eq!(
            captures.load(Ordering::Acquire),
            3,
            "takeover reconciled first, A then published, CLI Ctrl-C happened during B capture"
        );
        assert!(error.to_string().contains("interrupted"), "{error:#}");
        let a = store.request_by_id(&ahead.id).unwrap().unwrap();
        let b = store.request_by_id(&browser.id).unwrap().unwrap();
        let cli = store.current_request().unwrap().unwrap();
        assert_eq!(a.state, "done");
        assert_eq!(
            b.state, "running",
            "other client's ACK must survive CLI Ctrl-C"
        );
        assert!(b.error_code.is_none() && b.revision.is_none());
        assert_eq!(cli.state, "queued", "CLI's own later ACK also survives");
        assert!(a.seq < b.seq && b.seq < cli.seq);
        let reopened = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let (_, new_leader) = reconcile_workspace(
            &reopened,
            &options,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        assert_eq!(drain_requests(&reopened, &new_leader).unwrap(), 2);
        let b_done = reopened.request_by_id(&browser.id).unwrap().unwrap();
        let cli_done = reopened.request_by_id(&cli.id).unwrap().unwrap();
        assert_eq!(
            (b_done.state.as_str(), cli_done.state.as_str()),
            ("done", "done")
        );
        assert!(
            b_done.revision.unwrap().index_revision < cli_done.revision.unwrap().index_revision
        );
    }

    #[test]
    fn cli_ctrl_c_during_request_capture_leaves_own_request_running() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let options = IndexOptions::new(workspace.path().to_owned());
        let cancel = Arc::new(AtomicBool::new(false));
        let (_, old_owner) = reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
        drop(old_owner);
        let ahead = store.enqueue_request(&options, None).unwrap();
        let captures = std::sync::atomic::AtomicUsize::new(0);
        let error = enqueue_and_wait_observed(&store, &options, &cancel, |phase| {
            if phase.phase == "timing:capture" && captures.fetch_add(1, Ordering::AcqRel) > 1 {
                cancel.store(true, Ordering::Release);
            }
        })
        .unwrap_err();
        assert_eq!(
            captures.load(Ordering::Acquire),
            3,
            "takeover reconciled first; earlier FIFO row published before CLI capture"
        );
        assert!(error.to_string().contains("interrupted"), "{error:#}");
        assert_eq!(
            store.request_by_id(&ahead.id).unwrap().unwrap().state,
            "done"
        );
        let row = store.current_request().unwrap().unwrap();
        assert_eq!(
            row.state, "running",
            "Ctrl-C stops waiting but preserves the accepted request"
        );
        assert!(row.error_code.is_none() && row.revision.is_none());
    }

    #[test]
    fn recovery_pin_must_not_complete_a_request_admitted_after_its_capture() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let options = IndexOptions::new(workspace.path().to_owned());
        let cancel = Arc::new(AtomicBool::new(false));
        let (old_pin, session) = reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
        // The owner published its source capture first; only then did a client
        // commit this new ACK. It cannot claim that earlier publication.
        let later = store.enqueue_request(&options, None).unwrap();
        assert_eq!(
            store.request_by_id(&later.id).unwrap().unwrap().state,
            "queued"
        );
        assert_eq!(drain_requests(&store, &session).unwrap(), 1);
        let completed = store.request_by_id(&later.id).unwrap().unwrap();
        assert_eq!(completed.state, "done");
        assert!(completed.revision.unwrap().index_revision > old_pin.index_revision);
    }

    #[test]
    fn explicit_reconcile_replaces_corruption_without_upgrading_a_reader() {
        use crate::store::topology::{TopologyRoots, WorkspaceIdentity};
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        fs::write(work.path().join("a.js"), "function a() {}\n").unwrap();
        let roots = TopologyRoots::isolated_for_tests(
            state.path().join("cache"),
            state.path().join("data"),
        );
        let identity = WorkspaceIdentity::discover(Some(work.path()), work.path()).unwrap();
        let path = roots.index_db(&identity);
        let store = Store::open_for_tests(state.path(), work.path()).unwrap();
        let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
        let options = IndexOptions::new(work.path().to_owned());
        let (old, first_owner) = reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
        assert_eq!(store.status().unwrap().revision, old);
        drop(first_owner);
        drop(store);
        fs::write(&path, b"short").unwrap();
        let corrupt = fs::read(&path).unwrap();
        let pending = Store::open_for_tests(state.path(), work.path()).unwrap();
        assert!(pending.is_recreate_pending());
        let queued = pending.enqueue_request(&options, None).unwrap();
        let queue_before = fs::read(pending.request_db_path()).unwrap();
        let shared = roots.index_use_existing(&identity).unwrap();
        let busy = reconcile_workspace(&pending, &options, &cancel, |_| {}).unwrap_err();
        assert!(busy.to_string().contains("storage_busy"), "{busy:#}");
        assert_eq!(fs::read(&path).unwrap(), corrupt);
        assert_eq!(fs::read(pending.request_db_path()).unwrap(), queue_before);
        assert_eq!(
            pending.request_by_id(&queued.id).unwrap().unwrap().state,
            "queued"
        );
        assert!(pending.is_recreate_pending());
        drop(shared);
        let mut configured = options.clone();
        configured.max_file_bytes = 2_097_152;
        let (new, owner) = reconcile_workspace(&pending, &configured, &cancel, |_| {}).unwrap();
        assert_ne!(new.index_generation, old.index_generation);
        assert_eq!(new.index_revision, 1);
        assert_eq!(pending.status().unwrap().revision, new);
        assert_eq!(
            pending
                .recorded_index_options()
                .unwrap()
                .unwrap()
                .max_file_bytes,
            2_097_152
        );
        assert!(
            roots
                .leader(&identity)
                .unwrap_err()
                .to_string()
                .contains("storage_busy")
        );
        drop(owner);
        roots.leader(&identity).unwrap().verify().unwrap();
    }

    #[test]
    fn pending_read_refuses_unknown_options_and_daemon_config_does_not_fallback() {
        use crate::store::topology::{TopologyRoots, WorkspaceIdentity};
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        fs::write(work.path().join("a.js"), "function a() {}\n").unwrap();
        let roots = TopologyRoots::isolated_for_tests(
            state.path().join("cache"),
            state.path().join("data"),
        );
        let identity = WorkspaceIdentity::discover(Some(work.path()), work.path()).unwrap();
        let path = roots.index_db(&identity);
        let old = Store::open_for_tests(state.path(), work.path()).unwrap();
        drop(old);
        fs::write(&path, b"not sqlite format header").unwrap();
        let original = fs::read(&path).unwrap();
        let pending = Store::open_for_tests(state.path(), work.path()).unwrap();
        let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
        let missing = establish_serving_session(&pending, None, &cancel).unwrap_err();
        assert!(
            missing.to_string().contains("explicit trellis index"),
            "{missing:#}"
        );
        assert_eq!(fs::read(&path).unwrap(), original);
        let mut configured = IndexOptions::new(work.path().to_owned());
        configured.max_file_bytes = 2_097_152;
        let shared = roots.index_use_existing(&identity).unwrap();
        let busy = establish_serving_session(&pending, Some(&configured), &cancel).unwrap_err();
        assert!(busy.to_string().contains("storage_busy"), "{busy:#}");
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(pending.is_recreate_pending());
        drop(shared);
        let owner = establish_serving_session(&pending, Some(&configured), &cancel).unwrap();
        assert!(owner.is_leader());
        assert_eq!(pending.status().unwrap().revision.index_revision, 1);
        assert_eq!(
            pending
                .recorded_index_options()
                .unwrap()
                .unwrap()
                .max_file_bytes,
            2_097_152
        );
        assert!(
            roots
                .leader(&identity)
                .unwrap_err()
                .to_string()
                .contains("storage_busy")
        );
    }

    #[test]
    fn coordinator_capture_opens_reads_and_hashes_each_source_once() {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        fs::write(
            work.path().join("main.js"),
            "function a() { b(); } function b() {}\n",
        )
        .unwrap();
        fs::write(
            work.path().join("Lib.java"),
            "class Lib { void run() {} }\n",
        )
        .unwrap();
        let store = Store::open_for_tests(state.path(), work.path()).unwrap();
        let options = IndexOptions::new(work.path().to_owned());
        let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
        let expected = |files: &[String]| {
            files
                .iter()
                .map(|path| (path.clone(), (1, 1, 1)))
                .collect::<BTreeMap<_, _>>()
        };

        let mut published = None;
        let first_job = IndexJobCoordinator::prepare(&store, None).unwrap();
        let first_session = first_job.session();
        let first = first_job
            .run_observed(
                &options,
                &cancel,
                |_| {},
                |capture| published = Some(counts(capture)),
            )
            .unwrap();
        let (files, ops) = published.expect("capture observed");
        assert_eq!(files.len(), 2);
        assert_eq!(ops, expected(&files));
        assert_eq!(store.status().unwrap().revision, first);

        // A cancel after the single capture leaves the prior pair and the same counters.
        fs::write(work.path().join("main.js"), "function late() {}\n").unwrap();
        let mut cancelled = None;
        drop(first_session);
        let second_job = IndexJobCoordinator::prepare(&store, Some(first)).unwrap();
        let _second_session = second_job.session();
        let error = second_job
            .run_observed(
                &options,
                &cancel,
                |_| {},
                |capture| {
                    cancelled = Some(counts(capture));
                    cancel.store(true, Ordering::Release);
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"), "{error:#}");
        let (files, ops) = cancelled.expect("capture observed before cancel");
        assert_eq!(ops, expected(&files));
        assert_eq!(store.index_baseline().unwrap(), first);
        // Cancellation before COMMIT must not close the still-valid prior head.
        assert_eq!(store.status().unwrap().revision, first);
        assert_eq!(
            store.source_at("main.js", Some(first)).unwrap().unwrap().0,
            first
        );
    }
    #[test]
    fn reused_leader_session_is_bound_to_its_store_before_capture() {
        let state = tempfile::tempdir().unwrap();
        let first_root = tempfile::tempdir().unwrap();
        let second_root = tempfile::tempdir().unwrap();
        std::fs::write(
            first_root.path().join("a.js"),
            "function a() {}
",
        )
        .unwrap();
        std::fs::write(
            second_root.path().join("b.js"),
            "function b() {}
",
        )
        .unwrap();
        let first = Store::open_for_tests(&state.path().join("first"), first_root.path()).unwrap();
        let second =
            Store::open_for_tests(&state.path().join("second"), second_root.path()).unwrap();
        let session = first.leader_session().unwrap();
        let error = IndexJobCoordinator::prepare_with_session(&second, None, session)
            .err()
            .unwrap();
        assert!(
            error.to_string().contains("another workspace")
                || error.to_string().contains("wrong leader guard"),
            "{error:#}"
        );
    }

    #[test]
    fn reused_leader_is_reverified_after_capture_before_publish() {
        use crate::store::topology::{TopologyRoots, WorkspaceIdentity};
        use std::os::unix::fs::PermissionsExt;
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        std::fs::write(
            work.path().join("a.js"),
            "function a() {}
",
        )
        .unwrap();
        let roots = TopologyRoots::isolated_for_tests(
            state.path().join("cache"),
            state.path().join("data"),
        );
        let identity = WorkspaceIdentity::discover(Some(work.path()), work.path()).unwrap();
        let leader_path = roots.leader_lock(&identity);
        let store = Store::open(roots, identity).unwrap();
        let job = IndexJobCoordinator::prepare(&store, None).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let error = job
            .run_observed(
                &IndexOptions::new(work.path().to_owned()),
                &cancel,
                |_| {},
                |_| {
                    std::fs::remove_file(&leader_path).unwrap();
                    std::fs::write(&leader_path, uuid::Uuid::new_v4().to_string()).unwrap();
                    std::fs::set_permissions(&leader_path, std::fs::Permissions::from_mode(0o600))
                        .unwrap();
                },
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("leader") || error.to_string().contains("managed file"),
            "{error:#}"
        );
        assert_eq!(store.index_baseline().unwrap().index_revision, 0);
    }
}

#[cfg(test)]
mod queue_completion_retry_tests {
    use super::*;
    use std::fs;

    fn fixture() -> (
        tempfile::TempDir,
        Store,
        IndexOptions,
        IndexPin,
        Arc<LeaderSession>,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.js"), "function a() {}\n").unwrap();
        let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
        let options = IndexOptions::new(root);
        let (baseline, session) = reconcile_workspace(
            &store,
            &options,
            &Arc::new(std::sync::atomic::AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        (tmp, store, options, baseline, session)
    }

    #[test]
    fn failed_terminal_write_retries_exact_published_head_before_next_fifo_claim() {
        let (_tmp, store, options, baseline, session) = fixture();
        let first = store.enqueue_request(&options, None).unwrap();
        let second = store.enqueue_request(&options, None).unwrap();
        assert!(first.seq < second.seq);
        store.inject_queue_finish_failure(false);
        let error = drain_requests(&store, &session).unwrap_err();
        assert!(error.to_string().contains("storage_busy"));
        let published = store.status().unwrap().revision;
        assert_eq!(published.index_revision, baseline.index_revision + 1);
        assert_eq!(
            store.request_by_id(&first.id).unwrap().unwrap().state,
            "running"
        );
        assert_eq!(
            store.request_by_id(&second.id).unwrap().unwrap().state,
            "queued"
        );
        assert_eq!(drain_requests(&store, &session).unwrap(), 2);
        let q1 = store.request_by_id(&first.id).unwrap().unwrap();
        let q2 = store.request_by_id(&second.id).unwrap().unwrap();
        assert_eq!((q1.state.as_str(), q2.state.as_str()), ("done", "done"));
        assert_eq!(q1.revision.unwrap(), published);
        assert_eq!(
            q2.revision.unwrap().index_revision,
            published.index_revision + 1
        );
        assert_eq!(store.status().unwrap().revision, q2.revision.unwrap());
    }

    #[test]
    fn ambiguous_committed_terminal_is_verified_without_republishing_head() {
        let (_tmp, store, options, baseline, session) = fixture();
        let first = store.enqueue_request(&options, None).unwrap();
        let second = store.enqueue_request(&options, None).unwrap();
        store.inject_queue_finish_failure(true);
        assert_eq!(drain_requests(&store, &session).unwrap(), 2);
        let q1 = store.request_by_id(&first.id).unwrap().unwrap();
        let q2 = store.request_by_id(&second.id).unwrap().unwrap();
        assert_eq!((q1.state.as_str(), q2.state.as_str()), ("done", "done"));
        assert_eq!(
            q1.revision.unwrap().index_revision,
            baseline.index_revision + 1
        );
        assert_eq!(
            q2.revision.unwrap().index_revision,
            baseline.index_revision + 2
        );
    }
    #[test]
    fn cli_takeover_fast_path_retries_cached_first_completion_before_next_head() {
        let (_tmp, store, options, baseline, owner) = fixture();
        drop(owner);
        let q1 = store.enqueue_request(&options, None).unwrap();
        let q2 = store.enqueue_request(&options, None).unwrap();
        store.inject_queue_finish_failure(false);
        // The one CLI caller accepts Q3 and must also drain Q1/Q2. There is no
        // daemon, restart, extra waiter or manual second drain.
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (returned, held) = enqueue_and_wait(&store, &options, &cancel).unwrap();
        assert!(held.is_leader());
        let first = store.request_by_id(&q1.id).unwrap().unwrap();
        let second = store.request_by_id(&q2.id).unwrap().unwrap();
        assert_eq!(
            (first.state.as_str(), second.state.as_str()),
            ("done", "done")
        );
        assert!(q1.seq < q2.seq);
        assert_eq!(
            first.revision.unwrap().index_revision,
            baseline.index_revision + 2
        );
        assert_eq!(
            second.revision.unwrap().index_revision,
            baseline.index_revision + 3
        );
        assert_eq!(returned.index_revision, baseline.index_revision + 4);
        assert_eq!(store.status().unwrap().revision, returned);
    }

    #[test]
    fn cli_ordinary_drain_retries_cached_completion_without_republishing() {
        let (_tmp, store, options, baseline, owner) = fixture();
        drop(owner);
        // Pin Q1 to the pair the mandatory takeover capture will produce, so
        // the unpinned-head fast path is skipped and ordinary drain executes Q1.
        let takeover_pin = IndexPin {
            index_generation: baseline.index_generation,
            index_revision: baseline.index_revision + 1,
        };
        let q1 = store.enqueue_request(&options, Some(takeover_pin)).unwrap();
        let q2 = store.enqueue_request(&options, None).unwrap();
        store.inject_queue_finish_failure(false);
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (returned, held) = enqueue_and_wait(&store, &options, &cancel).unwrap();
        assert!(held.is_leader());
        let first = store.request_by_id(&q1.id).unwrap().unwrap();
        let second = store.request_by_id(&q2.id).unwrap().unwrap();
        assert_eq!(
            (first.state.as_str(), second.state.as_str()),
            ("done", "done")
        );
        assert!(q1.seq < q2.seq);
        assert_eq!(
            first.revision.unwrap().index_revision,
            baseline.index_revision + 2
        );
        assert_eq!(
            second.revision.unwrap().index_revision,
            baseline.index_revision + 3
        );
        assert_eq!(returned.index_revision, baseline.index_revision + 4);
        assert_eq!(store.status().unwrap().revision, returned);
    }
    #[test]
    fn cli_cached_claim_mismatch_fails_closed_instead_of_retrying_forever() {
        let (_tmp, store, options, baseline, session) = fixture();
        let q1 = store.enqueue_request(&options, None).unwrap();
        let q2 = store.enqueue_request(&options, None).unwrap();
        store.inject_queue_finish_failure(false);
        let initial = drain_requests(&store, &session).unwrap_err();
        assert!(
            initial
                .to_string()
                .contains("injected terminal write failure")
        );
        let published = store.status().unwrap().revision;
        assert_eq!(published.index_revision, baseline.index_revision + 1);
        let db = rusqlite::Connection::open(store.request_db_path()).unwrap();
        db.execute(
            "UPDATE requests SET claim_incarnation='changed-incarnation' WHERE id=?1",
            [&q1.id],
        )
        .unwrap();
        drop(db);
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let error = retry_cli_recorded_completion(&store, &session, &cancel, initial).unwrap_err();
        assert!(
            error.to_string().contains("cached claim changed"),
            "{error:#}"
        );
        assert_eq!(
            store.request_by_id(&q1.id).unwrap().unwrap().state,
            "running"
        );
        assert_eq!(
            store.request_by_id(&q2.id).unwrap().unwrap().state,
            "queued"
        );
        assert_eq!(store.status().unwrap().revision, published);
    }
}

#[cfg(test)]
mod marker_tests {
    use super::*;
    #[test]
    fn bounded_marker_channel_reports_losses_without_blocking_writer() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let markers = DiagnosticMarkers {
            sender,
            lost: std::sync::atomic::AtomicU64::new(0),
            origin: std::time::Instant::now(),
        };
        markers.emit("incarnation", "start", "kind=retention");
        markers.emit("incarnation", "end", "outcome=deferred");
        assert_eq!(markers.lost.load(Ordering::Acquire), 1);
        let first = receiver.try_recv().unwrap();
        assert!(first.contains("event=start"));
        assert!(first.contains("marker_lost=0"));
        markers.emit("incarnation", "end", "outcome=progress");
        let last = receiver.try_recv().unwrap();
        assert!(last.contains("event=end"));
        assert!(last.contains("marker_lost=1"));
        assert!(last.contains("monotonic_us="));
        assert!(last.contains("wall_ms="));
        assert_eq!(markers.lost.load(Ordering::Acquire), 0);
    }
}
