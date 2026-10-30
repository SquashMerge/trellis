//! Shared admission and publication boundary for CLI and authenticated daemon index jobs.
//! A job has one expected pair, one captured source set, and one complete paired commit.
use crate::{
    capture::Capture,
    indexer::{self, IndexOptions},
    model::{CancelFlag, IndexPin, IndexProgress},
    store::{PublishPermit, RecoveryBaseline, Store, requests::Request, topology::WriterSession},
};
use anyhow::{Result, ensure};
use std::sync::{Arc, atomic::Ordering};

#[cfg(test)]
thread_local! {
    static FULL_STREAM_RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
#[cfg(test)]
fn full_stream_runs_for_tests() -> usize {
    FULL_STREAM_RUNS.with(std::cell::Cell::get)
}

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
            || {},
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
            || {},
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
        let diagnostic_incarnation = diagnostics().map(|_| self.session.incarnation().to_string());
        let diagnostic_mode = std::sync::Mutex::new("unknown");
        let pin = self.run_with_capture(
            options,
            cancel,
            |progress| {
                if diagnostic_incarnation.is_some() {
                    match progress.phase.as_str() {
                        "mode:local" => *diagnostic_mode.lock().unwrap() = "local",
                        "mode:full" => *diagnostic_mode.lock().unwrap() = "full",
                        _ => {}
                    }
                }
            },
            |_| {},
            |_, _| {},
            PublicationAdmission {
                capture: Some(capture),
                unchanged_fast: false,
                cutoff,
            },
            || {},
        )?;
        if let Some(incarnation) = diagnostic_incarnation.as_deref() {
            let mode = *diagnostic_mode.lock().unwrap();
            let mut detail = format!(
                "generation={} revision={} mode={}",
                pin.index_generation, pin.index_revision, mode
            );
            // Attribute counters before releasing this publication's ownership.
            if matches!(mode, "local" | "full")
                && let Some(writer) = self.store.last_writer_diagnostic()
            {
                detail.push(' ');
                detail.push_str(&writer);
            }
            diagnostic_marker(incarnation, "index_watcher_complete", &detail);
        }
        Ok((pin, self.publication.take().expect("publication admitted")))
    }

    #[allow(clippy::too_many_arguments)]
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
        before_capture: impl FnOnce(),
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
            None => {
                before_capture();
                Capture::admit(options, cancel, &progress)?
            }
        };
        report("capture", phase_start.elapsed());
        phase_start = std::time::Instant::now();
        if unchanged_fast {
            cutoff()?;
        }
        if unchanged_fast
            && let Some(pin) = self
                .store
                .reuse_unchanged_native_head(
                    &capture,
                    self.session.writer_guard()?,
                    &self.expected,
                    cancel,
                )
                .map_err(|error| self.store.report_index_failure(error))?
        {
            progress(IndexProgress {
                phase: "mode:unchanged".into(),
                completed: 0,
                total: 1,
            });
            report("publish", phase_start.elapsed());
            observe(&capture);
            self.store
                .attest_post_acquisition_reconciliation(&self.session, pin)
                .map_err(|error| self.store.report_index_failure(error))?;
            return Ok(pin);
        }
        let root = std::fs::canonicalize(&options.workspace_root)?;
        if let Some(prepared) =
            self.store
                .prepare_local_revision(&capture, &self.expected, cancel)?
        {
            report("measure", phase_start.elapsed());
            phase_start = std::time::Instant::now();
            let changed = indexer::project_native_documents(
                options,
                &capture,
                &prepared.native,
                &prepared.changed_paths,
                cancel,
                &progress,
            )?;
            let graph = self
                .store
                .compose_local_revision(&capture, &prepared, changed, cancel)?;
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
                .attest_post_acquisition_reconciliation(&self.session, pin)
                .map_err(|error| self.store.report_index_failure(error))?;
            return Ok(pin);
        }
        progress(IndexProgress {
            phase: "mode:full".into(),
            completed: 0,
            total: 1,
        });
        let spool = crate::native_evidence::spool_full_capture(
            options,
            &capture,
            &root,
            self.store.root_id(),
            cancel,
            &progress,
            native_observe,
            &self.store.native_spool_dir()?,
        )?;
        #[cfg(test)]
        FULL_STREAM_RUNS.with(|count| count.set(count.get() + 1));
        report("measure", phase_start.elapsed());
        phase_start = std::time::Instant::now();
        // The spool verified the capture after completing all document work.
        // The publisher reattests it before the first version insert.
        observe(&capture);
        ensure!(!cancel.load(Ordering::Acquire), "index cancelled");
        self.store.verify_writer_session(&self.session)?;
        report("attest", phase_start.elapsed());
        phase_start = std::time::Instant::now();
        cutoff()?;
        let published = self.store.publish_validated_stream_recovery(
            &spool,
            &capture,
            self.session.writer_guard()?,
            self.expected.clone(),
            cancel,
        )?;
        report("publish", phase_start.elapsed());
        self.store
            .attest_post_acquisition_reconciliation(&self.session, published)
            .map_err(|error| self.store.report_index_failure(error))?;
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

struct RequestCoverage {
    request: crate::store::requests::Request,
    batch: crate::watch::DirtyBatch,
    identity: Arc<std::sync::atomic::AtomicU64>,
    options: IndexOptions,
    pin: Option<IndexPin>,
    verified: bool,
}

