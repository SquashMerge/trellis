//! Shared admission and publication boundary for CLI and authenticated daemon index jobs.
//! A job has one expected pair, one captured source set, and one complete paired commit.
use crate::{
    capture::Capture,
    indexer::{self, IndexOptions},
    model::{CancelFlag, IndexPin, IndexProgress},
    store::{PublishPermit, RecoveryBaseline, Store, topology::WriterSession},
};
use anyhow::{Result, ensure};
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
    session: Arc<WriterSession>,
    publication: Option<PublishPermit>,
}

impl IndexJobCoordinator {
    /// The control baseline admits known old indexes without exposing their evidence to readers.
    /// A supplied HTTP pair is checked before any source admission or worker is started.
    pub fn prepare(store: &Store, requested: Option<IndexPin>) -> Result<Self> {
        // Writer admission has its own Store gate around its data_version→COMMIT
        // window. It must finish before acquiring this publication's permit.
        let session = store.writer_session()?;
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
        session: Arc<WriterSession>,
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
        session: Arc<WriterSession>,
        publication: PublishPermit,
    ) -> Result<Self> {
        store.verify_writer_session(&session)?;
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

    pub fn session(&self) -> Arc<WriterSession> {
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

    /// Publish exactly the writer's admitted immutable snapshot. A watch cutoff
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
        self.store.begin_writer_publication(&self.session)?;
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
                self.session.writer_guard()?,
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
                self.store.verify_writer_session(&self.session)?;
                report("attest", phase_start.elapsed());
                phase_start = std::time::Instant::now();
                cutoff()?;
                let pin = self.store.publish_local_native_recovery(
                    &graph,
                    &capture,
                    prepared,
                    self.session.writer_guard()?,
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
        self.store.verify_writer_session(&self.session)?;
        report("attest", phase_start.elapsed());
        phase_start = std::time::Instant::now();
        cutoff()?;
        let published = self.store.publish_native_recovery(
            &graph,
            &capture,
            &native,
            self.session.writer_guard()?,
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
pub struct WatcherWork {
    watch: crate::watch::WatchSignals,
    /// Shared with every watcher this owner creates; read without its lock.
    health: Arc<crate::watch::WatcherHealth>,
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
    /// The owning worker's wake, installed on every watcher this owner creates.
    waker: Option<crate::watch::Waker>,
    /// Set only for retirement's attempt: the batch must be exactly this generation.
    generation_limit: Option<u64>,
}

/// The retirement attempt found ingress past its cutoff: it captures nothing.
#[derive(Debug)]
pub struct PastCutoff;
impl std::fmt::Display for PastCutoff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("watcher ingress passed the retirement cutoff")
    }
}
impl std::error::Error for PastCutoff {}

const PERIODIC_INVENTORY: std::time::Duration = std::time::Duration::from_secs(60);

impl WatcherWork {
    /// Construct only after the daemon commits mandatory startup reconciliation.
    pub fn new(
        store: &Store,
        session: &Arc<WriterSession>,
        options: &IndexOptions,
    ) -> Result<Self> {
        Self::publishing(
            store,
            session,
            options,
            Arc::new(crate::watch::WatcherHealth::default()),
        )
    }

    /// Like `new`, publishing watcher health through the owner's `health` flag
    /// for lock-free reads. Every replacement watcher shares the same flag.
    pub fn publishing(
        store: &Store,
        session: &Arc<WriterSession>,
        options: &IndexOptions,
        health: Arc<crate::watch::WatcherHealth>,
    ) -> Result<Self> {
        store.verify_writer_session(session)?;
        let watch = crate::watch::WatchSignals::publishing(
            options.workspace_root.clone(),
            options.scip_path.clone(),
            options.manifest_path.clone(),
            health.clone(),
        );
        Ok(Self {
            health,
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
            waker: None,
            generation_limit: None,
        })
    }

    /// Retirement's one attempt (Decision 0009 §5): reconcile only while the
    /// accepted generation is still `cutoff`. A later signal, before or during
    /// the capture, abandons it, so nothing after the cutoff is captured or
    /// acknowledged.
    pub fn reconcile_through_cutoff(
        &mut self,
        store: &Store,
        session: &Arc<WriterSession>,
        options: &IndexOptions,
        cancel: &CancelFlag,
        cutoff: u64,
    ) -> Result<bool> {
        if self.options_differ(options) {
            return Err(PastCutoff.into());
        }
        self.generation_limit = Some(cutoff);
        let outcome = self.reconcile_due(store, session, options, cancel, true);
        self.generation_limit = None;
        outcome
    }

    /// Wake `waker` after every accepted signal, on this and every later watcher.
    pub fn set_waker(&mut self, waker: crate::watch::Waker) {
        self.watch.set_waker(waker.clone());
        self.waker = Some(waker);
    }

    /// The current watcher's live ingress generation (replaced with the watcher).
    pub fn generation_handle(&self) -> Arc<std::sync::atomic::AtomicU64> {
        self.watch.generation_handle()
    }

    pub fn acknowledged_generation(&self) -> u64 {
        self.watch.acknowledged_generation()
    }

    /// `accepted_watch_intent` without the ingress-generation comparison, which
    /// readers make themselves against the live generation.
    pub fn intent_besides_generation(&self, options: &IndexOptions) -> bool {
        let now = std::time::Instant::now();
        let periodic_ready = now.duration_since(self.last_inventory) >= PERIODIC_INVENTORY
            && !self.retry_after.is_some_and(|deadline| deadline > now);
        self.options_differ(options) || self.watch.full_intent() || periodic_ready
    }

    fn options_differ(&self, options: &IndexOptions) -> bool {
        self.options.workspace_root != options.workspace_root
            || self.options.scip_path != options.scip_path
            || self.options.manifest_path != options.manifest_path
            || self.options.max_file_bytes != options.max_file_bytes
    }

    /// When `reconcile_due` next has work: a failed run's retry delay, else the
    /// debounce deadline of accepted signals, else the periodic inventory.
    /// Signals still in the ingress channel wake the worker themselves.
    pub fn next_due(&self) -> std::time::Instant {
        if let Some(retry) = self.retry_after
            && retry > std::time::Instant::now()
        {
            return retry;
        }
        let periodic = self.last_inventory + PERIODIC_INVENTORY;
        let accounted = self.watch.degraded()
            && self.last_accounted_generation
                == Some(
                    self.watch
                        .generation_handle()
                        .load(std::sync::atomic::Ordering::SeqCst),
                );
        if accounted {
            return periodic;
        }
        self.watch
            .next_deadline()
            .map_or(periodic, |deadline| deadline.min(periodic))
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

    /// Snapshot watcher health without consuming its pending event batch.
    pub fn watcher_degraded(&self) -> bool {
        self.watch.degraded()
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
        let periodic_ready = now.duration_since(self.last_inventory) >= PERIODIC_INVENTORY
            && !self.retry_after.is_some_and(|deadline| deadline > now);
        self.options_differ(options) || self.watch.accepted_unacked() || periodic_ready
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
        self.watch = crate::watch::WatchSignals::publishing(
            absent_watch_root,
            None,
            None,
            self.health.clone(),
        );
        assert!(self.watch.degraded());
        self.last_accounted_generation = Some(self.watch.drain().generation);
    }

    /// Push the periodic inventory out of a test's reach, so only a wake can
    /// make the worker reconcile. The next completed inventory resets it.
    #[doc(hidden)]
    pub fn delay_periodic_inventory_for_tests(&mut self) {
        self.last_inventory = std::time::Instant::now() + std::time::Duration::from_secs(3600);
    }

    /// Advance only the inventory deadline in a test. The next ordinary tick
    /// still uses the production admission and publication fences.
    #[doc(hidden)]
    pub fn force_periodic_inventory_for_tests(&mut self) {
        self.last_inventory = std::time::Instant::now() - std::time::Duration::from_secs(60);
    }

    /// A full capture always checks root, writer and selected publication fences.
    /// A failed capture keeps the dirty generation for a later signal or scan.
    pub fn reconcile_due(
        &mut self,
        store: &Store,
        session: &Arc<WriterSession>,
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
        session: &Arc<WriterSession>,
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
        session: &Arc<WriterSession>,
        options: &IndexOptions,
        cancel: &CancelFlag,
        force: bool,
        cutoffs: (
            impl FnOnce(&Capture, &crate::watch::WatchSignals),
            impl FnOnce(&crate::watch::WatchSignals),
        ),
    ) -> Result<bool> {
        let (observe, after_cutoff) = cutoffs;
        store.verify_writer_session(session)?;
        // The durable FIFO head may have different selected inputs from this
        // daemon's defaults. Keep its watcher until a new watcher has registered
        // for the new options; its full wake then closes the transition gap.
        if self.options_differ(options) {
            let next = crate::watch::WatchSignals::publishing(
                options.workspace_root.clone(),
                options.scip_path.clone(),
                options.manifest_path.clone(),
                self.health.clone(),
            );
            if let Some(waker) = &self.waker {
                next.set_waker(waker.clone());
            }
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
        let periodic = now.duration_since(self.last_inventory) >= PERIODIC_INVENTORY;
        let batch = self.watch.drain();
        // Retirement's attempt never captures past its cutoff; the capture's
        // own generation fence then keeps the batch at exactly the cutoff.
        if self
            .generation_limit
            .is_some_and(|limit| batch.generation != limit)
        {
            return Err(PastCutoff.into());
        }
        // Observe each new generation before its capture can acknowledge it.
        // The queue tick holds watcher_work across this call; the bounded
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
        // The first post-registration full inventory closes the startup/watch gap.
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
                store.verify_writer_session(session)?;
                store.verify_root()?;
                Ok(())
            };
            cutoff()?;
            let baseline = store.publication_index_baseline()?;
            let unchanged = store.selected_capture_unchanged(
                &captured,
                session.writer_guard()?,
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
                    // Keep it dirty for the next local startup reconciliation and full inventory.
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
) -> Result<(IndexPin, Arc<WriterSession>)> {
    // Explicit daemon indexing obeys the same queue-recovery barrier as
    // activation: confirm old-root terminal rows before ANY startup reconciliation publication.
    store.settle_foreign_root_requests()?;
    if store.is_recreate_pending() {
        return store.recreate_pending_writer_session(options, cancel);
    }
    let coordinator = IndexJobCoordinator::prepare(store, None)?;
    let session = coordinator.session();
    let pin = coordinator.run(options, cancel, progress)?;
    session.verify()?;
    Ok((pin, session))
}

/// The daemon-local writer drains the queue. Reclaimed running rows retain FIFO position.
/// Callers serialize this function with startup and other local native work.
pub fn drain_requests(store: &Store, session: &Arc<WriterSession>) -> Result<usize> {
    drain_requests_observed(store, session, |_, _| {})
}

/// Progress is advisory and local. The durable request row alone controls state.
pub fn drain_requests_observed(
    store: &Store,
    session: &Arc<WriterSession>,
    progress: impl Fn(&str, IndexProgress) + Sync,
) -> Result<usize> {
    drain_requests_observed_with_native(store, session, progress, |_, _, _| {})
}

/// Observe actual full-native extraction and completed validation for each
/// claimed request. The callback is scoped to this drain and retains no state.
pub fn drain_requests_observed_with_native(
    store: &Store,
    session: &Arc<WriterSession>,
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
    session: &Arc<WriterSession>,
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
    session: &Arc<WriterSession>,
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
        store.fail_changed_root_requests()?;
        anyhow::bail!("root_changed: captured root stopped after queue failure transition");
    }
    store.verify_writer_session(session)?;
    store.settle_foreign_root_requests()?;
    // A prior attempt may have published but failed its queue terminal write. Resolve
    // that exact cached result before any new claim or native publication.
    let mut completed = usize::from(store.retry_recorded_completion(session)?);
    if completed >= max_completed {
        return Ok(completed);
    }
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
        #[cfg(feature = "test-support")]
        store.run_after_claim_hook_for_tests();
        // Capture the authenticated expected pin from the same publication
        // admission. A BUSY COMMIT can be safely retried only if its terminal
        // queue reread and current control pin still match this baseline.
        let mut before_publish = None;
        let mut publication = None;
        let outcome = (|| {
            let options = request.options(std::path::Path::new(store.workspace_root()))?;
            let mut coordinator = IndexJobCoordinator::prepare_with_session(
                store,
                request.expected,
                session.clone(),
            )?;
            before_publish = Some(coordinator.expected.pin());
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
        // Ctrl-C stops this CLI waiter, not any accepted client's durable work.
        // The claim stays running; a later daemon activation reclaims the FIFO
        // head only after mandatory reconciliation.
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
            // pin stays running for the next validated startup reconciliation, never an
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
        // for the next daemon activation to reclaim after complete root reconciliation.
        store.record_and_finish_request(session, &request, outcome)?;
        drop(publication);
        completed += 1;
        if completed >= max_completed {
            break;
        }
    }
    Ok(completed)
}

pub(crate) fn retryable_completion_error(error: &anyhow::Error) -> bool {
    // Include storage_result's plain SQLite contention string, but never a
    // cached-claim or writer/root invariant that also uses storage_busy.
    #[cfg(test)]
    if error.to_string() == "storage_busy: injected terminal write failure" {
        return true;
    }
    crate::store::transient_storage_contention(error)
}

/// The CLI is its own sole queue driver. A transient completion failure must be
/// resolved while this verified writer is still held, not handed to a nonexistent
/// daemon or a future command. Never repeat the native publication for that head.
#[cfg(test)]
fn retry_cli_recorded_completion(
    store: &Store,
    session: &WriterSession,
    cancel: &CancelFlag,
    initial: anyhow::Error,
) -> Result<()> {
    if !store.has_recorded_completion(session)? || !retryable_completion_error(&initial) {
        return Err(initial);
    }
    loop {
        ensure!(
            !cancel.load(Ordering::Acquire),
            "index wait interrupted; accepted request remains queued"
        );
        store.verify_writer_session(session)?;
        match store.retry_recorded_completion(session) {
            Ok(true) => return Ok(()),
            Ok(false) => anyhow::bail!("storage_busy: cached completion disappeared"),
            Err(error) if retryable_completion_error(&error) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => return Err(error),
        }
    }
}

pub fn cooperative_maintenance_unit(
    store: &Store,
    session: &Arc<WriterSession>,
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
    let writer = match session.writer_guard() {
        Ok(writer) => writer,
        Err(_) => return Ok(MaintenanceOutcome::Deferred),
    };
    store.maintenance_step(writer, &permit, &probe, priority)
}

/// A checkout's daemon-local writer confirms old-root queue settlement, then
/// commits mandatory startup reconciliation before its scheduler may claim FIFO requests.
pub fn establish_serving_session(
    store: &Store,
    explicit_options: Option<&IndexOptions>,
    cancel: &CancelFlag,
) -> Result<Arc<WriterSession>> {
    store.settle_foreign_root_requests()?;
    if store.is_recreate_pending() {
        let options = explicit_options.ok_or_else(|| {
            anyhow::anyhow!(
                "recovery_required: index options unavailable; run explicit trellis index"
            )
        })?;
        let (_, session) = store.recreate_pending_writer_session(options, cancel)?;
        session.verify()?;
        return Ok(session);
    }
    let session = store.writer_session()?;
    let publication = store.enter_publication(cancel, std::time::Duration::from_millis(250))?;
    let expected = store.publication_index_baseline()?;
    let options = match explicit_options {
        Some(options) => options.clone(),
        None => store
            .recorded_index_options()?
            .unwrap_or_else(|| IndexOptions::new(std::path::PathBuf::from(store.workspace_root()))),
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
        use std::sync::mpsc;
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
                        release_rx
                            .recv_timeout(crate::testing::HANG_BACKSTOP)
                            .unwrap();
                    },
                )
            });
            paused_rx
                .recv_timeout(crate::testing::HANG_BACKSTOP)
                .unwrap();
            session.verify().unwrap();
            let precommit = store.claim_request(&session).unwrap_err();
            assert!(
                precommit.downcast_ref::<IndexNotReady>().is_some(),
                "an acquired EX without its own committed startup reconciliation cannot claim: {precommit:#}"
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

    #[cfg(unix)]
    #[test]
    fn claimed_request_requeues_same_seq_after_input_alias_drift() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("a.js");
        let manifest = workspace.path().join("package.json");
        let alias = workspace.path().join("tsconfig.json");
        fs::write(&source, "function before() {}\n").unwrap();
        fs::write(&manifest, "{\"name\":\"before\"}\n").unwrap();
        fs::hard_link(&manifest, &alias).unwrap();
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
                    && point == "alias"
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
        assert!(edited.load(Ordering::Acquire));
        assert_eq!(first, 0);
        assert_eq!(store.status().unwrap().revision, prior);
        let pending = store.request_by_id(&request.id).unwrap().unwrap();
        assert_eq!(pending.seq, request.seq);
        assert_eq!(pending.state, "queued");
        assert!(pending.error_code.is_none() && pending.claim_incarnation.is_none());
        assert_eq!(
            drain_requests_observed_with_native(&store, &owner, |_, _| {}, |_, _, _| {}).unwrap(),
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
        owner.verify().unwrap();

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
    fn takeover_watcher_switches_input_options_with_new_full_wake() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let head_options = IndexOptions::new(workspace.path().to_owned());
        let cancel = Arc::new(AtomicBool::new(false));
        let owner = establish_serving_session(&store, Some(&head_options), &cancel).unwrap();
        let mut work = WatcherWork::new(&store, &owner, &head_options).unwrap();
        let mut daemon_options = head_options.clone();
        daemon_options.max_file_bytes = 1024;
        let deadline = std::time::Instant::now() + crate::testing::HANG_BACKSTOP;
        while work.last_accounted_generation.is_none() {
            // Native watcher ingress can invalidate the cutoff of this first
            // inventory. Retry the normal tick until the replacement full wake
            // is acknowledged; never bypass the watch with force=true.
            work.reconcile_due(&store, &owner, &daemon_options, &cancel, false)
                .unwrap();
            assert!(
                std::time::Instant::now() < deadline,
                "new option watcher must finish its first full inventory"
            );
            if work.last_accounted_generation.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        assert_eq!(work.options.max_file_bytes, 1024);
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
        let mut work = WatcherWork::new(&store, &owner, &options).unwrap();
        // A registration failure is sticky. It still needs one verified full
        // inventory, but an accounted idle generation must not veto maintenance.
        let absent_watch_root = workspace.path().join("absent_watch_root");
        assert!(!absent_watch_root.exists());
        work.watch = crate::watch::WatchSignals::publishing(
            absent_watch_root,
            None,
            None,
            work.health.clone(),
        );
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
        work.retry_after = Some(std::time::Instant::now() + crate::testing::HANG_BACKSTOP);
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
        assert!(retryable_completion_error(&anyhow::anyhow!(
            "storage_busy: SQLite lock contention"
        )));
        assert!(!retryable_completion_error(&anyhow::anyhow!(
            "storage_busy: cached claim changed"
        )));
        assert!(!retryable_completion_error(&anyhow::anyhow!(
            "storage_busy: unresolved FIFO completion"
        )));
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
        owner.verify().unwrap();
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
        // Retain an accepted queue before exceptional index corruption begins.
        let pending_row = old
            .enqueue_request(&IndexOptions::new(work.path().to_owned()), None)
            .unwrap();
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
        owner.verify().unwrap();
        assert_eq!(
            pending
                .request_by_id(&pending_row.id)
                .unwrap()
                .unwrap()
                .state,
            "queued"
        );
        assert_eq!(pending.status().unwrap().revision.index_revision, 1);
        assert_eq!(
            pending
                .recorded_index_options()
                .unwrap()
                .unwrap()
                .max_file_bytes,
            2_097_152
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
        // Cancellation before COMMIT must not close the validated prior head
        // to daemon-local reads while this Store still awaits its new startup reconciliation.
        let prior = store.evidence_response_daemon_local().unwrap();
        assert_eq!(prior.status().unwrap().revision, first);
        assert_eq!(
            prior.source_at("main.js", Some(first)).unwrap().unwrap().0,
            first
        );
        prior.finish(()).unwrap();
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
        let session = first.writer_session().unwrap();
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
    fn captured_writer_is_reverified_after_root_replacement_before_publish() {
        use crate::store::topology::{TopologyRoots, WorkspaceIdentity};
        let state = tempfile::tempdir().unwrap();
        let work = state.path().join("work");
        let moved = state.path().join("moved");
        std::fs::create_dir(&work).unwrap();
        std::fs::write(work.join("a.js"), "function a() {}\n").unwrap();
        let roots = TopologyRoots::isolated_for_tests(
            state.path().join("cache"),
            state.path().join("data"),
        );
        let identity = WorkspaceIdentity::discover(Some(&work), &work)
            .unwrap()
            .attach_marker()
            .unwrap();
        let store = Store::open(roots, identity).unwrap();
        let job = IndexJobCoordinator::prepare(&store, None).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let error = job
            .run_observed(
                &IndexOptions::new(work.clone()),
                &cancel,
                |_| {},
                |_| {
                    std::fs::rename(&work, &moved).unwrap();
                    std::fs::create_dir(&work).unwrap();
                },
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("root_changed"), "{error:#}");
        assert!(store.status().is_err());
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
        Arc<WriterSession>,
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
    fn cached_completion_consumes_one_daemon_tick_before_next_fifo_claim() {
        let (_tmp, store, options, baseline, session) = fixture();
        let first = store.enqueue_request(&options, None).unwrap();
        let mut next_options = options.clone();
        next_options.max_file_bytes = 1_048_576;
        let second = store.enqueue_request(&next_options, None).unwrap();
        assert!(first.seq < second.seq);
        assert_ne!(first.options_json, second.options_json);

        store.inject_queue_finish_failure(false);
        let error = drain_one_request_observed(&store, &session, |_, _| {}).unwrap_err();
        assert!(error.to_string().contains("storage_busy"), "{error:#}");
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

        assert_eq!(
            drain_one_request_observed(&store, &session, |_, _| {}).unwrap(),
            1
        );
        let q1 = store.request_by_id(&first.id).unwrap().unwrap();
        let pending = store.request_by_id(&second.id).unwrap().unwrap();
        assert_eq!(q1.state, "done");
        assert_eq!(q1.revision, Some(published));
        assert_eq!(pending.state, "queued");
        assert!(pending.started_at.is_none());
        assert_eq!(store.status().unwrap().revision, published);
        assert_eq!(
            (&q1.id, q1.seq, &q1.options_json, q1.expected),
            (&first.id, first.seq, &first.options_json, first.expected)
        );
        assert_eq!(
            (
                &pending.id,
                pending.seq,
                &pending.options_json,
                pending.expected
            ),
            (
                &second.id,
                second.seq,
                &second.options_json,
                second.expected
            )
        );

        assert_eq!(
            drain_one_request_observed(&store, &session, |_, _| {}).unwrap(),
            1
        );
        let q1 = store.request_by_id(&first.id).unwrap().unwrap();
        let q2 = store.request_by_id(&second.id).unwrap().unwrap();
        assert_eq!(q1.revision, Some(published));
        assert_eq!(q2.state, "done");
        let next_pin = q2.revision.unwrap();
        assert_eq!(next_pin.index_revision, published.index_revision + 1);
        assert_ne!(next_pin, published);
        assert_eq!(store.status().unwrap().revision, next_pin);
        assert_eq!(
            (&q2.id, q2.seq, &q2.options_json, q2.expected),
            (
                &second.id,
                second.seq,
                &second.options_json,
                second.expected
            )
        );
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