/// The elected owner alone drives the advisory watcher and the durable FIFO.
/// This is an in-memory scheduler, not a second request queue.
pub struct WatcherWork {
    watch: crate::watch::WatchSignals,
    request_coverage: Option<RequestCoverage>,
    /// Armed when a row completes with a successor queued, until the next
    /// claim or an empty queue (T04, #136 owner amendment). The gate is spent
    /// once one full capture has started under the current options; intent
    /// accepted after that is before the next row's own cutoff, so that row's
    /// capture covers it.
    queue_gate: bool,
    gate_attempted: bool,
    #[cfg(all(test, feature = "test-support"))]
    full_capture_attempts: usize,
    #[cfg(all(test, feature = "test-support"))]
    reconcile_now_for_tests: Option<std::time::Instant>,
    /// Shared with every watcher this owner creates; read without its lock.
    health: Arc<crate::watch::WatcherHealth>,
    last_inventory: std::time::Instant,
    retry_after: Option<std::time::Instant>,
    last_accounted_generation: Option<u64>,
    last_accounted_pin: Option<IndexPin>,
    options: IndexOptions,
    #[cfg(feature = "test-support")]
    witness: Option<Arc<crate::daemon::causal_witness::CausalWitness>>,
    #[cfg(feature = "test-support")]
    witness_owner: Option<uuid::Uuid>,
    #[cfg(feature = "test-support")]
    witness_epoch: uuid::Uuid,
    #[cfg(feature = "test-support")]
    witness_ordinal: u64,
    #[cfg(feature = "test-support")]
    witness_ordinal_source: Option<Arc<std::sync::atomic::AtomicU64>>,
    #[cfg(feature = "test-support")]
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
        let watch =
            crate::watch::WatchSignals::publishing(options.workspace_root.clone(), health.clone());
        Ok(Self {
            health,
            watch,
            request_coverage: None,
            queue_gate: false,
            gate_attempted: false,
            #[cfg(all(test, feature = "test-support"))]
            full_capture_attempts: 0,
            #[cfg(all(test, feature = "test-support"))]
            reconcile_now_for_tests: None,
            last_inventory: std::time::Instant::now(),
            retry_after: None,
            last_accounted_generation: None,
            last_accounted_pin: None,
            options: options.clone(),
            #[cfg(feature = "test-support")]
            witness: None,
            #[cfg(feature = "test-support")]
            witness_owner: None,
            #[cfg(feature = "test-support")]
            witness_epoch: uuid::Uuid::nil(),
            #[cfg(feature = "test-support")]
            witness_ordinal: 0,
            #[cfg(feature = "test-support")]
            witness_ordinal_source: None,
            #[cfg(feature = "test-support")]
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
        (self.options.workspace_root != options.workspace_root
            && std::fs::canonicalize(&self.options.workspace_root).ok()
                != std::fs::canonicalize(&options.workspace_root).ok())
            || self.options.max_file_bytes != options.max_file_bytes
    }

    pub(crate) fn options(&self) -> IndexOptions {
        self.options.clone()
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
    #[cfg(feature = "test-support")]
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

    #[cfg(feature = "test-support")]
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

    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn full_capture_attempts_for_tests(&self) -> usize {
        self.full_capture_attempts
    }

    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn set_reconcile_now_for_tests(&mut self, now: Option<std::time::Instant>) {
        self.reconcile_now_for_tests = now;
    }

    fn snapshot_request(
        &mut self,
        request: &crate::store::requests::Request,
        options: &IndexOptions,
    ) -> Option<RequestCoverage> {
        if self.options_differ(options) {
            return None;
        }
        let batch = self.watch.drain();
        if !self.accepted_watch_intent(options) {
            return None;
        }
        Some(RequestCoverage {
            request: request.clone(),
            batch,
            identity: self.watch.generation_handle(),
            options: options.clone(),
            pin: None,
            verified: false,
        })
    }

    fn confirm_request(&mut self, store: &Store, session: &WriterSession, id: &str) -> Result<()> {
        if self.request_coverage.is_none() {
            return Ok(());
        }
        let row = store.request_by_id(id)?;
        let certificate = self.request_coverage.take().expect("coverage checked");
        let Some(pin) = certificate.pin else {
            return Ok(());
        };
        if row.as_ref().is_none_or(|row| {
            row.id != certificate.request.id
                || row.seq != certificate.request.seq
                || row.claim_incarnation != certificate.request.claim_incarnation
                || row.root_device != certificate.request.root_device
                || row.root_inode != certificate.request.root_inode
                || row.state != "done"
                || row.revision != Some(pin)
                || row.error_code.is_some()
                || row.finished_at.is_none()
        }) || !certificate.verified
            || session.incarnation().to_string()
                != certificate
                    .request
                    .claim_incarnation
                    .as_deref()
                    .unwrap_or_default()
            || self.options_differ(&certificate.options)
            || !Arc::ptr_eq(&self.watch.generation_handle(), &certificate.identity)
            || store.verify_writer_session(session).is_err()
            || !store.index_baseline().is_ok_and(|current| current == pin)
        {
            return Ok(());
        }
        if self.watch.acknowledge(&certificate.batch) {
            self.last_accounted_generation = Some(certificate.batch.generation);
            self.last_accounted_pin = Some(pin);
            self.last_inventory = std::time::Instant::now();
            self.retry_after = None;
            #[cfg(feature = "test-support")]
            self.report_request_pending(certificate.batch.generation);
        } else {
            self.last_accounted_pin = None;
            self.watch.require_full();
        }
        Ok(())
    }

    /// Announce request-covered intent; the worker ACKs after publishing status.
    #[cfg(feature = "test-support")]
    fn report_request_pending(&mut self, generation: u64) {
        let (Some(reporter), Some(lineage)) = (self.witness.clone(), self.causal_lineage()) else {
            return;
        };
        if self.witness_reported_generation != Some(generation) {
            reporter.watch_pending(lineage, generation);
            self.witness_reported_generation = Some(generation);
        }
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
        self.watch = crate::watch::WatchSignals::publishing(absent_watch_root, self.health.clone());
        assert!(self.watch.degraded());
        self.last_accounted_generation = Some(self.watch.drain().generation);
    }

    /// Push the periodic inventory out of a test's reach, so only a wake can
    /// make the worker reconcile. The next completed inventory resets it.
    #[doc(hidden)]
    pub fn delay_periodic_inventory_for_tests(&mut self) {
        self.last_inventory = std::time::Instant::now() + std::time::Duration::from_secs(3600);
    }

    #[doc(hidden)]
    pub fn expire_retry_for_tests(&mut self) {
        self.retry_after = Some(std::time::Instant::now());
    }

    #[doc(hidden)]
    pub fn defer_retry_for_tests(&mut self) {
        self.retry_after = Some(std::time::Instant::now() + std::time::Duration::from_secs(60));
    }

    #[doc(hidden)]
    pub fn periodic_inventory_due_for_tests(&self) -> bool {
        std::time::Instant::now().duration_since(self.last_inventory) >= PERIODIC_INVENTORY
    }

    /// Advance only the inventory deadline in a test. The next ordinary tick
    /// still uses the production admission and publication fences.
    #[doc(hidden)]
    pub fn force_periodic_inventory_for_tests(&mut self) {
        self.last_inventory = std::time::Instant::now() - std::time::Duration::from_secs(60);
    }

    pub(crate) fn set_queue_gate(&mut self) {
        self.queue_gate = true;
        self.gate_attempted = false;
    }

    pub(crate) fn clear_queue_gate(&mut self) {
        self.queue_gate = false;
        self.gate_attempted = false;
    }

    /// An armed gate holds the next claim until one capture has started under
    /// the current effective options. A registration is one-off, so
    /// continuous edits cannot hold the FIFO.
    pub(crate) fn gate_pending(&self, options: &IndexOptions) -> bool {
        self.queue_gate
            && (self.options_differ(options)
                || (!self.gate_attempted && self.accepted_watch_intent(options)))
    }

    /// Accepted ingress and the queue claim share the callback's liveness lock,
    /// and the caller holds the registration lock that chose `options`. If
    /// the gate became pending after its sample, the owner reconciles it
    /// before retrying this claim.
    pub(crate) fn claim_if_gate_clear(
        &mut self,
        options: &IndexOptions,
        claim: impl FnOnce() -> Result<Option<Request>>,
    ) -> Result<Option<Option<Request>>> {
        let _ingress = self.watch.lock_ingress();
        if self.gate_pending(options) {
            return Ok(None);
        }
        Ok(Some(claim()?))
    }

    pub(crate) fn reconcile_queue_gate(
        &mut self,
        store: &Store,
        session: &Arc<WriterSession>,
        options: &IndexOptions,
        cancel: &CancelFlag,
    ) -> Result<bool> {
        self.reconcile_due_with_mode(
            store,
            session,
            options,
            cancel,
            false,
            true,
            (|_, _| {}, |_| {}),
        )
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
        self.reconcile_due_with_mode(store, session, options, cancel, force, false, cutoffs)
    }

    #[allow(clippy::too_many_arguments)]
    fn reconcile_due_with_mode(
        &mut self,
        store: &Store,
        session: &Arc<WriterSession>,
        options: &IndexOptions,
        cancel: &CancelFlag,
        force: bool,
        skip_debounce: bool,
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
                self.health.clone(),
            );
            if let Some(waker) = &self.waker {
                next.set_waker(waker.clone());
            }
            self.watch = next;
            self.request_coverage = None;
            // New options need their own capture before an armed gate opens.
            self.gate_attempted = false;
            self.options = options.clone();
            self.last_accounted_generation = None;
            self.last_accounted_pin = None;
            #[cfg(feature = "test-support")]
            if let Some(source) = &self.witness_ordinal_source {
                self.witness_epoch = uuid::Uuid::new_v4();
                self.witness_reported_generation = None;
                self.witness_ordinal = source.fetch_add(1, Ordering::AcqRel) + 1;
            }
        }
        let now = std::time::Instant::now();
        #[cfg(all(test, feature = "test-support"))]
        let now = self.reconcile_now_for_tests.unwrap_or(now);
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
        #[cfg(feature = "test-support")]
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
                || (!skip_debounce && !self.watch.batch_ready_at(now)))
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
            #[cfg(all(test, feature = "test-support"))]
            {
                self.full_capture_attempts += 1;
            }
            // Spent once a capture starts, even if it fails: the next row's
            // capture covers what it left. A deferral with no capture, such
            // as retry backoff, leaves an armed gate pending.
            self.gate_attempted = true;
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
            let unchanged = store
                .selected_capture_unchanged(&captured, session.writer_guard()?, &baseline, cancel)
                .map_err(|error| store.report_index_failure(error))?;
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
                #[cfg(feature = "test-support")]
                let witnessed_pin = self
                    .witness
                    .as_ref()
                    .and_then(|_| store.index_baseline().ok());
                #[cfg(not(feature = "test-support"))]
                let witnessed_pin = None;
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
                // An armed gate stays armed until the next claim: its attempt
                // is spent, but an options change before the claim still
                // needs its own capture first.
                if accounted {
                    self.last_inventory = std::time::Instant::now();
                    self.retry_after = None;
                }
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
        None,
        |claim| claim(),
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
        None,
        |claim| claim(),
    )
}

pub(crate) type OwningWatcher =
    std::sync::Mutex<Option<(std::sync::Weak<WriterSession>, WatcherWork)>>;

pub(crate) fn drain_one_request_with_watcher(
    store: &Store,
    session: &Arc<WriterSession>,
    watcher: &OwningWatcher,
    progress: impl Fn(&str, IndexProgress) + Sync,
    before_claim: impl Fn(&dyn Fn() -> Result<Option<Request>>) -> Result<Option<Request>>,
) -> Result<usize> {
    drain_requests_observed_with_cancel(
        store,
        session,
        &Arc::new(std::sync::atomic::AtomicBool::new(false)),
        progress,
        |_, _, _| {},
        1,
        None,
        Some(watcher),
        before_claim,
    )
}

#[allow(clippy::too_many_arguments)]
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
    watcher: Option<&OwningWatcher>,
    before_claim: impl Fn(&dyn Fn() -> Result<Option<Request>>) -> Result<Option<Request>>,
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
    if completed > 0
        && let Some(watcher) = watcher
    {
        let mut guard = watcher.lock().unwrap();
        if let Some((owner, work)) = guard.as_mut()
            && owner
                .upgrade()
                .is_some_and(|owner| Arc::ptr_eq(&owner, session))
            && let Some(id) = work
                .request_coverage
                .as_ref()
                .map(|cert| cert.request.id.clone())
        {
            work.confirm_request(store, session, &id)?;
        }
    }
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
        // Cached completion is confirmed above. Recheck the gate at the
        // owning preclaim transition rather than relying on a worker sample.
        let Some(request) = before_claim(&|| store.claim_request(session))? else {
            break;
        };
        if let Some(watcher) = watcher {
            let mut guard = watcher.lock().unwrap();
            if let Some((owner, work)) = guard.as_mut()
                && owner
                    .upgrade()
                    .is_some_and(|owner| Arc::ptr_eq(&owner, session))
            {
                work.clear_queue_gate();
            }
        }
        #[cfg(feature = "test-support")]
        store.run_after_claim_hook_for_tests();
        #[cfg(feature = "test-support")]
        if std::env::var_os("TRELLIS_TEST_SEMANTIC_AMBIGUOUS_TERMINAL").is_some()
            && request.request_kind == crate::store::requests::RequestKind::SemanticImport
        {
            store.fail_next_completion_after_commit_for_tests();
        }
        #[cfg(all(feature = "test-support", unix))]
        if request.request_kind == crate::store::requests::RequestKind::SemanticImport
            && let Some(path) = std::env::var_os("TRELLIS_TEST_SEMANTIC_CRASH_AFTER_CLAIM_SOCKET")
        {
            use std::os::unix::net::UnixListener;
            let path = std::path::PathBuf::from(path);
            if !path.with_extension("done").exists() {
                let listener = UnixListener::bind(&path)?;
                let (_stream, _) = listener.accept()?;
            }
        }
        // Capture the authenticated expected pin from the same publication
        // admission. A BUSY COMMIT can be safely retried only if its terminal
        // queue reread and current control pin still match this baseline.
        let mut before_publish = None;
        let mut publication = None;
        let diagnostic_incarnation = diagnostics().map(|_| session.incarnation().to_string());
        let diagnostic_mode = std::sync::Mutex::new("unknown");
        let mut completion_diagnostic = None;
        let outcome = (|| {
            if request.request_kind == crate::store::requests::RequestKind::SemanticImport {
                before_publish = Some(Some(store.index_baseline()?));
                return run_semantic_batch_request(
                    store,
                    session,
                    &request,
                    watcher,
                    cancel,
                    &mut before_publish,
                );
            }
            let options = request.options(std::path::Path::new(store.workspace_root()))?;
            let mut coordinator = IndexJobCoordinator::prepare_with_session(
                store,
                request.expected,
                session.clone(),
            )?;
            before_publish = Some(coordinator.expected.pin());
            let candidate = std::cell::RefCell::new(None::<RequestCoverage>);
            let result = coordinator.run_with_capture(
                &options,
                cancel,
                |p| {
                    if let Some(incarnation) = diagnostic_incarnation.as_deref() {
                        match p.phase.as_str() {
                            "mode:local" => *diagnostic_mode.lock().unwrap() = "local",
                            "mode:full" => *diagnostic_mode.lock().unwrap() = "full",
                            "mode:unchanged" => *diagnostic_mode.lock().unwrap() = "unchanged",
                            _ => {}
                        }
                        if p.phase.starts_with("mode:") || p.phase.starts_with("timing:") {
                            diagnostic_marker(
                                incarnation,
                                "index_request_progress",
                                &format!(
                                    "request={} phase={} completed={} total={}",
                                    request.id, p.phase, p.completed, p.total
                                ),
                            );
                        }
                    }
                    progress(&request.id, p);
                },
                |capture| {
                    if let Some(cert) = candidate.borrow_mut().as_mut() {
                        cert.verified = capture.covers_watcher_request(
                            &cert.options,
                            &request.root_device,
                            &request.root_inode,
                        );
                    }
                },
                |key, stage| native_observe(&request.id, key, stage),
                PublicationAdmission {
                    capture: None,
                    unchanged_fast: true,
                    cutoff: || Ok(()),
                },
                || {
                    if let Some(watcher) = watcher {
                        let mut guard = watcher.lock().unwrap();
                        if let Some((owner, work)) = guard.as_mut()
                            && owner
                                .upgrade()
                                .is_some_and(|owner| Arc::ptr_eq(&owner, session))
                        {
                            *candidate.borrow_mut() = work.snapshot_request(&request, &options);
                        }
                    }
                },
            );
            if diagnostic_incarnation.is_some()
                && let Ok(pin) = &result
            {
                let mode = *diagnostic_mode.lock().unwrap();
                // Copy counters while this operation still owns publication.
                // An unchanged result did not write: its stored counters are old.
                let writer = if matches!(mode, "local" | "full") {
                    store.last_writer_diagnostic()
                } else {
                    None
                };
                let mut detail = format!(
                    "request={} generation={} revision={} mode={}",
                    request.id, pin.index_generation, pin.index_revision, mode
                );
                if let Some(writer) = writer {
                    detail.push(' ');
                    detail.push_str(&writer);
                }
                completion_diagnostic = Some(detail);
            }
            // A post-COMMIT error can be ambiguous. Retain the permit through
            // terminal ACK, cached completion or typed busy requeue even then.
            publication = coordinator.publication.take();
            if let Some(watcher) = watcher {
                let mut guard = watcher.lock().unwrap();
                if let Some((owner, work)) = guard.as_mut()
                    && owner
                        .upgrade()
                        .is_some_and(|owner| Arc::ptr_eq(&owner, session))
                {
                    if let Ok(pin) = &result
                        && work
                            .last_accounted_pin
                            .is_some_and(|accounted| accounted != *pin)
                    {
                        work.last_accounted_pin = None;
                    }
                    work.request_coverage = candidate.into_inner().and_then(|mut cert| {
                        cert.pin = result.as_ref().ok().copied();
                        (cert.verified && cert.pin.is_some()).then_some(cert)
                    });
                }
            }
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
            if request.request_kind == crate::store::requests::RequestKind::SemanticImport
                && let Some(selected) = store.recovery_index_baseline()?.pin()
                && Some(selected) != before
            {
                // A changed pin might be our committed import or an unrelated
                // native publication. Read the selected overlay for integrity,
                // but never infer a terminal result from the pin alone.
                let _ = store.semantic_at(selected)?;
                let job = request.semantic_job()?;
                let _ = store.semantic_batch_report_at(selected, job.descriptor_sha256())?;
            }
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
        if let (Some(incarnation), Some(detail)) =
            (diagnostic_incarnation.as_deref(), completion_diagnostic)
        {
            diagnostic_marker(incarnation, "index_request_complete", &detail);
        }
        if let Some(watcher) = watcher {
            let mut guard = watcher.lock().unwrap();
            if let Some((owner, work)) = guard.as_mut()
                && owner
                    .upgrade()
                    .is_some_and(|owner| Arc::ptr_eq(&owner, session))
            {
                work.confirm_request(store, session, &request.id)?;
            }
        }
        drop(publication);
        completed += 1;
        if completed >= max_completed {
            break;
        }
    }
    Ok(completed)
}

/// Opens a selected semantic input under `root` with the no-follow, nonblocking, regular-file
/// rules and returns the file with its length.
pub(crate) fn open_semantic_input(
    root: &std::path::Path,
    path: &std::path::Path,
) -> Result<(std::fs::File, u64)> {
    open_semantic_input_before_open(root, path, || {})
}

fn open_semantic_input_before_open(
    root: &std::path::Path,
    path: &std::path::Path,
    before_open: impl FnOnce(),
) -> Result<(std::fs::File, u64)> {
    crate::scip_admission::validate_selected_role(root, path)
        .map_err(|error| anyhow::anyhow!("invalid_request: {}", error.detail()))?;
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file(),
        "invalid_request: semantic input must be a regular file"
    );
    before_open();
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| anyhow::anyhow!("invalid_request: semantic input open: {error}"))?;
    let opened = file.metadata()?;
    ensure!(
        opened.is_file(),
        "invalid_request: semantic input must be a regular file"
    );
    Ok((file, opened.len()))
}

/// Digest and length of an input copied by `copy_semantic_input_to_stage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CopiedInput {
    pub(crate) sha256: String,
    pub(crate) len: u64,
}

/// Streams `reader` into `dest` in chunks of at most `chunk` bytes, hashing while copying.
/// Reads stop at `max + 1` bytes; a larger input is `LimitExceeded("artifact bytes")`.
/// `progress` receives the running total after every chunk.
pub(crate) fn copy_stream_to_stage(
    reader: impl std::io::Read,
    dest: &std::path::Path,
    max: u64,
    chunk: usize,
    mut progress: impl FnMut(u64) -> Result<()>,
) -> Result<CopiedInput> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)?;
    let mut reader = reader.take(max.saturating_add(1));
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; chunk];
    let mut total = 0u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > max {
            return Err(
                crate::scip_raw::ScipCaptureError::LimitExceeded("artifact bytes".into()).into(),
            );
        }
        hasher.update(&buffer[..read]);
        #[cfg(feature = "test-support")]
        if std::env::var("TRELLIS_TEST_SEMANTIC_STAGE_ENOSPC_AFTER_BYTES")
            .ok()
            .and_then(|bytes| bytes.parse::<u64>().ok())
            .is_some_and(|after| total > after)
        {
            return Err(std::io::Error::from_raw_os_error(libc::ENOSPC).into());
        }
        out.write_all(&buffer[..read])?;
        progress(total)?;
    }
    out.flush()?;
    Ok(CopiedInput {
        sha256: hex::encode(hasher.finalize()),
        len: total,
    })
}

/// Copies one selected artifact into the stage in 1 MiB chunks.
pub(crate) fn copy_semantic_input_to_stage(
    root: &std::path::Path,
    path: &std::path::Path,
    dest: &std::path::Path,
    max: u64,
    progress: impl FnMut(u64) -> Result<()>,
) -> Result<CopiedInput> {
    let (file, len) = open_semantic_input(root, path)?;
    if len > max {
        return Err(
            crate::scip_raw::ScipCaptureError::LimitExceeded("artifact bytes".into()).into(),
        );
    }
    copy_stream_to_stage(
        file,
        dest,
        max,
        crate::semantic_batch::limits::IO_BUFFER_BYTES as usize,
        progress,
    )
}

#[cfg(test)]
mod semantic_input_tests {
    use sha2::Digest;
    use std::{io::Read, path::Path};

    fn read_semantic_input_before_open(
        root: &Path,
        path: &Path,
        max: u64,
        before_open: impl FnOnce(),
    ) -> anyhow::Result<Vec<u8>> {
        let (file, len) = super::open_semantic_input_before_open(root, path, before_open)?;
        anyhow::ensure!(len <= max, "input bound");
        let mut bytes = Vec::new();
        file.take(max + 1).read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn read_semantic_input(root: &Path, path: &Path, max: u64) -> anyhow::Result<Vec<u8>> {
        read_semantic_input_before_open(root, path, max, || {})
    }

    #[test]
    fn input_outside_root_reports_bare_reason() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let path = elsewhere.path().join("selected.scip");
        std::fs::write(&path, b"abc").unwrap();
        let error = super::open_semantic_input(root.path(), &path).unwrap_err();
        assert_eq!(error.to_string(), "invalid_request: role outside root");
    }

    #[test]
    fn selected_input_replaced_by_symlink_before_open_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("selected.scip");
        let replacement = root.path().join("other.scip");
        std::fs::write(&path, b"abc").unwrap();
        std::fs::write(&replacement, b"abc").unwrap();
        let result = read_semantic_input_before_open(root.path(), &path, 3, || {
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(&replacement, &path).unwrap();
        });
        assert!(result.is_err(), "must not follow a replacement symlink");
    }

    #[test]
    fn selected_input_replaced_by_fifo_before_open_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("selected.scip");
        std::fs::write(&path, b"abc").unwrap();
        let result = read_semantic_input_before_open(root.path(), &path, 3, || {
            std::fs::remove_file(&path).unwrap();
            let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        });
        assert!(
            result.is_err(),
            "replacement FIFO must not block or be read"
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"next").unwrap();
        assert_eq!(read_semantic_input(root.path(), &path, 4).unwrap(), b"next");
    }

    #[test]
    fn artifact_copy_streams_and_hashes_in_bounded_chunks() {
        use std::io::Read;
        struct Spy<R>(R, usize);
        impl<R: Read> Read for Spy<R> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.1 = self.1.max(buffer.len());
                self.0.read(buffer)
            }
        }
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("big.scip");
        let bytes: Vec<u8> = (0..(2 * 1024 * 1024 + 17))
            .map(|i| (i % 251) as u8)
            .collect();
        std::fs::write(&source, &bytes).unwrap();
        let dest = root.path().join("copy.scip");
        let mut spy = Spy(std::fs::File::open(&source).unwrap(), 0);
        let mut totals = vec![];
        let copied = super::copy_stream_to_stage(&mut spy, &dest, 3 * 1024 * 1024, 1 << 20, |t| {
            totals.push(t);
            Ok(())
        })
        .unwrap();
        assert_eq!(copied.len, bytes.len() as u64);
        assert_eq!(copied.sha256, hex::encode(sha2::Sha256::digest(&bytes)));
        assert_eq!(std::fs::read(&dest).unwrap(), bytes);
        assert_eq!(spy.1, 1 << 20, "no read larger than one chunk");
        assert!(totals.len() >= 3);
        let over = root.path().join("over.scip");
        let error =
            super::copy_stream_to_stage(&bytes[..], &over, 1024, 1 << 20, |_| Ok(())).unwrap_err();
        assert!(error.to_string().contains("artifact bytes"));
        let via_file = root.path().join("via.scip");
        let copied =
            super::copy_semantic_input_to_stage(root.path(), &source, &via_file, 4 << 20, |_| {
                Ok(())
            })
            .unwrap();
        assert_eq!(copied.len, bytes.len() as u64);
        let symlink = root.path().join("link.scip");
        std::os::unix::fs::symlink(&source, &symlink).unwrap();
        assert!(
            super::copy_semantic_input_to_stage(
                root.path(),
                &symlink,
                &root.path().join("x"),
                4 << 20,
                |_| Ok(())
            )
            .is_err()
        );
    }

    #[test]
    fn bounded_regular_input_accepts_limit_and_rejects_limit_plus_one() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("selected.scip");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(read_semantic_input(root.path(), &path, 3).unwrap(), b"abc");
        assert!(read_semantic_input(root.path(), &path, 2).is_err());
        let fifo = root.path().join("pipe.scip");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        assert!(read_semantic_input(root.path(), &fifo, 3).is_err());
    }
}

/// Maps a typed admission failure to its typed failure code. The terminal row records that code
/// with a reason from a closed set; only the diagnostics show the full cause.
fn batch_error(error: anyhow::Error) -> anyhow::Error {
    use crate::scip_raw::ScipCaptureError;
    use crate::store::requests::TypedFailure;
    let typed = |code: &'static str, detail: &str| {
        anyhow::Error::from(TypedFailure {
            code,
            detail: detail.to_owned(),
        })
    };
    match error.downcast_ref::<ScipCaptureError>() {
        Some(ScipCaptureError::InvalidAdmission(detail)) => typed("invalid_request", detail),
        Some(ScipCaptureError::InvalidArtifact(detail)) => typed("invalid_artifact", detail),
        Some(ScipCaptureError::LimitExceeded(detail)) => typed("limit_exceeded", detail),
        Some(ScipCaptureError::Cancelled) => anyhow::anyhow!("index publication cancelled"),
        _ => crate::store::typed_storage_full(error),
    }
}

fn run_semantic_batch_request(
    store: &Store,
    session: &Arc<WriterSession>,
    request: &Request,
    watcher: Option<&OwningWatcher>,
    cancel: &CancelFlag,
    before_publish: &mut Option<Option<IndexPin>>,
) -> Result<IndexPin> {
    use crate::{
        semantic_batch::{
            BatchCounters, BatchLimits, LookupBudget, StageInput, StageOwner, stage_batch,
        },
        semantic_import::{DestinationMutations, join_staged},
        store::requests::SemanticImportJob,
    };
    use std::path::Path;
    let job = request.semantic_job()?;
    let root = Path::new(store.workspace_root());
    // The worker derives the input root. An external import reads the workspace; a generated
    // one reads its private generation directory, never a caller-supplied path.
    let input = match &job {
        SemanticImportJob::External(_) => StageInput::external(root),
        SemanticImportJob::Generated(options) => {
            let dir = store
                .producer_generation_dir(options.origin, &options.generation_id)
                .map_err(batch_error)?;
            StageInput::generated(root, options.origin, &options.generation_id, &dir)
        }
    }
    .map_err(|error| batch_error(error.into()))?;
    let limits =
        BatchLimits::effective().map_err(|error| anyhow::anyhow!("invalid_request: {error}"))?;
    // One budget serves every attempt below, so a destination change cannot spend it twice.
    let mut budget = LookupBudget::new(&limits);
    let mut counters = BatchCounters::default();
    let incarnation = session.incarnation().to_string();
    let mut admitted = stage_batch(
        store,
        &input,
        StageOwner {
            request_id: &request.id,
            incarnation: &incarnation,
        },
        Path::new(job.descriptor()),
        job.descriptor_sha256(),
        &limits,
        &mut counters,
        cancel,
    )
    .map_err(batch_error)?;
    let mut native_options = store
        .recorded_index_options()?
        .unwrap_or_else(|| IndexOptions::new(root.to_owned()));
    native_options.workspace_root = root.to_owned();
    for _ in 0..8 {
        store.verify_writer_session(session)?;
        let destination_pin = store.index_baseline()?;
        // A busy COMMIT is requeued only against the pin this attempt published from;
        // a watcher publication in an earlier attempt moved the head past the first one.
        *before_publish = Some(Some(destination_pin));
        // Destination edits apply only once the native revision differs from the captured one.
        #[cfg(feature = "test-support")]
        let mutations = {
            let edits = DestinationMutations::from_env();
            let destination_revision = store.with_native_reader(destination_pin, |reader| {
                Ok(reader.header()?.revision.id.clone())
            })?;
            if edits.is_empty() || destination_revision == admitted.descriptor().source_revision {
                DestinationMutations::default()
            } else {
                edits
            }
        };
        #[cfg(not(feature = "test-support"))]
        let mutations = DestinationMutations::default();
        let joined = join_staged(
            &mut admitted,
            store,
            destination_pin,
            &limits,
            &mut budget,
            &mut counters,
            mutations,
            cancel,
        )
        .map_err(batch_error)?;
        #[cfg(all(feature = "test-support", unix))]
        if store.with_native_reader(destination_pin, |reader| {
            Ok(reader.header()?.revision.id != admitted.descriptor().source_revision)
        })? && let Some(path) = std::env::var_os("TRELLIS_TEST_SEMANTIC_CUTOFF_SOCKET")
        {
            use std::os::unix::net::UnixListener;
            let path = std::path::PathBuf::from(path);
            if !path.exists() {
                let listener = UnixListener::bind(&path)?;
                std::fs::write(path.with_extension("ready"), [])?;
                let (_stream, _) = listener.accept()?;
            }
        }
        // The owner drains accepted watcher intent before the import cutoff.
        if let Some(watcher) = watcher {
            let mut held = watcher.lock().unwrap();
            if let Some((owner, work)) = held.as_mut()
                && owner
                    .upgrade()
                    .is_some_and(|owner| Arc::ptr_eq(&owner, session))
            {
                work.reconcile_due(store, session, &native_options, cancel, true)?;
            }
        }
        if store.index_baseline()? != destination_pin {
            continue;
        }
        // A generated batch is bound to the revision its generator read, so a head that moved
        // before or at the cutoff is terminal. Repeated on every revision-conflict retry.
        if matches!(job, SemanticImportJob::Generated(_))
            && store.with_native_reader(destination_pin, |reader| {
                Ok(reader.header()?.revision.id != admitted.descriptor().source_revision)
            })?
        {
            anyhow::bail!("stale_basis: head revision changed");
        }
        let generation = watcher.and_then(|watcher| {
            watcher
                .lock()
                .ok()
                .and_then(|held| held.as_ref().map(|(_, work)| work.generation_handle()))
        });
        let before_generation = generation.as_ref().map(|g| g.load(Ordering::SeqCst));
        let capture = Capture::admit(&native_options, cancel, &|_| {})?;
        let baseline = store.publication_index_baseline()?;
        if baseline.pin() != Some(destination_pin)
            || !store.selected_capture_unchanged(
                &capture,
                session.writer_guard()?,
                &baseline,
                cancel,
            )?
            || generation.as_ref().map(|g| g.load(Ordering::SeqCst)) != before_generation
        {
            if let Some(watcher) = watcher {
                let mut held = watcher.lock().unwrap();
                if let Some((owner, work)) = held.as_mut()
                    && owner
                        .upgrade()
                        .is_some_and(|owner| Arc::ptr_eq(&owner, session))
                {
                    work.reconcile_due(store, session, &native_options, cancel, true)?;
                    continue;
                }
            }
            anyhow::bail!("revision conflict: semantic import native reconciliation required");
        }
        let publication = store.enter_publication(cancel, std::time::Duration::from_millis(250))?;
        if generation.as_ref().map(|g| g.load(Ordering::SeqCst)) != before_generation
            || store.index_baseline()? != destination_pin
        {
            drop(publication);
            continue;
        }
        #[cfg(feature = "test-support")]
        if std::env::var_os("TRELLIS_TEST_SEMANTIC_BUSY_ONCE").is_some_and(|marker| {
            let marker = std::path::PathBuf::from(marker);
            if marker.exists() {
                false
            } else {
                std::fs::write(marker, b"busy injected").is_ok()
            }
        }) {
            drop(publication);
            return Err(crate::store::topology::StorageBusy.into());
        }
        #[cfg(feature = "test-support")]
        let pin = if std::env::var_os("TRELLIS_TEST_SEMANTIC_SQL_FAULT").is_some_and(|marker| {
            let marker = std::path::PathBuf::from(marker);
            if marker.exists() {
                false
            } else {
                std::fs::write(marker, b"fault injected").is_ok()
            }
        }) {
            store.publish_semantic_batch_with_fault_for_tests(
                &joined,
                admitted.stage_mut(),
                destination_pin,
                session.writer_guard()?,
                cancel,
                crate::store::BatchPublishFault::SqlFault,
            )
        } else {
            store.publish_semantic_batch(
                &joined,
                admitted.stage_mut(),
                destination_pin,
                session.writer_guard()?,
                cancel,
            )
        };
        #[cfg(not(feature = "test-support"))]
        let pin = store.publish_semantic_batch(
            &joined,
            admitted.stage_mut(),
            destination_pin,
            session.writer_guard()?,
            cancel,
        );
        #[cfg(all(feature = "test-support", unix))]
        if pin.is_ok()
            && let Some(path) = std::env::var_os("TRELLIS_TEST_SEMANTIC_CRASH_AFTER_PUBLISH_SOCKET")
        {
            use std::os::unix::net::UnixListener;
            let path = std::path::PathBuf::from(path);
            if !path.with_extension("done").exists() {
                let listener = UnixListener::bind(&path)?;
                let (_stream, _) = listener.accept()?;
            }
        }
        drop(publication);
        match pin {
            Err(error) if error.to_string().starts_with("revision conflict") => continue,
            result => return result.map_err(batch_error),
        }
    }
    anyhow::bail!("storage_busy: semantic import retry budget exceeded")
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
    store
        .maintenance_step(writer, &permit, &probe, priority)
        .map_err(|error| store.report_index_failure(error))
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
        sweep_abandoned_semantic_stages(store);
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
    sweep_abandoned_semantic_stages(store);
    Ok(session)
}

/// Startup reconciliation has committed and no attempt is active, so every stage directory
/// left in the index directory belongs to a crashed attempt.
fn sweep_abandoned_semantic_stages(store: &Store) {
    if let Ok(index_dir) = store.semantic_stage_parent() {
        crate::semantic_batch::staging::remove_abandoned_semantic_stages(&index_dir);
    }
    // Producer runs and pickup directories a crashed daemon left, except those a queued or
    // running generated import still names (D2). Nothing is importing yet at this point.
    crate::producer::run::sweep_state(store, false);
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod queue_completion_retry_tests;

#[cfg(test)]
mod marker_tests;
